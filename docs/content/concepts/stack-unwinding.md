+++
title = "Stack Unwinding"
weight = 2
insert_anchor_links = "heading"
+++

A sample is a single number: the program counter, the address of the one
instruction a thread was executing when the timer fired. But a flamegraph
needs the whole **call stack** — the chain of callers above that
instruction. Turning one PC into a backtrace is *stack unwinding*, and how
stax does it has a direct, practical consequence for how you build the code
you profile.

**The short version: build with frame pointers, or your profile will have no
call stacks.**

## Two ways to walk a stack

When a thread is paused mid-execution, its stack is a wall of bytes:
saved registers, locals, spilled values, return addresses, all interleaved.
The return addresses are in there — but nothing in the raw bytes says
*which* words they are. There are two ways to find them.

### Frame pointers

By convention, a function can dedicate one register — `rbp` on x86-64,
`x29` on AArch64 — to point at its stack frame. On entry it pushes the
*caller's* frame pointer, then sets its own. The result is a linked list
threaded through the stack:

```text
   [ frame pointer ] ─────► [ caller's frame pointer ] ─────► …
   [ return address ]       [ caller's return address ]
   [ locals … ]             [ locals … ]
```

Unwinding is then trivial: read the frame-pointer register, and at each node
the word next to it is a return address and the word it points to is the
next node. Walk until the chain ends. It is a handful of memory reads per
frame, needs no metadata, and is cheap enough to do **in the kernel, at
sample time**.

### Unwind tables

The alternative is to *omit* the frame pointer — freeing that register for
general use, and saving the push/set on every call — and instead ship
metadata that describes, for every instruction range, how to restore the
caller's registers. This is DWARF CFI (`.eh_frame`) on Linux and *compact
unwind* (`__unwind_info`) in Mach-O.

Table-based unwinding is precise and works on frame-pointer-less code, but it
is far more expensive: you need the unwind tables *and* a copy of the thread's
stack memory, and the walk is an interpreter, not a pointer chase. Profilers
that use it generally **copy the raw stack at sample time and unwind later**,
off the hot path.

## What stax does today

**stax unwinds with frame pointers, on both platforms.**

- **macOS** — `kperf`'s PET sampler walks the frame-pointer chain in the
  kernel and hands stax a finished backtrace per tick.
- **Linux** — `perf_event_open` is opened with `PERF_SAMPLE_CALLCHAIN`; the
  kernel walks the frame-pointer chain and the sample arrives with the
  call chain already attached.

stax does **not** do DWARF / compact-unwind table unwinding at this time.
That means it never copies the target's stack memory and never pays the
unwinder-interpreter cost — recording stays light — but it also means stax
sees exactly the frames the frame-pointer chain exposes, and no more.

## The consequence: build with frame pointers

If a function was compiled **without** a frame pointer, it is not a node in
the chain. The walker skips straight past it — or, worse, the chain breaks
and the backtrace simply ends there.

In practice that shows up as:

- **Shallow flamegraphs.** Stacks bottom out far earlier than the code's
  real call depth.
- **Missing callers.** A hot leaf still appears in
  [`stax top --sort self`](@/guide/inspecting-a-run.md#stax-top) — its PC was
  sampled directly — but the functions that *called* it are gone, so
  `--sort total` and [`stax flame`](@/guide/inspecting-a-run.md#stax-flame)
  are degraded or wrong.
- **Stacks that "teleport".** A frame-pointer-less frame in the middle gets
  silently dropped, so a callee appears to be called directly by its
  grandparent.

If your flamegraphs look suspiciously flat, missing frame pointers is the
first thing to check.

### Rust

Optimized Rust builds may omit the frame pointer. Force it on for the whole
build:

```bash
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release
```

Or make it permanent in `.cargo/config.toml`:

```toml
[build]
rustflags = ["-C", "force-frame-pointers=yes"]
```

Keep `debug = 1` (or higher) in your release profile too — stax wants line
tables for source-interleaved [`annotate`](@/guide/inspecting-a-run.md#stax-annotate)
output. stax's own workspace already sets `[profile.release] debug = 1`.

### C and C++

```text
-fno-omit-frame-pointer
```

Pass it to every translation unit you want to see in a backtrace — including
the hot dependencies, not just your own code.

### Platform defaults

- **Apple Silicon (`aarch64`).** Apple's ARM64 ABI *requires* a chained
  frame pointer in `x29`. System libraries and well-behaved code already
  have it — which is why frame-pointer unwinding is reliable on Apple
  Silicon. You still need to enable it for your *own* optimized build.
- **x86-64.** There is no ABI guarantee. Whether a given binary — yours, a
  dependency, a system library — has frame pointers depends entirely on how
  it was compiled. Recent Linux distributions have begun re-enabling
  frame pointers across their package sets, but you cannot assume it.

## JIT'd code

A JIT emits machine code at runtime, so *it* decides whether to set up a
frame pointer in the code it generates. If you want JIT'd functions to have
callers in a stax backtrace, have the JIT emit the frame-pointer
prologue/epilogue. Naming the code is a separate step — see
[Profiling JIT Code](@/guide/profiling-jit-code.md).

## On the roadmap

Table-based unwinding — capturing the raw stack at sample time and unwinding
it afterward against `.eh_frame` / `__unwind_info` — is a planned addition.
It would let stax profile frame-pointer-less binaries you cannot rebuild.
Until it lands, the rule stands: **build the code you profile with frame
pointers.**

## See also

- [Sampling](@/concepts/sampling.md) — what each sample measures.
- [Platform Support](@/concepts/platform-support.md) — the per-platform
  capture backends.
- [Inspecting a Run](@/guide/inspecting-a-run.md) — the views that degrade
  when frames are missing.
