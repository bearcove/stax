# Stax end-user, integrator, and developer improvement plan

This is the execution plan for making stax feel less like "a profiler with
clever hidden powers" and more like the obvious local observability substrate
for performance work.

The product thesis is:

```text
one recording = CPU stacks + off-CPU waits + target/executor spans + origins
```

The flagship path is:

```text
CPU queue/dispatch stack -> target lane -> named work item
```

This plan is intentionally written as one document so an agent can pick it up,
work through it, and know what "done" means without reconstructing context
from chat.

## Current baseline

Already landed before this plan was written:

- `5f6144b Surface target span time across reports`
  - Target spans now participate in `threads`, `top`, `flame`, CLI copy,
    docs, and web data as explicit target time/span counts.
  - Origin-linked spans can render under CPU dispatch stacks.
- `0532c4d Make stax-target easier to integrate`
  - `stax-target` has `Lane`, `OpenSpan`, `now_ns()`, origin helpers, an
    executor example, and a generic target-span integration guide.
- `fd49205 Guide empty profiling views toward useful surfaces`
  - Empty `top` / `flame` views now guide users toward `threads`, target lane
    tids, off-CPU interpretation, or `stax-target` integration.
- `2ad4c09 Refresh README around target spans`
  - First-contact README copy now describes CPU stacks, off-CPU waits, and
    cooperating target spans.
- `db181ba Explain target ingest diagnostics in diagnose`
  - `stax diagnose` interprets target-ingest counters for missing batches,
    bad durations, missing origins, and origins that do not link.

Known existing unrelated blocker:

- `cargo check --workspace --all-features --all-targets` currently fails in
  `stax-linux-capture/examples/*`. Do not confuse that with target-span work.
  Focused checks for touched packages are still required.

## Non-negotiable behavior

- stax is not just a CPU sampler.
- GPU-bound or executor-bound workloads must not look like "stax did not
  work"; they should lead users to target lanes, off-CPU waits, or integration
  steps.
- Target spans are exact-duration execution intervals, not CPU samples.
- Target spans must remain visible through the existing views:
  - `stax threads`
  - `stax top`
  - `stax flame`
  - timeline/web UI
  - diagnostics
- Origin-linked spans must preserve the causal path from a sampled CPU dispatch
  stack to target work.
- `stax-target` should be safe and boring when inactive: no background
  surprises beyond the capture gate worker, bounded queues, and no
  instrumentation cost on hot paths unless a matching recording is active.
- Do not hand-roll JSON output. If structured export is needed, use facet
  shapes and the repo's serialization conventions.
- Do not edit generated files. Edit source and regenerate.

## Workstream 1: first-class target/executor integration

Goal: make `stax-target` the polished crate an integrator naturally imports
for GPU queues, async executors, worker pools, model runtimes, codecs, and
other execution lanes the CPU sampler cannot observe directly.

### 1.1 API shape

Current API:

- `reporting_active()`
- `reporter_stats()`
- `current_span_origin()`
- `span_with_current_origin(...)`
- `report(lane, spans)`
- `try_report(lane, spans)`
- `CapturedOrigin`
- `SpanBuilder`
- `Lane::new(name)`
- `Lane::{reporting_active,current_origin,origin_if_active,capture_origin}`
- `Lane::reporter_stats`
- `Lane::{span,span_builder,span_with_origin,span_with_captured_origin,span_with_current_origin}`
- `Lane::{begin_span,begin_span_with_origin,begin_span_with_captured_origin}`
- `Lane::{report,try_report,report_if_active,report_one,report_one_if_active}`
- `OpenSpan::{finish,finish_and_report}`
- `now_ns()`

API polish completed:

- Done: keep the explicit `OpenSpan::finish_and_report` model instead of a
  RAII-on-drop guard.
  - `drop` is not a protocol action.
  - Integrators may still build their own best-effort guard on top, but the
    blessed API keeps completion/reporting explicit.
- Add queue/backpressure observability from the target side:
  - done: local dropped-batch count is exposed through `ReporterStats` and
    `stax diagnose` while capture is active
  - done: `ReporterStats` also exposes `worker_started`, `reporting_active`,
    and `connected_to_server` as target-local health state
  - done: `Lane::reporter_stats()` gives lane-centric integrations the same
    passive snapshot without arming the background worker

Acceptance criteria:

- Common integrator code is three or four obvious lines, not a hand-rolled
  sequence of wire structs.
