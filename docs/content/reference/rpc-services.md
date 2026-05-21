+++
title = "Programmatic Usage"
weight = 1
insert_anchor_links = "heading"
+++

The `stax` CLI and the browser UI are both just clients of `stax-server`.
Anything they do, your own code can do — by speaking the same
[vox](https://crates.io/crates/vox) RPC services directly. This page is the
programmatic surface.

## When to skip the CLI

The CLI is the right tool for one-shot commands and scripting. Talk to the
RPC layer directly when you want to:

- **stream** live updates instead of polling — a custom dashboard, an
  editor integration, a CI gate that reacts as samples arrive;
- **drive stax from another language** — the WebSocket transport plus
  generated TypeScript types is a complete client;
- **embed** run control into a larger tool.

## The three services

All three are defined in the **`stax-live-proto`** crate.

### `RunControl`

Run lifecycle — the surface behind `stax status`, `list`, `wait`, `stop`.

| method         | purpose                                          |
|----------------|--------------------------------------------------|
| `status`       | the daemon's current state + active run          |
| `list_runs`    | every run hosted, active and history             |
| `wait_active`  | block until a `WaitCondition` fires or the run stops |
| `stop_active`  | stop the active run cleanly                      |

### `Profiler`

The query surface — the live aggregator. Methods come in two shapes:

- **one-shot** — e.g. `top` returns a single snapshot;
- **`subscribe_*`** — push periodic updates over a `vox::Tx<…>` channel for
  as long as you hold it: `subscribe_top`, `subscribe_flamegraph`,
  `subscribe_annotated`, `subscribe_neighbors`, `subscribe_threads`,
  `subscribe_timeline`, and more.

The `subscribe_*` variants are how the web UI stays live without polling —
each panel holds one subscription and re-renders when an update arrives.

### `RunIngest`

The recorder-side ingest path: `start_run` opens a run and takes an
`Rx<IngestEvent>` channel that the recorder feeds samples into. This is an
internal service between the recording task and the registry — clients
querying or controlling runs do not need it.

## Connecting

`stax-server` listens on two transports simultaneously. Pick by trust level:

```text
local://$XDG_RUNTIME_DIR/stax-server.sock     # or /tmp/stax-server-$UID.sock
ws://127.0.0.1:8080
```

- **`local://…`** — a Unix domain socket, for trusted local clients on the
  same machine: the CLI, local agents, scripts. The socket path resolution
  matches [`STAX_SERVER_SOCKET`](@/reference/environment-variables.md):
  the env override, then `$XDG_RUNTIME_DIR/stax-server.sock`, then
  `/tmp/stax-server-$UID.sock`.
- **`ws://127.0.0.1:8080`** — the WebSocket, for browser clients. Override
  the bind with [`STAX_SERVER_WS_BIND`](@/reference/environment-variables.md).
  There is no authentication; keep it bound to loopback.

Both transports speak the same three services — the only difference is who
can reach them.

## Rust clients

`stax-live-proto` exports a generated client per service —
`RunControlClient` and `ProfilerClient` — alongside the shared types
(`ServerStatus`, `RunSummary`, `WaitCondition`, `WaitOutcome`, `TopSort`,
`ViewParams`, `FlamegraphUpdate`, `FlameNode`, `ThreadsUpdate`,
`OffCpuBreakdown`, `DiagnosticsSnapshot`, …). The `stax` CLI is itself a
straightforward consumer of these clients — `cli/src/main.rs` is the
worked example to read.

## TypeScript bindings

vox can generate TypeScript types for an RPC protocol, so a browser or
Node client gets the exact same shapes the Rust side uses — no hand-written
interfaces, no drift.

```bash
cargo xtask codegen
```

This regenerates the TypeScript bindings for `stax-live` into
`frontend/src/generated/`. Combined with the WebSocket transport, that is a
complete, typed client for the `Profiler` and `RunControl` services.

> **Generated code is generated.** Do not hand-edit anything under
> `frontend/src/generated/` — change the Rust protocol in `stax-live-proto`
> and re-run `cargo xtask codegen`.

## See also

- [Architecture](@/concepts/architecture.md) — where `stax-server` and its
  sockets sit.
- [The Web UI](@/guide/web-ui.md) — the reference browser client.
- [Environment Variables](@/reference/environment-variables.md) — socket and
  bind overrides.
