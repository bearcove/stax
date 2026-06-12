+++
title = "Run Lifecycle"
weight = 3
insert_anchor_links = "heading"
+++

A *run* is one recording session. `stax-server` tracks every run it has
hosted and enforces one simple rule. This page covers that rule and the
commands that observe, select, save, and control runs.

## One active run at a time

`stax-server` rejects a second `start_run` while one is in flight. If
`stax record` reports **`another run is already active`**, you have two
choices:

```bash
stax wait     # block until the active run stops on its own
stax stop     # ask the server to stop it now
```

This is deliberate: the live aggregator holds one run's data at a time, and
a single active run keeps `stax top` / `stax flame` unambiguous about *which*
run they describe.

## Which run do queries see?

The query commands — `top`, `flame`, `threads`, `annotate` — operate on the
daemon's **current query state**. While a recording is active, that state is
the active run. After a run stops, its snapshot stays selected. `stax open`
selects an archive, and `stax select-run <ID>` selects a stopped in-memory run
from `stax list`. So:

```bash
stax record -- ./bench       # run 1 — aggregator now holds run 1
stax wait --for-samples 5000
stax top                     # snapshot of run 1
stax stop                    # run 1 stops; aggregator keeps run 1
stax top                     # still works — same data

stax record -- ./bench       # run 2 — current query state switches to run 2
stax stop
stax select-run 1            # restore run 1 into the current query state
stax top --run 1             # one-off query of run 1; current state unchanged
```

In-memory run history does not survive a daemon restart. To keep a run beyond
the current server process, save it:

```bash
stax stop
stax save /tmp/stax-demo.staxdir
stax open /tmp/stax-demo.staxdir
```

`stax open` loads the saved run back into the current query state. For
stopped in-memory history, `top`, `flame`, `threads`, `annotate`, and
`diagnose` also accept `--run <ID>` for a non-mutating one-off query. It has
the same server-memory lifetime as `select-run`: save anything that must
survive a daemon restart.

## stax status

A snapshot of the daemon: the active run if there is one, plus when the
daemon itself started.

```bash
stax status
```

```text
active run:
  run 1  [recording]  pid 12345  4824 samples / 119 intervals  (./bench)
```

## stax list