- It is hard to accidentally report spans when no recording is active.
- It is hard to put origins in the wrong place without docs or diagnostics
  making it obvious.
- Existing direct `report(lane, spans)` users continue to work unless the
  repo intentionally decides to break them.

Verification:

```bash
cargo check -p stax-target --all-targets --message-format=short
cargo nextest list -p stax-target --all-targets
cargo nextest run -p stax-target --all-targets
```

## Workstream 2: examples and integration corpus

Goal: provide one blessed set of target programs that exercise the whole stax
story close to production paths.

### 2.1 Examples inside `stax-target`

Already present:

- `examples/probe.rs`
- `examples/executor.rs`
- `examples/thread_pool.rs`
  - CPU queue side captures origin.
  - Worker side reports spans under a named pool lane.
  - Includes multiple work names for per-span aggregation.
- `examples/async_executor.rs`
  - Shows a Tokio-style enqueue/poll/complete path.
  - Demonstrates where origin capture belongs when work moves between tasks.
- `examples/codec.rs`
  - Simulates decode/encode stages with CPU work, waits, and target spans.
  - Good for docs because codecs are easier to understand than GPU APIs.
- `examples/model_runtime.rs`
  - Simulates prefill/decode/attention/cache update lanes.
  - Mirrors the bee/hx shape without depending on bee.
- `examples/gpu_timestamps.rs`
  - Compile-checked Metal-style timestamp-counter skeleton without SDK
    dependencies.
  - Shows dispatch origin capture, external timestamp conversion, and
    completion-time reporting.
- `examples/bad_origins.rs`
  - Intentionally captures origins too early, from the wrong thread, or too
    far from available CPU samples.
  - Used to prove `stax diagnose` explains failures.
- `examples/corpus.rs`
  - Blessed all-in-one workload with CPU hot loops, off-CPU sleeps, linked
    executor spans, GPU-like exact timestamp spans, and intentionally broken
    origin spans.
  - `just demo-corpus` records it and prints `threads -n 0` plus `diagnose`.

### 2.2 GPU/Metal specialization

Add a GPU-facing example or guide-backed skeleton:

- A compile-checked skeleton now exists in `examples/gpu_timestamps.rs`.
- If a real Metal example becomes practical in this repo, add one that shows:
  - command encoding
  - timestamp-counter start/end conversion
  - origin capture at dispatch/queue time
  - reporting from the completion/reporting thread
- Until then, keep the full worked Metal example in docs and real consumers
  such as bee/hx.

Acceptance criteria:

- Each example has a doc comment with exact commands:
  - `stax record -- cargo run -p stax-target --example ...`
  - `stax threads`
  - `stax top --tid ...`
  - `stax flame --tid ...`
  - `stax diagnose`
- At least one example proves linked origins.
- At least one example proves diagnostic handling of bad origins.
- The examples are used by docs and, where practical, CI.

Verification:

```bash
cargo check -p stax-target --examples --message-format=short
cargo nextest run -p stax-target --all-targets
```

Optional live smoke, when daemons are available:

```bash
just demo-corpus
stax threads -n 0
stax top --tid <synthetic-tid> --sort self
stax flame --tid <synthetic-tid>
stax diagnose
```

## Workstream 3: better discovery in CLI and diagnostics

Goal: when a user stares at a flat or empty CPU view, stax should guide them to
the right surface without requiring prior knowledge of target lanes.

Already present:

- Metal command/dispatch frame hint when no target lane exists.
- Empty `top` / `flame` hints for:
  - off-CPU-only views
  - existing target lanes outside a `--tid` filter
  - thread activity with no frames yet
- `threads` always includes synthetic target lanes with spans past the normal
  limit.
- `threads` has a `kind` column (`thread` / `target`) so synthetic lanes are
  visually distinct from real sampled threads.
- `diagnose` prints hints for:
  - no target batches
  - batches/spans dropped while no run is active
  - batches/spans dropped because the pid does not match the active run target
  - batches/spans dropped inside stax-target because the local queue filled or
    the worker disconnected
  - bad span durations
  - missing origins
  - unlinked origins, including synthetic tid, no sampled thread, no user
    stack, and nearest PET sample too far
  - linked and too-far origin PET distance min/avg/max

CLI decisions and follow-up:

- Done: the compact `threads` lane row is enough for now; richer per-lane
  diagnostics should wait for evidence that `threads` is crowded again.
