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

For executor-style code, prefer carrying a `CapturedOrigin` token. It remembers
both "capture was active" and the optional OS-thread origin. If the platform
cannot capture an origin, lane-only views still work.

`Lane::begin_span`, `Lane::begin_span_with_origin`, and
`Lane::begin_span_with_captured_origin` perform the active-recording gate, so
worker-side timing can use `if let Some(open) = ...` directly.

Capture the origin at the queue/dispatch site, then time/report the work where
it actually runs:

```rust
struct Work {
    origin: stax_target::CapturedOrigin,
}

fn enqueue(lane: &stax_target::Lane) {
    submit(Work {
        origin: lane.capture_origin(),
    });
}

fn run_worker(lane: &stax_target::Lane, work: Work) {
    let open = lane.begin_span_with_captured_origin("decode chunk", work.origin);

    decode_chunk();

    if let Some(open) = open {
        open.finish_and_report(lane);
    }
}
```

That is the general executor form. For APIs that already give exact start/end
timestamps, build spans directly:

```rust
let origin = lane.capture_origin();
if let Some(span) = lane.span_with_captured_origin("kernel_name", start_ns, end_ns, origin) {
    lane.report_one(span);
}
```

Use `stax_target::now_ns()` when your target-side timestamps should come from
the same host clock stax expects. Use `SpanBuilder` when an integration wants
to validate or attach origins before deciding where to report a span.

## Recipes by integration style

### Thread pools

Capture at submission, carry the token in the work item, and open the span in
the worker:

```rust
struct Job {
    origin: stax_target::CapturedOrigin,
}

fn submit(pool: &Pool, lane: &stax_target::Lane) {
    pool.push(Job {
        origin: lane.capture_origin(),
    });
}

fn worker(lane: &stax_target::Lane, job: Job) {
    let open = lane.begin_span_with_captured_origin("run job", job.origin);
    run_job();
    if let Some(open) = open {
        open.finish_and_report(lane);
    }
}
```

### Async executors

Capture before the work crosses the async boundary. Open the span only when a
task or worker actually starts doing the work:

```rust
async fn schedule(tx: &Queue, lane: &stax_target::Lane) {
    tx.send(Job {
        origin: lane.capture_origin(),
    })
    .await;
}

async fn run_worker(lane: &stax_target::Lane, job: Job) {
    let open = lane.begin_span_with_captured_origin("poll work item", job.origin);
    poll_work_item().await;
    if let Some(open) = open {
        open.finish_and_report(lane);
    }
}
```

### Exact timestamp APIs

When an API gives exact start/end timestamps, capture the origin at dispatch
and report after completion:

```rust
fn dispatch(lane: &stax_target::Lane) {
    let origin = lane.capture_origin();
    let completion = submit_to_runtime();

    if let Some(span) = lane.span_with_captured_origin(
        completion.name,
        completion.start_ns,
        completion.end_ns,
        origin,
    ) {
        lane.report_one(span);
    }
}
```

### GPU timestamp counters

For Metal 4 or similar APIs, the important boundary is the timestamp
conversion. Convert the target timestamps into the same nanosecond clock domain
stax expects, then report ordinary target spans:

```rust
fn encode_kernel(lane: &stax_target::Lane, command: &mut Command) {
    let origin = lane.capture_origin();
    command.encode_dispatch();
    command.on_complete(move |timestamps| {
        let start_ns = gpu_timestamp_to_stax_ns(timestamps.start);
        let end_ns = gpu_timestamp_to_stax_ns(timestamps.end);
        if let Some(span) = lane.span_with_captured_origin(
            timestamps.kernel_name,
            start_ns,
            end_ns,
            origin,
        ) {
            lane.report_one(span);
        }
    });
}
```

### Bad-origin debugging

If spans arrive but do not show under CPU callers, run:

```bash
stax diagnose
```

The usual fixes are:

- capture at queue/dispatch time, not completion time
- capture on the OS thread that queued the work
- keep span timestamps in one monotonic nanosecond clock
- keep span names semantic and low-cardinality

## Demo workload

The repo includes several target-span demos:

| example | what it demonstrates |
|---------|----------------------|
| `executor` | minimal queue/worker split with `CapturedOrigin` |
| `thread_pool` | multiple workers sharing one logical target lane |
| `async_executor` | scheduling work into an async channel, then timing it in the async worker |
| `codec` | exact host timestamps with decode/encode lanes |
| `model_runtime` | semantic model-runtime lanes with `SpanBuilder` |
| `gpu_timestamps` | Metal-style timestamp-counter conversion without SDK dependencies |
| `bad_origins` | intentionally missing/stale/wrong-thread origins for `stax diagnose` |

For example:

```bash
stax record -- cargo run -p stax-target --example executor
stax threads
stax top --tid <executor-demo-tid> --sort self
stax top --tid <cpu-tid> --sort total
stax flame --tid <cpu-tid>
```

`stax threads` will show a synthetic lane named after the example, such as
`executor demo` or `model attention`. The synthetic lane's `target ms` is the
exact duration of reported work and `spans` is the span count. When origins
link, filtering flame/top to the CPU thread that queued work shows
`CPU caller -> lane -> job name`.

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
