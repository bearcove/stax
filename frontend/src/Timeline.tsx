import { useEffect, useRef, useState } from "react";
import { channel } from "@bearcove/vox-core";
import type {
  ProfilerClient,
  TargetLaneTimeline,
  TimelineBucket,
  TimeRange,
  TimelineUpdate,
} from "./generated/profiler.generated.ts";
import type { DisplayMode } from "./App.tsx";
import { timelineParams } from "./App.tsx";
import { cpuOnlyNs, formatDuration } from "./wire.ts";

/// Compact timeline strip across the top of the page. Each bucket is
/// drawn as a compact area; height is proportional to the selected
/// display metric relative to the busiest bucket in view. Drag across
/// the bars to brush-select a time range; click to clear.
export function Timeline({
  client,
  tid,
  range,
  onRangeChange,
  displayMode,
  onSelectTid,
  runId,
}: {
  client: ProfilerClient;
  tid: number | null;
  range: TimeRange | null;
  onRangeChange: (r: TimeRange | null) => void;
  displayMode: DisplayMode;
  onSelectTid: (tid: number) => void;
  runId: bigint | null;
}) {
  const [update, setUpdate] = useState<TimelineUpdate | null>(null);
  const barsRef = useRef<SVGSVGElement | null>(null);
  /// Live drag state: start/current x as fractions of the bars width.
  /// `null` when the user isn't currently dragging.
  const [drag, setDrag] = useState<{ x0: number; x1: number } | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    const [tx, rx] = channel<TimelineUpdate>();
    client.subscribeTimeline(timelineParams(tid, runId), tx).catch(() => {});
    (async () => {
      for await (const next of rx) {
        if (cancelled) break;
        setUpdate(next);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client, tid, runId]);

  if (!update || update.buckets.length === 0) {
    return <div className="timeline placeholder">timeline (waiting for samples…)</div>;
  }

  // The same display mode drives the flamegraph, thread selector, and
  // this strip so a target-lane investigation can make target time the
  // dominant visual signal everywhere at once.
  const max = update.buckets.reduce((m, b) => {
    const value = bucketMetricNs(b, displayMode);
    return value > m ? value : m;
  }, 0n);
  const maxF = max === 0n ? 1 : Number(max);
  const durSec = Number(update.recording_duration_ns) / 1e9;
  const durNs = update.recording_duration_ns;

  /// Map a clientX coordinate inside `barsRef` to a [0,1] fraction
  /// along the bars row, clamped to the visible area.
  const fracOf = (clientX: number): number => {
    const el = barsRef.current;
    if (!el) return 0;
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0) return 0;
    return Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
  };

  const fracToNs = (f: number): bigint => {
    if (durNs === 0n) return 0n;
    return BigInt(Math.round(f * Number(durNs)));
  };

  const onMouseDown = (e: React.MouseEvent) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const x0 = fracOf(e.clientX);
    setDrag({ x0, x1: x0 });

    const onMove = (ev: MouseEvent) => {
      setDrag({ x0, x1: fracOf(ev.clientX) });
    };
    const onUp = (ev: MouseEvent) => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      const x1 = fracOf(ev.clientX);
      setDrag(null);
      const lo = Math.min(x0, x1);
      const hi = Math.max(x0, x1);
      // Treat tiny drags (< ~0.5% of width) as clicks → clear the range.
      if (hi - lo < 0.005) {
        if (range) onRangeChange(null);
        return;
      }
      onRangeChange({ start_ns: fracToNs(lo), end_ns: fracToNs(hi) });
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };

  // Selection overlay — prefer the live drag state; fall back to the
  // committed range converted to fractions.
  const overlay = drag
    ? { lo: Math.min(drag.x0, drag.x1), hi: Math.max(drag.x0, drag.x1) }
    : range && durNs > 0n
      ? {
          lo: Number(range.start_ns) / Number(durNs),
          hi: Number(range.end_ns) / Number(durNs),
        }
      : null;

  // Build the area-chart path. Each bucket center sits at
  // (i + 0.5) / N along x; y is inverted (0 at top, 100 at bottom).
  // We start at the bottom-left, climb to each bucket's height,
  // then close back at the bottom-right -- producing a single
  // filled area instead of the discrete bars we used to draw.
  const n = update.buckets.length;
  const points: string[] = [];
  for (let i = 0; i < n; i++) {
    const x = ((i + 0.5) / n) * 100;
    const b = update.buckets[i];
    const value = Number(bucketMetricNs(b, displayMode));
    const y = max === 0n ? 100 : 100 - (value / maxF) * 100;
    points.push(`${x.toFixed(3)},${y.toFixed(3)}`);
  }
  const areaD =
    n === 0
      ? ""
      : `M 0,100 L ${points.join(" L ")} L 100,100 Z`;

  return (
    <div className="timeline">
      <svg
        ref={barsRef}
        className="timeline-graph"
        viewBox="0 0 100 100"
        preserveAspectRatio="none"
        onMouseDown={onMouseDown}
      >
        {areaD && <path className={`timeline-area mode-${displayMode}`} d={areaD} />}
        {overlay && (
          <rect
            className="timeline-brush"
            x={overlay.lo * 100}
            y={0}
            width={(overlay.hi - overlay.lo) * 100}
            height={100}
          />
        )}
      </svg>
      {update.target_lanes.length > 0 && (
        <TargetLaneTracks
          lanes={update.target_lanes}
          strings={update.strings}
          onSelectTid={onSelectTid}
        />
      )}
      <div className="timeline-footer">
        {displayModeLabel(displayMode)} ·{" "}
        {(
          Number(cpuOnlyNs(update.total_on_cpu_ns, update.total_target_ns)) / 1e9
        ).toFixed(2)}
        s CPU ·{" "}
        {(Number(update.total_on_cpu_ns) / 1e9).toFixed(2)}s active ·{" "}
        {(Number(update.total_target_ns) / 1e9).toFixed(2)}s target ·{" "}
        {(Number(update.total_off_cpu_ns) / 1e9).toFixed(2)}s off-CPU ·{" "}
        {durSec.toFixed(1)}s elapsed
        {range && (
          <>
            {" · "}
            <span className="timeline-range">
              brush {(Number(range.start_ns) / 1e9).toFixed(2)}s –{" "}
              {(Number(range.end_ns) / 1e9).toFixed(2)}s
            </span>{" "}
            <button
              className="timeline-clear"
              onClick={() => onRangeChange(null)}
              title="clear time-range filter"
            >
              clear
            </button>
          </>
        )}
      </div>
    </div>
  );
}