- Done: no separate `stax lanes`/`stax targets` command for this phase.
  `threads` remains the single discovery surface for real threads and
  synthetic lanes.
- Done: help text and `--help` copy mention CPU stacks, off-CPU waits,
  target spans, synthetic lanes, target time, and target span counts on the
  relevant commands.
- Done: stable CLI snapshot-style coverage started with
  `threads_output_keeps_target_lanes_past_limit`.

Acceptance criteria:

- A GPU-bound or executor-bound run with few CPU samples points the user at
  `threads`, target lanes, or `stax-target`.
- A run with target spans but no linked origins points the user at origin
  placement.
- A synthetic tid is easy to discover without `-n 2000`.
- Diagnostics distinguish "target did not report", "server dropped it", and
  "origin did not link", including why the origin did not link and whether
  stax-target dropped batches locally before the server saw them.

Verification:

```bash
cargo check -p stax --all-targets --message-format=short
cargo nextest run -p stax --all-targets empty_view_hint
cargo nextest run -p stax --all-targets target_ingest_hints
```

## Workstream 4: durable recording/export story

Goal: make runs saveable, reopenable, shareable, and comparable. The live
aggregator is powerful, but users eventually need artifacts for bug reports,
CI regressions, and before/after analysis.

Status as of 2026-06-12:

- Directory archive MVP is implemented:
  - `stax save <PATH>` writes a v2 directory archive with `manifest.json`
    plus typed chunks: `aggregator.json`, `binaries.json`, and
    `target-ingest.json`, plus an append-friendly typed `events.jsonl`
    replay stream.
  - `stax save <PATH>.stax` writes the same saved run as a single-file
    facet-json package containing aggregate chunks and event records.
  - `manifest.json` records archive version, save time, producer/version,
    OS/arch, run summaries, and archive-relative chunk filenames.
  - `stax open <PATH>` loads that archive back into `stax-server`'s current
    query state.
  - `stax open` and `stax compare` replay `events.jsonl` or embedded package
    events when present; aggregate chunks remain the fallback and inspection
    path for legacy/minimal archives.
  - `open` refuses to replace state while a recording is active.
- `stax compare <BASELINE> <CANDIDATE>` reads two archives directly and
  prints regression-oriented deltas. `stax compare --json` emits the same
  comparison as a facet-json report for CI and benchmark notes. Threshold
  flags such as `--fail-target-delta-ms`, `--fail-target-delta-pct`, and
  `--fail-unlinked-origins-delta` let CI fail directly on saved-run
  regressions; the JSON report includes `threshold_failures`.
- The archive is facet-json and versioned (`format_version = 2` for new
  saves). `stax open` and `stax compare` still read legacy v1 `archive.json`
  archives.
- The MVP stores:
  - run summary
  - manifest provenance: producer/version, OS, and architecture
  - raw aggregator streams: PET samples, intervals, target synthetic spans,
    wakeups, and thread names
  - binary/symbol metadata, including inline text bytes when present
  - target-ingest diagnostics, including origin-link counters and
    target-side queue drops
  - a chronological `events.jsonl` sidecar of facet-json `SavedEventLogEntry`
    records, produced from the saved snapshot and replayed by `open` and
    `compare` when present
- Reopened archives are queryable through the normal `threads`, `top`,
  `flame`, and `diagnose` surfaces.
- Archive compatibility is strict for now: `stax open` and `stax compare`
  accept v2 directory archives, `.stax` packages, and legacy v1
  `archive.json` archives, and reject other versions loudly.
- Stopped/opened runs now keep an in-memory query snapshot while
  `stax-server` stays alive. `stax select-run <ID>` restores one stopped run
  from `stax list` into the current query state. `top`, `flame`, `threads`,
  `annotate`, and `diagnose` accept `--run <ID>` as a non-mutating one-off
  query against stopped in-memory history.
- Current deliberate non-goals:
  - no `blobs/` layout yet
  - annotate depends on saved/host-available bytes in the same way the live
    binary registry does

### 4.1 Product surface

Add commands along these lines:

- `stax save <PATH>`
  - saves the current or most recent run
  - works after `stax stop` as long as the aggregator is still populated
  - implemented as a v2 directory archive containing `manifest.json`,
    `aggregator.json`, `binaries.json`, `target-ingest.json`, and
    `events.jsonl`
  - done: paths ending in `.stax` create a single-file package instead of a
    directory
