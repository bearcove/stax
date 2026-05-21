+++
title = "Symbolication"
weight = 4
insert_anchor_links = "heading"
+++

A raw sample is an address — `0x7f3a14002d40`. A useful profile shows a
name — `tokio::runtime::poll`. Turning the first into the second is
*symbolication*, and on a real system it takes more than one source of
truth: your own binary, stripped system libraries, the kernel, JIT'd code.
This page is how stax does it.

## The resolution chain

For any sampled address, stax works through a chain until something resolves
it:

1. **The mapped image's own symbol table.** Every loaded binary is tracked
   by its address range. stax reads its symbol table — Mach-O `LC_SYMTAB` on
   macOS, ELF `.symtab` + `.dynsym` on Linux — and binary-searches the
   sampled address against it. For code you compiled, this is usually the end
   of the story.
2. **Separate debug info** (Linux). System libraries ship *stripped*, so step
   1 finds nothing. stax then looks for detached debug files — see below.
3. **The dyld shared cache** (macOS). System library code on Apple Silicon
   doesn't exist as standalone files; its bytes and symbols live only in the
   shared cache, which stax maps and queries.
4. **Kernel symbols** for kernel-space addresses — see below.
5. **JIT records** for addresses in JIT'd code — see below.

If nothing resolves an address, stax falls back to `binary+0xoffset` so the
frame is at least attributable to a module.

## Stripped binaries on Linux

Linux distributions strip the symbol tables out of system libraries —
`libc.so.6`, `libstdc++.so.6`, `ld-linux.so` — and ship the symbols
separately, if at all. stax recovers them two ways, both keyed by the
library's **GNU build-id** (a hash baked into the ELF that uniquely
identifies the exact build).

### Local separate-debug files

The cross-distro convention is a detached `.debug` file at:

```text
/usr/lib/debug/.build-id/XX/YYYYYYYY….debug
```

…where `XX` is the first byte of the build-id in hex and the rest of the
filename is the remaining hex digits. These are installed by a library's
`-dbg` / `-debuginfo` / `-debugsource` package. When that package is present,
stax finds the file, parses its symbol table, and merges it into the
stripped image. One `stat` when it's missing — cheap either way.

### debuginfod

When the debug package *isn't* installed, stax can fetch the symbols over
the network from a **debuginfod** server — the same protocol `gdb` and
`perf` use. stax reads the standard configuration:

- the **`DEBUGINFOD_URLS`** environment variable (space- or
  semicolon-separated), and
- every `*.urls` file under **`/etc/debuginfod/`** (the Debian
  `libdebuginfod-common` package drops one there).

For each stripped image it issues an HTTPS `GET <server>/buildid/<hex>/debuginfo`.
Results are cached on disk under `$XDG_CACHE_HOME/stax/debuginfod/` (or
`~/.cache/stax/debuginfod/`), so the first session pays the network latency
and every later one is a local read. Misses are negative-cached too, so a
build-id that no server has is asked about exactly once.

If neither `DEBUGINFOD_URLS` nor `/etc/debuginfod/` is configured, stax skips
the network entirely — debuginfod is opt-in, by your environment.

> debuginfod and separate-debug files are a Linux feature. On macOS the
> equivalent — symbols for system libraries — comes from the dyld shared
> cache, which is always present.

## Kernel symbols

Kernel-space addresses are resolved separately:

- **macOS** — stax reads the on-disk kernel collection, building it with
  `kmutil` if needed, and estimates the KASLR slide from the kernel
  addresses it actually samples.
- **Linux** — stax reads `/proc/kallsyms`.

Either way, kernel frames in a flamegraph or `stax top` get real names, not
bare addresses.

## JIT'd code

JIT'd functions have no binary on disk at all. A JIT that emits a
[perf jitdump](@/guide/profiling-jit-code.md) file gives stax both the name
*and* the machine-code bytes for each compiled function — so JIT'd frames
symbolicate, and `stax annotate` can even disassemble them. (jitdump
tailing currently runs on macOS.)

## Demangling

Rust and C++ encode type and module information into mangled linker symbols
(`_ZN5tokio7runtime…`). stax demangles them into readable names
(`tokio::runtime::…`) for every view — `top`, `flame`, `annotate`, and the
web UI alike.

## Source and line numbers

Symbolication gives you a function name; **DWARF line tables** give you the
exact source file and line. When a binary carries debug info, stax reads its
line table and uses it to interleave source into
[`stax annotate`](@/guide/inspecting-a-run.md#stax-annotate) — each block of
disassembly is headed by the source line it came from. Rust's
`/rustc/<commit>/…` standard-library paths are remapped to your local
toolchain's `rust-src`, so standard-library frames show real source too.

This is why stax wants `debug = 1` (or higher) in your release profile: with
no line tables you still get function-level names, just not source.

## See also

- [Stack Unwinding](@/concepts/stack-unwinding.md) — producing the addresses
  that get symbolicated.
- [Platform Support](@/concepts/platform-support.md) — which symbol sources
  apply where.
- [Profiling JIT Code](@/guide/profiling-jit-code.md) — the jitdump contract.
- [Environment Variables](@/reference/environment-variables.md) —
  `DEBUGINFOD_URLS` and the cache location.
