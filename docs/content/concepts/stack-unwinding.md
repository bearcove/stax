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

## Two ways to walk a stack

When a thread is paused mid-execution, its stack is a wall of bytes: saved
registers, locals, spilled values, return addresses, all interleaved. The
return addresses are in there — but nothing in the raw bytes says *which*
words they are. There are two ways to find them.

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
is far more expensive: you need the unwind tables *and* a copy of the
thread's stack memory, and the walk is an interpreter, not a pointer chase.
So it is done by **copying the raw stack at sample time and unwinding it
afterwards**, off the hot path.

## What stax does

stax uses **frame pointers** wherever they are present, and on x86-64 Linux
additionally runs **`.eh_frame` DWARF unwinding** — on by default — to
recover the chains that frame-pointer-less code would otherwise truncate.

### macOS — frame pointers, in-kernel

`kperf`'s sampler walks the frame-pointer chain in the kernel and hands stax
a finished backtrace — user *and* kernel — per tick. stax does no unwinding
of its own. There is no DWARF or compact-unwind path on macOS; stax sees
exactly the frames the frame-pointer chain exposes.

This is reliable on **Apple Silicon** because Apple's ARM64 ABI *requires* a
chained frame pointer in `x29` — system libraries and well-behaved code
already have it.

### Linux — frame pointers, plus DWARF unwinding

By default the kernel walks the frame-pointer chain (`PERF_SAMPLE_CALLCHAIN`)
and the sample arrives with the call chain attached. On **aarch64** that is
the whole story — AArch64's ABI keeps `x29` as a frame pointer in practice,
so the chain is intact.

On **x86-64**, where there is no such ABI guarantee, stax adds a second path:
userspace **`.eh_frame` DWARF unwinding** with [framehop](https://docs.rs/framehop).
When it is active, each sample also carries the user registers and an 8 KiB
snapshot of the thread's stack; stax replays the unwind against each loaded
image's `.eh_frame` CFI — recovering the full chain through
frame-pointer-less code that the kernel walker would truncate.

**On by default.** Every mainstream x86-64 Linux distribution ships `libc`
built `-fomit-frame-pointer`, so the kernel's frame-pointer walk truncates
the moment a sample lands in `libc` — which covers most malloc/IO/syscall-
heavy workloads, whatever your own binary was built with. DWARF unwinding is
therefore on by default on x86-64 Linux. Opt out when you don't need it:

```bash
stax record --no-dwarf-unwind -- ./mybench     # off for this run
STAX_DWARF_UNWIND=0 stax record -- ./mybench   # same, via the environment
```

DWARF unwinding costs a per-sample register + 8 KiB stack copy. It is
**x86-64 Linux only** — `--no-dwarf-unwind` is a no-op on macOS and on
aarch64, where the frame-pointer chain already suffices.

## Build with frame pointers anyway

DWARF unwinding rescues binaries you *cannot* rebuild — distro `libc`,
`libstdc++`, a release artifact from someone else. For code you control,
frame pointers are still the better default: they are cheaper at record time
(no per-sample stack copy), and they are the **only** mechanism on macOS.

If a function was compiled **without** a frame pointer and DWARF unwinding is
not in play, it is not a node in the chain. The walker skips past it — or the
chain breaks and the backtrace ends there. That shows up as:

- **Shallow flamegraphs** — stacks bottom out far earlier than the real call
  depth.
- **Missing callers** — a hot leaf still appears in
  [`stax top --sort self`](@/guide/inspecting-a-run.md#stax-top), but the
  functions that *called* it are gone, so `--sort total` and
  [`stax flame`](@/guide/inspecting-a-run.md#stax-flame) are degraded.

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
tables for source-interleaved
[`annotate`](@/guide/inspecting-a-run.md#stax-annotate) output. stax's own
workspace already sets `[profile.release] debug = 1`.

### C and C++

```text
-fno-omit-frame-pointer
```

Pass it to every translation unit you want to see in a backtrace.

### Platform defaults

- **Apple Silicon (`aarch64`).** Apple's ARM64 ABI requires a chained frame
  pointer; you still need to enable it for your *own* optimized build.
- **aarch64 Linux.** The ABI keeps `x29` in practice — frame-pointer
  unwinding works without DWARF.
- **x86-64.** No ABI guarantee. On Linux, stax's DWARF fallback covers it;
  on macOS x86-64, frame pointers are the only option.

## JIT'd code

A JIT emits machine code at runtime, so *it* decides whether to set up a
frame pointer. If you want JIT'd functions to have callers in a stax
backtrace, have the JIT emit the frame-pointer prologue/epilogue. Naming the
code is a separate step — see [Profiling JIT Code](@/guide/profiling-jit-code.md).

## See also

- [Platform Support](@/concepts/platform-support.md) — the per-platform
  capture backends.
- [Symbolication](@/concepts/symbolication.md) — turning the unwound
  addresses into function names.
- [Sampling](@/concepts/sampling.md) — what each sample measures.
