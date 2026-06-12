+++
title = "Profiling GPU work"
weight = 6
insert_anchor_links = "heading"
+++

stax recordings are not limited to what the CPU sampler can see. A profiled
app can put **target/executor lanes** on the same timeline as its threads:
GPU command queues, accelerator work, runtime queues, or any other work the
target can timestamp and name. GPU work is the first concrete consumer, but
the mechanism is deliberately general — no export step, no second tool, no
correlation pass.

This page is the GPU specialization of the generic
[target-span integration contract](@/guide/integrating-target-spans.md).

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
marker plus one attributed synthetic execution interval, so kernel/job names
render like function names in `top`, `flame`, and the web UI timeline. The
legacy active-time fields include these durations for compatibility, and the
newer reporting surfaces break them out explicitly as `target` time and
target span counts. With origins, `top`/`flame` for the dispatching CPU tid
include the span under the sampled CPU stack that queued it:
`CPU caller -> lane -> span name`.

## A worked example: bee's `hx`

bee's Metal 4 runtime captures per-dispatch GPU timestamps and reports
them as the `"GPU tq1s"` lane (`bee/rust/helix-metal4/src/stax.rs`):

```bash
stax record -- ./target/release/hx run --cfg configs/production.jsonc …
stax threads | grep -i gpu
```

In the verified 2026-06-12 `hx` run, this lane had 6300 ingested kernel
spans. For synthetic lanes, `target ms` is the exact sum of reported span
durations and `spans` is the span count. The old active-time field still
includes the same duration so flame widths and older clients keep working,
but it does not mean a CPU thread was busy.

## Reading the results

- **`stax threads`** — existence + span count. Synthetic tids live
  at/above `0xFFF0_0000`; target lanes with spans are included even past the
  normal `-n` cutoff.
- **Web UI timeline** (`ws://127.0.0.1:8080`, see
  [The Web UI](@/guide/web-ui.md)) — the lane drawn against the real
  threads, spans named per kernel.
- **`stax top --tid <synthetic>` / `stax flame --tid <synthetic>`** —
  per-kernel aggregation. `top` reports total span duration in `target ms`
  and span count in `spans`. `flame` renders `(all) -> lane -> span name`
  with target time/span columns.
- **`stax top --tid <cpu tid>` / `stax flame --tid <cpu tid>`** — when the
  target reports span origins, these thread-scoped views include the GPU work
  queued from that CPU thread. `--sort total` is useful for charging GPU time
  to dispatch callers; `--sort self` still shows the kernel/span names.
- **`stax diagnose`** — target ingest counters: batches, recorded/dropped
  spans, no-active-run / wrong-pid drops, total target duration, lanes, and
  origin link/unlink counts. It also reports why origins did not link
  (`bad_tid`, `no_thread`, `no_stack`, `too_far`) and the min/avg/max PET
  sample distance for linked or too-far origins. Use this when spans are
  missing or CPU-stack attribution is missing.

## Interpreting a GPU-bound target

Expect `stax top` to look almost empty — single-digit samples, allocator
noise. That IS the finding: the CPU is idle and the time lives in off-CPU
waits (`stax threads`) and the GPU lane. Do not conclude "stax is a CPU
profiler and can't help here"; the recording already contains the GPU
story.
