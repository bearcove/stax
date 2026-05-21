+++
title = "Architecture"
weight = 0
insert_anchor_links = "heading"
+++

stax is three programs, not one. That split exists for a single reason:
sampling needs privilege, but you should not have to run a profiler — or your
build tooling, or an AI agent driving it — as root. stax isolates the
privilege into one small daemon and keeps everything else unprivileged.

## The three processes

| component     | privilege | role                                              |
|---------------|-----------|---------------------------------------------------|
| `staxd`       | root      | the privileged capture helper                     |
| `stax-server` | user      | run registry + live aggregator + RPC services     |
| `stax`        | user      | the CLI                                           |

`staxd` does platform-specific, privileged things; `stax-server` is the
unprivileged brain that every client queries; `stax` is the command you type.
What `staxd` *is* differs sharply between the two platforms.

## staxd — the privileged helper

### On macOS — a kperf streamer

The macOS `staxd` **owns** the private `kperf` / `kdebug` / `kpc`
frameworks. It arms the PET sampler, subscribes to scheduler events, and
streams the raw kernel trace records out over a local socket. Because `kperf`
is a single-owner, machine-wide resource, it allows one recording session at
a time.

It is installed once, by `sudo stax setup`, as a **LaunchDaemon** running as
root. On macOS `staxd` is **required** — it is the only path to kperf.

### On Linux — a perf fd broker

The Linux `staxd` is a different shape entirely: a **stateless file-descriptor
broker**. It does the one privileged thing — `perf_event_open` per CPU — and
hands the resulting descriptors back to the unprivileged caller over a Unix
socket (via `SCM_RIGHTS`). The instant it has replied, **it is out of the
data path**: the unprivileged side owns the kernel ring buffers and drains
them directly.

It brokers four kinds of descriptor: the per-CPU sampling rings, the
context-switch rings (off-CPU), the `sched:sched_waking` tracepoint rings
(wakeup attribution — the tracepoint lives in root-only tracefs), and the
hardware-counter group (PMU).

On Linux `staxd` is **optional**. When `perf_event_paranoid` is permissive,
`stax-server` opens `perf_event_open` itself, in-process, and no daemon is
needed. The broker is what makes stax work on a **locked-down host**, and
what unlocks **hardware counters** and **wakeup attribution** there. It is
installed by `sudo stax setup` as a **systemd service**. See
[Platform Support](@/concepts/platform-support.md#linux--two-recording-modes).

## stax-server — the unprivileged brain

`stax-server` runs as your user on both platforms. It is where everything
interesting lives:

- **the run registry** — one active run plus the history `stax list` shows;
- **the live aggregator** — folds the incoming event stream into the
  flamegraph, top-N tables, per-thread breakdowns, timeline, and more;
- **the binary registry** — loaded images, symbol tables, and code bytes
  that turn raw addresses into names and disassembly;
- **two [vox RPC services](@/reference/rpc-services.md)** — `RunControl` for
  lifecycle and `Profiler` for queries.

Recording happens **in-process**: `stax-server` drives a per-run task that
reads from the capture backend (`staxd` on macOS; `staxd` *or* an in-process
`perf_event_open` on Linux) and feeds the aggregator directly. There is no
separate recording-driver process and no on-disk archive in the loop.

## stax — the CLI

`stax` is the command you type. It owns no socket and holds no state. Every
subcommand — `record`, `top`, `flame`, `wait`, … — opens a connection to
`stax-server`, makes one or more RPC calls, prints the result, and exits. For
`stax record -- <command>`, the CLI launches the target itself (suspended)
and hands the PID to `stax-server`. If `stax-server` isn't running, the CLI
fails loudly rather than silently doing nothing.

## How a sample travels

**macOS** — `staxd` stays in the data path, streaming records:

```text
   target ──sampled by──► staxd (root) ──raw kdebug records──► stax-server
                                                                aggregator
```

**Linux** — `staxd` brokers descriptors, then drops out:

```text
   staxd (root) ──perf fds (SCM_RIGHTS), once──► stax-server
   target ──────────sampled into──► kernel ring buffers ──drained by──► stax-server
                                                                         aggregator
```

(With a permissive `perf_event_paranoid`, the Linux path has no `staxd` at
all — `stax-server` opens the perf fds itself.)

Either way, the samples end up in `stax-server`'s aggregator, and from there
both clients query the same data:

```text
                       stax-server
                  RunControl + Profiler
                ┌───────────┴───────────┐
        local:// socket           ws://127.0.0.1:8080
         ┌──────────┐               ┌──────────┐
         │   stax   │               │ browser  │
         │   CLI    │               │   UI     │
         └──────────┘               └──────────┘
```

A run started from the CLI shows up in the browser, and vice versa — they are
both just clients of the same daemon.

## The two sockets — and a TCC footnote

`stax-server` listens on two transports at once:

- a **Unix domain socket** for trusted local clients (the CLI, local agents);
- a **WebSocket** on `ws://127.0.0.1:8080` for browsers.

The Unix socket deliberately lives at `$XDG_RUNTIME_DIR/stax-server.sock` or
`/tmp/stax-server-$UID.sock` — *outside* `~/Library/Group Containers`. On
macOS, a process that touches an app-data path triggers
`kTCCServiceSystemPolicyAppData` prompts even when it is signed by the right
team; keeping the socket out of app-container paths avoids that prompt. Both
paths are overridable — see
[Environment Variables](@/reference/environment-variables.md).

## Why the split is worth it

A single-process profiler that needs kernel access has to run *entirely* as
root — including, in stax's agent-driven workflows, your editor or CI
tooling. The three-process design means:

- the root surface is one small daemon you install once and audit once
  (and on Linux, often do not need at all);
- `stax record` and every query run as you;
- the daemon outlives any individual command, so backgrounding or killing a
  `stax record` invocation never loses the run.

## See also

- [Platform Support](@/concepts/platform-support.md) — what each backend captures.
- [Programmatic Usage](@/reference/rpc-services.md) — the RPC services.
- [Run Lifecycle](@/guide/run-lifecycle.md) — how the run registry behaves.
