+++
title = "Troubleshooting"
weight = 6
insert_anchor_links = "heading"
+++

When stax misbehaves, it is almost always one of a handful of things: a
daemon that isn't running, a run that's in the way, or — on Linux — a host
that won't let an unprivileged process sample. This page covers the
diagnostic tools and the common errors.

## Diagnostic commands

### stax diagnose

Dumps `stax-server`'s internals: telemetry phases, counters, histograms, and
recent events.

```bash
stax diagnose
```

Reach for this when numbers look wrong — samples not landing, intervals not
being counted — and you want to see the pipeline's own accounting rather than
guessing.

### stax dump

Asks every running stax process (`staxd`, `stax-server`, `stax`) to write a
SIGUSR1 telemetry/debug snapshot into its log output.

```bash
stax dump
```

This is the heavier sibling of `stax diagnose`: it captures deep per-process
state for after-the-fact analysis. Read it back with the log commands below.

## Reading the logs

### macOS

Both daemons log through macOS unified logging (`os_log`) — nothing on disk:

```bash
# stax-server — your user, no sudo
log stream --predicate 'subsystem == "eu.bearcove.stax-server"'

# staxd — root LaunchDaemon, needs sudo
sudo log stream --predicate 'subsystem == "eu.bearcove.staxd"'

# the CLI itself
log stream --predicate 'subsystem == "eu.bearcove.stax"'
```

Swap `stream` for `show --last 10m` to query past events. Or use
**Console.app** with *Include Info/Debug Messages* enabled.

### Linux

`staxd` runs under systemd, so its logs are in the journal:

```bash
journalctl -u eu.bearcove.staxd -f
```

`stax-server` is a process you started yourself — its logs go wherever you
pointed its stdout/stderr. The CLI logs to stderr. Raise the level on any of
them with `RUST_LOG` (see
[Environment Variables](@/reference/environment-variables.md)).

## Common errors

### `error: stax-server isn't running`

Every subcommand except `setup` needs `stax-server`. Start it:

- **macOS** — the LaunchAgent didn't load. Reload it:

  ```bash
  launchctl bootstrap "gui/$(id -u)" \
    ~/Library/LaunchAgents/eu.bearcove.stax-server.plist
  ```

  Confirm with `launchctl list eu.bearcove.stax-server` — you want a pid and
  a `0` exit status.

- **Linux** — just run it: `stax-server &`. See
  [Getting Started](@/guide/getting-started.md).

### `another run is already active`

stax allows [one active run at a time](@/guide/run-lifecycle.md). End the
current one first:

```bash
stax wait     # block until it finishes on its own
stax stop     # or stop it now
```

### `stax top` returns `(no samples yet — is a recording in progress?)`

Either no run is active, or the run hasn't ingested any samples yet — very
early in a run's life. Confirm a run exists with `stax status`, or block
until data is in:

```bash
stax wait --for-samples 100
```

### Linux: shallow stacks, no kernel frames, or no hardware counters

The in-process Linux recorder is limited by `perf_event_paranoid`. If stacks
come back shallow, kernel frames are missing, or PMU columns are empty, the
host is restricting unprivileged `perf_event_open`. Two fixes:

```bash
# loosen the host policy for the session…
sudo sysctl kernel.perf_event_paranoid=1

# …or, better, install the staxd broker so the policy stops mattering:
sudo stax setup
```

The `staxd` broker is also what unlocks **wakeup attribution** and
**hardware counters** on Linux — see
[Platform Support](@/concepts/platform-support.md#linux--perf-event-paranoid).

### Linux: frame-pointer-less binary, truncated stacks

If a flamegraph is suspiciously flat on x86-64 Linux, check whether DWARF
unwinding got turned off — a stray `--no-dwarf-unwind` or
`STAX_DWARF_UNWIND=0`. It is on by default precisely because
`-fomit-frame-pointer` builds (most C/C++ `-O2`, many Rust release builds)
would otherwise truncate; drop the override and re-record:

```bash
stax record -- ./mybench   # DWARF unwinding is the default
```

See [Stack Unwinding](@/concepts/stack-unwinding.md).

### macOS asks whether `stax-server` can access another app's data

`stax-server` is touching a path under `~/Library/Group Containers`, which
triggers a `kTCCServiceSystemPolicyAppData` prompt even for a correctly
signed binary. By default the server's socket lives at
`$XDG_RUNTIME_DIR/stax-server.sock` or `/tmp/stax-server-$UID.sock`,
*outside* app-container paths, precisely to avoid this. If you've overridden
[`STAX_SERVER_SOCKET`](@/reference/environment-variables.md), point it back
outside `~/Library`.

## Limitations

- **macOS: hardened-runtime targets are out of scope.** An app with the
  hardened runtime and no `get-task-allow` entitlement cannot be attached to.
- **Run history is in-memory.** `stax list` shows history, but it does not
  survive a `stax-server` restart.
- **No per-run query selector yet.** Queries hit whichever run is active or
  most recent — see [Run Lifecycle](@/guide/run-lifecycle.md).

## Still stuck?

Capture a `stax diagnose` dump and the relevant log output, and open an issue
at [github.com/bearcove/stax](https://github.com/bearcove/stax/issues).
