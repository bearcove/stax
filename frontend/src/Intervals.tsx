import { useEffect, useState } from "react";
import { channel } from "@bearcove/vox-core";
import type {
  IntervalEntry,
  IntervalListUpdate,
  LiveFilter,
  ProfilerClient,
  TargetSpanEntry,
  TargetSpanGroup,
  TargetSpanListUpdate,
  ThreadInfo,
} from "./generated/profiler.generated.ts";
import {
  formatDuration,
  offCpuTotal,
  reasonKeyOfTag,
  reasonSegments,
  REASON_LABEL,
  type ReasonKey,
} from "./wire.ts";
import { viewParams } from "./App.tsx";

const SYNTH_TID_BASE = 0xfff00000;

/// Drill-down view of every off-CPU interval attributed to the
/// selected flame subtree. Each row carries: time the wait started,
/// duration, reason classification, and the waker (which thread +
/// what function pulled this thread back onto a CPU). Sorted
/// newest-first; the first batch arrives ~immediately and updates
/// streaming.
export function IntervalsPanel({
  client,
  flameKey,
  tid,
  filter,
  runId,
  threads,
  onSelectTid,
}: {
  client: ProfilerClient;
  flameKey: string;
  tid: number | null;
  filter: LiveFilter;
  runId: bigint | null;
  threads: ThreadInfo[];
  onSelectTid: (tid: number) => void;
}) {
  const [update, setUpdate] = useState<IntervalListUpdate | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    const [tx, rx] = channel<IntervalListUpdate>();
    client
      .subscribeIntervals(flameKey, viewParams(tid, filter, runId), tx)
      .catch(() => {});
    (async () => {
      for await (const next of rx) {
        if (cancelled) break;
        setUpdate(next);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client, flameKey, tid, filter, runId]);

  if (!update) {
    return <div className="placeholder">streaming intervals…</div>;
  }
  if (update.entries.length === 0) {
    return (
      <div className="placeholder">
        no off-CPU intervals attributed to this stack yet
      </div>
    );
  }

  const segs = reasonSegments(update.by_reason);
  const offTotal = offCpuTotal(update.by_reason);
  const wakeeName = (t: number) =>
    threads.find((th) => th.tid === t)?.name ?? null;

  return (
    <div className="intervals-pane">
      <div className="intervals-header">
        <span>
          <strong>{update.total_intervals.toString()}</strong> intervals ·{" "}
          {formatDuration(update.total_duration_ns)} total
        </span>
        <span className="intervals-header-meta">
          showing {update.entries.length} most recent
        </span>
        {segs.map((s) => {
          const pct =
            offTotal === 0n
              ? 0
              : Math.round((Number(s.ns) / Number(offTotal)) * 1000) / 10;
          return (
            <span
              key={s.reason}
              className={`reason-chip reason-chip--${s.reason}`}
              title={`${formatDuration(s.ns)} · ${pct.toFixed(1)}% of off-CPU`}
            >
              <span className="reason-chip-name">{REASON_LABEL[s.reason]}</span>
              <span className="reason-chip-value">{formatDuration(s.ns)}</span>
            </span>
          );
        })}
      </div>
      <div className="intervals-body">
        <table className="intervals-table">
          <thead>
            <tr>
              <th>start</th>
              <th>duration</th>
              <th>reason</th>
              <th>tid</th>
              <th>woken by</th>
            </tr>
          </thead>
          <tbody>
            {update.entries.map((e, i) => (
              <IntervalRow
                key={i}
                entry={e}
                strings={update.strings}
                wakeeName={wakeeName}
                onSelectTid={onSelectTid}
              />
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

export function TargetSpansPanel({
  client,
  flameKey,
  tid,
  filter,
  runId,
  threads,
  onSelectTid,
  onSelectOrigin,
}: {
  client: ProfilerClient;
  flameKey: string;
  tid: number | null;
  filter: LiveFilter;
  runId: bigint | null;
  threads: ThreadInfo[];
  onSelectTid: (tid: number) => void;
  onSelectOrigin: (tid: number, address: bigint | null) => void;
}) {
  const [update, setUpdate] = useState<TargetSpanListUpdate | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    const [tx, rx] = channel<TargetSpanListUpdate>();
    client
      .subscribeTargetSpans(flameKey, viewParams(tid, filter, runId), tx)
      .catch(() => {});
    (async () => {
      for await (const next of rx) {
        if (cancelled) break;
        setUpdate(next);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client, flameKey, tid, filter, runId]);

  if (!update) {
    return <div className="placeholder">streaming target spans…</div>;
  }

  const threadName = (t: number) =>
    threads.find((th) => th.tid === t)?.name ?? null;
  const summary = buildTargetSummary(update);
  const targetLanes = threads
    .filter((thread) => thread.tid >= SYNTH_TID_BASE && thread.target_spans > 0n)
    .sort((a, b) => {
      if (a.target_ns !== b.target_ns) {
        return a.target_ns > b.target_ns ? -1 : 1;
      }
      if (a.target_spans !== b.target_spans) {
        return a.target_spans > b.target_spans ? -1 : 1;
      }
      return a.tid - b.tid;
    })
    .slice(0, 4);

  if (update.entries.length === 0 && update.groups.length === 0) {
    return (
      <TargetSpansEmptyState
        tid={tid}
        selectedName={tid == null ? null : threadName(tid)}
        lanes={targetLanes}
        onSelectTid={onSelectTid}
      />
    );
  }

  return (
    <div className="intervals-pane target-spans-pane">
      <div className="intervals-header">
        <span>
          <strong>{update.total_spans.toString()}</strong> spans ·{" "}
          {formatDuration(update.total_duration_ns)} total
        </span>
        <span className="intervals-header-meta">
          {update.groups.length} groups · showing {update.entries.length} most
          recent
        </span>
      </div>
      <TargetSummaryStrip
        summary={summary}
        strings={update.strings}
        threadName={threadName}
        onSelectTid={onSelectTid}
        onSelectOrigin={onSelectOrigin}
      />
      <div className="intervals-body">
        <table className="intervals-table target-spans-table target-groups-table">
          <caption>top target work</caption>
          <thead>
            <tr>
              <th>count</th>
              <th>total</th>
              <th>max</th>
              <th>lane</th>
              <th>span</th>
              <th>origin</th>
            </tr>
          </thead>
          <tbody>
            {update.groups.map((group, i) => (
              <TargetSpanGroupRow
                key={i}
                group={group}
                strings={update.strings}
                threadName={threadName}
                onSelectTid={onSelectTid}
                onSelectOrigin={onSelectOrigin}
              />
            ))}
          </tbody>
        </table>
        <table className="intervals-table target-spans-table">
          <caption>recent target spans</caption>
          <thead>
            <tr>
              <th>start</th>
              <th>duration</th>
              <th>lane</th>
              <th>span</th>
              <th>origin</th>
            </tr>
          </thead>
          <tbody>
            {update.entries.map((e, i) => (
              <TargetSpanRow
                key={i}
                entry={e}
                strings={update.strings}
                threadName={threadName}
                onSelectTid={onSelectTid}
                onSelectOrigin={onSelectOrigin}
              />
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function TargetSpansEmptyState({
  tid,
  selectedName,
  lanes,
  onSelectTid,
}: {
  tid: number | null;
  selectedName: string | null;
  lanes: ThreadInfo[];
  onSelectTid: (tid: number) => void;
}) {
  const selectedLabel =
    tid == null ? "this view" : `${selectedName ?? "tid"} ${tid}`;
  const hasLanes = lanes.length > 0;
  const realThreadSelected = tid != null && tid < SYNTH_TID_BASE;
  const title = hasLanes
    ? realThreadSelected
      ? "no linked target spans for this CPU thread"
      : "target spans are on another lane"
    : "no target spans reported yet";
  const detail = hasLanes
    ? `Target lanes with spans exist outside ${selectedLabel}.`
    : "A cooperating target has not reported spans for the active run.";
  return (
    <div className="placeholder target-empty-state">
      <div className="target-empty-title">{title}</div>
      <div className="target-empty-detail">{detail}</div>
      {hasLanes ? (
        <div className="target-empty-lanes">
          {lanes.map((lane) => (
            <button
              key={lane.tid}
              type="button"
              className="target-empty-lane"
              onClick={() => onSelectTid(lane.tid)}
              title={`tid ${lane.tid}`}
            >
              <span className="target-empty-lane-name">
                {lane.name ?? `tid ${lane.tid}`}
              </span>
              <span className="target-empty-lane-meta">
                {formatDuration(lane.target_ns)} ·{" "}
                {lane.target_spans.toString()} spans
              </span>
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}

function TargetSpanGroupRow({
  group,
  strings,
  threadName,
  onSelectTid,
  onSelectOrigin,
}: {
  group: TargetSpanGroup;
  strings: string[];
  threadName: (tid: number) => string | null;
  onSelectTid: (tid: number) => void;
  onSelectOrigin: (tid: number, address: bigint | null) => void;
}) {
  const lane = group.lane_name != null ? strings[group.lane_name] : "(lane)";
  const span = group.span_name != null ? strings[group.span_name] : "(span)";
  return (
    <tr>
      <td className="col-count">{group.count.toString()}</td>
      <td className="col-duration">
        {formatDuration(group.total_duration_ns)}
      </td>
      <td className="col-duration">{formatDuration(group.max_duration_ns)}</td>
      <LaneCell tid={group.tid} lane={lane} onSelectTid={onSelectTid} />
      <td className="col-span">{span}</td>
      <OriginCell
        originTid={group.origin_tid}
        originLinked={group.origin_linked}
        originAddress={group.origin_address}
        originFunctionName={group.origin_function_name}
        originBinary={group.origin_binary}
        strings={strings}
        threadName={threadName}
        onSelectOrigin={onSelectOrigin}
      />
    </tr>
  );
}

function TargetSpanRow({
  entry,
  strings,
  threadName,
  onSelectTid,
  onSelectOrigin,
}: {
  entry: TargetSpanEntry;
  strings: string[];
  threadName: (tid: number) => string | null;
  onSelectTid: (tid: number) => void;
  onSelectOrigin: (tid: number, address: bigint | null) => void;
}) {
  const startSec = (Number(entry.start_ns) / 1e9).toFixed(3);
  const lane = entry.lane_name != null ? strings[entry.lane_name] : "(lane)";
  const span = entry.span_name != null ? strings[entry.span_name] : "(span)";
  return (
    <tr>
      <td className="col-start">{startSec}s</td>
      <td className="col-duration">{formatDuration(entry.duration_ns)}</td>
      <LaneCell tid={entry.tid} lane={lane} onSelectTid={onSelectTid} />
      <td className="col-span">{span}</td>
      <OriginCell
        originTid={entry.origin_tid}
        originLinked={entry.origin_linked}
        originAddress={entry.origin_address}
        originFunctionName={entry.origin_function_name}
        originBinary={entry.origin_binary}
        strings={strings}
        threadName={threadName}
        onSelectOrigin={onSelectOrigin}
      />
    </tr>
  );
}

type LaneSummary = {
  tid: number;
  laneName: number | null;
  totalDurationNs: bigint;
  count: bigint;
  maxDurationNs: bigint;
};

type OriginSummary = {
  originTid: number;
  originLinked: boolean;
  originAddress: bigint | null;
  originFunctionName: number | null;
  originBinary: number | null;
  totalDurationNs: bigint;
  count: bigint;
  maxDurationNs: bigint;
};

type TargetSummary = {
  topLane: LaneSummary | null;
  topSpan: TargetSpanGroup | null;
  topOrigin: OriginSummary | null;
  originSpans: bigint;
  linkedOriginSpans: bigint;
};

function buildTargetSummary(update: TargetSpanListUpdate): TargetSummary {
  const lanes = new Map<string, LaneSummary>();
  const origins = new Map<string, OriginSummary>();
  let originSpans = 0n;
  let linkedOriginSpans = 0n;

  for (const group of update.groups) {
    const laneKey = `${group.tid}:${group.lane_name ?? ""}`;
    const lane =
      lanes.get(laneKey) ??
      {
        tid: group.tid,
        laneName: group.lane_name,
        totalDurationNs: 0n,
        count: 0n,
        maxDurationNs: 0n,
      };
    lane.totalDurationNs += group.total_duration_ns;
    lane.count += group.count;
    if (group.max_duration_ns > lane.maxDurationNs) {
      lane.maxDurationNs = group.max_duration_ns;
    }
    lanes.set(laneKey, lane);

    if (group.origin_tid != null) {
      originSpans += group.count;
      if (group.origin_linked) linkedOriginSpans += group.count;
      const originKey = [
        group.origin_tid,
        group.origin_linked ? "linked" : "unlinked",
        group.origin_address?.toString() ?? "",
        group.origin_function_name ?? "",
        group.origin_binary ?? "",
      ].join(":");
      const origin =
        origins.get(originKey) ??
        {
          originTid: group.origin_tid,
          originLinked: group.origin_linked,
          originAddress: group.origin_address,
          originFunctionName: group.origin_function_name,
          originBinary: group.origin_binary,
          totalDurationNs: 0n,
          count: 0n,
          maxDurationNs: 0n,
        };
      origin.totalDurationNs += group.total_duration_ns;
      origin.count += group.count;
      if (group.max_duration_ns > origin.maxDurationNs) {
        origin.maxDurationNs = group.max_duration_ns;
      }
      origins.set(originKey, origin);
    }
  }

  const byDuration = <T extends { totalDurationNs: bigint; count: bigint }>(
    a: T,
    b: T,
  ) =>
    a.totalDurationNs === b.totalDurationNs
      ? a.count === b.count
        ? 0
        : a.count > b.count
          ? -1
          : 1
      : a.totalDurationNs > b.totalDurationNs
        ? -1
        : 1;

  return {
    topLane: [...lanes.values()].sort(byDuration)[0] ?? null,
    topSpan: update.groups[0] ?? null,
    topOrigin: [...origins.values()].sort(byDuration)[0] ?? null,
    originSpans,
    linkedOriginSpans,
  };
}

function TargetSummaryStrip({
  summary,
  strings,
  threadName,
  onSelectTid,
  onSelectOrigin,
}: {
  summary: TargetSummary;
  strings: string[];
  threadName: (tid: number) => string | null;
  onSelectTid: (tid: number) => void;
  onSelectOrigin: (tid: number, address: bigint | null) => void;
}) {
  const laneName =
    summary.topLane?.laneName != null
      ? strings[summary.topLane.laneName]
      : "(lane)";
  const spanName =
    summary.topSpan?.span_name != null
      ? strings[summary.topSpan.span_name]
      : "(span)";
  const spanLaneName =
    summary.topSpan?.lane_name != null
      ? strings[summary.topSpan.lane_name]
      : "(lane)";
  const originFn =
    summary.topOrigin?.originFunctionName != null
      ? strings[summary.topOrigin.originFunctionName]
      : null;
  const originBin =
    summary.topOrigin?.originBinary != null
      ? strings[summary.topOrigin.originBinary]
      : null;
  const originThreadName =
    summary.topOrigin != null ? threadName(summary.topOrigin.originTid) : null;
  const originLabel =
    originFn ??
    (summary.topOrigin?.originLinked ? "(linked origin)" : "(unlinked origin)");

  return (
    <div className="target-summary-strip">
      {summary.topLane ? (
        <button
          type="button"
          className="target-summary-card clickable"
          onClick={() => onSelectTid(summary.topLane!.tid)}
          title={`tid ${summary.topLane.tid}`}
        >
          <span className="target-summary-label">top lane</span>
          <span className="target-summary-value">{laneName}</span>
          <span className="target-summary-meta">
            {formatDuration(summary.topLane.totalDurationNs)} ·{" "}
            {summary.topLane.count.toString()} spans
          </span>
        </button>
      ) : null}
      {summary.topSpan ? (
        <div className="target-summary-card">
          <span className="target-summary-label">top span</span>
          <span className="target-summary-value">{spanName}</span>
          <span className="target-summary-meta">
            {spanLaneName} · {formatDuration(summary.topSpan.total_duration_ns)} ·{" "}
            {summary.topSpan.count.toString()} spans
          </span>
        </div>
      ) : null}
      {summary.topOrigin ? (
        <button
          type="button"
          className="target-summary-card clickable"
          onClick={() =>
            onSelectOrigin(
              summary.topOrigin!.originTid,
              summary.topOrigin!.originAddress,
            )
          }
          title={
            originBin
              ? `${originLabel} · ${originBin}`
              : `${originLabel} · ${originThreadName ?? `tid ${summary.topOrigin.originTid}`}`
          }
        >
          <span className="target-summary-label">top origin</span>
          <span className="target-summary-value">{originLabel}</span>
          <span className="target-summary-meta">
            {originThreadName ?? `tid ${summary.topOrigin.originTid}`} ·{" "}
            {formatDuration(summary.topOrigin.totalDurationNs)}
          </span>
        </button>
      ) : (
        <div className="target-summary-card">
          <span className="target-summary-label">top origin</span>
          <span className="target-summary-value muted">(none)</span>
          <span className="target-summary-meta">0 spans with origin</span>
        </div>
      )}
      <div className="target-summary-card">
        <span className="target-summary-label">origin coverage</span>
        <span className="target-summary-value">
          {summary.linkedOriginSpans.toString()} /{" "}
          {summary.originSpans.toString()} linked
        </span>
        <span className="target-summary-meta">
          {summary.originSpans.toString()} spans with origin
        </span>
      </div>
    </div>
  );
}

function LaneCell({
  tid,
  lane,
  onSelectTid,
}: {
  tid: number;
  lane: string;
  onSelectTid: (tid: number) => void;
}) {
  return (
    <td className="col-lane">
      <button
        type="button"
        className="target-lane-link"
        onClick={() => onSelectTid(tid)}
        title={`tid ${tid}`}
      >
        {lane}
      </button>
    </td>
  );
}

function OriginCell({
  originTid,
  originLinked,
  originAddress,
  originFunctionName,
  originBinary,
  strings,
  threadName,
  onSelectOrigin,
}: {
  originTid: number | null;
  originLinked: boolean;
  originAddress: bigint | null;
  originFunctionName: number | null;
  originBinary: number | null;
  strings: string[];
  threadName: (tid: number) => string | null;
  onSelectOrigin: (tid: number, address: bigint | null) => void;
}) {
  const originFn =
    originFunctionName != null ? strings[originFunctionName] : null;
  const originBin = originBinary != null ? strings[originBinary] : null;
  const originThreadName = originTid != null ? threadName(originTid) : null;
  return (
    <td className={`col-origin${originTid == null ? " empty" : ""}`}>
      {originTid != null ? (
        <button
          type="button"
          className="waker-link"
          onClick={() => onSelectOrigin(originTid, originAddress)}
          title={
            originBin
              ? `${originFn ?? "(unresolved)"} · ${originBin}`
              : originFn ?? `tid ${originTid}`
          }
        >
          {originFn ??
            (originLinked ? "(linked origin)" : "(unlinked origin)")}
          <span className="waker-tid">
            {" "}
            · {originThreadName ?? `tid ${originTid}`}
          </span>
        </button>
      ) : (
        "(none)"
      )}
    </td>
  );
}

function IntervalRow({
  entry,
  strings,
  wakeeName,
  onSelectTid,
}: {
  entry: IntervalEntry;
  strings: string[];
  wakeeName: (tid: number) => string | null;
  onSelectTid: (tid: number) => void;
}) {
  const reasonKey: ReasonKey = reasonKeyOfTag(entry.reason.tag);
  const wakerFn =
    entry.waker_function_name != null
      ? strings[entry.waker_function_name]
      : null;
  const wakerBin =
    entry.waker_binary != null ? strings[entry.waker_binary] : null;
  const wakerThreadName =
    entry.waker_tid != null ? wakeeName(entry.waker_tid) : null;
  const startSec = (Number(entry.start_ns) / 1e9).toFixed(3);
  return (
    <tr>
      <td className="col-start">{startSec}s</td>
      <td className="col-duration">{formatDuration(entry.duration_ns)}</td>
      <td
        className="col-reason"
        style={{ ["--reason-color" as string]: `var(--reason-${reasonKey})` }}
      >
        {REASON_LABEL[reasonKey]}
      </td>
      <td className="col-tid">{entry.tid}</td>
      <td
        className={`col-waker${wakerFn || entry.waker_tid != null ? "" : " empty"}`}
      >
        {entry.waker_tid != null ? (
          <button
            type="button"
            className="waker-link"
            onClick={() => onSelectTid(entry.waker_tid!)}
            title={
              wakerBin
                ? `${wakerFn ?? "(unresolved)"} · ${wakerBin}`
                : (wakerFn ?? `0x${(entry.waker_address ?? 0n).toString(16)}`)
            }
          >
            {wakerFn ??
              `0x${(entry.waker_address ?? 0n).toString(16)}`}
            <span className="waker-tid">
              {" "}
              · {wakerThreadName ?? `tid ${entry.waker_tid}`}
            </span>
          </button>
        ) : (
          "(none)"
        )}
      </td>
    </tr>
  );
}
