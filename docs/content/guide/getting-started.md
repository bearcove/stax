+++
title = "Getting Started"
weight = 0
insert_anchor_links = "heading"
+++

This page takes you from a fresh checkout to a working install — three
binaries built, `stax-server` running, and (on macOS, or a locked-down Linux
host) the privileged `staxd` in place.

## What you are installing

stax is three programs — [Architecture](@/concepts/architecture.md) covers
the *why*; here is the short version:

| program       | role                                      | privilege | how it runs                |
|---------------|-------------------------------------------|-----------|----------------------------|
| `stax`        | the CLI you type commands into            | user      | (not a daemon)             |
| `stax-server` | run registry + live aggregator + RPC      | user      | long-running daemon        |
| `staxd`       | the privileged capture helper             | root      | LaunchDaemon / systemd unit|

`stax record` drives the capture; every event flows into `stax-server`, which
is what `stax top`, `stax flame`, and the web UI query.

## Prerequisites

- **macOS** (Apple Silicon or Intel) or **Linux** — see
  [Platform Support](@/concepts/platform-support.md) for what each captures.
- A recent Rust toolchain. stax tracks a recent stable; the workspace sets
  `rust-version = "1.91"`.
- **macOS only:** a code-signing identity. `cargo xtask install` looks for a
  *Developer ID Application* or *Apple Development* identity from
  `security find-identity`.

## Step 1 — build and install the binaries

From a fresh checkout, on either platform:

```bash
cargo xtask install
```

That builds release binaries for `stax`, `staxd`, and `stax-server`, and
copies them into `~/.cargo/bin/`.

On **macOS** it additionally codesigns the binaries and **bootstraps
`stax-server` as a per-user LaunchAgent** — so the server is running right
now and on every login.

On **Linux** it stops at copying the binaries; the next two steps are
manual.

> **Code-signing identity (macOS).** Override the auto-detected identity with
> `STAX_CODESIGN_IDENTITY=<identity-or-hash>`. Set it to `-` only when you
> explicitly want ad-hoc signing — see
> [Environment Variables](@/reference/environment-variables.md).

## Step 2 — start stax-server

**macOS:** already done — `cargo xtask install` loaded the LaunchAgent.

**Linux:** `stax-server` is unprivileged; start it yourself. For a quick
session, background it:

```bash
stax-server &
```

To have it always available, wrap it in a systemd *user* unit, or start it
from your shell profile. It needs no privilege and no arguments.

## Step 3 — install staxd (the privileged helper)

```bash
sudo stax setup
```

On **macOS** this installs `staxd` as a **LaunchDaemon**. It is **required** —
`staxd` is the only path to `kperf`. From here on, `stax record` is
unprivileged.

On **Linux** this installs `staxd` as a **systemd service**
(`eu.bearcove.staxd.service`). It is **optional**: when
`perf_event_paranoid` is permissive, `stax-server` opens `perf_event_open`
in-process and no daemon is needed. Install it anyway if you want stax to
work on a locked-down host, or to get **hardware counters** and **wakeup
attribution** — see
[Platform Support](@/concepts/platform-support.md#linux--two-recording-modes).

## Verify the install

The real test is `stax status` — it connects to `stax-server` and prints its
state:

```bash
stax status
```

```text
no active run
```

If it prints `error: stax-server isn't running`, the server isn't up — see
[Troubleshooting](@/guide/troubleshooting.md).

Platform-specific checks:

```bash
# macOS
launchctl list eu.bearcove.stax-server   # should show a pid and exit code 0
test -S /var/run/staxd.sock              # staxd's socket exists

# Linux (if you installed staxd)
systemctl status eu.bearcove.staxd       # should be active (running)
test -S /run/staxd.sock                  # staxd's socket exists
```

## Your first recording

Record a short-lived command end to end:

```bash
stax record -- ./target/release/mybench
```

`stax record` launches the program, drives the capture, and streams every
event into `stax-server` until the program exits (or you press Ctrl-C). Then,
from the same or another shell:

```bash
stax top -n 10
```

```text
   42.184ms       3812 samples  mycrate::translate (mybench)
    9.001ms        812 samples  mycrate::lower (mybench)
    …
```

That's the whole loop: **record**, then **query**.

## Where to next

- [Recording a Run](@/guide/recording.md) — attaching to a PID, frequency, DWARF unwinding
- [Inspecting a Run](@/guide/inspecting-a-run.md) — flamegraphs, per-thread breakdowns, disassembly
- [Run Lifecycle](@/guide/run-lifecycle.md) — `wait`, `stop`, and the one-active-run rule
- [Troubleshooting](@/guide/troubleshooting.md) — when an install step doesn't take
