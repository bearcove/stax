+++
title = "Integrating Target Spans"
weight = 5
insert_anchor_links = "heading"
+++

Use `stax-target` when the interesting work is not directly visible to the
CPU sampler: GPU kernels, accelerator queues, async executors, worker pools,
media engines, or any runtime that can name and timestamp work.

The integration goal is:

```text
CPU queue/dispatch stack -> target lane -> named span
```

No trace export, no second profiler, no timestamp correlation pass. The spans
land in the same `threads`, `top`, `flame`, timeline, and web UI views as CPU
samples and off-CPU intervals.

## The pattern

Add the crate:

```toml
[dependencies]
stax-target = { path = "../stax/stax-target" }
```

Create one lane per logical executor:

```rust
let lane = stax_target::Lane::new("decoder worker");
```

Gate capture before paying instrumentation costs. When no matching stax
recording is active, this is just one relaxed atomic load:

```rust
if lane.reporting_active() {
    let origin = lane.current_origin();
    enqueue_work(origin);
}
```

`Lane::begin_span` and `Lane::begin_span_with_origin` perform the same gate,
so worker-side timing can use `if let Some(open) = ...` directly.

Capture the origin at the queue/dispatch site, then time/report the work where
it actually runs:

```rust
struct Work {
    origin: Option<stax_target::TargetSpanOrigin>,
}

fn enqueue(lane: &stax_target::Lane) {
    let origin = lane.reporting_active().then(|| lane.current_origin()).flatten();
    submit(Work { origin });
}

fn run_worker(lane: &stax_target::Lane, work: Work) {
    let open = work
        .origin
        .and_then(|origin| lane.begin_span_with_origin("decode chunk", origin));

    decode_chunk();

    if let Some(open) = open {
        open.finish_and_report(lane);
    }
}
```

That is the general executor form. For APIs that already give exact start/end
timestamps, build spans directly:

```rust
if let Some(origin) = lane.current_origin() {
    lane.report_one(lane.span_with_origin("kernel_name", start_ns, end_ns, origin));
}
```

Use `stax_target::now_ns()` when your target-side timestamps should come from
the same host clock stax expects.

## Demo workload

The repo includes an executor-style demo:

```bash
stax record -- cargo run -p stax-target --example executor
stax threads
stax top --tid <executor-demo-tid> --sort self
stax top --tid <cpu-tid> --sort total
stax flame --tid <cpu-tid>
```

`stax threads` will show an `executor demo` synthetic lane. The synthetic
lane's `target ms` is the exact duration of reported work and `spans` is the
span count. When origins link, filtering flame/top to the CPU thread that
queued work shows `CPU caller -> executor demo -> job name`.

## Diagnostics

`stax diagnose` reports target ingest health:

- batches and spans received/recorded
- dropped spans with invalid durations
- per-lane duration and span totals
- origin-linked and origin-unlinked counts

If spans show up on the synthetic lane but not under CPU callers, check the
origin counters first. Unlinked origins usually mean the target captured the
origin too far from the queue point, used the wrong thread, or the CPU sampler
did not catch a nearby PET sample.

## Specializations

- [Profiling GPU Work](@/guide/profiling-gpu-work.md) shows the same target
  span contract applied to Metal 4 timestamp-counter kernels.
- JIT code naming is a different contract: see
  [Profiling JIT Code](@/guide/profiling-jit-code.md).