Every run the daemon has hosted — active and finished, oldest first. History
is server-memory history; it does not survive a daemon restart unless you save
the current queryable run with [`stax save`](#stax-save). Use
[`stax select-run`](#stax-select-run) to make an older stopped row current
again before querying it.

```bash
stax list
```

```text
  run 1  [stopped]    pid 11000  9421 samples / 244 intervals  (./bench)
  run 2  [recording]  pid 12345  4824 samples / 119 intervals  (./bench)
```

## stax wait

Block until something happens. With no flags, it waits for the active run to
reach `Stopped`. With a condition flag, it returns as soon as that condition
fires.

```bash
stax wait --for-samples 5000 --timeout-ms 10000
```

```text
condition met:
  run 2  [recording]  pid 12345  5012 samples / 124 intervals  (./bench)
```

| flag                  | meaning                                                          |
|-----------------------|------------------------------------------------------------------|
| *(none)*              | wait for the active run to stop                                  |
| `--for-samples <N>`   | return after at least N PET samples have been ingested           |
| `--for-seconds <N>`   | return after N seconds of wall-clock time                        |
| `--until-symbol <S>`  | return once a symbol containing substring S is seen (case-sensitive) |
| `--timeout-ms <MS>`   | hard cap on the whole wait                                       |

The first three are **mutually exclusive** — pass at most one. `--timeout-ms`
is independent and can be combined with any of them.

`stax wait` is the backbone of scripted and agent-driven profiling: start a
recording in the background, `wait` until there's enough data, then query.

### Exit codes

| code | situation                                          |
|------|----------------------------------------------------|
| `0`  | condition met, or the run reached `Stopped` cleanly|
| `1`  | timed out, or no active run, or any other error    |

A timeout prints `timed out` and exits `1`, so `stax wait … || handle-it`
works as expected in a script.

## stax stop

Ask the daemon to stop the active run cleanly. It prints the final summary.

```bash
stax stop
```

```text
stopped:
  run 2  [stopped]  pid 12345  5012 samples / 124 intervals  (./bench)
```

`stax stop` exits non-zero if there is no active run. Stopping a run does not
discard its data — the aggregator stays queryable until the next recording
(see [above](#which-run-do-queries-see)).

## stax save

Write the current or most recent queryable run to a directory archive.

```bash
stax save /tmp/stax-demo.staxdir
```

The current archive format is v2: `manifest.json` plus typed facet-json
chunks (`aggregator.json`, `binaries.json`, and `target-ingest.json`) plus an
append-friendly `events.jsonl` sidecar. The manifest records archive version,
save time, producer/version, OS/arch, run summaries, and archive-relative chunk
filenames. The chunks store raw aggregator streams, binary/symbol metadata, and
target-ingest diagnostics; `events.jsonl` serializes typed `SavedEventLogEntry`
records for future replay/import tooling. The archive preserves target spans
and origin-linked stacks for later `threads`, `top`, `flame`, and `diagnose`
queries.

`stax save` works while a run is active, and after `stax stop`, until the
next `stax record` resets the live aggregator.

Archive compatibility is strict in the current format: `stax open` and
`stax compare` accept v2 manifest archives and legacy v1 `archive.json`
archives, and reject other versions loudly. Treat saved archives as
developer/regression artifacts for the matching stax format until a migration
policy or stable package format lands.

## stax open

Load a saved run archive into the daemon's current query state.

```bash
stax open /tmp/stax-demo.staxdir
stax threads -n 0
stax top -n 20
stax flame --threshold-pct 0
```

`stax open` accepts the archive directory, the v2 `manifest.json` inside it,
or a legacy v1 `archive.json`. It refuses to replace state while a recording
is active; stop the active run first.

## stax select-run

Restore a stopped in-memory run from `stax list` into the daemon's current
query state.

```bash
stax list
stax select-run 1
stax top -n 20
stax flame --threshold-pct 0
stax diagnose --run 1
```

`select-run` refuses to replace state while a recording is active. It is a
server-memory selector, not persistence: after a daemon restart, use
[`stax open`](#stax-open) on a saved archive instead.
For one-off inspection, `stax threads --run 1`, `stax top --run 1`,
`stax flame --run 1`, `stax annotate --run 1 …`, and
`stax diagnose --run 1` query the stopped run without changing the current
query state.

## stax compare

Compare two saved archives without loading either one into `stax-server`.

```bash
stax compare /tmp/before.staxdir /tmp/after.staxdir
stax compare --json /tmp/before.staxdir /tmp/after.staxdir > /tmp/stax-compare.json
stax compare \
  --fail-target-delta-ms 25 \
  --fail-target-delta-pct 10 \
  --fail-unlinked-origins-delta 0 \
  /tmp/before.staxdir /tmp/after.staxdir
```

The command reads each archive's typed manifest/chunks directly and prints
deltas for PET samples, on/off-CPU interval time, target time, target span
counts, origin-link counts, ingest drops, and the top target lanes by
duration. The default is a human table; `--json` prints a facet-json report
with named baseline/candidate/delta fields for scripts and benchmark notes.
Threshold flags make the command fail directly when a candidate regresses
past your limit, and JSON output includes the same decision in
`threshold_failures`.
Legacy v1 `archive.json` inputs are accepted too. Use it for quick
before/after checks and regression notes; use `stax open` when you want to
inspect one archive through `threads`, `top`, `flame`, `diagnose`, or the web
UI.

For a live persistence smoke with the blessed target-span corpus:

```bash
just archive-smoke
```

That recipe starts a checkout-local `stax-server` on a temporary socket,
records `stax-target/examples/corpus.rs` with the checkout CLI, saves the run
to a temporary archive directory, reopens it, exercises `stax select-run` and
non-mutating per-command `--run`, queries the restored run through
`threads`/`top`/`flame`/`diagnose`, and runs text plus JSON `stax compare`
against itself.

The focused target-span workflow runs the static verifier on normal PRs. The
archive and web smokes are available as manual `workflow_dispatch` checks in
`.github/workflows/target-spans.yml` because they need a runner that can
manage local stax servers, recording, Vite, and Playwright.

## Putting it together

```bash
stax record -- ./bench &           # background the recording
stax wait --for-samples 10000 \
          --timeout-ms 60000 || {  # bail if it never gets there
  echo "not enough samples in 60s"; stax stop; exit 1
}
stax top -n 20
stax stop
stax save /tmp/stax-demo.staxdir
```
