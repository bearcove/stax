# Using stax from an agent

stax is built around a long-running unprivileged daemon, **stax-server**, that
hosts the run registry, the live aggregator, and two vox services
(`RunControl` + `Profiler`). Both human users on the CLI and AI agents talk
to the same surface.

This document is the agent-facing manual: install, lifecycle, query, wait.
stax runs on **macOS** and **Linux**; platform differences are called out
where they matter.

## stax is NOT just a CPU sampler — read this before concluding anything

A recording captures the **whole timeline** of the target, not just on-CPU
stacks. If the workload you care about is GPU-bound or wait-bound, stax is
still the right tool — that is the whole point. One recording holds:

- **On-CPU PET samples** — what `top`, `flame`, `annotate` aggregate.
- **Off-CPU intervals + wakeup attribution** — every blocked stretch with
  why-blocked classification. `stax threads` prints the per-thread
  on/off-CPU breakdown on the CLI; the web UI timeline shows the intervals.
- **GPU (and other accelerator) lanes** via **`stax-target`** — a profiled
  app links the `stax-target` crate and reports named spans (kernel
  dispatches, command-buffer stages) with `mach_absolute_time`-derived
  timestamps; they ingest as a **synthetic thread** per `(pid, lane)` on the
  same timebase as everything else. No correlation step, no chrome-trace
  export, no second tool. See `stax-target/src/lib.rs` and
  `stax-server/src/target_ingest.rs`; the guide page is
  `docs/content/guide/profiling-gpu-work.md`.

A cooperating process pays nothing when not recorded: the target polls a
capture gate (~1s) and only captures spans while a recording of its pid is
active (`stax_target::reporting_active()`).

Known consumer: **bee's `hx`** reports Metal 4 per-dispatch GPU timestamps
as the `"GPU tq1s"` lane (and TTS lanes) — see
`bee/rust/helix-metal4/src/stax.rs`, whose module doc says it plainly:
"stax is THE profiler for this work".

Gotchas that have actually misled agents (verified 2026-06-12):

- Synthetic GPU lanes have **zero on-CPU time**, so the default
  `stax threads` cutoff (top ~20 by on-CPU) HIDES them. Use
  `stax threads -n 2000 | grep -i gpu` — the lane row shows its span count.
  Synthetic tids live at/above `0xFFF0_0000`.
- `stax top --tid <synthetic>` / `stax flame --tid <synthetic>` currently
  return no data even when the lane holds thousands of ingested spans —
  the CLI tree views aggregate PET samples only (likely a gap/bug; the
  synthetic-symbol machinery exists precisely so span names render as
  frames). Use the **web UI timeline** for per-kernel inspection until that
  is fixed.
- A GPU-bound target looks nearly EMPTY in `stax top` — a handful of
  samples, allocator noise at the top. That is not "stax doesn't work";
  that is the answer (the CPU is idle). The story is in `threads`, the
  off-CPU intervals, and the GPU lane.

## Install

One command from a fresh checkout builds and stages the binaries:

```
cargo xtask install
```

…builds release binaries for `stax`, `staxd`, and `stax-server` and copies
them to `~/.cargo/bin/`.

- **macOS** — also codesigns the binaries and **bootstraps `stax-server` as a
  per-user LaunchAgent**, so it is running now and on every login.
- **Linux** — stops at copying the binaries; you start `stax-server`
  yourself (next step).

On macOS, `cargo xtask install` prefers `Developer ID Application` and then
`Apple Development` identities from `security find-identity`. Override with
`STAX_CODESIGN_IDENTITY=<identity-or-hash>`; set it to `-` only when you
explicitly want ad-hoc signing.

### Start stax-server (Linux)

On macOS the LaunchAgent runs it for you. On Linux, start it yourself — it is
unprivileged and takes no arguments:

```
stax-server &
```

### Install staxd (the privileged helper)

```
sudo stax setup
```