function TargetLaneTracks({
  lanes,
  strings,
  onSelectTid,
}: {
  lanes: TargetLaneTimeline[];
  strings: string[];
  onSelectTid: (tid: number) => void;
}) {
  const max = lanes.reduce(
    (m, lane) =>
      lane.buckets.reduce((lm, value) => (value > lm ? value : lm), m),
    0n,
  );
  const maxF = max === 0n ? 1 : Number(max);

  return (
    <div className="timeline-target-lanes">
      {lanes.map((lane) => {
        const label =
          lane.lane_name != null ? strings[lane.lane_name] : `tid ${lane.tid}`;
        const n = lane.buckets.length || 1;
        return (
          <button
            key={lane.tid}
            type="button"
            className="timeline-target-lane"
            onClick={() => onSelectTid(lane.tid)}
            title={`tid ${lane.tid} · ${formatDuration(lane.total_target_ns)} target · ${lane.target_spans.toString()} spans`}
          >
            <span className="timeline-target-lane-label">{label}</span>
            <svg
              className="timeline-target-lane-bars"
              viewBox="0 0 100 12"
              preserveAspectRatio="none"
              aria-hidden="true"
            >
              {lane.buckets.map((value, i) => {
                if (value === 0n) return null;
                const h = Math.max(1, (Number(value) / maxF) * 12);
                return (
                  <rect
                    key={i}
                    x={(i / n) * 100}
                    y={12 - h}
                    width={100 / n}
                    height={h}
                  />
                );
              })}
            </svg>
            <span className="timeline-target-lane-meta">
              {formatDuration(lane.total_target_ns)}
            </span>
          </button>
        );
      })}
    </div>
  );
}

function bucketMetricNs(bucket: TimelineBucket, mode: DisplayMode): bigint {
  switch (mode) {
    case "on_cpu":
      return bucket.on_cpu_ns;
    case "cpu":
      return cpuOnlyNs(bucket.on_cpu_ns, bucket.target_ns);
    case "target":
      return bucket.target_ns;
    case "off_cpu":
      return bucket.off_cpu_ns;
    case "wall":
      return bucket.on_cpu_ns + bucket.off_cpu_ns;
  }
}

function displayModeLabel(mode: DisplayMode): string {
  switch (mode) {
    case "on_cpu":
      return "active";
    case "cpu":
      return "CPU";
    case "target":
      return "target";
    case "off_cpu":
      return "off-CPU";
    case "wall":
      return "wall";
  }
}
