+++
title = "Architecture"
weight = 0
insert_anchor_links = "heading"
+++

stax is three programs, not one. That split exists for a single reason:
sampling the kernel needs root, but you should not have to run a profiler —
or your build tooling — as root. stax isolates the privilege into one small,
long-lived daemon and keeps everything else unprivileged.

## The three processes

| component     | privilege | launchd kind  | socket                                                 |
|---------------|-----------|---------------|--------------------------------------------------------|
| `staxd`       | root      | LaunchDaemon  | `/var/run/staxd.sock`                                  |
| `stax-server` | user      | LaunchAgent   | `$XDG_RUNTIME_DIR/stax-server.sock` or `/tmp/stax-server-$UID.sock` |
| `stax`        | user      | *(CLI)*       | *(none)*                                               |

### staxd — the privileged kernel owner

`staxd` is the only part that runs as root. On macOS it owns the private
`kperf` / `kdebug` frameworks: it arms the PET sampler, attaches counters,
and streams raw kernel records out over its local socket. It is installed
once, by `sudo stax setup`, as a LaunchDaemon. Keeping it tiny and
long-lived means the privileged surface is small and auditable.

After `staxd` is installed, `stax record` itself is unprivileged — it just
asks `staxd`, over the socket, to start sampling.

### stax-server — the unprivileged brain

`stax-server` runs as your user, as a LaunchAgent, started on login. It is
where everything interesting lives:

- **the run registry** — one active run plus the history `stax list` shows;
- **the live aggregator** — folds the incoming sample stream into the
  flamegraph, top-N tables, and per-thread breakdowns;
- **the binary registry** — the loaded images, symbol tables, and code bytes
  that turn raw addresses into names and disassembly;
- **three [vox RPC services](@/reference/rpc-services.md)** — the query and
  control surface.

Recording happens *in-process*: `stax-server` drives a per-run task that
reads from `staxd` (macOS) or `perf_event_open` (Linux) and feeds the
aggregator directly. There is no separate recording-driver process.

### stax — the CLI

`stax` is the command you type. It owns no socket and holds no state. Every
subcommand — `record`, `top`, `flame`, `wait`, … — opens a connection to
`stax-server` (and, for `record`, to `staxd`), makes one or more RPC calls,
prints the result, and exits. If `stax-server` isn't running, the CLI fails
loudly rather than silently doing nothing.

## How a sample travels

```text
   target process
        │  (sampled by)
        ▼
   ┌─────────┐   raw kernel records    ┌──────────────┐
   │  staxd  │ ──────────────────────► │  stax-server │
   │ (root)  │   over /var/run/...     │  aggregator  │
   └─────────┘                         └──────┬───────┘
                                              │ vox RPC
                          ┌───────────────────┼───────────────────┐
                          ▼                                       ▼
                  local:// socket                          ws://127.0.0.1:8080
                   ┌──────────┐                              ┌──────────┐
                   │   stax   │                              │ browser  │
                   │   CLI    │                              │   UI     │
                   └──────────┘                              └──────────┘
```

1. `stax record` asks `staxd` to sample the target.
2. `staxd` streams raw records to `stax-server`, which folds them into the
   live aggregator.
3. `stax` (over the Unix socket) and the [web UI](@/guide/web-ui.md) (over
   the WebSocket) both query that same aggregator.

The two clients are interchangeable: a run started from the CLI shows up in
the browser, and vice versa.

## The two sockets — and a TCC footnote

`stax-server` listens on two transports at once:

- a **Unix domain socket** for trusted local clients (the CLI, local agents);
- a **WebSocket** on `ws://127.0.0.1:8080` for browsers.

The Unix socket deliberately lives at `$XDG_RUNTIME_DIR/stax-server.sock` or
`/tmp/stax-server-$UID.sock` — *outside* `~/Library/Group Containers`. A bare
LaunchAgent or CLI that touches an app-data path triggers macOS
`kTCCServiceSystemPolicyAppData` prompts even when it is signed by the right
team. Keeping the socket out of app-container paths avoids that prompt
entirely. Both paths are overridable; see
[Environment Variables](@/reference/environment-variables.md).

## Why the split is worth it

A single-process profiler that needs kernel access has to run *entirely* as
root — including, in stax's agent-driven workflows, your editor or CI
tooling. The three-process design means:

- the root surface is one small daemon you install once and audit once;
- `stax record` and every query run as you;
- the daemon outlives any individual command, so backgrounding or killing a
  `stax record` invocation never loses the run.

## See also

- [Platform Support](@/concepts/platform-support.md) — what `staxd` and the
  Linux backend each capture.
- [Programmatic Usage](@/reference/rpc-services.md) — the RPC services
  `stax-server` exposes.
- [Run Lifecycle](@/guide/run-lifecycle.md) — how the run registry behaves.