- **macOS** — installs `staxd` as a root **LaunchDaemon**. **Required**:
  `staxd` is the only path to `kperf`.
- **Linux** — installs `staxd` as a **systemd service** (`eu.bearcove.staxd`).
  **Optional**: when `perf_event_paranoid` is permissive, `stax-server`
  records in-process with no daemon. Install it to profile on a locked-down
  host, and to unlock PMU counters + wakeup attribution.

From this point on, `stax record …` is unprivileged.

### Privileged agent commands

On Amos's development machine (macOS), agents have a passwordless privileged
wrapper:

```
sudo -n /usr/local/sbin/stax-agent setup --yes
sudo -n /usr/local/sbin/stax-agent dump
```

Use `stax-agent` instead of interactive `sudo stax` for privileged stax
operations. The `-n` is intentional: if sudo would ask for a password, fail
fast and ask the user rather than blocking in an interactive prompt.

`log` is also configured for passwordless sudo on this machine. Use:

```
sudo -n log show --last 5m --predicate 'subsystem == "eu.bearcove.staxd"'
sudo -n log stream --predicate 'subsystem == "eu.bearcove.staxd"'
```

### What runs where

| component     | privilege | macOS              | Linux                       | socket |
|---------------|-----------|--------------------|-----------------------------|--------|
| `staxd`       | root      | LaunchDaemon       | systemd service *(optional)* | `/var/run/staxd.sock` (macOS) · `/run/staxd.sock` (Linux) |
| `stax-server` | user      | LaunchAgent        | run it yourself             | `$XDG_RUNTIME_DIR/stax-server.sock` or `/tmp/stax-server-$UID.sock` |
| `stax`        | user      | (CLI)              | (CLI)                       | (no socket) |

`stax-server` also binds **`ws://127.0.0.1:8080`** for the web UI. Override
with `STAX_SERVER_WS_BIND=host:port`.

What `staxd` *does* differs by platform:

- **macOS** — owns `kperf` / `kdebug` / `kpc`, and **streams** raw trace
  records to `stax-server` for the whole recording.
- **Linux** — a stateless `perf_event_open` **fd broker**: it does the one
  privileged `perf_event_open` per CPU, hands the descriptors back over the
  socket, and then drops out of the data path entirely.

On macOS the default `stax-server` socket intentionally lives outside
`~/Library/Group Containers`. A bare LaunchAgent/CLI touching app-data paths
triggers `kTCCServiceSystemPolicyAppData` prompts even when it is signed by
the right team.

### Logs

- **macOS** — both daemons log via unified logging (`os_log`); nothing on
  disk:

  ```
  log stream --predicate 'subsystem == "eu.bearcove.stax-server"'
  sudo -n log stream --predicate 'subsystem == "eu.bearcove.staxd"'
  ```

  Past events: `log show --last 10m --predicate '…'`. Or **Console.app** →
  *Include Info / Debug Messages*, filter by subsystem.

- **Linux** — `staxd` runs under systemd; read its log from the journal:

  ```
  journalctl -u eu.bearcove.staxd -f
  ```

  `stax-server` logs to wherever you pointed its stdout/stderr. Raise the
  level on any binary with `RUST_LOG`.

### Verifying the install

```
stax status                              # talks to stax-server
```

macOS:

```
test -S /var/run/staxd.sock              # staxd socket exists
launchctl list eu.bearcove.stax-server   # should show pid + 0
```

Linux (only if you installed `staxd`):

```
test -S /run/staxd.sock                  # staxd socket exists
systemctl status eu.bearcove.staxd       # should be active (running)
```

## Concurrency model

**One active run at a time.** `stax-server` rejects a second recording while
one is in flight. If you hit this, your options are:

```
stax wait     # block until the active run stops
stax stop     # ask stax-server to stop it now
```

`stax list` shows every run the daemon has hosted (active + history,
in-memory only for now — persistence is a follow-up).

### Which run does `stax top` / `stax annotate` query?

