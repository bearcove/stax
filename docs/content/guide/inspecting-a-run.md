+++
title = "Inspecting a Run"
weight = 2
insert_anchor_links = "heading"
+++

Once a run has samples or cooperating target spans, five commands let you
look at it from different angles — from a one-line leaderboard down to
individual machine instructions. They all read the **current** aggregator
state, so they work on a run that is still recording.

> **Which run do they query?** `top`, `flame`, `threads`, and `annotate`
> operate on `stax-server`'s current query state. While a recording is active,
> that is the active run; after a run stops, its snapshot stays selected.
> Use `stax select-run <ID>` to restore a stopped in-memory run from
> `stax list`, or pass `--run <ID>` to a reporting command for a one-off
> non-mutating query of that stopped run. Use `stax open <DIR>` to restore a
> saved archive. See [Run Lifecycle](@/guide/run-lifecycle.md).

## stax top

The hottest functions or target-span names, as a flat leaderboard.

```bash
stax top -n 10 --sort self
```

```text
 active ms  target ms  samples    spans  function
    42.184      0.000     3812        0  mycrate::translate (mybench)
     9.001      0.000      812        0  cranelift::lower (libcranelift.dylib)
    …
```

One line per function/span: active time, target-executor time, PET samples,
target span count, demangled name, and the binary it came from. For a
cooperating synthetic lane, `target ms` is the sum of reported span durations
and `spans` is span count.

| flag                | meaning                                                   |
|---------------------|-----------------------------------------------------------|
| `-n, --limit <N>`   | how many entries to return — default **20**               |
| `--sort self`       | leaf-only attribution: where the program *is* — default   |
| `--sort total`      | any-frame attribution: functions that *contain* hot code  |
| `--tid <TID>`       | restrict to one thread — default: all threads             |
| `--run <RUN_ID>`    | query a run without changing selected query state         |

