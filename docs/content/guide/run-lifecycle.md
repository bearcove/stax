+++
title = "Run Lifecycle"
weight = 3
insert_anchor_links = "heading"
+++

A *run* is one recording session. `stax-server` tracks every run it has
hosted and enforces one simple rule. This page covers that rule and the four
commands that observe and control runs.

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
aggregator's current contents: whichever run is **active**, or, if none is,
the **most recent** one. The aggregator stays populated after a run stops,
and is only cleared by the next `stax record`. So:

```bash
stax record -- ./bench       # run 1 — aggregator now holds run 1
stax wait --for-samples 5000
stax top                     # snapshot of run 1
stax stop                    # run 1 stops; aggregator keeps run 1
stax top                     # still works — same data

stax record -- ./bench       # run 2 — aggregator reset; run 1's data is gone
```

To keep an older run queryable, stop it and *don't* start a new recording
until you're done looking. Per-`RunId` querying is on the roadmap.

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
is in-memory only for now; it does not survive a daemon restart.

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

## Putting it together

```bash
stax record -- ./bench &           # background the recording
stax wait --for-samples 10000 \
          --timeout-ms 60000 || {  # bail if it never gets there
  echo "not enough samples in 60s"; stax stop; exit 1
}
stax top -n 20
stax stop
```