There's no run selector yet. They operate on the **current** aggregator
state, which is whichever run is active *or* the most recent one — the
aggregator stays populated until the next recording resets it. So the
working flow is:

```
stax record …           # start a recording (in another shell or backgrounded)
stax wait --for-samples 5000
stax top                # snapshot of the active run
stax stop               # stops the run; aggregator stays queryable
stax top                # still works — same data as above
stax record …           # NEW run resets the aggregator; the previous one is gone
```

If you need to query an older run later, you'll have to stop the active
one first (so its data sticks around) and avoid starting a new recording
until you're done. Per-`RunId` querying is on the roadmap.

## Lifecycle from an agent's POV

Typical agent flow:

```
stax record -- ./bench         # 1. start a recording (blocks until done,
                               #    or use `&` to background it)

stax wait --for-samples 10000  # 2. block until enough samples land
                               #    (or --for-seconds N, --until-symbol foo)

stax top -n 20 --sort self     # 3. inspect the hot leaf functions

stax annotate 0x10004ad60      # 4. get per-instruction sample counts +
                               #    interleaved source for one function
```

If you need to abort:

```
stax stop
```

## Subcommands reference

All subcommands except `setup` connect to `stax-server` via its local socket.
They fail loudly if the daemon isn't running.

### `stax record [-- COMMAND…]`

Start a recording. Either launch a child:

```
stax record -- ./target/release/foo --bench bar
```

…or attach to an existing process:

```
stax record --pid 12345
```

Pass exactly one of a launch command or `--pid` — not both, not neither.

Useful flags:

- `-F, --frequency <HZ>` — sampling rate (default 900)
- `-l, --time-limit <SECS>` — stop after N seconds (otherwise Ctrl-C)
- `-p, --pid <PID>` — attach to an existing process instead of launching one
- `--dwarf-unwind` — Linux x86-64 only: force `.eh_frame` DWARF unwinding of
  user stacks. Auto-detected by default (on when the target omits frame
  pointers); `STAX_DWARF_UNWIND=0` forces it off. No-op on macOS.
- `--daemon-socket <PATH>` — override `staxd`'s socket

`stax record` does no sampling itself. It launches the target (or resolves
`--pid`), hands it to `stax-server` over the `RunControl` service, and
`stax-server` drives the capture on an in-process per-run task — every PET
sample, off-CPU interval, wakeup edge, binary load, and thread-name event
folds straight into the live aggregator. So `stax record` needs
`stax-server`; on macOS it also needs `staxd`; on Linux `staxd` is needed
only on a locked-down host.

### `stax status`

Snapshot of the daemon. Prints the active run if any, plus when the
daemon itself started.

```
$ stax status
active run:
  run 1  [recording]  pid 12345  4824 samples / 119 intervals  (./bench)
```

### `stax list`

Every run the daemon has hosted (active + history, oldest first).

```
$ stax list
  run 1  [stopped]  pid 11000  9421 samples / 244 intervals  (./bench)
  run 2  [recording]  pid 12345  4824 samples / 119 intervals  (./bench)
```

### `stax wait [--for-samples N | --for-seconds N | --until-symbol NEEDLE] [--timeout-ms MS]`

Block until a condition fires, the active run reaches `Stopped`, or
the optional hard `--timeout-ms` elapses.

| flag                 | meaning                                                            |
|----------------------|--------------------------------------------------------------------|
| (none)               | wait for the active run to stop                                    |
| `--for-samples N`    | return after at least N PET samples have been ingested             |
| `--for-seconds N`    | return after N seconds of wall-clock time                          |
| `--until-symbol S`   | return once a symbol containing S has been seen (case-sensitive)   |
| `--timeout-ms MS`    | hard cap on the whole wait; exit code 1 + “timed out” message      |

Mutually exclusive across the first three (pass at most one).

```
$ stax wait --for-samples 5000 --timeout-ms 10000
condition met:
  run 2  [recording]  pid 12345  5012 samples / 124 intervals  (./bench)
```