The output columns are active time, target-executor time, PET samples, target
span count, and function/span name. `--sort self` answers "what instruction is
the CPU or target lane running"; `--sort total` answers "what work is
responsible", and will rank callers like `main` or a runtime's poll loop
highly because hot code runs *underneath* them.
For target lanes, `--sort self --tid <synthetic>` shows per-span names;
`--sort total --tid <synthetic>` can surface the lane aggregate. For direct
target rankings such as "most invoked" or "most target time", use
[`stax target top`](#stax-target). When spans carry origins, `--tid <real CPU
tid>` includes target lane work linked to that CPU thread. The origin is a
provenance link; the target work does not become CPU execution under the
dispatch stack.

When `stax top` sees Metal command/dispatch frames but no synthetic target
lane, it prints a hint to stderr suggesting Metal 4 timestamp-counter
cooperation through `stax-target`. Empty `top` views also print discovery
hints when the run has off-CPU/thread activity or target lanes outside a
`--tid` filter, pointing you at `stax threads -n 0`, the relevant lane tid, or
generic `stax-target` span integration.

## stax flame

The active flamegraph, printed as an indented tree — the same data the
[web UI](@/guide/web-ui.md) renders, in a form you (or an agent) can read in
a terminal.

```bash
stax flame -d 4 --threshold-pct 2
```

```text
# stax flame · total active 2.503s · target 0.000s · off-CPU 4.122s

  active   target   spans     %  frame
 2503.00     0.00       0 100.0  (root)
 1201.00     0.00       0  48.0    └─ vox_jit::translate  (libvox.dylib)
  901.00     0.00       0  36.0      └─ cranelift::lower  (libcranelift.dylib)
  402.00     0.00       0  16.0        └─ cranelift::regalloc  (libcranelift.dylib)
  200.00     0.00       0   8.0      └─ vox_postcard::deserialize  (libvox.dylib)
  802.00     0.00       0  32.1    └─ tokio::runtime::poll_task  (libtokio.dylib)
        …18 more frames
```

Children are sorted by active time, descending, at every level. CPU threads
contribute scheduler-derived on-CPU time; cooperating target lanes contribute
reported span duration and render as `(all) -> lane -> span name`. When spans
carry origins, filtering to the origin CPU tid keeps the lane tree and limits
it to work linked to that CPU origin.

| flag                       | meaning                                                            |
|----------------------------|--------------------------------------------------------------------|
| `-d, --max-depth <N>`      | stop printing below depth N — default **12**. Cut subtrees collapse to `…N more frames` |
| `--threshold-pct <PCT>`    | hide subtrees below this share of total active time — default **1.0**; pass `0` for the whole tree |
| `--tid <TID>`              | restrict to one thread — default: all threads                      |
| `--run <RUN_ID>`           | query a run without changing selected query state                  |

The flamegraph the server holds is unbounded; `--max-depth` only controls
how much the CLI prints. Like `top`, `flame` prints a Metal cooperation hint
to stderr when Metal command/dispatch frames are visible but no target lane
has reported spans. If the flame root is otherwise empty while the run has
off-CPU/thread activity or target lanes elsewhere, it prints the same
discovery hints as `top`.

## stax threads

Per-thread and synthetic-lane active/off-CPU breakdown, sorted by total
activity. CPU thread target columns include origin-linked target spans queued
from that thread as provenance-linked parallel work. Synthetic target lanes
with spans are included even when they
would otherwise fall past the normal `-n` cutoff. Use it to decide *which
thread or lane* is worth flaming before you call `stax flame --tid`.

```bash
stax threads -n 5
```

```text
    cpu ms  target ms off-CPU ms  samples    spans    kind   blocked  tid    name
   1240.20       0.00      31.40     1102        0  thread      lock  501    main
    860.00       0.00      99.00      710        0  thread     sleep  592    tokio-runtime-worker
      0.00     220.10       0.00      198      198  target         -  4293918720 GPU tq1s
    …
```

The `cpu ms` column is real on-CPU time. `target ms` is exact span duration:
for synthetic lanes it is lane-active target time; for CPU threads it is
origin-linked target work queued from that tid, not CPU-busy time. `samples`
is PET sample count and `spans` is target span count. The `kind` column says
whether the row is a real sampled thread or a synthetic target lane.

The `blocked` column names the **largest off-CPU bucket** for that thread —
one of `idle`, `lock`, `sem`, `ipc`, `ioR`, `ioW`, `ready`, `sleep`, `conn`,
`other`. It tells you *why* a thread spent time off-CPU.

| flag              | meaning                                                |
|-------------------|--------------------------------------------------------|
| `-n, --limit <N>` | how many threads to print — default **20**; `0` for all|
| `--run <RUN_ID>`  | query a run without changing selected query state      |

Off-CPU intervals are recorded on both macOS and Linux. The *waker*
attribution shown elsewhere needs the `staxd` broker on Linux — see
[Platform Support](@/concepts/platform-support.md).

## stax target

Target-focused queries for cooperating lanes. Use them once `stax threads` has
shown synthetic lanes, or when you already know target spans exist and want the
answer without CPU rows mixed in.

```bash
stax target lanes
stax target top --by time
stax target top --by count
```

```text
 target ms    count     avg ms     max ms    kind  lane                     span
    220.10      198      1.112      4.300   metal  GPU tq1s                 tq6_1s_rows
```

`stax target lanes` lists synthetic target lanes by exact target time. `stax
target top` ranks lane + span/shader names and aggregates across CPU origin
groups, so a shader dispatched from several stacks still appears once.

| flag              | meaning                                              |
|-------------------|------------------------------------------------------|
| `-n, --limit <N>` | how many rows to print — default **20**; `0` for all |
| `--by time`       | rank by total target duration — default             |
| `--by count`      | rank by invocation/span count                        |
| `--by avg`        | rank by average duration                             |
| `--by max`        | rank by max observed duration                        |
| `--tid <TID>`     | filter to a target lane tid or origin-linked CPU tid |
| `--run <RUN_ID>`  | query a run without changing selected query state    |

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
| `--run <RUN_ID>` | query a run without changing selected query state          |

Disassembly works on both `aarch64` and `x86_64`. For JIT'd code, the code
bytes come from the [jitdump](@/guide/profiling-jit-code.md) record, so
annotation works without re-reading the target's memory.

## A typical session

```bash
stax record -- ./bench &
stax wait --for-samples 10000   # block until there's enough data
stax threads -n 5               # which thread is hot?
stax target top --by time       # which target spans/shaders dominate?
stax target top --by count      # which target spans/shaders run most often?
stax flame --tid 501 -d 8       # flame just that thread
stax top -n 20 --sort self      # the hot leaves
stax annotate 'hot_fn'          # down to the instruction
stax stop                       # end the run; data stays queryable
```
