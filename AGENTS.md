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

- **On-CPU PET samples** — what `top`, `flame`, `annotate` aggregate for
  CPU threads.
- **Off-CPU intervals + wakeup attribution** — every blocked stretch with
  why-blocked classification. `stax threads` prints the per-thread
  on/off-CPU breakdown on the CLI; the web UI timeline shows the intervals.
- **Target/executor lanes** via **`stax-target`** — a profiled app links the
  `stax-target` crate and reports named spans (GPU kernels, command-buffer
  stages, accelerator jobs, runtime/executor work) with timestamps on the
  same timebase as the recording; they ingest as a **synthetic thread** per
  `(pid, lane)`. Span names are synthetic symbols, so
  `stax top --tid <synthetic>` and `stax flame --tid <synthetic>` render
  span names like function frames. CLI and web views now break out explicit
  `target` time/span counts alongside active time/PET samples. If the target
  also reports a span origin, `top` and `flame` for the origin CPU tid include
  that target work under the sampled CPU stack that queued it:
  `CPU caller -> lane -> span`. No correlation step, no chrome-trace export,
  no second tool. See `stax-target/src/lib.rs` and
  `stax-server/src/target_ingest.rs`; the GPU worked example is
  `docs/content/guide/profiling-gpu-work.md`.

A cooperating process pays nothing when not recorded: the target polls a
capture gate (~1s) and only captures spans while a recording of its pid is
active (`stax_target::reporting_active()`).

Known consumer: **bee's `hx`** reports Metal 4 per-dispatch GPU timestamps
as the `"GPU tq1s"` lane (and TTS lanes) — see
`bee/rust/helix-metal4/src/stax.rs`, whose module doc says it plainly:
"stax is THE profiler for this work".

Gotchas that have actually misled agents (verified 2026-06-12):

- Synthetic target lanes use the **target** columns for exact span duration
  and span count. The legacy `active`/`on_cpu_ns` fields still include that
  time so older clients and flame widths keep working; do not read target
  lane active time as CPU-busy time. Synthetic tids live at/above
  `0xFFF0_0000`.
- `stax top --tid <synthetic>` should show span names with duration sums and
  span counts. `stax flame --tid <synthetic>` should render
  `(all) -> lane -> span name`. If either is empty while `stax threads`
  shows spans for that tid, treat it as a regression in the target-ingest
  aggregation path.
- When the target reports span origins, `stax flame --tid <real CPU tid>`
  can show the causal path as `(all) -> CPU caller -> lane -> span name`.
  `stax top --tid <real CPU tid> --sort total` will then attribute the span
  duration to the CPU dispatch stack; `--sort self` still surfaces the
  per-kernel span names.
- A GPU-bound target looks nearly empty in CPU-only rows — a handful of
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

`stax list` shows every run the daemon has hosted (active + history).
That list is still server-memory history; use `stax save <DIR>` to persist
the current queryable run before starting another recording or restarting
`stax-server`.

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
one first (so its data sticks around), save it with `stax save <DIR>`, and
reopen it later with `stax open <DIR>`. Per-`RunId` querying is still on the
roadmap.

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

When you need a known-good target-span recording that proves stax is not just
CPU sampling, use the blessed corpus:

```
just demo-corpus
stax top --tid <corpus-executor-tid> --sort self
stax flame --tid <cpu-tid> --threshold-pct 0
```

`just demo-corpus` records `stax-target/examples/corpus.rs`, then prints
`stax threads -n 0` and `stax diagnose`. Expect CPU thread rows, off-CPU
waits, synthetic target lanes (`corpus executor`, `corpus gpu`,
`corpus bad origins`), linked origins, and intentional bad-origin diagnostics.

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

### `stax save <PATH>`

Write the current or most recent queryable run to a directory archive. The
archive contains `archive.json`, a versioned facet-json payload with the run
summary, raw aggregator streams, binary/symbol metadata, and target-ingest
diagnostics. It is meant for bug reports, handoff, and replaying
`threads`/`top`/`flame` after the live process is gone.

```
$ stax save /tmp/stax-demo.staxdir
saved: /tmp/stax-demo.staxdir
```

`stax save` needs some queryable run state. It works while a run is active,
and after `stax stop`, until the next recording resets the live aggregator.
Archive compatibility is strict in the current format: `stax open` and
`stax compare` accept `format_version = 1` and reject other versions loudly.

### `stax open <PATH>`

Load a saved directory archive into `stax-server`'s current query state.
After opening, the usual views operate on the restored run:

```
$ stax open /tmp/stax-demo.staxdir
opened: /tmp/stax-demo.staxdir
$ stax threads -n 0
$ stax top -n 20
$ stax flame --threshold-pct 0
```

`stax open` refuses to replace state while a recording is active. Stop the
run first. It accepts either the archive directory or the `archive.json` file
inside it.

### `stax compare <BASELINE> <CANDIDATE>`

Compare two saved archives without touching `stax-server` state.

```
$ stax compare /tmp/before.staxdir /tmp/after.staxdir
```

It reads each archive's typed `archive.json` directly and prints deltas for
PET samples, on/off-CPU interval time, target time, target span counts,
origin-link counts, ingest drops, and the top target lanes by duration. Use
`stax open` when you want to inspect one archive through `threads`, `top`,
`flame`, or `diagnose`; use `compare` for quick before/after notes.

For a live save/reopen smoke with the blessed target-span corpus:

```
just archive-smoke
```