- `stax open <PATH>`
  - loads a saved run into a queryable local server state
  - implemented by replacing the current server query state; active recordings
    are rejected
- `stax export <PATH> --format ...`
  - optional after the internal format exists
  - not a substitute for native persisted runs
- `stax compare <A> <B>`
  - implemented for typed directory archives and `.stax` packages
  - compares PET sample counts, on/off-CPU interval time, target time, target
    span counts, origin-link counts, ingest drops, and top target lanes
  - done: `--json` produces named baseline/candidate/delta fields without
    scraping the human table
  - done: threshold flags fail the command directly on positive deltas for
    active time, target time, off-CPU time, unlinked/missing origins, and
    ingest drops

### 4.2 Storage model

Do not design this only as a final aggregate. Users need enough data to answer
new questions after recording.

Candidate archive contents:

- manifest:
  - stax version
  - platform
  - run config
  - target pid / command
  - start/stop wall times
  - clock metadata
- event stream:
  - PET samples
  - off-CPU intervals
  - wakeup edges
  - thread names
  - binary load/unload events
  - target span batches
  - jitdump or JIT image records
- symbol/binary metadata:
  - loaded image paths
  - build ids / UUIDs
  - main binary marker
  - optional copied JIT code bytes where needed for annotate
- derived indexes:
  - optional, versioned cache only
  - must be rebuildable from stored events

Implementation direction:

- Done: emit an append-friendly `events.jsonl` sidecar with typed
  `SavedEventLogEntry` records.
- Done: replay `events.jsonl` / embedded package events for `stax open` and
  `stax compare` when present, falling back to aggregate chunks for legacy or
  intentionally minimal archives.
- Use facet-shaped records, not manual JSON.
- Consider a directory archive first for simplicity:
  - `manifest`
  - done: `events.jsonl` sidecar
  - `symbols`
  - `blobs/`
- Done: a single-file `.stax` package wraps the current schema without adding
  compression/container dependencies.
- Keep run persistence orthogonal to live recording. The live aggregator should
  remain fast and simple.

### 4.3 Query model

Add per-run querying:

- The current "active or most recent aggregator" model is useful but limited.
- Saved runs imply explicit run identity in CLI and RPC:
  - done: `stax select-run <ID>` restores stopped in-memory history into the
    current query state
  - done: reporting commands accept `--run <ID>` as a non-mutating one-off
    query against stopped in-memory history
  - done: `ViewParams.run`, `RunViewParams.run`, and `TimelineParams.run`
    provide non-mutating per-RPC `RunId` selectors for Profiler snapshots,
    subscriptions, timeline, wakers, threads, target-span details, and
    diagnostics
  - `--archive <PATH>` or `stax open`
- Web UI should be able to inspect a saved run without a live target.

Acceptance criteria:

- A user can record, stop, save, restart stax-server, reopen, and run:
  - `stax threads`
  - `stax top`
  - `stax flame`
  - `stax annotate` where code bytes/symbols are available
- Saved runs preserve target spans and origin-linked flame paths.
- The archive format has a version and clear compatibility story.
- Done: bug reports can attach one `.stax` file or one `.staxdir` directory.
- Before/after notes can use `stax compare` without mutating live server state.

Verification:

```bash
stax record -- <blessed-demo>
stax stop
stax save /tmp/demo.staxdir
stax open /tmp/demo.staxdir
stax threads -n 0
stax top -n 20 --sort self
stax flame -d 8 --threshold-pct 0
```

Implemented test coverage:

```bash
cargo nextest run -p stax-server --all-targets -E 'test(save_open_restores_query_state_and_target_diagnostics)'
cargo nextest run -p stax-server --all-targets -E 'test(select_run_restores_stopped_run_query_snapshot)'
cargo nextest run -p stax --all-targets -E 'test(read_saved_archive_accepts_v2_manifest_layout) | test(read_saved_archive_accepts_legacy_v1_archive_json_layout) | test(summarize_archive_counts_target_and_origin_dimensions)'
```

## Workstream 5: blessed integration test/demo corpus

Goal: one target app becomes the oracle for "stax is not just CPU samples".
Docs, CI, CLI snapshots, and web UI checks all use the same workload.

### 5.1 Corpus binary

Added `stax-target/examples/corpus.rs` as the first blessed corpus workload:

- CPU hot loop with stable symbol names.
- Off-CPU waits:
  - sleep
