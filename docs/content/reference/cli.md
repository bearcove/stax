+++
title = "CLI Reference"
weight = 0
insert_anchor_links = "heading"
+++

Every `stax` subcommand and flag. Defaults are stated explicitly. For
task-oriented walkthroughs, see the [Guide](@/guide/_index.md).

```text
stax <COMMAND> [OPTIONS]
```

Every subcommand except `record` and `setup` connects to `stax-server` over
its local socket and fails loudly if the daemon is not running.

## Global options

`stax` is built on [figue](https://github.com/bearcove/figue), so the
standard builtins are available before any subcommand:

| flag                          | effect                                      |
|-------------------------------|---------------------------------------------|
| `-h, --help`                  | show help and exit `0`                      |
| `--html-help`                 | open HTML help in the browser and exit      |
| `-V, --version`               | show version and exit `0`                   |
| `--completions <bash,zsh,fish>` | print a shell completion script           |

## `stax record`

Start a recording. Forwards every event to `stax-server` for the web UI and
the query subcommands. See [Recording a Run](@/guide/recording.md).

```text
stax record [OPTIONS] [-- COMMAND…]
```

| flag / arg                | type            | default                | meaning                                                         |
|---------------------------|-----------------|------------------------|-----------------------------------------------------------------|
| `-F, --frequency <HZ>`    | `u32`           | `900`                  | sampling frequency, in hertz                                    |
| `-l, --time-limit <SECS>` | `u64`           | *(none — unlimited)*   | stop after this many seconds                                    |
| `-p, --pid <PID>`         | `u32`           | *(none)*               | attach to an existing process instead of launching one          |
| `--no-dwarf-unwind`       | `bool`          | `false`                | Linux x86-64: disable `.eh_frame` DWARF unwinding of user stacks |
| `--daemon-socket <PATH>`  | `String`        | `/var/run/staxd.sock`  | local socket of the privileged `staxd` daemon                   |
| `[-- COMMAND…]`           | positional      | *(none)*               | command to launch and profile; use `--` to protect its flags    |

You must supply **either** `--pid` **or** a launch command — not both, not
neither. `stax record --pid 1 -- ./foo` and a bare `stax record` are both
errors.

On x86-64 Linux, `.eh_frame` DWARF unwinding is **on by default** — the
system `libc` is built `-fomit-frame-pointer`, so the kernel's stack walk
truncates for any sample landing in it. `--no-dwarf-unwind` turns it off (so
does `STAX_DWARF_UNWIND=0`); it is a no-op on macOS and aarch64. See
[Stack Unwinding](@/concepts/stack-unwinding.md).

`--daemon-socket` defaults to `/var/run/staxd.sock`; on Linux that resolves
to `/run/staxd.sock`, where `sudo stax setup` installs the systemd-managed
`staxd`. On Linux, if no `staxd` socket exists, stax records in-process.

## `stax setup`

Codesign this `stax` binary or, **when run as root**, install `staxd` as a
LaunchDaemon. `sudo stax setup` is the privileged install step from
[Getting Started](@/guide/getting-started.md).

```text
stax setup [OPTIONS]
```

| flag         | type   | default | meaning                                          |
|--------------|--------|---------|--------------------------------------------------|
| `-y, --yes`  | `bool` | `false` | skip the confirmation prompt before `codesign`   |

## `stax status`

Print the current state of `stax-server`: the active run, if any, plus when
the daemon started. Takes no options. See
[Run Lifecycle](@/guide/run-lifecycle.md#stax-status).

## `stax list`

List every run `stax-server` has hosted — active and history, oldest first.
Takes no options. History is server-memory history and does not survive a
daemon restart unless you save the current queryable run with
[`stax save`](#stax-save). See
[Run Lifecycle](@/guide/run-lifecycle.md#stax-list).

## `stax wait`

Block until a condition fires, the active run stops, or the timeout elapses.
See [Run Lifecycle](@/guide/run-lifecycle.md#stax-wait).

```text
stax wait [OPTIONS]
```

| flag                   | type     | default | meaning                                                       |
|------------------------|----------|---------|---------------------------------------------------------------|
| `--for-samples <N>`    | `u64`    | *(none)*| return after at least N PET samples have landed               |
| `--for-seconds <N>`    | `u64`    | *(none)*| return after N seconds of wall-clock time                     |
| `--until-symbol <S>`   | `String` | *(none)*| return once a symbol containing substring S is seen (case-sensitive) |
| `--timeout-ms <MS>`    | `u64`    | *(none)*| hard deadline for the whole wait                              |

`--for-samples`, `--for-seconds`, and `--until-symbol` are **mutually
exclusive** — pass at most one. `--timeout-ms` is independent. With no flags,
`wait` blocks until the active run stops. Exit codes:
[Exit Codes](@/reference/exit-codes.md).

## `stax stop`

Ask `stax-server` to stop the active run cleanly and print the final summary.
Takes no options. Exits non-zero if there is no active run. See
[Run Lifecycle](@/guide/run-lifecycle.md#stax-stop).

## `stax save`

Save the current or most recent queryable run to a directory archive. The
archive contains `archive.json`, a versioned facet-json payload with the run
summary, raw aggregator streams, binary/symbol metadata, and target-ingest
diagnostics.

```text
stax save <PATH>
```

| arg      | type                | meaning                                      |
|----------|---------------------|----------------------------------------------|
| `<PATH>` | positional `String` | directory archive to create or overwrite into |

`stax save` works while a run is active, and after `stax stop`, until the
next recording resets the live aggregator.

## `stax open`

Open a saved run archive into `stax-server`'s current query state.

```text
stax open <PATH>
```

| arg      | type                | meaning                                                    |
|----------|---------------------|------------------------------------------------------------|
| `<PATH>` | positional `String` | archive directory, or the `archive.json` file inside it    |

After `stax open`, `threads`, `top`, `flame`, and `diagnose` operate on the
restored run. `open` refuses to replace state while a recording is active.

## `stax compare`

Compare two saved run archives without touching `stax-server` state.

```text
stax compare <BASELINE> <CANDIDATE>
```

| arg           | type                | meaning                                               |
|---------------|---------------------|-------------------------------------------------------|
| `<BASELINE>`  | positional `String` | baseline archive directory or `archive.json` file     |
| `<CANDIDATE>` | positional `String` | candidate archive directory or `archive.json` file    |

The comparison reads the typed archive payload directly and prints deltas for
PET samples, on/off-CPU interval time, target time, target span counts,
origin-link counts, ingest drops, and the top target lanes by duration.

## `stax top`

Snapshot the top-N functions or target-span names of the current run. See
[Inspecting a Run](@/guide/inspecting-a-run.md#stax-top).

```text
stax top [OPTIONS]
```

| flag                | type     | default  | meaning                                              |
|---------------------|----------|----------|------------------------------------------------------|
| `-n, --limit <N>`   | `u32`    | `20`     | maximum number of entries to return                  |
| `--sort <MODE>`     | `String` | `self`   | `self` (leaf-only) or `total` (any frame)            |
| `--tid <TID>`       | `u32`    | *(none)* | restrict to one thread; default is all threads       |

Output columns are active time, target-executor time, PET samples, target
span count, and function/span name. For a synthetic target lane,
`--tid <TID>` shows per-span durations in `target ms` and span counts in
`spans`. When target spans carry origins, filtering to the origin CPU tid also
includes those spans under the CPU dispatch stack; `--sort total` then charges
the target duration to dispatch callers. If Metal command/dispatch frames are
visible but no target lane is present, `stax top` prints a stderr hint about
`stax-target` Metal 4 timestamp-counter cooperation. If the view is empty but
the run has off-CPU/thread activity or target lanes outside a `--tid` filter,
`top` prints a discovery hint for `stax threads -n 0`, target-lane tids, or
`stax-target` integration.

## `stax flame`

Print the active flamegraph as an indented tree. See
[Inspecting a Run](@/guide/inspecting-a-run.md#stax-flame).

```text
stax flame [OPTIONS]
```

| flag                       | type    | default  | meaning                                                        |
|----------------------------|---------|----------|----------------------------------------------------------------|
| `-d, --max-depth <N>`      | `usize` | `12`     | stop printing below depth N; cut subtrees collapse to a summary |
| `--threshold-pct <PCT>`    | `f64`   | `1.0`    | hide subtrees below this percent of total active time; `0` for all |
| `--tid <TID>`              | `u32`   | *(none)* | restrict to one thread; default is all threads                  |

Cooperating target lanes render as `(all) -> lane -> span name`, with per-node
active time, target time, span count, and percent columns. When target spans
carry origins, `--tid <cpu tid>` can render
`(all) -> CPU caller -> lane -> span name`. Like `top`, `flame` prints a Metal
cooperation hint when Metal command/dispatch frames are visible but no
synthetic target lane has reported spans. Empty flame views also get the same
`threads` / target-lane / `stax-target` discovery hints as `top`.

## `stax threads`

Per-thread and synthetic-lane CPU/target/off-CPU breakdown for the current
run, sorted by total activity. CPU thread rows include origin-linked target
spans queued from that thread, and synthetic target lanes with spans are
included even if they fall past the normal `-n` cutoff. The output includes a
`kind` column: `thread` for real sampled threads and `target` for synthetic
target lanes. See
[Inspecting a Run](@/guide/inspecting-a-run.md#stax-threads).

```text
stax threads [OPTIONS]
```

| flag                | type  | default | meaning                                                  |
|---------------------|-------|---------|----------------------------------------------------------|
| `-n, --limit <N>`   | `u32` | `20`    | maximum threads to print; `0` prints every thread        |

## `stax annotate`

Disassemble and annotate one function from the current run. See
[Inspecting a Run](@/guide/inspecting-a-run.md#stax-annotate).

```text
stax annotate <TARGET> [OPTIONS]
```

| flag / arg          | type     | default  | meaning                                                          |
|---------------------|----------|----------|------------------------------------------------------------------|
| `<TARGET>`          | positional `String` | *(required)* | hex address (`0x10004ad60`) or a substring of a demangled name |
| `--tid <TID>`       | `u32`    | *(none)* | restrict to one thread; default is all threads                   |

A name substring is matched case-insensitively against the run's top-256
leaf-self functions; the hottest match wins.

## `stax diagnose`

Dump `stax-server` diagnostics, including target-span ingest counters
(batches, recorded/dropped spans, lane totals, origin link/unlink counts,
unlinked-origin reasons, PET origin-distance min/avg/max, and stax-target local
queue drops). It also prints target-ingest hints for missing batches, invalid
span durations, missing origins, origins that failed to link, batches that
arrived with no active run, batches from the wrong pid, and target-side
queue-full / worker-disconnected drops. Takes no options. See
[Troubleshooting](@/guide/troubleshooting.md#diagnostic-commands--stax-diagnose).

## `stax dump`

Ask every running stax process (`staxd`, `stax-server`, `stax`) to write a
SIGUSR1 telemetry/debug snapshot into unified logging. Takes no options. See
[Troubleshooting](@/guide/troubleshooting.md#diagnostic-commands--stax-dump).
