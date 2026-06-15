+++
title = "Target observability roadmap"
weight = 6
insert_anchor_links = "heading"
+++

stax should feel like the obvious observability substrate for local
performance work: one recording, one timeline, CPU stacks, off-CPU waits,
target/executor lanes, source, counters, and enough provenance to move between
them without exporting to a second profiler.

This roadmap is the work plan for getting from today's target spans to that
integrated surface. It is intentionally concrete about Metal because bee's
Metal 4 runtime is the first serious consumer, but the model must stay general
enough for async executors, thread pools, codecs, model runtimes, storage
engines, and other accelerators.

## Metal timing and counter terms

This section is a capability inventory, not an impossibility claim. A working
Metal 4 integration should be judged by code that runs on the target machine.
The useful reference here is
[`Kr1sso/tracy-metal4`](https://github.com/Kr1sso/tracy-metal4), which ports
Tracy's Metal GPU zones to Metal 4.

When this document says **Metal timestamp surface**, it means the Metal 4
timestamp heap path:

- `MTL4CounterHeapDescriptor`
- `MTL4CounterHeapTypeTimestamp`
- `MTL4CounterHeap`
- `MTL4ComputeCommandEncoder::writeTimestampWithGranularity:intoHeap:atIndex:`
- `MTL4RenderCommandEncoder::writeTimestampWithGranularity:afterStage:intoHeap:atIndex:`
- `MTL4CommandBuffer::writeTimestampIntoHeap:atIndex:`
- `MTL4CommandBuffer::resolveCounterHeap:withRange:intoBuffer:waitFence:updateFence:`
- `MTLDevice::sizeOfCounterHeapEntry`
- `MTLDevice::queryTimestampFrequency`

That path gives accurate GPU timestamps and per-dispatch or per-zone durations.
It is enough for Tracy-style GPU zones: reserve two heap slots, write a start
timestamp before the work, write an end timestamp after the work, resolve the
heap, and align GPU ticks to the CPU timeline. `tracy-metal4` does exactly
that with a two-heap ring, `MTL4TimestampGranularityPrecise` by default,
`MTL4CounterHeapTypeTimestamp`, `resolveCounterRange:`, and Tracy
`GpuZoneBegin`/`GpuZoneEnd`/`GpuTime` events.

In the macOS 27 SDK headers available while this was written,
`MTL4CounterHeapType` only has `Invalid` and `Timestamp`. So "timestamp heap"
is the precise name for this path. It is not a claim that Metal 4 has no other
observability surfaces.

When this document says **Metal hardware counter sample-buffer surface**, it
means the programmable counter API that exposes named counter sets and result
structures:

- `MTLDevice::counterSets`
- `MTLCounterSet`
- `MTLCounterSampleBufferDescriptor`
- `MTLDevice::newCounterSampleBufferWithDescriptor:error:`
- `MTLDevice::supportsCounterSampling:`
- `MTLCounterSamplingPointAtDispatchBoundary`
- `MTLComputeCommandEncoder::sampleCountersInBuffer:atSampleIndex:withBarrier:`
- `MTLRenderCommandEncoder::sampleCountersInBuffer:atSampleIndex:withBarrier:`
- `MTLBlitCommandEncoder::sampleCountersInBuffer:atSampleIndex:withBarrier:`
- `MTLAccelerationStructureCommandEncoder::sampleCountersInBuffer:atSampleIndex:withBarrier:`
- `MTLCounterSampleBuffer::resolveCounterRange:`
- `MTLCommonCounterSetStatistic`
- `MTLCommonCounterSetStageUtilization`
- `MTLCounterResultStatistic::computeKernelInvocations`
- `MTLCounterResultStageUtilization::totalCycles`
- common counter names such as `MTLCommonCounterComputeKernelInvocations`,
  `MTLCommonCounterTotalCycles`, and stage cycle counters

The immediate unknown is not whether the counter API exists; it does. The
unknown is how it composes with bee's current `MTL4ComputeCommandEncoder`
recording path, because the public MTL4 compute encoder header exposes
timestamp writes but not `sampleCountersInBuffer`, and `MTL4CounterHeap` is a
timestamp heap in the inspected SDK. The roadmap therefore requires a small
proof step before wiring programmable counters into stax: enumerate counter
sets, create sample buffers, try the classic compute sampling path, try the
MTL4 command path, and record the exact compiler/runtime result.

Programmatic GPU capture is a third, separate Metal surface:

- `MTLCaptureManager`
- `MTLCaptureDescriptor`
- `MTLCaptureScope`
- capture objects such as an MTL4 command queue
- `.gputrace` document output

stax should link to captures when a target produces them, but capture should be
a companion artifact, not the ordinary live path.

## Current state

The current target-span path works at the basic level:

- A target links `stax-target`.
- A lane reports named spans with absolute nanosecond timestamps.
- `TargetIngest` turns each `(pid, lane)` into a synthetic tid.
- Each distinct span name becomes a synthetic symbol in a synthetic binary.
- `stax threads`, `stax top`, `stax flame`, the web flamegraph, and the target
  details panel can aggregate exact target time and span counts.
- `stax target lanes` and `stax target top --by time|count|avg|max` provide a
  target-only CLI discovery path for lane and span/shader rankings.
- Explicit lane kinds let Metal lanes render with Metal coloring and iconography
  without name heuristics.
- `stax-target` has typed record scaffolding for dispatch/source/shader,
  attachment, and counter metadata, plus a richer dispatch builder. Ingest
  diagnostics count those records, but the full server archive/query/UI story is
  still pending.

The important semantic correction is that target lanes are **parallel
execution lanes**. A CPU origin is provenance: "this CPU stack queued this
target work." It is not containment. A future wall-time view may nest target
work under a CPU stack only when the target also reports a matching wait or
completion origin that proves synchronous dispatch-and-wait behavior.

The current gaps are:

- target spans only carry name, time, lane, kind, and optional queue origin;
- typed dispatch/queue/command-buffer/runtime ids exist on the wire/API, but
  are not yet indexed into rich server query surfaces;
- wait/completion origins exist on the wire/API, but are not yet linked or
  classified server-side;
- attachment, shader/source, and counter records exist on the wire/API, but are
  not yet surfaced beyond diagnostics;
- no durable source/counter/attachment payload in saved archives;
- no UI selection model that treats a target dispatch as an inspectable object
  with source, buffers, counters, origins, and links.

## Data model

Add a typed target-observability model that sits beside the existing synthetic
lane aggregation instead of replacing it. The synthetic lane model stays useful
because it makes target work appear in all existing views immediately.

Core identities:

- `target_runtime_id`: one target-side instrumentation runtime in one process.
- `lane_id`: logical executor lane, command queue, accelerator queue, or worker
  pool.
- `queue_id`: optional concrete queue/device queue identity.
- `command_buffer_id`: optional command-buffer submission identity.
- `dispatch_id`: one target work item or GPU dispatch.
- `shader_id`: stable shader/function identity, when applicable.
- `source_id`: source blob or source-map identity.
- `attachment_id`: buffer, tensor, image, file, request, batch, or model object
  attached to a dispatch.
- `counter_set_id`: named set of counters with layout and unit metadata.

Dispatch record:

- `dispatch_id`
- lane/runtime/queue/command-buffer ids
- display name
- start/end timestamps in stax's monotonic nanosecond clock
- optional dispatch origin
- optional wait origin(s)
- optional completion/fault origin
- optional shader id
- optional source location or source range
- optional argument metadata
- optional buffer/tensor attachments
- optional counter sample ids
- target-side tags such as model, phase, pulse, batch, stream, request, or
  runtime-specific classifier

Origin record:

- CPU tid
- capture timestamp
- optional captured stack id if stax-target later supports target-side stack
  capture
- link status after ingest: linked, missing thread, no stack, too far, synthetic
  tid, wrong pid, stale, outside run
- nearest PET sample distance when linked or too far

Counter record:

- counter set id
- dispatch id or command-buffer id
- sample point: before dispatch, after dispatch, command-buffer begin/end, wait
  begin/end, or runtime-defined point
- values as typed numeric counters, not ad hoc strings
- unit metadata: ticks, cycles, invocations, bytes, percent, count, ns
- error value handling for counters that resolved to `MTLCounterErrorValue`

Source record:

- source id
- language: Metal, Rust, C, C++, shader IR, SQL, regex, bytecode, etc.
- original path when available
- content hash
- source text or archive blob reference
- line table or function-to-range map
- shader/function id mapping
- build flavor and compiler flags where relevant

Attachment record:

- attachment id
- dispatch id
- kind: buffer, tensor, texture, file, socket, request, model layer, batch,
  command-buffer resource, runtime object
- stable label and slot/index
- size and offset metadata
- optional logical shape, dtype, role, layer, head, token range, or batch range
- privacy policy: metadata-only by default; no raw payload capture unless an
  explicit future opt-in says so

## stax-target crate work

`stax-target` should become the polished crate integrators import. It should
keep the current cheap span API and add a richer builder path for advanced
integrations.

Required API shape:

- `Lane::new` for generic target lanes.
- `Lane::metal` for Metal/GPU lanes with explicit icon/color semantics.
- `Lane::capture_origin` and free `current_span_origin`.
- `Lane::span` / `Lane::span_with_captured_origin` for the existing simple
  case.
- `Lane::dispatch_builder(name)` for the richer case.
- builder methods for timestamps, dispatch origin, wait origin, completion
  origin, runtime/queue/command-buffer ids, shader id, source location,
  attachments, and counters.
- source registration APIs that can be called at startup or lazily when a
  shader/pipeline is first used.
- counter definition APIs so counter values are self-describing.
- reporter stats that distinguish queued, sent, dropped, disconnected,
  disabled, unsupported, and schema-version mismatch states.

The crate should preserve the "boringly correct" behavior:

- no work when no stax recording is active;
- one relaxed active-gate check on hot paths;
- bounded queues;
- explicit drop counters;
- reconnect after stax-server restart;
- no panics in target processes;
- no generated JSON strings; use the repo's typed serialization path;
- feature flags for optional runtime integrations.

Useful helper modules:

- `stax_target::metal` for Metal-specific lane kinds, shader metadata,
  timestamp conversion helpers, and counter metadata helpers.
- `stax_target::executor` for queue/enqueue/dequeue/run/wait helpers.
- `stax_target::source` for source registration and line maps.

## Server ingest and aggregation

`TargetIngest` should keep producing the existing synthetic lane data so older
views and mental models continue to work. In parallel, it should retain typed
target records for richer queries.

Ingest responsibilities:

- validate timestamps, durations, ids, and pid ownership;
- intern runtime/lane/queue/command-buffer/dispatch/shader/source ids;
- publish synthetic lane symbols for lane and span/shader names;
- record exact duration and count aggregates per lane, span, shader, source,
  command buffer, and origin;
- link dispatch origins to nearest PET stack when possible;
- link wait/completion origins separately from dispatch origins;
- classify linked, unlinked, stale, and invalid origins;
- store recent individual dispatches with enough metadata for UI details;
- keep bounded memory for live runs;
- save typed target records into archives.

Aggregation surfaces:

- lane -> span/shader flame tree;
- target top by total time, self time, count, average, p50/p95/p99 duration;
- origin -> target work provenance table;
- command buffer summary: dispatch count, total GPU time, elapsed queue time,
  wait time, completion/fault status;
- shader summary: total time, invocation count, source location, counter totals;
- attachment summary: which buffers/tensors were used by expensive dispatches;
- counter summary: per-dispatch and per-shader totals/averages/rates.

Diagnostics should answer:

- Are target batches arriving?
- Are target batches from the active pid?
- Are timestamps in-range and monotonic?
- Are origins present?
- Are origins linking?
- Why are origins not linking?
- Are origins stale relative to PET samples?
- Are wait origins present?
- Are target spans parallel only, or proven synchronous?
- Are source registrations present for shaders?
- Are counter definitions present?
- Are counter samples present, unsupported, disabled, or failing to resolve?
- Is the target-side queue dropping records?

## CLI work

The existing commands should stay useful:

- `stax threads` continues to make synthetic lanes impossible to miss.
- `stax top --tid <synthetic>` continues to aggregate span/shader names.
- `stax flame --tid <synthetic>` continues to render `(all) -> lane -> span`.
- `stax diagnose` continues to report ingest health.

Add a target-focused query family or equivalent flags. Exact command names can
change during implementation, but the user-facing questions must be answerable:

- "What target lanes exist?"
- "Which shaders/spans took the most total time?"
- "Which shaders/spans ran most often?"
- "Which dispatches are the outliers?"
- "Which CPU stack queued this target work?"
- "Which CPU stack waited for it?"
- "Which command buffer did it belong to?"
- "Which source file/function/line is this shader?"
- "Which buffers/tensors were attached?"
- "Which counters were collected, and what changed?"

Likely commands:

- `stax target lanes`
- `stax target top --by time|count|avg|p95|counter:<name>`
- `stax target dispatches --lane ... --shader ...`
- `stax target origins --dispatch ...`
- `stax target shaders`
- `stax target source <shader-or-source-id>`
- `stax target counters`
- `stax diagnose --target` or richer target sections in existing diagnose

Discovery hints:

- If Metal command/dispatch frames appear but no Metal lane exists, suggest
  `stax-target` plus `Lane::metal` and Metal timestamp cooperation.
- If Metal lanes exist but origins do not link, point at origin diagnostics.
- If shader names exist but no sources, suggest source registration or recorded
  metallib source extraction.
- If counter sets are available but no counter samples arrive, say counters are
  not enabled or unsupported for the chosen path.
- If a CPU tid has only provenance-linked target work, say it is parallel work,
  not CPU execution.

## Web UI work

The web UI should make target work inspectable without leaving the flamegraph.

Flamegraph:

- explicit target colors and icons from lane kind;
- signposts for target dispatches in the actual flamegraph;
- target-time mode where width is exact target duration;
- CPU mode that peels target spans out;
- wall/critical-path mode only when wait/completion evidence exists;
- selection of target nodes as first-class objects.

Target details panel:

- summary: total time, count, average, p95/p99, min/max;
- recent dispatches;
- CPU dispatch origin stack link;
- wait/completion origin stack link when present;
- source location and inline source snippet;
- buffer/tensor attachments;
- command-buffer grouping;
- counter values and derived rates;
- capture artifact link when a `.gputrace` exists.

Target lane/timeline:

- swimlanes for target lanes below CPU threads;
- command-buffer and dispatch blocks;
- hover links between CPU dispatch stack, target lane block, and wait site;
- highlight all dispatches for the same shader/source/attachment;
- show parallel work as parallel, not as a fake child of the CPU stack.

Search and ranking:

- filter by lane, shader, source file, CPU origin, attachment, counter, command
  buffer, or text;
- sort by time, count, duration percentiles, or counter values;
- keep URL state for selected dispatch/shader/source.

## Archives and compare

Saved archives need to preserve enough typed target data to reopen the same
story later:

- target records and schema version;
- source blobs or content-addressed source references;
- shader/source maps;
- counter definitions and samples;
- attachments metadata;
- command-buffer and wait/completion links;
- diagnostics counters.

`stax compare` should grow target-aware deltas:

- target time by lane/shader/source;
- dispatch count by lane/shader/source;
- duration percentiles;
- counter totals and rates;
- missing-origin and stale-origin deltas;
- missing-source and missing-counter deltas;
- attachment footprint deltas when metadata exists.

CI thresholds should support:

- target duration increase;
- invocation count increase;
- p95/p99 duration increase;
- counter increase by name;
- origin-link regression;
- source/counter coverage regression.

## Bee Metal integration

Bee is the flagship integration. The goal is not merely "show GPU tq1s"; it is
"from a stax flamegraph, inspect the expensive Metal dispatch, jump to the
Metal source, see what buffers/tensors it touched, see counters when available,
and jump back to the CPU dispatch/wait stack."

Immediate cleanup:

- use `stax_target::Lane::metal("GPU tq1s")` for the tq1s GPU lane;
- update comments that still imply GPU work is contained under the CPU stack;
- keep TTS and other non-Metal lanes explicit about their kind;
- audit timestamp conversion and calibration against CPU time.

Timestamp path:

- keep `MTL4CounterHeap` timestamp pairs around each dispatch;
- reserve a stable `dispatch_id` when reserving timestamp indices;
- record command-buffer id and queue/lane id with each reservation;
- record `MTL4TimestampGranularity` and whether the mode is precise or relaxed;
- resolve timestamps after queue completion;
- report exact begin/end timestamps and duration.

Origin and wait path:

- capture dispatch origin immediately before the dispatch crosses into the GPU
  command stream;
- capture wait origin around `Metal4Context::commit_and_wait`;
- for classic Metal paths, capture wait origin around command-buffer waits;
- record completion/fault feedback from MTL4 commit feedback;
- classify a dispatch as "synchronous under CPU stack" only when dispatch and
  wait origins prove it.

Shader identity:

- stable shader id from library flavor + function name + pipeline identity;
- record Metal function name and display name separately when useful;
- record deterministic/fast library flavor;
- include pipeline creation metadata once per pipeline;
- avoid heuristics based on Rust crate or symbol names.

Source correlation:

- bee already builds merged `.metal` sources and compiles with
  `-gline-tables-only -frecord-sources`;
- first reliable path: generate a source manifest in `helix-metal/build.rs`
  beside the metallib and register it through `stax-target`;
- opportunistic path: recover recorded source/source maps from the embedded
  `.air`/`.metallib` or Apple tooling when available;
- source ids should be content-addressed so repeated recordings do not bloat
  archives unnecessarily;
- stax should be able to render the exact shader/function source range for the
  selected dispatch.

Buffer and tensor attachments:

- hook bee's MTL4 argument table binding path;
- capture slot/index, role/name, byte offset, buffer length, logical tensor
  shape, dtype, layer/head/token/batch ranges when known;
- never capture raw tensor data by default;
- cap attachment count and string sizes;
- keep enough metadata to answer "what did this dispatch read/write?"

Hardware counters:

- probe `MTLDevice::counterSets` and `supportsCounterSampling` on the target
  machine;
- enumerate available common and device-specific counter sets;
- prove whether the sample-buffer API can be used with bee's current MTL4
  encoder path;
- if not, decide between a classic-Metal profiling path, a capture-backed
  path, or waiting for an MTL4-specific public sampling hook;
- once proven, capture before/after samples around dispatches or command
  buffers;
- resolve counters after completion;
- report typed counter values to stax with units and error handling;
- keep counters behind an explicit profiling mode because barriers and counter
  sampling can perturb performance.

Programmatic capture:

- keep bee's `.gputrace` capture path;
- add MTL4 capture scopes around meaningful phases;
- report capture artifact path into stax when a capture is active;
- let stax link a dispatch/shader to the capture artifact without requiring
  captures for the normal live view.

## Generalization beyond GPU

The same model should cover non-GPU integrations:

- async executor task enqueue, poll, wait, and completion;
- thread-pool job enqueue, steal, run, and wait;
- codec packet/frame work;
- model-runtime operator dispatch;
- database query planning/execution;
- JIT compilation stages;
- storage or network request lifecycles.

The general shape is:

```text
CPU origin --queues--> target lane --runs--> named work
CPU wait   --awaits--> target work
target work --uses--> attachments
target work --has--> counters/source/diagnostics
```

Only GPU integrations need Metal-specific shader/counter/capture fields. The
core stax model should stay domain-neutral.

## Critical path and wall time

The real prize is a view that can explain:

"This CPU stack dispatched this target work, then waited here, and the wall
time was dominated by these target dispatches."

Rules:

- default target lane rendering is parallel;
- CPU origin means provenance only;
- a CPU stack owns target wall time only when a wait/completion origin proves
  the CPU actually waited for that target work;
- asynchronous work without a matching wait stays cross-linked, not nested;
- if dispatch and wait happen on the same sampled stack, a wall-time or
  critical-path view can show `CPU stack -> target lane -> dispatch`;
- if dispatch and wait happen on different stacks, the UI should show links
  between stacks and lane blocks.

Needed data:

- dispatch origin;
- wait begin/end origin;
- completion/fault event;
- stable dispatch/command-buffer ids;
- enough timestamp overlap data to know whether CPU and target work ran in
  parallel or serially.

Views:

- dual flamegraph: CPU active/off-CPU on one side, target work on the other;
- critical-path flamegraph when waits prove containment;
- swimlane timeline for exact overlap;
- hover/click linking between dispatch, target lane, and wait site.

## Demo corpus and tests

The blessed corpus should grow until it proves the whole model without needing
bee for every regression:

- CPU work with real symbols;
- off-CPU waits;
- target lane spans;
- linked dispatch origins;
- bad/stale/missing origins;
- synthetic Metal lane kind;
- fake shader/source registration;
- fake attachments;
- fake counters;
- wait/completion links;
- one synchronous dispatch-and-wait case;
- one asynchronous cross-lane case.

Regression checks:

- `TargetIngest` unit/integration tests for typed records and diagnostics;
- CLI snapshots for `threads`, `top`, `flame`, `target ...`, and `diagnose`;
- archive save/open/compare smokes;
- browser smoke for flamegraph selection, target details, source, attachments,
  counters, and mobile layout;
- bee integration probe for MTL4 timestamp lane, source registration, and
  counter capability discovery.

The corpus should remain production-shaped: real stax-server ingest, real
archives, real browser where UI behavior matters.

## Phased execution plan

### Phase 0: Ground truth and naming

Deliverables:

- document the exact Metal timestamp, programmable counter, and capture
  surfaces, including the `tracy-metal4` timestamp-zone reference;
- remove ambiguous "real counter" wording from comments/docs;
- audit current bee comments for containment language;
- add a tiny Metal capability probe in bee or a throwaway example that:
  - prints `MTLDevice.counterSets` with counter names;
  - prints `supportsCounterSampling:` for the relevant sampling points;
  - creates `MTLCounterSampleBuffer` objects for statistic/stage-utilization
    sets when available;
  - proves the classic `MTLComputeCommandEncoder::sampleCountersInBuffer`
    path on a tiny dispatch;
  - attempts the corresponding MTL4 command path used by bee and records the
    exact compiler/runtime result;
  - records whether Xcode GPU captures expose additional counters for the same
    dispatch.

Done when:

- docs and code comments distinguish MTL4 timestamp heaps, Tracy-style GPU
  zones, programmable counter sample buffers, and `.gputrace` capture;
- `stax diagnose`/CLI copy never implies GPU time is CPU time;
- the counter probe has real output on the target machine.

### Phase 1: Typed target records

Deliverables:

- add typed ids and records to the stax live protocol;
- extend `stax-target` builders;
- preserve existing `TargetSpan` convenience APIs;
- add source/counter/attachment registration structs;
- regenerate generated bindings from source.

Done when:

- existing target-span examples still work;
- the corpus can emit dispatch ids, sources, attachments, and fake counters;
- archive save/open preserves the new typed records.

### Phase 2: Server ingest and diagnostics

Deliverables:

- ingest typed records;
- keep synthetic lane aggregation;
- add indexes by lane, shader, source, origin, command buffer, and counter;
- add diagnostics for missing/invalid/stale origins, missing sources, missing
  counters, and unsupported counter modes.

Done when:

- `stax top` and `stax flame` still render target lanes;
- new diagnostics explain every intentionally bad corpus case;
- save/open/compare works with the extended corpus.

### Phase 3: CLI target queries

Deliverables:

- target-focused lane/span/shader/dispatch/counter/source queries;
- top-by-time, top-by-count, top-by-duration-percentile, top-by-counter;
- clear empty-state and discovery hints;
- documentation and reference updates.

Done when:

- from a saved corpus archive, the CLI can answer most invoked, most time,
  source for selected shader, attachments for selected dispatch, and counter
  summaries;
- hints point users toward `stax-target` or Metal cooperation when CPU-only
  data is insufficient.

### Phase 4: Web flamegraph integration

Deliverables:

- target selection object model;
- flamegraph signposts and target visual treatment;
- target details panel with source, origins, attachments, and counters;
- hover/click cross-links;
- browser smoke coverage.

Done when:

- selecting a target node in the flamegraph shows the correct shader/source,
  dispatches, origins, attachments, and counters;
- hover highlights parallel lane links without pretending they are CPU
  children;
- desktop and mobile smokes pass.

### Phase 5: Bee timestamp and origin upgrade

Deliverables:

- `Lane::metal("GPU tq1s")`;
- stable dispatch/command-buffer ids;
- dispatch origin and wait origin;
- MTL4 commit feedback;
- timestamp calibration audit;
- source manifest registration from bee's build.

Done when:

- a live `hx` recording shows Metal lane visuals;
- dispatches link to CPU queue stacks;
- waits link separately when present;
- selected shaders show source from bee's shipped metallib/source manifest.

### Phase 6: Bee attachments

Deliverables:

- argument-table attachment capture;
- tensor/buffer metadata caps;
- stax-target attachment records;
- UI/CLI display.

Done when:

- selecting a bee dispatch shows the buffer/tensor slots it used, with sizes,
  offsets, roles, and shapes where available;
- no raw tensor contents are captured by default.

### Phase 7: Counters

Deliverables:

- capability probe result in diagnostics;
- proof of sample-buffer compatibility or documented fallback;
- optional counter sampling mode;
- counter definition and sample reporting;
- UI/CLI ranking by counters.

Done when:

- stax can say exactly why counters are unavailable, disabled, or present;
- when enabled on supported hardware/path, stax shows counter values per
  dispatch and aggregates them by shader/source/lane;
- counter sampling overhead is explicit.

### Phase 8: Critical path

Deliverables:

- wait/completion record ingestion;
- synchronous vs asynchronous classification;
- wall/critical-path query mode;
- cross-lane hover/link behavior.

Done when:

- the corpus proves one nested synchronous case and one linked asynchronous
  case;
- stax never nests target time under CPU stacks without wait evidence;
- the UI can explain dispatch stack, target work, and wait stack together.

### Phase 9: Polish and integrator docs

Deliverables:

- `stax-target` crate docs;
- guide pages for executors, thread pools, and GPU;
- CLI help copy;
- web UI guide updates;
- worked bee/hx example;
- integration checklist.

Done when:

- a new integrator can add target spans, origins, source, attachments, and
  optional counters by following docs alone;
- `stax diagnose` tells them what they got wrong.

## Agent rules for this roadmap

- Do not reintroduce name heuristics for Metal lanes; use explicit lane kind.
- Do not call target time CPU time.
- Do not nest target work under a CPU stack unless wait evidence proves it.
- Do not hand-write JSON payloads; use typed protocol structures.
- Do not edit generated files directly; edit source and regenerate.
- Prefer production-shaped integration tests and smokes over tiny isolated
  tests when the behavior crosses process, archive, CLI, or browser surfaces.
- When investigating Metal, read SDK headers and `objc2-metal` generated
  bindings before concluding an API is absent.
- Keep bee-specific hooks behind the general stax-target model.

## Definition of success

A user recording bee's `hx` should be able to open stax and answer, from the
same run:

- Which CPU stacks are active?
- Which threads are waiting, and why?
- Which Metal lanes are running?
- Which shaders ran most often?
- Which shaders took the most time?
- Which individual dispatches were outliers?
- Which CPU stack queued a selected dispatch?
- Which CPU stack waited for it, if any?
- Which Metal source produced the selected shader?
- Which buffers/tensors were attached?
- Which counters were collected?
- How did this run compare to a previous archive?

That is the bar: stax becomes the place where CPU, wait, target, source,
metadata, and counters meet.