- Target lanes:
  - executor lane
  - GPU-simulated exact-timestamp lane
  - intentionally flawed diagnostics lane
- Linked origins:
  - queue from a CPU thread
  - report work on another worker
  - expect `CPU caller -> lane -> work item`
- Bad origins:
  - missing origin
  - stale origin
  - synthetic target tid
  - missing origin tid
  - bad duration span
- JIT/code-symbol path if practical, but keep it separate if that makes the
  corpus too broad.

### 5.2 Oracle outputs

The corpus should produce stable enough outputs for checks:

- `stax threads -n 0`
  - shows CPU thread rows
  - shows target lane rows
  - target lane not buried
- `stax top --tid <lane>`
  - shows named spans with target time and span counts
- `stax flame --tid <lane>`
  - shows `(all) -> lane -> span`
- `stax flame --tid <cpu>`
  - shows CPU caller -> lane -> span for linked origins
- `stax diagnose`
  - reports batches/spans
  - reports bad durations
  - reports target-side queue drops
  - reports linked and unlinked origins with reason and distance diagnostics
  - prints hints for intentionally bad cases

### 5.3 CI shape

CI does not need privileged profiling for every check.

Layers:

- Compile-only:
  - examples and corpus compile on supported platforms.
- In-process/unit:
  - target ingest tests feed synthetic batches into server state.
  - CLI formatting/hint tests use constructed wire structs.
- Live smoke:
  - platform-gated
  - runs where daemon privileges are available
  - produces a small artifact log for failures
- Web UI smoke:
  - done: `just web-target-smoke` uses browser automation against a
    deterministic saved corpus run restored into a checkout-local server
  - checks nonblank flame, target columns, lane visibility, target-span detail
    rendering, and desktop/mobile overflow

Acceptance criteria:

- One command can generate a demo run with CPU, off-CPU, target spans, and
  origins: `just demo-corpus`.
- Docs use that command instead of hand-wavy examples.
- Regressions in target ingest, CLI display, or web lane rendering are caught
  before a human records bee/hx and notices manually.

## Workstream 6: web UI target-time polish

Goal: make target time a first-class way to navigate the recording, not just
extra numbers attached to CPU-centric views.

Current state:

- Web UI receives target time/span fields in the same protocol as CLI.
- Docs say hover text and flame/top surfaces expose target counts.
- Added a target display mode. The topbar metric selector now includes target
  time; flame widths, top-table bars/primary durations, thread dropdown
  ordering/bars, and the timeline strip all pivot to target duration.
- Target mode uses distinct target colors in the timeline, top table, and
  thread switcher.
- Added a target-span detail RPC and web tab. It streams individual synthetic
  span intervals with lane/span names, duration, origin tid, and origin-link
  status from the existing aggregator event log.
- The target-span detail RPC/web tab now also groups target work by
  lane/span/origin, with count, total duration, max duration, and newest
  occurrence.
- Target-span details now include origin symbol addresses and include
  origin-linked spans when the selected tid is the CPU dispatch thread, so the
  web UI can jump from target work back to the queueing CPU symbol.
- The topbar now has a run selector backed by `RunControl::list_runs` and
  `ViewParams.run` / `RunViewParams.run`, so the web UI can inspect stopped
  in-memory history without changing the server's selected query state.

Implemented and deferred work:

- Metric selector:
  - done: active time
  - done: CPU-only time
  - done: target time
  - done: off-CPU time
  - done: wall time
  - later: wait-reason-specific modes
- Flamegraph:
  - done: target-time width mode
  - target-span count visible where useful
  - origin-linked target spans naturally visible under CPU callers
- Threads/lanes:
  - done: run selector switches all live panels between current query state
    and stopped in-memory run history
  - done: target mode sorts/bars by target duration, so lanes float up
  - done: CLI regression test proves synthetic target lanes remain visible
    even when `stax threads -n` cuts off ordinary threads
  - done: clicking a lane focuses flame/top/timeline to that tid
- Timeline:
  - done: selected metric drives the timeline area; target mode shows
    target duration over time
  - done: lane tracks show top target lanes as per-bucket target-time rows
    and click through to the synthetic lane tid
  - span hover/detail:
    - name
    - duration
    - lane
    - count/aggregate if grouped
    - origin tid and nearest CPU stack if linked
