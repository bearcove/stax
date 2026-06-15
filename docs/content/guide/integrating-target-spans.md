+++
title = "Integrating Target Spans"
weight = 5
insert_anchor_links = "heading"
+++

Use `stax-target` when the interesting work is not directly visible to the
CPU sampler: GPU kernels, accelerator queues, async executors, worker pools,
media engines, or any runtime that can name and timestamp work.

The integration goal is a linked graph, not a fake single-thread stack:

```text
CPU queue/dispatch stack --origin--> target lane -> named span
```

No trace export, no second profiler, no timestamp correlation pass. The spans
land in the same `threads`, `top`, `flame`, timeline, and web UI views as CPU
samples and off-CPU intervals. Target lanes remain parallel execution lanes;
if a future integration also reports the CPU wait/completion side, stax can
offer a separate wall-time/critical-path view that nests target work under a
stack only when the same stack both dispatched and awaited that work.

## The pattern

Add the crate:

```toml
[dependencies]
stax-target = { path = "../stax/stax-target" }
```

Create one lane per logical executor:

```rust
let lane = stax_target::Lane::new("decoder worker");
let _gpu_lane = stax_target::Lane::metal("GPU tq1s");
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

For richer integrations, use `Lane::dispatch_builder(name)` and
`TargetRecordBatch`. The simple span API keeps working, but the richer path can
attach stable dispatch/shader/source ids, wait/completion origins, attachment
ids, and counter sample ids:

```rust
let dispatch_id = stax_target::TargetDispatchId::new(42);
let shader_id = stax_target::TargetShaderId::new(7);
let origin = lane.capture_origin();

let span = lane
    .dispatch_builder("tq6_1s_rows")
    .with_captured_origin(origin)
    .with_dispatch_id(dispatch_id)
    .with_shader_id(shader_id)
    .timestamps(start_ns, end_ns)
    .build();

if let Some(span) = span {
    lane.report_batch(vec![span], stax_target::TargetRecordBatch::default());
}
```

Metadata-only batches are allowed. For example, a target can register source or
counter definitions before dispatches resolve; `stax diagnose` will show that
the records are arriving even if no spans have landed yet.

For integration health logs, admin endpoints, or assertions in your own test
fixtures, read `lane.reporter_stats()` or `stax_target::reporter_stats()`.
That snapshot is passive: it reports whether the background worker has been
armed, the last capture-gate state, whether the worker currently has a
stax-server connection, and local drop counters, but it does not start polling
by itself. Use `lane.reporting_active()` as the real capture gate.

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

If spans arrive but do not link back to CPU dispatch origins, run:

```bash
stax diagnose
```

The usual fixes are:

- capture at queue/dispatch time, not completion time
- capture on the OS thread that queued the work
- keep span timestamps in one monotonic nanosecond clock
- keep span names semantic and low-cardinality

`stax diagnose` names the failure mode:

- `bad_tid` means the origin tid was itself a synthetic target lane; capture
  on a real CPU thread before reporting target work.
- `no_thread` means no PET samples were recorded for that tid in this run;
  check that the profiled process/thread is really the one dispatching work.
- `no_stack` means the tid was sampled, but without a user stack; check frame
  pointers / DWARF unwinding or whether the dispatch site only sampled in
  kernel/runtime glue.
- `too_far` means the nearest sampled CPU stack was outside stax's origin
  window; move origin capture closer to the queue/submit point.

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
| `corpus` | blessed CPU/off-CPU/target-span/origin diagnostics workload |

For example:

```bash
just demo-corpus
stax top --tid <corpus-executor-tid> --sort self
stax target top --by time
stax target top --by count
stax top --tid <cpu-tid> --sort total
stax flame --tid <cpu-tid> --threshold-pct 0
```

`just demo-corpus` records `stax-target/examples/corpus.rs`, then prints
`stax threads -n 0` and `stax diagnose`. The thread table will show synthetic
lanes such as `corpus executor`, `corpus gpu`, and `corpus bad origins`. A
synthetic lane's `target ms` is the exact duration of reported work and
`spans` is the span count. When origins link, filtering flame/top to the CPU
thread that queued work limits the target lane tree to that origin's work; the
web target-span details show the linked CPU dispatch stack.

## Diagnostics

`stax diagnose` reports target ingest health:

- batches and spans received/recorded
- batches/spans dropped because no run was active or the pid did not match
- batches/spans dropped in stax-target before reaching the server because the
  local queue filled or the background worker disconnected
- dropped spans with invalid durations
- per-lane duration and span totals
- origin-linked and origin-unlinked counts
- unlinked-origin reasons: `bad_tid`, `no_thread`, `no_stack`, `too_far`
- linked and too-far origin PET distance min/avg/max

If spans show up on the synthetic lane but do not link to CPU dispatch
origins, check the origin counters first. Unlinked origins usually mean the
target captured the origin too far from the queue point, used the wrong
thread, or the CPU sampler did not catch a nearby PET sample. If `too_far`
dominates, the average/max distance tells you how stale the origin was
relative to the nearest CPU stack.

Inside the cooperating process, `reporter_stats()` answers a different
question: whether the local reporter worker is armed/connected and whether it
has already dropped batches before the server could see them. That is the
right signal for target-side logs such as "we are being recorded but the local
queue is overflowing".

## Specializations

- [Profiling GPU Work](@/guide/profiling-gpu-work.md) shows the same target
  span contract applied to Metal 4 timestamp-counter kernels.
- JIT code naming is a different contract: see
  [Profiling JIT Code](@/guide/profiling-jit-code.md).
