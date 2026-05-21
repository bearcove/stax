+++
title = "Getting Started"
weight = 0
insert_anchor_links = "heading"
+++

This page takes you from a fresh checkout to a working install with both
daemons running, in about five minutes.

## What you are installing

stax is not a single binary. It is three programs and two daemons —
[Architecture](@/concepts/architecture.md) covers the *why*; here is the
short version:

| program       | role                                      | privilege | runs as          |
|---------------|-------------------------------------------|-----------|------------------|
| `stax`        | the CLI you type commands into            | user      | (not a daemon)   |
| `stax-server` | run registry + live aggregator + RPC      | user      | LaunchAgent      |
| `staxd`       | the kperf/kdebug owner (macOS)            | root      | LaunchDaemon     |

`stax record` talks to `staxd` to start sampling; every sample flows into
`stax-server`, which is what `stax top`, `stax flame`, and the web UI query.

## Prerequisites

- **macOS** (Apple Silicon or Intel) or **Linux** — see
  [Platform Support](@/concepts/platform-support.md) for what each captures.
- A recent Rust toolchain. stax tracks a recent stable; the workspace sets
  `rust-version = "1.91"`.
- On macOS, a code-signing identity. `cargo xtask install` looks for a
  *Developer ID Application* or *Apple Development* identity from
  `security find-identity`.
- On macOS, the ability to run one `sudo` command to install the privileged
  daemon.

## Install — one command

From a fresh checkout:

```bash
cargo xtask install
```

That builds release binaries for `stax`, `staxd`, and `stax-server`, copies
them into `~/.cargo/bin/`, codesigns them on macOS, and **bootstraps
`stax-server` as a per-user LaunchAgent** so it starts on login and is
running right now.

> **Code-signing identity.** On macOS, override the auto-detected identity
> with `STAX_CODESIGN_IDENTITY=<identity-or-hash>`. Set it to `-` only when
> you explicitly want ad-hoc signing — see
> [Environment Variables](@/reference/environment-variables.md).

## Install the privileged daemon (macOS)

`stax-server` runs as your user, but the kperf/kdebug machinery needs root.
One privileged step installs `staxd` as a LaunchDaemon:

```bash
sudo stax setup
```

From this point on, `stax record …` is unprivileged — `staxd` already holds
the privilege, and `stax` just talks to it over a socket.

On Linux there is no equivalent step: stax uses `perf_event_open` directly.
You may need to relax `perf_event_paranoid` — see
[Platform Support](@/concepts/platform-support.md).

## Verify the install

```bash
stax status                              # talks to stax-server
test -S /var/run/staxd.sock              # staxd's socket exists (macOS)
launchctl list eu.bearcove.stax-server   # should show a pid and exit code 0
```

`stax status` is the real test: it connects to `stax-server` over its local
socket and prints the daemon's state. With no recording yet, you'll see
something like:

```text
no active run
daemon up since 2026-05-21 09:14:02
```

If it instead prints `error: stax-server isn't running`, the LaunchAgent
didn't load — see [Troubleshooting](@/guide/troubleshooting.md).

## Your first recording

Record a short-lived command end to end:

```bash
stax record -- ./target/release/mybench
```

`stax record` launches the program, tells `staxd` to sample it, and streams
every sample into `stax-server` until the program exits (or you press
Ctrl-C). Then, from the same or another shell:

```bash
stax top -n 10
```

```text
   42.184ms       3812 samples  mycrate::translate (mybench)
    9.001ms        812 samples  mycrate::lower (mybench)
    …
```

That's the whole loop: **record**, then **query**. The next pages go deep on
each half.

## Where to next

- [Recording a Run](@/guide/recording.md) — attaching to a PID, frequency, time limits
- [Inspecting a Run](@/guide/inspecting-a-run.md) — flamegraphs, per-thread breakdowns, disassembly
- [Run Lifecycle](@/guide/run-lifecycle.md) — `wait`, `stop`, and the one-active-run rule
- [Troubleshooting](@/guide/troubleshooting.md) — when an install step doesn't take