- Details panels:
  - done: target spans tab answers span count, total duration, max duration,
    lane/name, origin tid/link state, and most recent individual spans
  - done: target spans tab has a compact top-lane, top-span, top-origin, and
    origin-coverage strip before the detailed tables
  - done: lane cells in target-span tables switch directly to the synthetic
    lane tid
  - done: grouped origin cells show who queued linked work and switch to that
    CPU thread
  - done: linked origin cells focus the origin symbol's family tree after
    selecting the CPU thread
- Empty states:
  - done: target-span detail pane distinguishes no reported spans from
    target lanes that exist outside the selected thread
  - done: target-span detail pane offers direct lane selection when
    synthetic lanes have spans
  - done: top table and flamegraph zero-data states now distinguish waiting
    for a recording, hidden rows, off-CPU-only selections, target-bound
    selections, and target spans on another lane

Acceptance criteria:

- A user can start from the web UI, notice a target lane, click it, see span
  names and totals, then navigate to the CPU dispatch stack if origins linked.
- Target time can drive visual prominence, not just appear in a tooltip.
- Mobile/small windows do not overlap labels or controls.

Verification:

```bash
pnpm --dir frontend build
```

For visual changes, run the local server and use browser verification against
desktop and mobile viewports. Check that flame/timeline canvases are nonblank
and target lanes are visible.

## Workstream 7: integrator ergonomics and docs

Goal: make it obvious where to place instrumentation in real code.

Docs to keep current:

- `README.md`
- `AGENTS.md`
- `docs/content/guide/integrating-target-spans.md`
- `docs/content/guide/profiling-gpu-work.md`
- `docs/content/guide/inspecting-a-run.md`
- `docs/content/guide/troubleshooting.md`
- `docs/content/reference/cli.md`
- `docs/content/reference/rpc-services.md`
- `docs/content/concepts/sampling.md`
- `docs/content/guide/web-ui.md`

Add more concrete integration recipes:

- GPU dispatch/completion
  - origin capture at command/dispatch queueing
  - timestamps from GPU/Metal API
  - report from completion/reporting callback
- Async executor
  - origin capture when work is scheduled
  - span starts when worker begins doing work
  - span ends at completion
- Thread pool
  - origin token carried in work item
  - worker lane naming
- Codec/model runtime
  - lanes per stage
  - span names as semantic operations
  - avoid per-item cardinality explosions in names
- Bad origin troubleshooting
  - how stale/wrong-thread origins show up
  - what `stax diagnose` means

Acceptance criteria:

- A new integrator can copy one example and get useful `threads`, `top`, and
  `flame` output without reading server internals.
- The docs use the same terms as the CLI:
  - target time
  - spans
  - synthetic lanes
  - origins
  - CPU dispatch stack
- Known gaps are not left as chat-only knowledge.

Verification:

```bash
ddc build
git diff --check
```

## Workstream 8: developer experience and maintainability

Goal: make stax easier to evolve without breaking its cross-surface behavior.

Tasks:

- Add protocol-level comments when target fields are added or repurposed.
- Keep frontend generated bindings regenerated from source only.
- Add tests at the lowest useful layer:
  - target ingest server tests for aggregation semantics
  - CLI tests for wording and hints
  - corpus/live tests for end-to-end behavior
  - web visual tests for layout and target-lane navigation
- Consider Tracey requirements for:
  - checked: this repo currently has no Tracey config/spec files or
    `[impl ...]`/`[verify ...]` annotations to extend
  - if Tracey is introduced later, first requirements should cover target
    spans in existing views, origin-linked attribution, persistence/reopen
    semantics, and diagnostics/hints
- Add `just` recipes or documented commands for:
  - done: `just fmt-check` and `just diff-check` separate verification from
    formatting
  - done: focused Rust checks (`just check-target`, `just test-target`,
    `just check-cli`, `just test-cli-target-lanes`, `just check-live-proto`,
    `just check-live`, `just check-server`, `just test-cli-compare-json`,
    `just test-server-target-ingest`, `just test-server-run-params`,
    `just check-mac-kperf-parse`, `just test-mac-kperf-timebase`)
  - done: docs build (`just docs`)
  - done: frontend build/typecheck (`just frontend-check`)
  - done: aggregate focused target-span verification (`just target-span-check`)
  - done: blessed demo run (`just demo-corpus`)
  - done: `just archive-smoke` for save/reopen/compare using the blessed
    target-span corpus through a checkout-local server/CLI, including
    `stax select-run` and zero-regression `stax compare --json` thresholds
  - done: `just web-target-smoke` for checkout-local browser verification of
    the run selector, target lanes, target display mode, and target-span
    details
  - done: GitHub Actions focused target-span workflow, with `bearcove/vox`
    checked out as the required sibling and live archive/browser smokes gated
    behind manual dispatch

