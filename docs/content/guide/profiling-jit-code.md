+++
title = "Profiling JIT Code"
weight = 7
insert_anchor_links = "heading"
+++

By default a sampling profiler sees JIT'd code as `<unresolved>` —
`(no binary mapped at 0x…)`. The machine code lives in an anonymous `mmap`,
not in any Mach-O or ELF with a symbol table. stax already has the machinery
to fix this; it just needs the JIT runtime to cooperate by emitting a
**perf jitdump** file.

This page is the contract. Any JIT — Cranelift, a custom backend, V8, the
JVM — that follows it will light up in `stax top`, `stax flame`, and
`stax annotate`.

> **Platform.** jitdump tailing currently runs on the macOS capture backend.
> The contract below is platform-neutral — a runtime that emits the file is
> ready for jitdump support wherever stax surfaces it.

## How stax consumes a jitdump

- The runtime writes a growing stream of records to **`/tmp/jit-<pid>.dump`**,
  where `<pid>` is the *profiled process's* PID.
- stax watches for that path to appear and, once it does, opens a tailer.
- The tailer parses the 40-byte global header and, on every tick, reads
  newly-appended `JIT_CODE_LOAD` records — emitting a synthetic binary-load
  event per compiled function.

From then on, that address range resolves to the name you gave it. Because
each record **carries the code bytes**, `stax annotate` can disassemble JIT'd
functions too — no `task_for_pid` / memory read needed.

So the whole job is: emit the file, emit one `JIT_CODE_LOAD` per compiled
function, and keep appending.

## The file format

Reference: perf's own `jitdump-specification.txt`. All integers are
little-endian on `aarch64` / `x86_64`; stax accepts the magic in either
endianness and infers.

### Global header — 40 bytes, written once

| field        | type | value                                |
|--------------|------|--------------------------------------|
| `magic`      | u32  | `0x4A695444` (`"JiTD"`), host-endian |
| `version`    | u32  | `1`                                  |
| `total_size` | u32  | `40` (size of this header)           |
| `elf_mach`   | u32  | `EM_AARCH64` = 183 (stax ignores it) |
| `pad1`       | u32  | `0`                                  |
| `pid`        | u32  | `getpid()`                           |
| `timestamp`  | u64  | any monotonic value                  |
| `flags`      | u64  | `0`                                  |

### Record prefix — 16 bytes, every record

| field        | type | value                                  |
|--------------|------|----------------------------------------|
| `id`         | u32  | `0` = `JIT_CODE_LOAD`                   |
| `total_size` | u32  | prefix + body, **including** name and code |
| `timestamp`  | u64  | monotonic; ordering only               |

### `JIT_CODE_LOAD` body

| field         | type            | value                                |
|---------------|-----------------|--------------------------------------|
| `pid`         | u32             | `getpid()`                           |
| `tid`         | u32             | thread id (`0` is fine)              |
| `vma`         | u64             | load address — **stax keys on this** |
| `code_addr`   | u64             | same as `vma` for a JIT              |
| `code_size`   | u64             | length of the machine code           |
| `code_index`  | u64             | per-process incrementing counter     |
| `name`        | char[]          | NUL-terminated, free-form UTF-8      |
| `native_code` | u8[`code_size`] | the actual bytes                     |

`total_size = 16 + 40 + name.len() + 1 + code_size`.

stax surfaces only `JIT_CODE_LOAD` (`id = 0`) today, and skips records with
any other `id` silently — you do not need `CODE_CLOSE`, unwind, or
debug-info records.

## A minimal producer (Rust)

```rust,noexec
// Once, at file creation: write the 40-byte global header above.
// Per compiled function, append one JIT_CODE_LOAD record:
fn register(file: &mut std::fs::File, name: &str, addr: u64, code: &[u8]) {
    use std::io::Write;
    let total = 16 + 40 + name.len() + 1 + code.len();
    let mut r = Vec::with_capacity(total);
    r.extend_from_slice(&0u32.to_ne_bytes());             // id = JIT_CODE_LOAD
    r.extend_from_slice(&(total as u32).to_ne_bytes());   // total_size
    r.extend_from_slice(&timestamp().to_ne_bytes());      // timestamp
    r.extend_from_slice(&pid().to_ne_bytes());            // pid
    r.extend_from_slice(&0u32.to_ne_bytes());             // tid
    r.extend_from_slice(&addr.to_ne_bytes());             // vma
    r.extend_from_slice(&addr.to_ne_bytes());             // code_addr
    r.extend_from_slice(&(code.len() as u64).to_ne_bytes());
    r.extend_from_slice(&index().to_ne_bytes());          // code_index
    r.extend_from_slice(name.as_bytes());
    r.push(0);                                            // NUL terminator
    r.extend_from_slice(code);                            // native_code
    file.write_all(&r).and_then(|_| file.flush()).ok();
}
```

Then profile it like anything else:

```bash
stax record -- ./your-jit-app
stax wait --for-samples 40000
stax top -n 10 --sort self      # JIT functions now show by name
stax annotate 'my_jit::func'    # per-instruction sample counts
```

## Gotchas learned the hard way

- **Dump the *finalized* bytes, not the assembler buffer.** With Cranelift,
  `CompiledCode::code_buffer()` gives the right *length*, but copy the bytes
  from the address `JITModule::get_finalized_function` returns — those have
  relocations applied, so `stax annotate` shows real `bl` / `call` targets
  instead of zeroed call slots.
- **The path is keyed by the *target's* PID.** Use `std::process::id()`, not
  a parent's PID. If you launch under `stax record -- env FOO=bar ./app`,
  `env` `exec`s `app`, so the PID is stable.
- **Flush after every record.** The tailer polls; an unflushed record is just
  a function that never gets a name. Partial trailing records are fine — the
  tailer only consumes whole records and re-reads from the boundary.
- **Truncate on create.** A stale dump from a previous run with recycled
  addresses will mis-symbolicate.
- **One record per function is enough.** No `CODE_CLOSE`, no unwind tables.

## See also

- [Stack Unwinding](@/concepts/stack-unwinding.md) — JIT'd code still needs
  frame pointers to appear in a backtrace at all.
- [Inspecting a Run](@/guide/inspecting-a-run.md) — what `top`, `flame`, and
  `annotate` show once your functions are named.
