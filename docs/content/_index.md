+++
title = "stax"
insert_anchor_links = "heading"
+++

<div class="hero">

# stax

<p class="hero-tagline">A live profiler for macOS and Linux — CPU stacks, off-CPU waits, cooperating target spans, and annotated disassembly while your program runs.</p>

</div>

```bash
# record a program (or attach to a running one with --pid)
stax record -- ./target/release/mybench

# from another shell — or to an AI agent — query the live run
stax wait --for-samples 10000     # block until data lands
stax top -n 10 --sort self        # hottest leaf functions or target spans
stax flame -d 6                   # active flamegraph, as a tree
stax annotate 'mycrate::hot_fn'   # per-instruction sample counts
```

stax records on-CPU stacks, off-CPU waits, and cooperating target spans, then
turns them into flamegraphs, top-N functions/spans, per-thread and per-lane
breakdowns, and annotated disassembly — all queryable *while the recording is
still running*.

Every view is a plain CLI subcommand: text output, meaningful exit codes, no
GUI required. That puts stax exactly where a graphical profiler can't go —
over an SSH session to a remote machine, inside a CI job, or driven
end-to-end by an AI agent. There is a browser UI when you want one, but
nothing depends on it.

## Choose your path

<div class="guide-cards">
<a class="guide-card" href="/guide">
  <div class="guide-card__icon"><img src="/icons/guide.svg" alt="" loading="lazy"></div>
  <h3 id="guide">Guide</h3>
  <p class="tagline">Learn stax step by step</p>
  <p class="description">Install the daemons, record your first run, read flamegraphs, profile JIT'd code, and troubleshoot when something goes wrong.</p>
</a>
<a class="guide-card" href="/concepts">
  <div class="guide-card__icon"><img src="/icons/concepts.svg" alt="" loading="lazy"></div>
  <h3 id="concepts">Concepts</h3>
  <p class="tagline">Understand how it works</p>
  <p class="description">The three-process architecture, what each platform can capture, how stacks get unwound, and what sampling actually measures.</p>
</a>
<a class="guide-card" href="/reference">
  <div class="guide-card__icon"><img src="/icons/reference.svg" alt="" loading="lazy"></div>
  <h3 id="reference">Reference</h3>
  <p class="tagline">Look it up fast</p>
  <p class="description">Every subcommand and flag, the RPC services for programmatic clients, environment variables, and exit codes.</p>
</a>
</div>

## Why stax

- **Live first, saveable when needed.** The aggregator updates continuously;
  `stax top`, `stax flame`, and the web UI all read the *current* state of a
  run that is still going. Use `stax select-run` or per-command `--run` for
  stopped in-memory history, and `stax save`, `stax open`, `stax compare`,
  and `stax compare --json` when you need durable artifacts, before/after
  notes, or CI-readable deltas.
- **Built for agents as much as humans.** Every query is a subcommand with
  plain-text output and meaningful exit codes. `stax wait --for-samples N`
  lets a script block until there's enough data to look at.
- **On-CPU *and* off-CPU.** stax doesn't just show where the CPU time goes —
  it correlates scheduler events to show *why* a thread was blocked: lock,
  sleep, I/O, IPC.
- **Cooperating target spans.** GPU, accelerator, and executor work reported
  through `stax-target` lands on the same timeline as synthetic lanes, with
  explicit target time/span counts in `threads`, `top`, `flame`, and the web
  UI.
- **Down to the instruction.** `stax annotate` disassembles a hot function and
  attributes samples to individual instructions, interleaved with source.
- **Symbolicates stripped binaries.** On Linux, stax pulls symbols from local
  debug packages and [debuginfod](@/concepts/symbolication.md); on macOS, from
  the dyld shared cache — so system-library frames get real names.
- **JIT-aware.** A JIT that emits a [perf jitdump](@/guide/profiling-jit-code.md)
  file gets its compiled functions symbolicated and disassembled like any
  other code.

## Quick links

- [Getting Started](@/guide/getting-started.md) — install the daemons and verify
- [Recording a Run](@/guide/recording.md) — launch a target or attach to a PID
- [Architecture](@/concepts/architecture.md) — `stax`, `stax-server`, `staxd`
- [Stack Unwinding](@/concepts/stack-unwinding.md) — frame pointers, DWARF, and what your build needs
- [CLI Reference](@/reference/cli.md) — every subcommand and flag
- [GitHub](https://github.com/bearcove/stax) — source and issues