Acceptance criteria:

- The path from spec/docs -> protocol -> server -> CLI -> web is auditable.
- A future agent can see which test or demo proves a product claim.
- New target-span behavior does not require remembering a live bee/hx recording
  to verify correctness.

## Execution order

### Phase A: lock in target integration ergonomics

1. Finish `stax-target` API helpers.
2. Add thread-pool, async-executor, codec/model-runtime, and bad-origin
   examples.
3. Update integration docs to use the new helpers.
4. Add focused tests/checks.

Done when:

- `stax-target` examples compile.
- Docs have one recipe per integration style.
- `stax diagnose` can explain the bad-origin example.

### Phase B: make discovery impossible to miss

1. Improve `stax threads` lane visibility.
2. Add wrong-pid/no-active-run target-ingest counters.
3. Add origin age/distance diagnostics if server has enough data.
4. Align CLI help/reference/AGENTS docs.

Done when:

- Synthetic lanes are obvious in `threads`.
- `diagnose` differentiates target did not report, wrong pid, bad duration,
  missing origin, local stax-target drops, and unlinked origin reasons.

### Phase C: build the blessed corpus

1. Add the corpus binary. Done: `stax-target/examples/corpus.rs`.
2. Add scripts or docs for recording it. Done: `just demo-corpus` and the
   target-span integration guide.
3. Done: add CLI snapshot-like assertions where stable. Done so far:
   `threads_output_keeps_target_lanes_past_limit`.
4. Done: integration guide uses the corpus as the default demo workload.

Done when:

- One command produces a run proving CPU, off-CPU, target lane, linked origin,
  and bad-origin diagnostics.

### Phase D: persistence and reopen

1. Done: design typed directory archive schema.
2. Done: save current or most recent run.
3. Done: reopen saved run into server query state.
4. Done: query saved run through existing CLI surfaces.
5. Done: preserve target spans, origin-linked stacks, and ingest diagnostics.
6. Done: document archive compatibility.
7. Done: add `just archive-smoke` for the blessed corpus persistence path
   through a checkout-local server/CLI.
8. Done: add stopped-run query snapshots plus `stax select-run <ID>` for
   restoring in-memory history into the current query state.
9. Done: add reporting-command `--run <ID>` query selectors for `threads`, `top`,
   `flame`, `annotate`, and `diagnose`, backed by non-mutating per-RPC run
   selectors.
10. Done: normalize macOS kdebug mach tick timestamps to nanoseconds at the
    parser pipeline boundary, so `stax-target` origin timestamps and PET
    sample timestamps share one clock domain on Intel and Apple Silicon Macs.
11. Done: add single-file `.stax` packages for save/open/compare without
    introducing a new compression/container dependency.
12. Done: make v2 archive reads event-log-driven when an event stream is
    present, for both daemon `open` and CLI `compare`.

Done when:

- Record -> stop -> save -> restart server -> open -> threads/top/flame works.
- Record multiple runs -> `stax list` -> `stax select-run <ID>` ->
  threads/top/flame works while the daemon stays alive.
- Record multiple runs -> `stax top --run <ID>` / `stax flame --run <ID>` /
  `stax diagnose --run <ID>` works for stopped in-memory history.
- A macOS corpus run reports linked origins with sane PET distances instead
  of every good origin being `too_far`.

Deferred:

- `just archive-smoke` / `just web-target-smoke` are wired as manual
  `workflow_dispatch` checks; making them required by default waits until the
  runner contract for local stax server/browser/profiling access is settled.
- `blobs/` remains future work after the v2 aggregate chunk layout and replay
  stream.

### Phase E: web target-time polish

1. Metric selector.
2. Lane-first thread/timeline affordances.
3. Span hover/detail.
4. CPU origin navigation.
5. Empty-state guidance.
6. Done: browser verification for the empty-state slice used the in-app
   browser against a checkout-local `stax-server` on an alternate websocket
   port, including a narrow mobile viewport smoke. Earlier stale installed
   daemon/protocol mismatch on `ws://127.0.0.1:8080` remains a useful warning:
   browser tests should point at a checkout-local server.
