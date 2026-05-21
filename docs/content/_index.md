+++
title = "stax"
insert_anchor_links = "heading"
+++

<div class="hero">

# stax

<p class="hero-tagline">A live sampling profiler for macOS and Linux — flamegraphs, hot functions, and annotated disassembly, streaming while your program runs.</p>

</div>

```bash
# record a program (or attach to a running one with --pid)
stax record -- ./target/release/mybench

# from another shell — or to an AI agent — query the live run
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

- **Live, not post-mortem.** There is no record-then-open step. The aggregator
  updates continuously; `stax top`, `stax flame`, and the web UI all read the
  *current* state of a run that is still going.
- **Built for agents as much as humans.** Every query is a subcommand with
  plain-text output and meaningful exit codes. `stax wait --for-samples N`
  lets a script block until there's enough data to look at.
- **On-CPU *and* off-CPU.** On macOS, stax correlates `kdebug` scheduler
  events, so it knows not just where the CPU time goes but *why* a thread was
  blocked — lock, sleep, I/O, IPC.
- **Down to the instruction.** `stax annotate` disassembles a hot function and
  attributes samples to individual instructions, interleaved with source.
- **JIT-aware.** A JIT that emits a [perf jitdump](@/guide/profiling-jit-code.md)
  file gets its compiled functions symbolicated and disassembled like any
  other code.

## Quick links

- [Getting Started](@/guide/getting-started.md) — install the daemons and verify
- [Recording a Run](@/guide/recording.md) — launch a target or attach to a PID
- [Architecture](@/concepts/architecture.md) — `stax`, `stax-server`, `staxd`
- [Stack Unwinding](@/concepts/stack-unwinding.md) — why your build needs frame pointers
- [CLI Reference](@/reference/cli.md) — every subcommand and flag
- [GitHub](https://github.com/bearcove/stax) — source and issues