Exit codes:

| code | situation                                            |
|------|------------------------------------------------------|
| 0    | condition met, or run reached `Stopped` cleanly      |
| 1    | timed out, or no active run, or other error          |

### `stax stop`

Ask the daemon to stop the active run cleanly. Prints the final
summary.

```
$ stax stop
stopped:
  run 2  [stopped]  pid 12345  5012 samples / 124 intervals  (./bench)
```

Exits non-zero if there's no active run.

### `stax top [-n N] [--sort self|total] [--tid TID]`

Snapshot the top-N hottest functions in the active run.

- `--sort self` (default) — leaf-only attribution (where the program is
  *now*).
- `--sort total` — any-frame attribution (functions that *contain* hot
  code, including their callers).

Output is one line per entry: `<self ms> <self samples> <function> (<binary>)`.

```
$ stax top -n 5
   42.184ms       3812 samples  vox_jit::translate (libvox.dylib)
    9.001ms        812 samples  cranelift::lower (libcranelift.dylib)
    …
```

### `stax threads [-n N]`

Per-thread on/off-CPU breakdown for the current run, sorted by
on-CPU time descending. Use it to figure out *which thread* is
worth flaming.

```
$ stax threads -n 5
 on-CPU ms off-CPU ms    samples   blocked  tid    name
   1240.20      31.40       1102      lock  501    main
    860.00      99.00        710     sleep  592    tokio-runtime-worker
    220.10      14.50        198      idle  600    grpc-pool
    …
```

The `blocked` column names the largest off-CPU bucket for that
thread (`idle`, `lock`, `sem`, `ipc`, `ioR`, `ioW`, `ready`,
`sleep`, `conn`, `other`). Off-CPU intervals are recorded on both macOS
and Linux.

`-n 0` prints every thread. Default 20.

### `stax flame [-d MAX_DEPTH] [--threshold-pct PCT] [--tid TID]`

Print the on-CPU flamegraph as an indented Markdown tree, sorted by
`on_cpu_ns` descending at each level. Same data the web UI renders;
this is the agent-friendly view of "where is the time going."

- `-d / --max-depth N` — cut off the tree at depth N (default 12).
  Children below the cut-off are summarised as `…N more frames`.
- `--threshold-pct PCT` — hide subtrees whose share of total
  on-CPU falls below `PCT` (default 1%; pass `0` for the whole tree).
- `--tid` — filter to one thread.

Operates on the current run's aggregator (same rules as `stax top`).

```
$ stax flame -d 4 --threshold-pct 2
# stax flame · total on-CPU 2.503s · off-CPU 4.122s

`​``
   2503ms 100.0%  (root)
   1201ms  48.0%    └─ vox_jit::translate  (libvox.dylib)
    901ms  36.0%      └─ cranelift::lower  (libcranelift.dylib)
    402ms  16.0%        └─ cranelift::regalloc  (libcranelift.dylib)
    200ms   8.0%      └─ vox_postcard::deserialize  (libvox.dylib)
    802ms  32.1%    └─ tokio::runtime::poll_task  (libtokio.dylib)
        …18 more frames
`​``
```

### `stax annotate <TARGET> [--tid TID]`

Disassemble + annotate one function from the current run.

`TARGET` is either:
- a **hex address** (`0x10004ad60`) — passed straight through to the
  Profiler RPC.
- a **substring of a function name** (`translate`, `cranelift::lower`,
  `MyType::method`) — case-insensitive. The CLI asks for the top 256
  leaf-self functions and picks the hottest one whose demangled name
  matches; the address that wins gets logged so you can re-target by
  address next time.

If nothing matches, you'll see the hottest names that *did* land —
useful when nothing's been sampled yet, or your symbol got merged into a
parent (try a name from `stax top` directly).