7. Done: add `just web-target-smoke`, which records and reopens the blessed
   corpus into a checkout-local server, starts Vite, and drives a browser
   through target mode and target-span details at desktop and mobile widths.

Done when:

- The web UI can answer "what target work dominated?" and "who queued it?"
  without leaving the UI, and can do so for stopped in-memory runs selected
  from the topbar.

### Phase F: cleanup, docs, and developer workflows

1. Public docs and README reflect all shipped behavior.
2. Agent manual reflects operational workflows and pitfalls.
3. Add or update `just`/docs commands for routine verification.
4. Done: checked Tracey coverage path; repo is not currently configured for
   Tracey, so there is no coverage file to update.
5. Done: stale public roadmap/copy sweep; save/open/compare and per-`RunId`
   query selectors are no longer described as absent.

Done when:

- The repository, docs, CLI help, and web UI tell the same story.

## Global verification matrix

Run focused checks after each slice:

```bash
cargo fmt
cargo check -p <touched-package> --all-targets --message-format=short
cargo nextest list -p <touched-package> --all-targets <filter>
cargo nextest run -p <touched-package> --all-targets <filter>
git diff --check
ddc build
```

For frontend changes:

```bash
pnpm --dir frontend typecheck
pnpm --dir frontend build
```

For live target-span behavior when daemons are available:

```bash
stax record -- cargo run -p stax-target --example executor
stax threads -n 0
stax top --tid <synthetic-tid> --sort self
stax flame --tid <synthetic-tid> --threshold-pct 0
stax diagnose
```

Do not treat the broad workspace all-target check as the only signal until the
known `stax-linux-capture/examples/*` failures are resolved. Report them
plainly when they block broad verification.

## Open design decisions

Resolved for this phase:

- Format v2 became a chunked directory layout first, then a single-file
  `.stax` package that wraps the same data. The directory shape keeps
  manifests, events, symbols, and future blobs inspectable while the schema
  settles.
- Detailed archive inspection should continue through `stax open` and the
  normal query surfaces. Direct archive-local CLI should stay focused on
  `stax compare` until there is a clear use case for a second query engine.
- Target-side queue-drop counters are both local and server-visible:
  `stax_target::reporter_stats()` / `Lane::reporter_stats()` for in-process
  health, and `stax diagnose` for run/recording health while capture is
  active.
- `stax threads` remains the lane discovery surface for now. Add `stax lanes`
  only if `threads` becomes crowded again despite the `kind` column, target
  counts, and always-visible synthetic lanes.
- Metal-specific code in this repo should stay SDK-neutral and
  compile-checked. The real Metal 4 integration belongs in consumers such as
  bee/hx, with this repo documenting the contract and providing
  `examples/gpu_timestamps.rs`.
- No RAII span guard in the blessed API for now. Explicit finish/report keeps
  completion semantics visible and avoids treating `drop` as a protocol
  action.
- Stable CI corpus checks should be compile/unit/CLI-format checks first.
  Done: focused checks run on PRs/pushes in `.github/workflows/target-spans.yml`.
  Live `just archive-smoke` / `just web-target-smoke` are gated by manual
  workflow dispatch until the runner contract for recording and browser access
  is settled.
- Saved-run format v2 is implemented as a chunked directory layout and a
  single-file `.stax` package for new saves, with an append-friendly
  `events.jsonl` sidecar in the directory form and embedded event records in
  the package form. `open` and `compare` replay those records when present and
  keep aggregate chunks as fallback/inspection material. Stopped-run history
  has both a state-changing selector via `select-run` and non-mutating per-RPC
  `RunId` selectors for reporting surfaces.

Still open:

- Whether binary text bytes should move out of `binaries.json` into a `blobs/`
  layout for large archives and more inspectable package contents.
- Whether the manual live smokes should become required checks once the runner
  can provide browser dependencies and stax daemon/profiling access.

## Working rule for future agents

When continuing this plan:

1. Pick the next unchecked item in execution order.
2. Read the relevant source path before changing behavior.
3. Make the smallest coherent slice.
4. Update docs in the same slice when user-facing behavior changes.
5. Run focused checks.
6. Commit the slice.
7. Keep going.

The work is complete only when the product claim is true from all primary
surfaces: crate API, CLI, diagnostics, web UI, docs, saved recordings, and the
blessed demo corpus.
