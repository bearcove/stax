+++
title = "Recording a Run"
weight = 1
insert_anchor_links = "heading"
+++

`stax record` is the only command that *produces* data. Everything else
queries it. This page covers the two ways to point stax at a target and the
flags that shape the recording.

## Two ways to choose a target

A recording targets exactly one process. You either **launch** it or
**attach** to one that already exists — never both, never neither.

### Launch a command

Put the command after `--`. The `--` is what keeps the target's own flags
from being parsed by stax:

```bash
stax record -- ./target/release/mybench --iterations 5000
```

stax spawns the program, samples it for its whole lifetime, and the
recording stops when the program exits. Without `--`, a flag like
`--iterations` would be interpreted as a flag *to stax* and rejected.

### Attach to a running process

```bash
stax record --pid 12345
```

stax attaches to PID 12345 and samples until you stop it (Ctrl-C, or
`stax stop` from another shell, or `--time-limit`). The process keeps
running afterwards — attaching is non-destructive.

> **Pick one.** `stax record --pid 123 -- ./foo` is an error
> (`specify either --pid or a command to launch, not both`), and so is
> `stax record` with neither.

## Foreground vs background

By default `stax record` runs in the foreground and blocks until the
recording ends. Two common patterns:

```bash
# Foreground: record a benchmark end to end, then query.
stax record -- ./bench
stax top

# Background: start recording, keep querying while it runs.
stax record -- ./long-running-server &
stax wait --for-samples 20000
stax flame
```

The run lives in `stax-server`, not in the `stax record` process — so the
recording keeps going while you query it from other shells. Use
[`stax stop`](@/guide/run-lifecycle.md) to end it cleanly.

## Flags

### `-F, --frequency <HZ>`

Sampling frequency, in hertz. **Default: 900.**

```bash
stax record -F 1999 -- ./bench
```

Higher frequency means finer-grained data and more overhead; lower means the
opposite. 900–1999 Hz is a sensible range for most work. See
[Sampling](@/concepts/sampling.md) for what the number actually controls.

### `-l, --time-limit <SECONDS>`

Stop the recording after N seconds, even if the target is still running.
Unlimited by default — without it, a launched target records until it exits
and an attached target records until you stop it.

```bash
# attach to a server, record exactly 30 seconds, then stop
stax record --pid $(pgrep -n myserver) --time-limit 30
```

### `-p, --pid <PID>`

Attach to an existing process instead of launching one. Mutually exclusive
with a launch command.

### `--no-dwarf-unwind`

**Linux only.** Turn off `.eh_frame` DWARF unwinding of user stacks and fall
back to the kernel's frame-pointer walk.

On x86-64 Linux, DWARF unwinding is **on by default**: the system `libc` is
built `-fomit-frame-pointer`, so the kernel's stack walk truncates for any
sample landing in `libc` — which covers most malloc/IO/syscall-heavy
workloads, whatever your own binary was built with. `--no-dwarf-unwind` (or
`STAX_DWARF_UNWIND=0`) opts out, saving a per-sample register + 8 KiB stack
copy.

x86-64 only (aarch64 keeps a frame pointer by ABI); no-op on macOS. See
[Stack Unwinding](@/concepts/stack-unwinding.md).

### `--daemon-socket <PATH>`

The local socket of the privileged `staxd` daemon. **Default:
`/var/run/staxd.sock`** (on Linux this resolves to `/run/staxd.sock`, the
path `sudo stax setup` installs). Override it only if you run `staxd`
somewhere non-standard. On Linux, if no `staxd` socket is present, stax
records [in-process](@/concepts/platform-support.md#linux--two-recording-modes)
instead.

## How a recording runs

`stax record` does not do the sampling itself. It launches the target (or
resolves the `--pid`), hands it to `stax-server`, and `stax-server` drives
the capture on an in-process per-run task — folding every sample, off-CPU
interval, wakeup, and image load straight into the live aggregator the query
commands read. See [Architecture](@/concepts/architecture.md).

So `stax record` needs `stax-server` to be running; without it the command
fails immediately. On macOS it also needs `staxd`; on Linux `staxd` is
needed only on a locked-down host — see
[Platform Support](@/concepts/platform-support.md#linux--two-recording-modes). If a
prerequisite is missing, fix it first — see
[Troubleshooting](@/guide/troubleshooting.md).

## After recording

The moment a run has samples, it is queryable — you do not have to wait for
it to finish:

- [`stax top`](@/guide/inspecting-a-run.md#stax-top) — hottest functions
- [`stax flame`](@/guide/inspecting-a-run.md#stax-flame) — the flamegraph
- [`stax threads`](@/guide/inspecting-a-run.md#stax-threads) — per-thread breakdown
- [`stax annotate`](@/guide/inspecting-a-run.md#stax-annotate) — annotated disassembly

Only one run can be active at a time. If `stax record` reports *another run
is already active*, see [Run Lifecycle](@/guide/run-lifecycle.md).
