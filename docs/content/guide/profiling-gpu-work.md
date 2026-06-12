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
   anywhere). A target can also attach a `TargetSpanOrigin` captured with
   `stax_target::current_span_origin()` at dispatch/queue time; that gives
   stax the CPU tid and timestamp needed to borrow the nearest sampled CPU
   stack.

Server-side (`TargetIngest`), each `(pid, lane)` becomes a **synthetic
thread** — a pseudo-tid at/above `0xFFF0_0000` — and each distinct span
name becomes a synthetic symbol. Each reported span records one sample
marker plus one attributed synthetic execution interval, so kernel names
render like function names in `top`, `flame`, and the web UI timeline. With
origins, `top`/`flame` for the dispatching CPU tid include the span under the
sampled CPU stack that queued it: `CPU caller -> lane -> span name`.

## A worked example: bee's `hx`

bee's Metal 4 runtime captures per-dispatch GPU timestamps and reports
them as the `"GPU tq1s"` lane (`bee/rust/helix-metal4/src/stax.rs`):

```bash
stax record -- ./target/release/hx run --cfg configs/production.jsonc …
stax threads | grep -i gpu
```

In the verified 2026-06-12 `hx` run, this lane had 6300 ingested kernel
spans. For synthetic lanes, the `samples` column is the span count, and
the `active ms` column is lane active time synthesized from the reported
span durations.

## Reading the results

- **`stax threads`** — existence + span count. Synthetic tids live
  at/above `0xFFF0_0000`; pass `-n 0` if you want every thread row.
- **Web UI timeline** (`ws://127.0.0.1:8080`, see
  [The Web UI](@/guide/web-ui.md)) — the lane drawn against the real
  threads, spans named per kernel.
- **`stax top --tid <synthetic>` / `stax flame --tid <synthetic>`** —
  per-kernel aggregation. `top` reports total span duration in the `ms`
  column and span count in the `samples` column. `flame` renders
  `(all) -> lane -> span name`.
- **`stax top --tid <cpu tid>` / `stax flame --tid <cpu tid>`** — when the
  target reports span origins, these thread-scoped views include the GPU work
  queued from that CPU thread. `--sort total` is useful for charging GPU time
  to dispatch callers; `--sort self` still shows the kernel/span names.

## Interpreting a GPU-bound target

Expect `stax top` to look almost empty — single-digit samples, allocator
noise. That IS the finding: the CPU is idle and the time lives in off-CPU
waits (`stax threads`) and the GPU lane. Do not conclude "stax is a CPU
profiler and can't help here"; the recording already contains the GPU
story.
