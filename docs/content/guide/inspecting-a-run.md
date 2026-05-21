+++
title = "Inspecting a Run"
weight = 2
insert_anchor_links = "heading"
+++

Once a run has samples, four commands let you look at it from different
angles — from a one-line leaderboard down to individual machine
instructions. They all read the **current** aggregator state, so they work
on a run that is still recording.

> **Which run do they query?** There is no run selector yet. `top`, `flame`,
> `threads`, and `annotate` operate on whichever run is active — or, if none
> is, the most recent one, which stays queryable until the next `stax record`
> resets the aggregator. See [Run Lifecycle](@/guide/run-lifecycle.md).

## stax top

The hottest functions, as a flat leaderboard.

```bash
stax top -n 10 --sort self
```

```text
   42.184ms       3812 samples  mycrate::translate (mybench)
    9.001ms        812 samples  cranelift::lower (libcranelift.dylib)
    …
```

One line per function: self time in milliseconds, self sample count,
demangled name, and the binary it came from.

| flag                | meaning                                                   |
|---------------------|-----------------------------------------------------------|
| `-n, --limit <N>`   | how many entries to return — default **20**               |
| `--sort self`       | leaf-only attribution: where the program *is* — default   |
| `--sort total`      | any-frame attribution: functions that *contain* hot code  |
| `--tid <TID>`       | restrict to one thread — default: all threads             |

`--sort self` answers "what instruction is the CPU running"; `--sort total`
answers "what work is responsible", and will rank callers like `main` or a
runtime's poll loop highly because hot code runs *underneath* them.

## stax flame

The on-CPU flamegraph, printed as an indented tree — the same data the
[web UI](@/guide/web-ui.md) renders, in a form you (or an agent) can read in
a terminal.

```bash
stax flame -d 4 --threshold-pct 2
```

```text
# stax flame · total on-CPU 2.503s · off-CPU 4.122s

   2503ms 100.0%  (root)
   1201ms  48.0%    └─ vox_jit::translate  (libvox.dylib)
    901ms  36.0%      └─ cranelift::lower  (libcranelift.dylib)
    402ms  16.0%        └─ cranelift::regalloc  (libcranelift.dylib)
    200ms   8.0%      └─ vox_postcard::deserialize  (libvox.dylib)
    802ms  32.1%    └─ tokio::runtime::poll_task  (libtokio.dylib)
        …18 more frames
```

Children are sorted by on-CPU time, descending, at every level.

| flag                       | meaning                                                            |
|----------------------------|--------------------------------------------------------------------|
| `-d, --max-depth <N>`      | stop printing below depth N — default **12**. Cut subtrees collapse to `…N more frames` |
| `--threshold-pct <PCT>`    | hide subtrees below this share of total on-CPU — default **1.0**; pass `0` for the whole tree |
| `--tid <TID>`              | restrict to one thread — default: all threads                      |

The flamegraph the server holds is unbounded; `--max-depth` only controls
how much the CLI prints.

## stax threads

Per-thread on/off-CPU breakdown, sorted by on-CPU time descending. Use it to
decide *which thread* is worth flaming before you call `stax flame --tid`.

```bash
stax threads -n 5
```

```text
 on-CPU ms off-CPU ms    samples   blocked  tid    name
   1240.20      31.40       1102      lock  501    main
    860.00      99.00        710     sleep  592    tokio-runtime-worker
    220.10      14.50        198      idle  600    grpc-pool
    …
```

The `blocked` column names the **largest off-CPU bucket** for that thread —
one of `idle`, `lock`, `sem`, `ipc`, `ioR`, `ioW`, `ready`, `sleep`, `conn`,
`other`. It tells you *why* a thread spent time off-CPU, which `stax flame`
(on-CPU only) cannot.

| flag              | meaning                                                |
|-------------------|--------------------------------------------------------|
| `-n, --limit <N>` | how many threads to print — default **20**; `0` for all|

Off-CPU attribution is a macOS feature — see
[Platform Support](@/concepts/platform-support.md).

## stax annotate

Disassemble one function and attribute samples to individual instructions,
interleaved with source.

```bash
stax annotate translate
```

```text
stax: matched "translate" → vox_jit::translate (3812 self samples)
; vox_jit::translate (rust) @ 0x10004ad58
; src/translate.rs:412
  0x10004ad58      0 samples    push rbp
  0x10004ad59      0 samples    mov  rbp, rsp
  0x10004ad5c     14 samples    mov  rax, qword ptr [rsi]
  …
```

The `TARGET` argument is either:

- **A hex address** (`0x10004ad60`) — passed straight to the profiler.
- **A substring of a demangled name** (`translate`, `mycrate::lower`,
  `MyType::method`) — case-insensitive. stax takes the top 256 leaf-self
  functions, picks the hottest one whose name matches, and logs the address
  it chose so you can re-target by address next time.

If nothing matches, stax prints the hottest names that *did* land — useful
when nothing's been sampled yet, or your symbol got merged into a parent.

| flag          | meaning                                          |
|---------------|--------------------------------------------------|
| `--tid <TID>` | restrict to one thread — default: all threads    |

Disassembly works on both `aarch64` and `x86_64`. For JIT'd code, the code
bytes come from the [jitdump](@/guide/profiling-jit-code.md) record, so
annotation works without re-reading the target's memory.

## A typical session

```bash
stax record -- ./bench &
stax wait --for-samples 10000   # block until there's enough data
stax threads -n 5               # which thread is hot?
stax flame --tid 501 -d 8       # flame just that thread
stax top -n 20 --sort self      # the hot leaves
stax annotate 'hot_fn'          # down to the instruction
stax stop                       # end the run; data stays queryable
```
