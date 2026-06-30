---
name: stax
description: Profile a program with stax (live CPU stacks, off-CPU waits, target/GPU spans, annotated disassembly) to find where time actually goes. Use whenever you need to measure performance, find hot functions, or answer "why is this slow" instead of guessing. Covers building with the right debug symbols, record → wait → flame → top → annotate, the flame-before-top rule, on-CPU vs off-CPU, and where the full reference lives.
---

stax is a live profiler driven entirely from the CLI: record a program, then
query the run *while it is still going* (or after it exits — the run stays in
the server's query state). Text output, meaningful exit codes; works over SSH,
in CI, and from an agent.

This skill is the quickstart plus the habits that matter. **It is not the
complete reference.** The authoritative docs are:

- **`AGENTS.md` in the stax repo** — full subcommand reference, daemon/concurrency
  model, run lifecycle, RPC services, common pitfalls. Read it for anything below.
- **<https://stax.bearcove.eu>** — guide, concepts, install, platform/unwinding details.
- **`stax --help`, `stax <cmd> --help`, `stax --html-help`** — exact flags. Plus a
  **web UI** (timeline + flamegraph) when you want one; nothing depends on it.

## Rule #1: measure, never guess

If a question is measurable, measure it. Do not theorize about why something is
slow, which function dominates, or whether a path is hot — record it and read
the profile. A loaded profile answers most "why" questions for free; reaching
for source-reading or a hunch first is the mistake.

## Build for profiling: get the symbols right BEFORE recording

A profiler is only as good as the symbols in the binary. Get this right first or
every later view is degraded.

- **Plain `cargo build --release`** is unstripped by default, so `flame`/`top`
  *do* resolve your function names — but release sets `debug = false`, so there
  are **no line tables**: `stax annotate` can't interleave source, and inlined
  frames collapse into their callers. Fine for a first look, not for real work.
- **Best practice — a dedicated profile that inherits release and adds just
  enough debug info:**
  ```toml
  # Cargo.toml (workspace root)
  [profile.profiling]
  inherits = "release"
  debug = "line-tables-only"   # line numbers + inline attribution; cheap, ideal for a profiler
  strip = false                # never strip — stripping removes the symbol table → raw addresses
  ```
  ```bash
  cargo build --profile profiling --example mybench
  stax record -- ./target/profiling/examples/mybench arg1 arg2
  ```
  `debug = "line-tables-only"` is the lean sweet spot. Use `debug = true` (= 2)
  only if you also want variables/types in `annotate`; `debug = 1` is similar to
  line-tables for profiling.
- **The "abuse release" shortcut:** add `debug = "line-tables-only"` straight to
  `[profile.release]`. Keeps every optimization, just emits line tables. A
  separate `profiling` profile is cleaner (keeps release artifacts lean), but
  in-place works when you can't add a profile.
- **Never `strip = true`** for a binary you intend to profile — you'll get
  addresses, not names.
- You don't symbolicate *system/library* frames yourself: stax pulls those from
  debuginfod / local debug packages (Linux) and the dyld shared cache (macOS).
  The debug-info advice above is about **your own code** — debuginfod won't have
  your crate.
- **Unwinding:** on Linux x86-64 stax recovers full stacks from `.eh_frame` by
  default, so you do **not** need frame pointers. (`--no-dwarf-unwind` opts out.)
  macOS/aarch64 already walk full stacks.

## Workflow

1. **Profile the built binary, not `cargo run`** (that profiles the compile too).
   Build with symbols (above), then `stax record -- ./path/to/bin …`, or attach
   with `stax record --pid <PID>`. Defaults: 900 Hz (`-F`), runs until the target
   exits (`-l <secs>` to cap). The target launches suspended so you catch startup;
   on exit the run is left in query state automatically.

2. **If the target is still running, wait for data:**
   ```bash
   stax wait --for-samples 10000      # or --for-seconds N, --until-symbol SUBSTR; --timeout-ms MS
   ```

3. **`stax flame` FIRST. Always.** The load-bearing step.
   ```bash
   stax flame -d 12 --threshold-pct 1
   ```
   Prints the call tree as an indented tree with each frame's share of total
   active time: caller → callee, siblings side by side. It tells you *what* is
   expensive **and how the costs nest** (stacked under one parent vs. independent
   siblings) — that structure is the answer to most perf questions, drawn for you.
   `-d` caps depth (deeper → `…<N more frames>`), `--threshold-pct` hides small
   frames (`0` = all), `--tid` filters one thread.

4. **`stax top` is for drilling AFTER flame — not for rebuilding the tree.**
   ```bash
   stax top -n 20 --sort self     # hottest leaves
   stax top -n 20 --sort total    # hottest frames in any stack position
   ```
   `top` is a *flat ranking of frames in isolation*; it shows no nesting. Use it
   to confirm a leaf you already located in the flame, or to read exact sample
   counts. If you catch yourself mentally reassembling a call tree from `top`,
   stop and run `flame` — that exact move is how you guess the trunk wrong.

5. **`stax annotate` → instruction level** (needs the debug info from step "Build"):
   ```bash
   stax annotate 'mycrate::hot_fn'    # substring of demangled name, or a 0x address
   ```
   Per-instruction sample counts interleaved with source; hottest matching leaf wins.

## Reading the numbers

- **on-CPU vs off-CPU.** `flame`/`top` rank *active* (on-CPU) time. A run can
  show a large *off-CPU* total — time blocked on locks, sleep, I/O, IPC, not
  burning CPU. Small active + huge off-CPU = the program is mostly *waiting*. Use
  `stax threads -n 20` for the per-thread on-CPU / off-CPU / target-lane split,
  and stax correlates the scheduler event so you see *why* it blocked.
- **target/executor spans** (`target ms` / `spans` columns) are exact-duration
  work reported by an app linking `stax-target` — GPU kernels, accelerator
  queues, executors. Rank them with `stax target lanes` / `stax target top
  --by time|count|avg|max`; with origins, `flame` renders CPU dispatch → lane →
  named work.

## Runs: inspect, save, compare (incl. CI gating)

```bash
stax status                 # active run + history
stax list                   # every run the server has hosted
stax top --run <ID>         # query a past run without selecting it (most views take --run)
stax select-run <ID>        # restore a stopped run as the active query state
stax save out.stax          # .stax package (or a dir path for unpacked chunks + events.jsonl)
stax open out.stax          # load a saved archive back into query state
stax compare base.stax cand.stax            # human diff of two saved runs
stax compare --json --fail-active-delta-ms 50 base.stax cand.stax   # regression gate for CI
stax diagnose               # server-side ingest / origin-link / sample diagnostics
stax dump                   # ask running stax processes to dump telemetry to the system log
```

## Setup (once per machine)

stax needs privileged sampling access. `stax setup` codesigns the binary
(macOS); run as root it installs the `staxd` helper (LaunchDaemon on macOS;
see AGENTS.md "Install" for Linux `stax-server` + `staxd`). Do this before
recording.

## Anti-patterns

- **Profiling `cargo run …`** — you measure the build. Build, then record the artifact.
- **Recording a stripped / `debug = false` binary** and then being surprised by
  raw addresses or `annotate` with no source. Set up the profiling profile first.
- **Reading `top` and reasoning about the call graph from the flat list** — the
  trap. `flame` shows the graph.
- **Concluding a function is the bottleneck by reading code.** Record and read the flame.
- **Treating a big off-CPU total as CPU cost** — check `stax threads`; it may be waiting.
- **Treating this skill as the whole map.** For any command's full behavior, read
  `AGENTS.md` and `stax <cmd> --help`.
