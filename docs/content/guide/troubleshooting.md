+++
title = "Troubleshooting"
weight = 6
insert_anchor_links = "heading"
+++

When stax misbehaves, it is almost always one of a handful of things: a
daemon that isn't running, a run that's in the way, or a target stax can't
attach to. This page covers the diagnostic tools and the common errors.

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
SIGUSR1 telemetry/debug snapshot into the system log.

```bash
stax dump
```

The snapshots land in unified logging (macOS) — read them with the `log`
commands below. This is the heavier sibling of `stax diagnose`: it captures
deep per-process state for after-the-fact analysis.

## Reading the logs

Both daemons log through macOS unified logging (`os_log`). Nothing is written
to a file on disk.

```bash
# stax-server — your user, no sudo
log stream --predicate 'subsystem == "eu.bearcove.stax-server"'

# staxd — root LaunchDaemon, needs sudo
sudo log stream --predicate 'subsystem == "eu.bearcove.staxd"'

# the CLI itself
log stream --predicate 'subsystem == "eu.bearcove.stax"'
```

Swap `stream` for `show --last 10m` to query past events instead of
following live ones. Or open **Console.app**, enable *Include Info Messages*
and *Include Debug Messages* from the Action menu, and filter by subsystem.

## Common errors

### `error: stax-server isn't running`

The LaunchAgent isn't loaded. `cargo xtask install` loads it; to do it by
hand:

```bash
launchctl bootstrap "gui/$(id -u)" \
  ~/Library/LaunchAgents/eu.bearcove.stax-server.plist
```

Confirm with `launchctl list eu.bearcove.stax-server` — you want a pid and a
`0` exit status.

### `another run is already active`

stax allows [one active run at a time](@/guide/run-lifecycle.md). End the
current one first:

```bash
stax wait     # block until it finishes on its own
stax stop     # or stop it now
```

### `stax record` says `stax-server unreachable`

The recording daemon is down. Sampling still starts, but the events have
nowhere to go and no query will work. Fix `stax-server` first (see the
LaunchAgent error above), then re-record.

### `stax top` returns `(no samples yet — is a recording in progress?)`

Either no run is active, or the run hasn't ingested any PET samples yet —
very early in a run's life. Confirm a run exists with `stax status`, or
block until data is in:

```bash
stax wait --for-samples 100
```

### macOS asks whether `stax-server` can access another app's data

`stax-server` is touching a path under `~/Library/Group Containers`, which
triggers a `kTCCServiceSystemPolicyAppData` prompt even for a correctly
signed binary. By default the server's socket lives at
`$XDG_RUNTIME_DIR/stax-server.sock` or `/tmp/stax-server-$UID.sock`,
*outside* app-container paths, precisely to avoid this. If you've overridden
[`STAX_SERVER_SOCKET`](@/reference/environment-variables.md), point it back
outside `~/Library`.

## Limitations

- **Hardened-runtime targets are out of scope.** The attachment helper is
  same-uid and aimed at ordinary local developer processes. A target with
  the hardened runtime and no `get-task-allow` entitlement cannot be
  attached to.
- **Run history is in-memory.** `stax list` shows history, but it does not
  survive a `stax-server` restart. Persistence is a follow-up.
- **No per-run query selector yet.** Queries hit whichever run is active or
  most recent — see [Run Lifecycle](@/guide/run-lifecycle.md).

## Still stuck?

Capture a `stax diagnose` dump and the relevant `log show` output, and open
an issue at [github.com/bearcove/stax](https://github.com/bearcove/stax/issues).
