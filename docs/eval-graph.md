# Lazy evaluation graph imports (initial slice)

`stax graph` and the **eval dataflow** UI tab inspect self-contained
`vix-eval-graph` **version 1** JSONL files. This surface is independent of
sampling, target-span queues, the currently selected run, and active recordings.
It does not require rebuilding Vix against stax's Facet/Vox dependencies.

## CLI: no daemon needed

```sh
cargo build -p stax
# Each command reopens the immutable capture file in its own local namespace.
target/debug/stax graph test-data/eval-graph/shared.jsonl
target/debug/stax graph capture.jsonl --view callables --limit 50
target/debug/stax graph capture.jsonl --view invocations --module 1 --site 42
target/debug/stax graph capture.jsonl --view search --query payload --kind invocation
target/debug/stax graph capture.jsonl --view inspect --node invocation:1
target/debug/stax graph capture.jsonl --view neighborhood --node thunk:1 \
  --direction incoming --offset 0 --limit 50
```

Output is escaped, structured JSON. `summary` is the default view. Pages have
an exact total and offset, and a limit of 1–200. `callables` ranks exact **recorded
body starts** per module/source site, separately from individual invocation
records. `invocations` selects the entries for one exact module/site pair.
Search matches typed node IDs, names, source file metadata and observed previews.
`neighborhood` pages aggregated incoming/outgoing/both relations, preserving
unique shared nodes, argument indices, observed signed runtime states,
multiplicities, and first/last observation sequence/timestamp. It does not turn
a DAG into a call tree. The original JSONL retains each individual observation.

A partial capture is queryable; the CLI warns on stderr. A successful command
means the query succeeded, **not** that the stream or evaluation was complete.
Read `summary.complete`, diagnostics, and execution statuses independently.
Unsupported headers/versions, I/O errors and size-limit violations fail the
import instead of returning a truncated graph.

## UI / server

Build the updated `stax-server`, regenerate bindings with
`cargo run -p xtask -- codegen`, and build the frontend with `pnpm build` in
`frontend/`. Connect the existing frontend to that server, open **eval dataflow**,
and enter a **server-local** JSONL path. No profiler run is needed. Select a
callable to page its individual invocations, or search nodes; select a node to
inspect source/bindings and navigate the bounded shared-node SVG/relation table.
Incoming and outgoing neighbors use the same typed node identity. Pagination is
explicit: the SVG is only the current relation page, not a whole-program layout.

The one-shot `EvalGraph` Vox service exposes `import_path`, `captures`, `close`,
`summary`, `search`, `callables`, `invocations`, `inspect`, and `neighborhood`.
Its schema lives in `stax-live-proto/src/eval_graph.rs`; TypeScript bindings are
generated through `all_services()`, not edited by hand. Imports and potentially
large map queries run off the async runtime worker and never select, stop or replace a profiler recording.

The server retains at most **four captures**, with explicit close and **no
silent eviction**. Each import gets a new capture ID, even when reopening the
same file. Node IDs such as `thunk:1` are valid only within that capture. The
registry is process-local: **keep the original JSONL file and reopen its path
after restarting stax-server**. Graphs are not embedded into `.stax` packages or
archive directories in this slice; no archive sidecar promises are made.

## What these measurements do (and do not) mean

- This is **lazy dataflow**, not a temporal call tree or per-call timing view.
  Timestamps are observation order, not a measured function duration.
- Invocation records count actual fresh **instrumented source/runtime-thunk body
  starts**, not every logical call, force request or cache lookup. Callable
  rankings include the producer's site kinds rather than pretending all entries
  are ordinary functions. Nullary bodies can be recorded.
- `force_requests` counts the instrumented requests in the file. Coverage is
  within instrumented bodies; eq/deep/scheduler/stub requests and uninstrumented
  paths may be absent. It is **not an evaluation count**.
- `cache_computes`, `cache_hits` and `cache_waits` count cache result codes 0/1/2.
  Cache result 0 is a cache compute/miss outcome, **not a thunk claim/evaluation
  count**. Invocation attributes keep the corresponding per-entry counts.
- Argument edges bind each recorded invocation/argument index to a capture-local
  thunk lifetime ID. Thunks with equal previews remain **different nodes**.
  Previews are optional, already-observed text, at most 256 UTF-8 bytes; import
  never forces a thunk or performs content deduplication. First observed preview
  is kept, with its truncation flag.
- Source metadata includes module/site, kind, name, file/origin, byte range,
  line/column and parameter names. It is metadata, **not verified executed source
  text**: this format has no embedded source hash/text. `<runtime>` sites may
  have range zero. Invocation bindings may include `$thunk` and `capture:name`.