```
$ stax annotate translate
stax: matched "translate" → vox_jit::translate (3812 self samples)
; vox_jit::translate (rust) @ 0x10004ad58
; src/translate.rs:412
  0x10004ad58      0 samples    push rbp
  0x10004ad59      0 samples    mov  rbp, rsp
  0x10004ad5c     14 samples    mov  rax, qword ptr [rsi]
  …
```

Disassembly works on `x86_64` and `aarch64`. `--tid` filters to one thread;
omit for whole-process.

### `stax diagnose`

Dump `stax-server` diagnostics: telemetry phases, counters, histograms, and
recent events. Use it when numbers look wrong and you want the pipeline's own
accounting.

### `stax dump`

Ask every running stax process (`staxd`, `stax-server`, `stax`) to write a
SIGUSR1 telemetry/debug snapshot into its log output (see [Logs](#logs)).

### `stax setup`

Privileged install of `staxd` — a LaunchDaemon on macOS, a systemd service on
Linux. Agents on Amos's machine should use
`sudo -n /usr/local/sbin/stax-agent setup --yes`, not interactive
`sudo stax setup`. Not part of the routine agent flow.

## Wire / RPC services

Programmatic clients can skip `stax` and talk to `stax-server`'s vox services
directly. Both live in `stax-live-proto` and are exposed on the local socket
*and* `ws://127.0.0.1:8080`:

- **`RunControl`** — lifecycle: `status`, `list_runs`, `diagnostics`,
  `start_attach`, `wait_active`, `stop_active`.
- **`Profiler`** — query surface: `top`, `flamegraph`, `threads`,
  `annotated`, `timeline`, `neighbors`, `intervals`, `wakers`, … most with a
  `subscribe_*` variant that pushes periodic updates over a `vox::Tx<…>`
  (a one-shot call is the snapshot form).

There is no separate ingest service — recording runs in-process inside
`stax-server`.

Connect with:

- `local://$XDG_RUNTIME_DIR/stax-server.sock` or `/tmp/stax-server-$UID.sock`,
  for trusted local agents
- `ws://127.0.0.1:8080`, for browser clients

TypeScript bindings are generated into `frontend/src/generated/` by
`cargo run -p xtask -- codegen` (or `pnpm codegen` from `frontend/`).

## Common pitfalls

- **`error: stax-server isn't running`** — start the server. macOS: the
  LaunchAgent isn't loaded —

      launchctl bootstrap "gui/$(id -u)" \
        ~/Library/LaunchAgents/eu.bearcove.stax-server.plist

  Linux: just run `stax-server &`.

- **`another run is already active`** — single-active-run model. Use
  `stax wait` or `stax stop` first.

- **`stax record` fails immediately** — `stax-server` is down (recording
  runs *inside* it, so there is nowhere for the run to live). On macOS,
  `staxd` must also be installed. Fix the daemon first.

- **Linux: shallow stacks / no kernel frames / no PMU columns** — the
  in-process recorder is gated by `perf_event_paranoid`. Lower it
  (`sudo sysctl kernel.perf_event_paranoid=1`) or, better, install the
  `staxd` broker (`sudo stax setup`) so the host setting stops mattering.

- **Linux: a suspiciously flat flamegraph on x86-64** — the target was built
  `-fomit-frame-pointer`. stax auto-detects this; force it with
  `stax record --dwarf-unwind`.

- **macOS asks whether `stax-server` can access another app's data** — the
  server is touching an app/container data path. By default it uses
  `$XDG_RUNTIME_DIR/stax-server.sock` or `/tmp/stax-server-$UID.sock`, not a
  path under `~/Library/Group Containers`.

- **`stax top` returns `(no samples yet — is a recording in progress?)`** —
  either no run is active, or the run hasn't ingested any samples yet (very
  early in the lifecycle). Try `stax status` to confirm a run exists, or
  `stax wait --for-samples 100` to block until data is in.

- **Hardened-runtime targets** (macOS) are out of scope. The attachment
  helper is same-uid and intended for normal local developer processes.
