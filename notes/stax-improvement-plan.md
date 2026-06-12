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
- `current_span_origin()`
- `span_with_current_origin(...)`
- `report(lane, spans)`
- `try_report(lane, spans)`
- `CapturedOrigin`
- `SpanBuilder`
- `Lane::new(name)`
- `Lane::{reporting_active,current_origin,origin_if_active,capture_origin}`
- `Lane::{span,span_builder,span_with_origin,span_with_captured_origin,span_with_current_origin}`
- `Lane::{begin_span,begin_span_with_origin,begin_span_with_captured_origin}`
- `Lane::{report,try_report,report_if_active,report_one,report_one_if_active}`
- `OpenSpan::{finish,finish_and_report}`
- `now_ns()`

Remaining API polish:

- Decide whether a RAII guard is worth adding.
  - Current explicit `OpenSpan::finish_and_report` is correct and avoids
    "drop is protocol" footguns.
  - A RAII guard could be convenient, but only if it is opt-in and clearly
    documented as best-effort telemetry.
- Add queue/backpressure observability from the target side:
  - local dropped-batch count
  - connection state
  - last gate state
  - maybe exposed through tracing and/or a lightweight accessor

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
  - bad span durations
  - missing origins
  - unlinked origins, including synthetic tid, no sampled thread, no user
    stack, and nearest PET sample too far
  - linked and too-far origin PET distance min/avg/max

Remaining CLI work:

- Teach `stax diagnose` about queue drops in `stax-target`, if target-side
  counters are exposed.
- Decide whether the lane table should grow a richer per-lane diagnostic view
  or whether the compact per-lane reason row is enough.
- Add a `stax lanes` or `stax targets` command only if `threads` cannot remain
  clear enough.
  - Prefer improving `threads` first.
- Make help text and `--help` copy mention target spans in the same places
  docs do.
- Add CLI snapshot tests once the blessed corpus exists.

Acceptance criteria:

- A GPU-bound or executor-bound run with few CPU samples points the user at
  `threads`, target lanes, or `stax-target`.
- A run with target spans but no linked origins points the user at origin
  placement.
- A synthetic tid is easy to discover without `-n 2000`.
- Diagnostics distinguish "target did not report", "server dropped it", and
  "origin did not link", including why the origin did not link.

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

### 4.1 Product surface

Add commands along these lines:

- `stax save <PATH>`
  - saves the current or most recent run
  - works after `stax stop` as long as the aggregator is still populated
- `stax open <PATH>`
  - loads a saved run into a queryable local server state
  - or starts a read-only server if that fits the architecture better
- `stax export <PATH> --format ...`
  - optional after the internal format exists
  - not a substitute for native persisted runs
- `stax compare <A> <B>`
  - later, after saved runs exist

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

- Prefer an append-friendly event log with typed records.
- Use facet-shaped records, not manual JSON.
- Consider a directory archive first for simplicity:
  - `manifest`
  - `events`
  - `symbols`
  - `blobs/`
- A single-file `.stax` archive can come after the schema is proven.
- Keep run persistence orthogonal to live recording. The live aggregator should
  remain fast and simple.

### 4.3 Query model

Add per-run querying:

- The current "active or most recent aggregator" model is useful but limited.
- Saved runs imply explicit run identity in CLI and RPC:
  - `--run <ID>` for live history
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
- Bug reports can attach one file or one directory.

Verification:

```bash
stax record -- <blessed-demo>
stax stop
stax save /tmp/demo.stax
stax open /tmp/demo.stax
stax threads -n 0
stax top -n 20 --sort self
stax flame -d 8 --threshold-pct 0
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
  - Playwright or browser automation against a deterministic saved/demo run
  - checks nonblank flame, target columns, lane visibility, click behavior

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

Remaining work:

- Metric selector:
  - active time
  - CPU time
  - target time
  - off-CPU time
  - maybe wait reason
- Flamegraph:
  - target-time width mode
  - target-span count visible where useful
  - origin-linked target spans naturally visible under CPU callers
- Threads/lanes:
  - target lanes visually distinct from CPU threads
  - synthetic lanes not hidden in long thread lists
  - clicking a lane focuses flame/top/timeline to that tid
- Timeline:
  - lane track for target spans
  - span hover/detail:
    - name
    - duration
    - lane
    - count/aggregate if grouped
    - origin tid and nearest CPU stack if linked
- Details panels:
  - answer "who queued this?"
  - answer "how many times and total duration?"
  - answer "which CPU thread dispatched it?"
- Empty states:
  - same guidance as CLI:
    - no samples yet
    - CPU idle/off-CPU
    - target integration available
    - target spans arriving but origins not linking

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
  - target spans in existing views
  - origin-linked attribution
  - persistence/reopen semantics
  - diagnostics/hints
- Add `just` recipes or documented commands for:
  - focused Rust checks
  - docs build
  - frontend build
  - blessed demo run
  - save/reopen smoke after persistence exists

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
  missing origin, and unlinked origin reasons.

### Phase C: build the blessed corpus

1. Add the corpus binary. Done: `stax-target/examples/corpus.rs`.
2. Add scripts or docs for recording it. Done: `just demo-corpus` and the
   target-span integration guide.
3. Add CLI snapshot-like assertions where stable.
4. Use it in docs. Started: integration guide uses the corpus as the default
   demo workload.

Done when:

- One command produces a run proving CPU, off-CPU, target lane, linked origin,
  and bad-origin diagnostics.

### Phase D: persistence and reopen

1. Design typed event/archive schema.
2. Save current run.
3. Reopen saved run.
4. Query saved run through CLI.
5. Preserve target spans and origins.
6. Document archive compatibility.

Done when:

- Record -> stop -> save -> restart server -> open -> threads/top/flame works.

### Phase E: web target-time polish

1. Metric selector.
2. Lane-first thread/timeline affordances.
3. Span hover/detail.
4. CPU origin navigation.
5. Empty-state guidance.
6. Browser verification.

Done when:

- The web UI can answer "what target work dominated?" and "who queued it?"
  without leaving the UI.

### Phase F: cleanup, docs, and developer workflows

1. Public docs and README reflect all shipped behavior.
2. Agent manual reflects operational workflows and pitfalls.
3. Add or update `just`/docs commands for routine verification.
4. Add Tracey coverage if the repo is configured for it.
5. Remove stale "roadmap" statements once features land.

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

- Should persistence be a directory archive first, or a single `.stax` file
  immediately?
- Should saved-run querying load into `stax-server`, or should the CLI be able
  to query archives directly?
- Should target-side queue-drop counters be pushed to the server, exposed
  locally through an API, or both?
- Should there be a distinct `stax lanes` command, or should `stax threads`
  remain the single discovery surface?
- How much Metal-specific sample code belongs in this repo versus docs that
  point at bee/hx?
- Should RAII span guards exist, or should stax keep the explicit
  finish/report model only?
- Which corpus checks are stable enough for CI without making tests brittle?

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
