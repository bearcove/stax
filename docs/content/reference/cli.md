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
| `-F, --frequency <HZ>`    | `u32`           | `900`                  | PET sampling frequency, in hertz                                |
| `-l, --time-limit <SECS>` | `u64`           | *(none — unlimited)*   | stop after this many seconds                                    |
| `-p, --pid <PID>`         | `u32`           | *(none)*               | attach to an existing process instead of launching one          |
| `--daemon-socket <PATH>`  | `String`        | `/var/run/staxd.sock`  | local socket of the privileged `staxd` daemon                   |
| `[-- COMMAND…]`           | positional      | *(none)*               | command to launch and profile; use `--` to protect its flags    |

You must supply **either** `--pid` **or** a launch command — not both, not
neither. `stax record --pid 1 -- ./foo` and a bare `stax record` are both
errors.

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
Takes no options. History is in-memory and does not survive a daemon
restart. See [Run Lifecycle](@/guide/run-lifecycle.md#stax-list).

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

## `stax top`

Snapshot the top-N functions of the current run. See
[Inspecting a Run](@/guide/inspecting-a-run.md#stax-top).

```text
stax top [OPTIONS]
```

| flag                | type     | default  | meaning                                              |
|---------------------|----------|----------|------------------------------------------------------|
| `-n, --limit <N>`   | `u32`    | `20`     | maximum number of entries to return                  |
| `--sort <MODE>`     | `String` | `self`   | `self` (leaf-only) or `total` (any frame)            |
| `--tid <TID>`       | `u32`    | *(none)* | restrict to one thread; default is all threads       |

## `stax flame`

Print the on-CPU flamegraph as an indented tree. See
[Inspecting a Run](@/guide/inspecting-a-run.md#stax-flame).

```text
stax flame [OPTIONS]
```

| flag                       | type    | default  | meaning                                                        |
|----------------------------|---------|----------|----------------------------------------------------------------|
| `-d, --max-depth <N>`      | `usize` | `12`     | stop printing below depth N; cut subtrees collapse to a summary |
| `--threshold-pct <PCT>`    | `f64`   | `1.0`    | hide subtrees below this percent of total on-CPU; `0` for all   |
| `--tid <TID>`              | `u32`   | *(none)* | restrict to one thread; default is all threads                  |

## `stax threads`

Per-thread on/off-CPU breakdown for the current run, sorted by on-CPU time
descending. See
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

Dump `stax-server` diagnostics: telemetry phases, counters, histograms, and
recent events. Takes no options. See
[Troubleshooting](@/guide/troubleshooting.md#diagnostic-commands--stax-diagnose).

## `stax dump`

Ask every running stax process (`staxd`, `stax-server`, `stax`) to write a
SIGUSR1 telemetry/debug snapshot into unified logging. Takes no options. See
[Troubleshooting](@/guide/troubleshooting.md#diagnostic-commands--stax-dump).