- Repeated-input tuple grouping is not implemented here. The UI exposes exact
  bindings for manual comparison, not input equivalence. In particular `$thunk`
  is self/provenance; runtime-thunk objects may omit mapper/make environments.
- `complete=true` means a well-formed, fully flushed stream with a matching
  footer and resolved definitions, **not successful execution**. Execution
  statuses such as `done`, `poison`, or `out_of_fuel` are displayed separately.
  The producer-defined header `scope` and execution status strings are retained.
- Header `include` selectors and `max_bytes` budgets are displayed explicitly.
  A complete **selected** stream is not a whole-program graph. A producer footer
  `complete:false, reason:"byte_budget"` is a deliberate partial stop, not a
  malformed stream; `partial_reason` and diagnostics explain it. Missing footer
  remains a distinct truncation/I/O-failure signal. Allocation IDs can have gaps
  or arrive out of numeric order; optional `born_ns` is preserved as an exact
  decimal attribute (full u128), distinct from emission observation `ts_ns`.

## Validation and explicit resource bounds

The importer validates the format/version/semantics header, typed nonzero IDs,
record shapes, contiguous sequences, monotonically nondecreasing timestamps,
duplicate definitions, source ranges, thunk identity combinations, preview
bounds, and footer completion flag/record count/finality. Forward references
resolve after reading. Missing modules/sites/nodes, unknown external/null thunk
endpoints, unknown event types, malformed records, sequence/count mismatches,
or missing/truncated footer make the graph explicitly **partial**, with exact
diagnostic count and up to 100 bounded diagnostic examples. Missing endpoints
remain visible as distinct placeholder nodes; unknown identities are never
coalesced by text or a null address.

Default per-import limits are **256 MiB**, **2,000,000 records**, **1 MiB per
record including delimiter**, **1,000,000 nodes**, **1,000,000 sites/modules each**,
**2,000,000 aggregated relations**, and **1,000 execution status records**.
Individual text fields are limited to 16 KiB; site parameters to 256 strings of
at most 1 KiB each. Oversize metadata records are diagnosed as partial; overall
file/record/graph limits reject the import entirely. These are deliberate
initial limits, not an assertion that a heavy Vix capture fits in memory. Larger
workloads need an explicitly designed indexed/on-disk store, not silent graph
retention or unbounded limit increases. Queries currently scan immutable maps off the async runtime worker;
there are no subscriptions or whole-program layout promises.

## Deterministic checks

```sh
CARGO_BUILD_JOBS=2 cargo test -p stax-eval-graph
CARGO_BUILD_JOBS=2 cargo test -p stax-server
CARGO_BUILD_JOBS=2 cargo build -p stax -p stax-server
cargo run -p xtask -- codegen
(cd frontend && pnpm typecheck && pnpm build)
```

Tests cover shared neighbors, many forces versus one cache-compute outcome,
equal previews with distinct IDs, forward/missing references, wrong versions,
truncation, sequence/count/time errors, JSON escaping, signed states, execution
outcome versus stream completeness, import ID isolation, explicit limits,
registry close/reopen, and queries through the production local-socket router.
`test-data/eval-graph/shared.jsonl` is a small reproducible import fixture.

`test-data/eval-graph/vix-shared.jsonl` is a real four-worker producer capture of
`let x = payload(); [twice(x), twice(x)]` returning `[42, 42]`: two recorded
`vix::twice` entries share an argument ID and `vix::payload` starts once. Its
original absolute source path is metadata only; no test opens that path.


### Frontend checkout prerequisite

The repository's existing frontend workspace overrides and Vite/TypeScript
aliases expect an adjacent `../oss/facet` source checkout (relative to stax).
A successful frozen install alone does not verify those linked paths exist.
For this implementation's Linux verification, the missing environment path was
created as a symlink to an isolated sparse checkout of facet commit
`7d629aa0eaaff4cf3d3215f62d242c080f8b2e44` (the revision recorded in Vox
0.10.0-rc.1's `.cargo_vcs_info.json`), with its declared `@noble/hashes@2.2.0`
dependency installed in scratch. No stax dependency or lockfile was rewritten
for a machine-specific absolute path. Provision the matching source/dependencies
when reproducing frontend checks on another host.

### Observed UI limitation outside the graph lane

On the isolated empty-run server used for browser verification, the existing
Profiler subscription path displayed `ChannelError: request scope ended before
channel close` immediately after connecting, before opening the graph tab or
issuing graph requests. The independent EvalGraph lane remained usable: import,
source-site ranking, invocation/identity inspection, neighborhoods, partial
warnings and capture lifecycle all worked. This initial graph change does not
claim to fix that separate empty-run Profiler channel problem.
