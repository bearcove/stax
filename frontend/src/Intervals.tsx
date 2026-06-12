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
  threads,
  onSelectTid,
}: {
  client: ProfilerClient;
  flameKey: string;
  tid: number | null;
  filter: LiveFilter;
  threads: ThreadInfo[];
  onSelectTid: (tid: number) => void;
}) {
  const [update, setUpdate] = useState<IntervalListUpdate | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    const [tx, rx] = channel<IntervalListUpdate>();
    client
      .subscribeIntervals(flameKey, viewParams(tid, filter), tx)
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
  }, [client, flameKey, tid, filter]);

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
  threads,
  onSelectTid,
  onSelectOrigin,
}: {
  client: ProfilerClient;
  flameKey: string;
  tid: number | null;
  filter: LiveFilter;
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
      .subscribeTargetSpans(flameKey, viewParams(tid, filter), tx)
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
  }, [client, flameKey, tid, filter]);

  if (!update) {
    return <div className="placeholder">streaming target spans…</div>;
  }

  const threadName = (t: number) =>
    threads.find((th) => th.tid === t)?.name ?? null;
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
  onSelectOrigin,
}: {
  group: TargetSpanGroup;
  strings: string[];
  threadName: (tid: number) => string | null;
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
      <td className="col-lane">{lane}</td>
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
  onSelectOrigin,
}: {
  entry: TargetSpanEntry;
  strings: string[];
  threadName: (tid: number) => string | null;
  onSelectOrigin: (tid: number, address: bigint | null) => void;
}) {
  const startSec = (Number(entry.start_ns) / 1e9).toFixed(3);
  const lane = entry.lane_name != null ? strings[entry.lane_name] : "(lane)";
  const span = entry.span_name != null ? strings[entry.span_name] : "(span)";
  return (
    <tr>
      <td className="col-start">{startSec}s</td>
      <td className="col-duration">{formatDuration(entry.duration_ns)}</td>
      <td className="col-lane">{lane}</td>
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
