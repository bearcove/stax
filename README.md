# stax

A live profiler for macOS and Linux — CPU stacks, off-CPU waits,
cooperating target spans, and annotated disassembly while your program runs.

```bash
# record a program (or attach to a running one with --pid)
stax record -- ./target/release/mybench

# from another shell — or from an AI agent — query the live run
stax wait --for-samples 10000     # block until data lands
stax threads -n 20                # CPU/off-CPU/target-lane breakdown
stax top -n 10 --sort self        # hottest leaf functions or target spans
stax flame -d 6                   # active flamegraph, as a tree
stax annotate 'mycrate::hot_fn'   # per-instruction sample counts
```

stax records on-CPU stacks, off-CPU waits, and cooperating target spans, then
turns them into flamegraphs, top-N functions/spans, per-thread and per-lane
breakdowns, and annotated disassembly — all queryable *while the recording is
still running*.

When a target links `stax-target`, GPU kernels, accelerator queues, executors,
and runtime lanes can report exact-duration spans into the same recording.
With origins, stax can render the causal path as CPU dispatch stack -> target
lane -> named work.

Every view is a plain CLI subcommand: text output, meaningful exit codes, no
GUI required. That puts stax exactly where a graphical profiler can't go —
over an SSH session to a remote machine, inside a CI job, or driven
end-to-end by an AI agent. There is a browser UI when you want one, but
nothing depends on it.

![A screenshot of stax showing a flamegraph on top, top-N functions bottom-left, and annotated disassembly bottom-right](https://github.com/user-attachments/assets/929b4b42-cdd9-4e35-8a91-ee7b029e94e2)

## Why stax

- **Live first, saveable when needed.** The aggregator updates continuously;
  `stax top`, `stax flame`, and the web UI all read the *current* state of a
  run that is still going. Use `stax select-run` or per-command `--run` for
  stopped in-memory history, and `stax save`, `stax open`, and `stax compare`
  when you need a durable artifact or before/after notes.
- **Built for agents as much as humans.** Every query is a subcommand with
  plain-text output and meaningful exit codes. `stax wait --for-samples N`
  lets a script block until there is enough data to look at.
- **On-CPU *and* off-CPU.** stax doesn't just show where the CPU time goes —
  it correlates scheduler events to show *why* a thread was blocked: lock,
  sleep, I/O, IPC.
- **Target/executor spans.** GPU, accelerator, executor, and runtime work
  reported through `stax-target` shows up in `threads`, `top`, `flame`, the
  timeline, and the web UI with explicit target time and span counts.
- **Down to the instruction.** `stax annotate` disassembles a hot function
  and attributes samples to individual instructions, interleaved with
  source.
- **Symbolicates stripped binaries.** On Linux, stax pulls symbols from local
  debug packages and debuginfod; on macOS, from the dyld shared cache — so
  system-library frames get real names.
- **DWARF-aware unwinding.** On Linux x86-64, stax recovers full call stacks
  from `.eh_frame` even when the target was built without frame pointers.
- **JIT-aware.** A JIT that emits a perf jitdump file gets its compiled
  functions symbolicated and disassembled like any other code.

## Documentation

- **Guide, concepts & reference**: <https://stax.bearcove.eu> — installing the
  daemons, recording and inspecting runs, platform support, stack unwinding
  (frame pointers vs. unwind tables), target-span integration, and
  programmatic usage.
- **Agent manual**: [AGENTS.md](AGENTS.md) — driving stax from an AI agent.

The site sources live in `docs/` and are built with
[dodeca](https://github.com/bearcove/dodeca) (`ddc serve` locally, deployed to
GitHub Pages on push to `main`).

## License

Licensed under either of

  * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
  * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
