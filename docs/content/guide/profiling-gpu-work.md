+++
title = "Profiling GPU work"
weight = 6
insert_anchor_links = "heading"
+++

stax recordings are not limited to what the CPU sampler can see. A profiled
app can put **GPU command queues** (or any other accelerator lane) on the
same timeline as its threads, with named spans, on the same clock — no
export step, no second tool, no correlation pass.

## How it works

The app links the **`stax-target`** crate and does two things:

1. **Gate capture** on `stax_target::reporting_active()` — one relaxed
   atomic load, safe on hot paths. A background worker (spawned on first
   use; threads named `stax-target` / `stax-target-io`) polls stax-server
   about once a second: "is a recording of my pid active?" Attach and
   detach propagate within one poll period. No server, no socket, server
   restart — all degrade to "off" and recover on a later poll. The app
   pays its span-capture cost (e.g. GPU timestamp heaps) only while
   recorded.

2. **Report spans** with `stax_target::report(lane, spans)` — fire and
   forget, bounded queue, drop-newest; each `TargetSpan` is a name plus
   absolute `mach_absolute_time`-derived nanoseconds (Apple Silicon GPU
   timestamps share that timebase, which is why no correlation step exists
   anywhere).

Server-side (`TargetIngest`), each `(pid, lane)` becomes a **synthetic
thread** — a pseudo-tid at/above `0xFFF0_0000` — and each distinct span
name becomes a synthetic symbol, so kernel names render like function
names.

## A worked example: bee's `hx`

bee's Metal 4 runtime captures per-dispatch GPU timestamps and reports
them as the `"GPU tq1s"` lane (`bee/rust/helix-metal4/src/stax.rs`):

```bash
stax record -- ./target/release/hx run --cfg configs/production.jsonc …
stax threads -n 2000 | grep -i gpu
#  on-CPU ms off-CPU ms  samples  blocked  tid         name
#       0.00       0.00     6300        -  4293918722  GPU tq1s
```

6300 ingested kernel spans from one ASR run. The `samples` column counts
spans for synthetic lanes; on/off-CPU are zero by construction.

## Reading the results

- **`stax threads -n <big>`** — existence + span count. Synthetic lanes
  sort last (zero on-CPU), so the default cutoff hides them: pass a large
  `-n`.
- **Web UI timeline** (`ws://127.0.0.1:8080`, see
  [The Web UI](@/guide/web-ui.md)) — the lane drawn against the real
  threads, spans named per kernel.
- **`stax top --tid <synthetic>` / `stax flame --tid <synthetic>`** —
  currently return nothing: the CLI tree views aggregate PET samples only.
  Treat the web UI as the per-kernel view for now.

## Interpreting a GPU-bound target

Expect `stax top` to look almost empty — single-digit samples, allocator
noise. That IS the finding: the CPU is idle and the time lives in off-CPU
waits (`stax threads`) and the GPU lane. Do not conclude "stax is a CPU
profiler and can't help here"; the recording already contains the GPU
story.
