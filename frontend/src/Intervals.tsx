import { useEffect, useState } from "react";
import { channel } from "@bearcove/vox-core";
import type {
  IntervalEntry,
  IntervalListUpdate,
  LiveFilter,
  ProfilerClient,
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
