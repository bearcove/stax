---
name: stax
description: Profile a program with stax (live CPU stacks, off-CPU waits, target spans, annotated disassembly) to find where time actually goes. Use whenever you need to measure performance, find hot functions, or answer "why is this slow" instead of guessing. Covers record → wait → flame → top → annotate, the flame-before-top rule, and reading off-CPU vs on-CPU time.
---

stax is a live profiler. You drive it entirely from the CLI: record a program,
then query the run *while it is still going* (or after it exits — the run stays
in the server's query state). Every view is text with meaningful exit codes, so
it works over SSH, in CI, and from an agent.

## The rule that matters most: measure, never guess

If a question is measurable, measure it. Do not theorize about why something is
slow, which function dominates, or whether a path is hot — record it and read
the profile. A profile that's already loaded answers most "why" questions for
free; reaching for source-reading or a hunch first is the mistake.

## Workflow

1. **Build a release binary and profile *it*, not `cargo run`.**
   `cargo run` profiles the compile too. Build first, then point stax at the
   artifact:
   ```bash
   cargo build --release --example mybench
   stax record -- ./target/release/examples/mybench arg1 arg2
   ```
   - Attach to something already running with `stax record --pid <PID>` instead.
   - Default sampling is 900 Hz (`-F`). `-l <secs>` stops after N seconds; by
     default it runs until the target exits (or Ctrl-C).
   - `record` launches the target *suspended* and starts sampling before it
     runs, so you catch startup. When the target exits, the run is left in the
     server's query state automatically.

2. **If querying a still-running target, wait for data first.**
   ```bash
   stax wait --for-samples 10000      # or --for-seconds N, --until-symbol SUBSTR
   ```

3. **`stax flame` FIRST. Always.** This is the load-bearing step.
   ```bash
   stax flame -d 12 --threshold-pct 1
   ```
   `flame` prints the call tree as an indented tree with each frame's share of
   total active time — caller → callee, with siblings shown side by side. It
   tells you both *what* is expensive and *how the costs nest*: which work is
   stacked under one parent vs. which are independent siblings. That structure
   is the answer to most perf questions, drawn for you.
   - `-d/--max-depth` (default 12) caps print depth; deeper children collapse to
     `…<N more frames>`.
   - `--threshold-pct` (default 1.0) hides frames below that share; `0` prints all.
   - `--tid` filters to one thread.

4. **`stax top` is for drilling, AFTER flame — not for reconstructing the tree.**
   ```bash
   stax top -n 20 --sort self     # hottest leaves
   stax top -n 20 --sort total    # hottest frames counting any stack position
   ```
   `top` is a *flat ranking of frames in isolation*. It does not show nesting.
   Use it to confirm a leaf you already located in the flame tree, or to read
   exact sample counts — never to guess the call structure in your head. (If you
   find yourself mentally reassembling a tree from `top`, stop and run `flame`.)

5. **`stax annotate` goes to the instruction level.**
   ```bash
   stax annotate 'mycrate::hot_fn'    # substring of demangled name, or a 0x address
   ```
   Per-instruction sample counts with interleaved source lines. The substring
   matches against the run's top-N leaves; the hottest match wins.

## Reading the numbers

- **active (on-CPU) vs off-CPU.** `flame`/`top` rank *active* (on-CPU) time. A
  run can show a huge *off-CPU* total (in `stax threads`) — that's time blocked
  on I/O, locks, or sleeping, not burning CPU. A small active total with a giant
  off-CPU total means the program is mostly waiting, not computing. Use
  `stax threads -n 20` for the per-thread CPU / off-CPU / target-lane split.
- **target spans** (the `target ms` / `spans` columns) are exact-duration work
  reported by a target that links `stax-target` — GPU kernels, executors,
  accelerator queues. `stax target lanes` / `stax target top` rank those; with
  origins, `flame` renders CPU dispatch stack → target lane → named work.

## Inspecting, saving, comparing runs

```bash
stax status                 # active run + history
stax list                   # every run the server has hosted
stax top --run <ID>         # query a specific past run without selecting it
stax select-run <ID>        # restore a stopped run as the active query state
stax save out.stax          # save current/most-recent run to a .stax package
stax open out.stax          # load a saved archive back into query state
stax compare a.stax b.stax  # diff two saved runs (--json for machine-readable)
```

## One-time setup (macOS)

stax needs kperf access. `stax setup` codesigns the binary; run as root it
installs `staxd` as a LaunchDaemon. Do this once per machine before recording.

## Anti-patterns

- Profiling `cargo run …` (you measure the build) instead of the built binary.
- Running `stax top` and reasoning about the call graph from the flat list —
  that's exactly the trap; `flame` shows the graph.
- Concluding a function is the bottleneck from reading code. Record and read the
  flame instead.
- Treating a large off-CPU total as CPU cost — check `stax threads` to see
  whether the program is computing or waiting.
