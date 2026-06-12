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

## The two services

Both are defined in the **`stax-live-proto`** crate and exposed by
`stax-server` on **both** transports (the local socket and the WebSocket).

### `RunControl`

The lifecycle and control plane — the surface behind `stax status`, `list`,
`diagnose`, `wait`, `stop`, and the start of `stax record`.

| method          | purpose                                                    |
|-----------------|------------------------------------------------------------|
| `status`        | the active run plus server-wide info                       |
| `list_runs`     | every run hosted, active and history                       |
| `diagnostics`   | active run plus target-span ingest counters                |
| `start_attach`  | begin a recording by attaching to a pid                    |
| `wait_active`   | block until a `WaitCondition` fires or the run stops       |
| `stop_active`   | stop the active run cleanly, returning its final summary   |
| `save_current`  | write the current queryable run to a directory archive     |
| `open_saved`    | load a saved archive into the current query state          |

`start_attach` is how recordings begin: for `stax record -- <argv>`, the CLI
launches the target suspended and hands the PID to this call.

### `Profiler`

The query surface — the live aggregator. Most methods come in two shapes:

- **one-shot** — `top`, `flamegraph`, `threads`, `annotated`, `timeline`,
  `neighbors`, `intervals`, `pet_samples`, `target_spans`, `wakers`, `cfg`,
  `total_on_cpu_ns` — return a single snapshot;
- **`subscribe_*`** — the same queries, but each pushes periodic updates
  over a `vox::Tx<…>` channel for as long as you hold it.

The `subscribe_*` variants are how the web UI stays live without polling —
each panel holds one subscription and re-renders when an update arrives.
There is also `set_paused` / `is_paused` to freeze and resume ingestion.

`target_spans` / `subscribe_target_spans` return both grouped target work
(lane + span name + origin, with count and total/max duration) and capped
recent individual spans for detail panes.

Every query takes a `ViewParams` (thread filter, time range, exclude-symbol
list), so any view can be scoped — the equivalent of the CLI's `--tid`.

## Connecting

`stax-server` listens on two transports simultaneously. Pick by trust level:

```text
local://$XDG_RUNTIME_DIR/stax-server.sock     # or /tmp/stax-server-$UID.sock
ws://127.0.0.1:8080
```

- **`local://…`** — a Unix domain socket, for trusted local clients on the
  same machine: the CLI, local agents, scripts. The path resolution matches
  [`STAX_SERVER_SOCKET`](@/reference/environment-variables.md): the env
  override, then `$XDG_RUNTIME_DIR/stax-server.sock`, then
  `/tmp/stax-server-$UID.sock`.
- **`ws://127.0.0.1:8080`** — the WebSocket, for browser clients. Override
  the bind with [`STAX_SERVER_WS_BIND`](@/reference/environment-variables.md).
  There is no authentication; keep it bound to loopback.

Both transports speak both services — the only difference is who can reach
them.

## Rust clients

`stax-live-proto` exports a generated client per service —
`RunControlClient` and `ProfilerClient` — alongside the shared types
(`ServerStatus`, `RunSummary`, `WaitCondition`, `WaitOutcome`, `TopSort`,
`ViewParams`, `FlamegraphUpdate`, `FlameNode`, `ThreadsUpdate`,
`DiagnosticsSnapshot`, …). The `stax` CLI is itself a straightforward
consumer of these clients — `cli/src/main.rs` is the worked example to read.

## TypeScript bindings

vox generates TypeScript types from the Rust service definitions, so a
browser or Node client gets the exact same shapes the Rust side uses — no
hand-written interfaces, no drift.

```bash
cargo run -p xtask -- codegen     # or, from frontend/: pnpm codegen
```

This regenerates the bindings into `frontend/src/generated/` —
`profiler.generated.ts`, `runcontrol.generated.ts`, and a `theme.css`. The
[web UI](@/guide/web-ui.md) is the reference client: a Vite + React app that
subscribes to `Profiler` over the WebSocket transport.

> **Generated code is generated.** Do not hand-edit anything under
> `frontend/src/generated/` — change the Rust protocol in `stax-live-proto`
> and re-run codegen.

## See also

- [Architecture](@/concepts/architecture.md) — where `stax-server` and its
  sockets sit.
- [The Web UI](@/guide/web-ui.md) — the reference browser client.
- [Environment Variables](@/reference/environment-variables.md) — socket and
  bind overrides.