That records `stax-target/examples/corpus.rs`, saves it to a temporary
archive directory, reopens it, queries `threads`/`top`/`flame`/`diagnose`,
and runs `stax compare` against the archive itself.

### `stax top [-n N] [--sort self|total] [--tid TID]`

Snapshot the top-N hottest functions in the active run.

- `--sort self` (default) — leaf-only attribution (where the program is
  *now*).
- `--sort total` — any-frame attribution (functions that *contain* hot
  code, including their callers).

Output is one line per entry with active time, target-executor time, PET
sample count, target span count, and symbol name. For synthetic target lanes,
`target ms` is span duration and `spans` is span count. When spans carry
origins, `--tid <real CPU tid>` includes those spans under the CPU stack that
queued them.

```
$ stax top -n 5
 active ms  target ms  samples    spans  function
    42.184      0.000     3812        0  vox_jit::translate (libvox.dylib)
     9.001      0.000      812        0  cranelift::lower (libcranelift.dylib)
    …
```

### `stax threads [-n N]`

Per-thread and synthetic-lane active/off-CPU breakdown for the current run,
sorted by total activity. Use it to figure out *which thread or lane* is
worth flaming.

```
$ stax threads -n 5
    cpu ms  target ms off-CPU ms  samples    spans    kind   blocked  tid    name
   1240.20       0.00      31.40     1102        0  thread      lock  501    main
    860.00       0.00      99.00      710        0  thread     sleep  592    tokio-runtime-worker
      0.00     220.10       0.00      198      198  target         -  4293918720 GPU tq1s
    …
```

The `cpu ms` column is real on-CPU time. `target ms` is exact duration from
cooperating target spans: for synthetic lanes it is lane active time; for CPU
threads it is origin-linked target work queued by that thread. `samples` is PET
sample count and `spans` is target span count. The `kind` column is `thread`
for real sampled threads and `target` for synthetic target lanes. The `blocked`
column names the largest off-CPU bucket for that thread (`idle`, `lock`, `sem`,
`ipc`, `ioR`, `ioW`, `ready`, `sleep`, `conn`, `other`). Off-CPU intervals are
recorded on both macOS and Linux.

`-n 0` prints every thread. Default 20. Synthetic target lanes with spans are
included even when they would otherwise fall past the cutoff.

### `stax flame [-d MAX_DEPTH] [--threshold-pct PCT] [--tid TID]`

Print the active flamegraph as an indented Markdown tree, sorted by active
time descending at each level. Same data the web UI renders; this is the
agent-friendly view of "where is the time going." Cooperating target
lanes render as `(all) -> lane -> span name`; when spans carry origins,
filtering to the origin CPU tid renders `(all) -> CPU caller -> lane -> span`.

- `-d / --max-depth N` — cut off the tree at depth N (default 12).
  Children below the cut-off are summarised as `…N more frames`.
- `--threshold-pct PCT` — hide subtrees whose share of total active time
  falls below `PCT` (default 1%; pass `0` for the whole tree).
- `--tid` — filter to one thread. Origin-linked target spans are included
  for the CPU thread that queued them.

Operates on the current run's aggregator (same rules as `stax top`).

```
$ stax flame -d 4 --threshold-pct 2
# stax flame · total active 2.503s · target 0.000s · off-CPU 4.122s

`​``
  active   target   spans     %  frame
 2503.00     0.00       0 100.0  (root)
 1201.00     0.00       0  48.0    └─ vox_jit::translate  (libvox.dylib)
  901.00     0.00       0  36.0      └─ cranelift::lower  (libcranelift.dylib)
  402.00     0.00       0  16.0        └─ cranelift::regalloc  (libcranelift.dylib)
  200.00     0.00       0   8.0      └─ vox_postcard::deserialize  (libvox.dylib)
  802.00     0.00       0  32.1    └─ tokio::runtime::poll_task  (libtokio.dylib)
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

Dump `stax-server` diagnostics: active run state plus target-span ingest
counters (batches, recorded/dropped spans, lane totals, and origin
link/unlink counts, unlinked-origin reasons, and PET origin-distance
min/avg/max, plus target-side stax-target queue drops). It also prints
target-ingest hints for missing batches, bad span durations, missing origins,
origins that do not link to a sampled CPU stack, batches that arrived with no
active run, batches from the wrong pid, and local stax-target queue overflow or
worker-disconnect drops. For unlinked origins it distinguishes synthetic target
tids, tids with no PET samples, sampled tids with no user stacks, and origins
too far from the nearest sample. Use it when numbers look wrong and you want
the pipeline's own accounting.

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
  `annotated`, `timeline`, `neighbors`, `intervals`, `target_spans`,
  `wakers`, … most with a
  `subscribe_*` variant that pushes periodic updates over a `vox::Tx<…>`
  (a one-shot call is the snapshot form). `target_spans` returns grouped
  target work by lane/span/origin plus capped recent individual spans.

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

- **`stax top` returns `(no samples or target spans yet — is a recording in progress?)`** —
  either no run is active, or the run hasn't ingested any PET samples or
  target spans yet (very early in the lifecycle). Try `stax status` to
  confirm a run exists, or `stax wait --for-samples 100` to block until CPU
  samples are in. If the CLI prints an extra hint about off-CPU/thread
  activity, target lanes, or `stax-target`, follow that hint first: the CPU
  view may be empty because the interesting work is waiting, hidden behind an
  executor, or filtered away from an existing synthetic lane.

- **Hardened-runtime targets** (macOS) are out of scope. The attachment
  helper is same-uid and intended for normal local developer processes.
