+++
title = "Environment Variables"
weight = 2
insert_anchor_links = "heading"
+++

Every environment variable stax reads at runtime, what it does, and what it
defaults to.

## Summary

| variable                  | read by                  | default                                   |
|---------------------------|--------------------------|-------------------------------------------|
| `STAX_SERVER_SOCKET`      | `stax`, `stax-server`    | *(see [socket resolution](#stax-server-socket))* |
| `STAX_SERVER_WS_BIND`     | `stax-server`            | `127.0.0.1:8080`                          |
| `STAX_CODESIGN_IDENTITY`  | `cargo xtask install`    | auto-detected                             |
| `STAX_DWARF_UNWIND`       | recorder (Linux)         | *(unset → auto-detect)*                   |
| `DEBUGINFOD_URLS`         | recorder (Linux)         | *(unset → debuginfod disabled)*           |
| `XDG_RUNTIME_DIR`         | `stax`, `stax-server`    | *(unset → falls back to `/tmp`)*          |
| `XDG_CACHE_HOME`          | recorder (Linux)         | *(unset → `~/.cache`)*                    |
| `RUST_LOG`                | all binaries             | *(see [logging](#rust-log))*              |

## `STAX_SERVER_SOCKET`

Overrides the path of `stax-server`'s local Unix domain socket — the one the
CLI and local RPC clients connect to.

When resolving the socket, stax tries, in order:

1. `STAX_SERVER_SOCKET`, if set **and the path exists**;
2. `$XDG_RUNTIME_DIR/stax-server.sock`, if `XDG_RUNTIME_DIR` is set and the
   path exists;
3. `/tmp/stax-server-$UID.sock`.

Keep the socket **outside** `~/Library/Group Containers`. A path inside an
app container triggers macOS `kTCCServiceSystemPolicyAppData` prompts even
for a correctly signed binary — see
[Architecture](@/concepts/architecture.md#the-two-sockets-and-a-tcc-footnote).

## `STAX_SERVER_WS_BIND`

The `host:port` `stax-server` binds its WebSocket to, for browser clients.

**Default: `127.0.0.1:8080`.**

```bash
STAX_SERVER_WS_BIND=127.0.0.1:9000 stax-server
```

To make it permanent, set it in the LaunchAgent plist's
`EnvironmentVariables`. Keep it on a loopback address — the WebSocket has no
authentication. See [The Web UI](@/guide/web-ui.md).

## `STAX_CODESIGN_IDENTITY`

The code-signing identity `cargo xtask install` uses when codesigning the
stax binaries on macOS.

By default the installer prefers a **Developer ID Application** identity,
then an **Apple Development** identity, from `security find-identity`. Set
this variable to a specific identity name or hash to override that choice:

```bash
STAX_CODESIGN_IDENTITY="Developer ID Application: …" cargo xtask install
```

Set it to `-` only when you explicitly want **ad-hoc** signing. See
[Getting Started](@/guide/getting-started.md).

## `STAX_DWARF_UNWIND`

**Linux only.** Controls `.eh_frame` DWARF unwinding of user stacks (x86-64).

By default stax auto-detects — it enables DWARF unwinding when the target
binary omits frame pointers. The override:

- `STAX_DWARF_UNWIND=0` — force DWARF unwinding **off**, even for a
  frame-pointer-less binary.
- The [`--dwarf-unwind`](@/reference/cli.md#stax-record) flag forces it
  **on**.

No effect on macOS or on aarch64. See
[Stack Unwinding](@/concepts/stack-unwinding.md).

## `DEBUGINFOD_URLS`

**Linux only.** A space- or semicolon-separated list of
[debuginfod](https://sourceware.org/elfutils/Debuginfod.html) server URLs.
When set, stax fetches missing symbols for stripped system libraries over
HTTPS, keyed by build-id.

stax also reads `*.urls` files under `/etc/debuginfod/` (the standard
location the `libdebuginfod-common` package populates). If neither source is
configured, debuginfod lookup is disabled and stax does no network I/O.

Fetched debug files are cached under
`$XDG_CACHE_HOME/stax/debuginfod/` (see `XDG_CACHE_HOME` below). See
[Symbolication](@/concepts/symbolication.md).

## `XDG_RUNTIME_DIR`

Not a stax-specific variable, but stax uses it to locate `stax-server`'s
socket (see [`STAX_SERVER_SOCKET`](#stax-server-socket) above). When it is
unset, stax falls back to `/tmp/stax-server-$UID.sock`.

## `XDG_CACHE_HOME`

Not stax-specific. On Linux, stax stores its
[debuginfod](#debuginfod-urls) cache under `$XDG_CACHE_HOME/stax/debuginfod/`,
falling back to `~/.cache/stax/debuginfod/` when unset. A cache hit makes
every session after the first one resolve symbols without network I/O.

## `RUST_LOG`

Standard `tracing` / `env_logger` filter, honored by every stax binary.

When `RUST_LOG` is unset, stax picks sensible defaults rather than going
silent:

- the **CLI** sets `info,cranelift_jit=warn,cranelift_codegen=warn` — the
  `cranelift_*` crates log every JIT'd function at `info`, which would flood
  the terminal, so they are turned down;
- **tracing** falls back to `info,stax=info,stax_vox_observe=info`.

Set `RUST_LOG` yourself to dig deeper:

```bash
RUST_LOG=debug,stax=trace stax record -- ./bench
```

The daemons route `tracing` through macOS unified logging — read it with the
`log` commands in [Troubleshooting](@/guide/troubleshooting.md#reading-the-logs).
