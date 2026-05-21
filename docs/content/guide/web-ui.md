+++
title = "The Web UI"
weight = 4
insert_anchor_links = "heading"
+++

The CLI is one face of stax. The other is a browser UI that renders the same
live run — flamegraph, top-N functions, and annotated disassembly — and
updates continuously as samples land.

## The WebSocket endpoint

`stax-server` listens on two transports. The CLI uses a Unix domain socket;
the browser uses a WebSocket:

```text
ws://127.0.0.1:8080
```

Both speak the same [vox RPC](@/reference/rpc-services.md) services. The
WebSocket is bound automatically when `stax-server` starts — there is no
separate command to launch it.

Override the bind address with `STAX_SERVER_WS_BIND`:

```bash
STAX_SERVER_WS_BIND=127.0.0.1:9000 stax-server
```

To make it stick, set it in the LaunchAgent plist's `EnvironmentVariables` —
see [Environment Variables](@/reference/environment-variables.md).

> **Bind to loopback.** The default `127.0.0.1` keeps the endpoint local to
> your machine. There is no authentication on the WebSocket; do not bind it
> to a public interface.

## What the UI shows

The browser client connects to the WebSocket and subscribes to the
`Profiler` service. It mirrors the three-pane layout of a profiler like
*Instruments*:

- **Flamegraph** — the on-CPU call tree, the same data as
  [`stax flame`](@/guide/inspecting-a-run.md#stax-flame), rendered as a
  zoomable graph instead of an indented tree.
- **Top-N functions** — the hot-leaf leaderboard, the same data as
  [`stax top`](@/guide/inspecting-a-run.md#stax-top).
- **Annotated disassembly** — per-instruction sample counts for a selected
  function, the same data as
  [`stax annotate`](@/guide/inspecting-a-run.md#stax-annotate).

Because every panel is driven by a `subscribe_*` RPC, the view refreshes on
its own while a recording is in progress — there is nothing to reload.

## Using it

1. Make sure `stax-server` is running — `stax status` confirms it.
2. Start a recording: `stax record -- ./mybench`.
3. Open the browser client pointed at `ws://127.0.0.1:8080`.

The UI and the CLI are interchangeable: a run started from the CLI shows up
in the browser, and vice versa. They are both just clients of the same
daemon.

## Building your own client

The web UI is a vox RPC client like any other. If you want to build your
own dashboard, or drive stax from a script in another language, the
WebSocket transport is the entry point — and vox can generate TypeScript
bindings for the protocol. See
[Programmatic Usage](@/reference/rpc-services.md) for the service surface and
[`cargo xtask codegen`](@/reference/rpc-services.md#typescript-bindings) for
the generated types.
