# Making JIT'd code show up in stax

By default a sampling profiler sees JIT'd code as `<unresolved>` /
`(no binary mapped at 0x…)`: the machine code lives in an anonymous
`mmap`, not in any Mach-O with a symbol table. stax already has the
machinery to fix this — it just needs the runtime to cooperate by
emitting a **perf jitdump** file. This note documents the contract so
any JIT (cranelift, a custom backend, V8, …) can light up in `stax top`,
`stax flame`, and `stax annotate`.

## How stax consumes it

- The runtime writes a growing stream of records to
  **`/tmp/jit-<pid>.dump`** (`<pid>` = the profiled process's PID).
- stax's DYLD-insert preload notices the target `open()` that path and
  tells the recorder.
- `stax-mac-kperf-parse`'s `JitdumpTailer`
  (`stax-mac-kperf-parse/src/jitdump_tail.rs`) opens it, parses the
  40-byte global header, and on every tick reads newly-appended
  `JIT_CODE_LOAD` records, emitting a synthetic `BinaryLoadedEvent` per
  function. From then on that address range resolves to the name you
  gave it, and `stax annotate` can disassemble it because the record
  **carries the code bytes** (no `task_for_pid` / `mach_vm_read`
  needed).

So: emit the file, emit one `JIT_CODE_LOAD` per compiled function, and
keep appending. Partial trailing records are fine — the tailer only
consumes whole records and re-reads from the boundary next tick.

## The format

Reference: `linux/tools/perf/Documentation/jitdump-specification.txt`.
All integers little-endian on aarch64/x86_64 (stax accepts the magic in
either endianness and infers).

### Global header — 40 bytes, written once at file creation

| field        | type | value                                   |
|--------------|------|-----------------------------------------|
| `magic`      | u32  | `0x4A695444` ("JiTD"), host-endian      |
| `version`    | u32  | `1`                                     |
| `total_size` | u32  | `40` (size of this header)              |
| `elf_mach`   | u32  | `EM_AARCH64` = 183 (stax ignores it)    |
| `pad1`       | u32  | `0`                                     |
| `pid`        | u32  | `getpid()`                              |
| `timestamp`  | u64  | any monotonic value                     |
| `flags`      | u64  | `0`                                     |

### Record prefix — 16 bytes, every record

| field        | type | value                                   |
|--------------|------|-----------------------------------------|
| `id`         | u32  | `0` = `JIT_CODE_LOAD` (only one stax surfaces today) |
| `total_size` | u32  | prefix + body, **including** name and code |
| `timestamp`  | u64  | monotonic; ordering only                |

### `JIT_CODE_LOAD` body

| field        | type            | value                                  |
|--------------|-----------------|----------------------------------------|
| `pid`        | u32             | `getpid()`                             |
| `tid`        | u32             | thread id (0 is fine)                  |
| `vma`        | u64             | load address — **stax keys on this**   |
| `code_addr`  | u64             | same as `vma` for JIT                   |
| `code_size`  | u64             | length of the machine code             |
| `code_index` | u64             | per-process incrementing counter       |
| `name`       | char[]          | NUL-terminated, free-form UTF-8         |
| `native_code`| u8[`code_size`] | the actual bytes                       |

`total_size = 16 + 40 + name.len() + 1 + code_size`.

## Gotchas learned the hard way

- **Dump the *finalized* bytes, not the assembler buffer.** With
  cranelift, `CompiledCode::code_buffer()` gives the right *length*, but
  copy the bytes from the address `JITModule::get_finalized_function`
  returns — those have relocations applied, so `stax annotate` shows
  real `bl` targets instead of zeroed call slots.
- **Path is keyed by the *target's* PID.** If you launch under
  `stax record -- env FOO=bar ./app`, `env` `exec`s `app` so the PID is
  stable — `/tmp/jit-<that pid>.dump` is correct. Use
  `std::process::id()`, not a parent.
- **Flush after every record.** The tailer polls; an unflushed record is
  just a function that never gets a name.
- **One record per function is enough.** You don't need `CODE_CLOSE` /
  unwind / debug-info records; stax skips unknown ids silently.
- **Truncate on create.** A stale dump from a previous run with
  recycled addresses will mis-symbolicate.

## Minimal producer (Rust, what dwarf-json does)

```rust
// once, at file creation: write the 40-byte header above.
// per compiled function, append:
fn register(name: &str, addr: u64, code: &[u8]) {
    let total = 16 + 40 + name.len() + 1 + code.len();
    let mut r = Vec::with_capacity(total);
    r.extend_from_slice(&0u32.to_ne_bytes());            // id = JIT_CODE_LOAD
    r.extend_from_slice(&(total as u32).to_ne_bytes());
    r.extend_from_slice(&ts.to_ne_bytes());              // timestamp
    r.extend_from_slice(&pid.to_ne_bytes());
    r.extend_from_slice(&0u32.to_ne_bytes());            // tid
    r.extend_from_slice(&addr.to_ne_bytes());            // vma
    r.extend_from_slice(&addr.to_ne_bytes());            // code_addr
    r.extend_from_slice(&(code.len() as u64).to_ne_bytes());
    r.extend_from_slice(&index.to_ne_bytes());           // code_index
    r.extend_from_slice(name.as_bytes());
    r.push(0);
    r.extend_from_slice(code);
    file.write_all(&r).and_then(|_| file.flush()).ok();
}
```

Then:

```
stax record -- ./your-jit-app
stax wait --for-samples 40000
stax top -n 10 --sort self          # JIT fns now show by name
stax annotate 'my_jit::func'        # per-instruction sample counts
```

(Working reference implementation: `dwarf-json`'s `src/jitdump.rs` +
the `register(...)` call in `src/jit.rs`.)
