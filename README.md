# stax

A live sampling profiler for macOS and Linux — flamegraphs, hot functions,
and annotated disassembly, streaming while your program runs.

```bash
# record a program (or attach to a running one with --pid)
stax record -- ./target/release/mybench

# from another shell — or from an AI agent — query the live run
stax wait --for-samples 10000     # block until data lands
stax top -n 10 --sort self        # hottest leaf functions
stax flame -d 6                   # on-CPU flamegraph, as a tree
stax annotate 'mycrate::hot_fn'   # per-instruction sample counts
```

stax records on-CPU and off-CPU stacks and turns them into flamegraphs,
top-N functions, per-thread breakdowns, and annotated disassembly — all
queryable *while the recording is still running*.

Every view is a plain CLI subcommand: text output, meaningful exit codes, no
GUI required. That puts stax exactly where a graphical profiler can't go —
over an SSH session to a remote machine, inside a CI job, or driven
end-to-end by an AI agent. There is a browser UI when you want one, but
nothing depends on it.

![A screenshot of stax showing a flamegraph on top, top-N functions bottom-left, and annotated disassembly bottom-right](https://github.com/user-attachments/assets/929b4b42-cdd9-4e35-8a91-ee7b029e94e2)

## Why stax

- **Live, not post-mortem.** There is no record-then-open step. The
  aggregator updates continuously; `stax top`, `stax flame`, and the web UI
  all read the *current* state of a run that is still going.
- **Built for agents as much as humans.** Every query is a subcommand with
  plain-text output and meaningful exit codes. `stax wait --for-samples N`
  lets a script block until there is enough data to look at.
- **On-CPU *and* off-CPU.** On macOS, stax correlates `kdebug` scheduler
  events, so it knows not just where the CPU time goes but *why* a thread
  was blocked — lock, sleep, I/O, IPC.
- **Down to the instruction.** `stax annotate` disassembles a hot function
  and attributes samples to individual instructions, interleaved with
  source.
- **JIT-aware.** A JIT that emits a perf jitdump file gets its compiled
  functions symbolicated and disassembled like any other code.

## Documentation

- **Guide, concepts & reference**: <https://stax.bearcove.eu> — installing the
  daemons, recording and inspecting runs, platform support, stack unwinding
  (frame pointers vs. unwind tables), and programmatic usage.
- **Agent manual**: [AGENTS.md](AGENTS.md) — driving stax from an AI agent.

The site sources live in `docs/` and are built with
[dodeca](https://github.com/bearcove/dodeca) (`ddc serve` locally, deployed to
GitHub Pages on push to `main`).

## License

Licensed under either of

  * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
  * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
