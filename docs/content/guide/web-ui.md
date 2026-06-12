+++
title = "The Web UI"
weight = 4
insert_anchor_links = "heading"
+++

The CLI is one face of stax. The other is a browser UI that renders the same
live run — flamegraph, top-N functions/spans, timeline, annotated
disassembly — and updates continuously as samples and target spans land.

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

> **Bind to loopback.** The default `127.0.0.1` keeps the endpoint local to
> your machine. There is no authentication on the WebSocket; do not bind it
> to a public interface.

## Running the frontend

The UI lives in the repo under `frontend/` — a Vite + React + TypeScript app
(`stax-live-frontend`). Run it with [pnpm](https://pnpm.io):

```bash
cd frontend
pnpm install
pnpm dev          # Vite dev server on http://localhost:5173
```

`pnpm build` produces a static bundle in `dist/` if you'd rather serve it
some other way; `pnpm preview` serves that bundle locally.

By default the UI connects to `ws://127.0.0.1:8080`. Point it elsewhere with
a `?ws=` query parameter — `http://localhost:5173/?ws=ws://127.0.0.1:9000`.

## What the UI shows

The UI connects to the `Profiler` service and subscribes to a stream per
panel, so every view refreshes on its own while a recording is in progress —
there is nothing to reload. The layout, top to bottom:

- **Topbar** — connection status, a pause toggle for ingestion, a thread
  switcher, a symbol search (substring or regex), display-mode pills
  (active / CPU / target / off-CPU / wall), PMU-metric pills (IPC / L1-d
  misses / branch mispredicts), a binary-kind filter, and a light/dark toggle.
- **Timeline** — the selected display metric per bucket. Active mode includes
  sampled CPU time plus cooperating target spans; CPU mode peels target spans
  back out; target mode shows target-span duration over time; wall mode is
  active plus off-CPU. Drag-to-brush time-range selection scopes every other
  panel.
- **Off-CPU reason legend** and a **wakers panel** — what threads were
  blocked on, and who woke them.
- **Flamegraph** — the active tree (CPU time plus cooperating target spans),
  the same data as
  [`stax flame`](@/guide/inspecting-a-run.md#stax-flame), rendered as a
  zoomable graph with focus / drop-symbol / Esc keyboard shortcuts and
  off-CPU reason stripes. CPU mode makes flame width sampled CPU time only;
  target mode makes flame width exact target-span duration. Hover text breaks
  out CPU, active, target time, and span counts. Origin-linked target spans
  appear under the CPU stack that queued them when that thread is selected.
- **Top-N table** — the hot-leaf or target-span leaderboard, the same data as
  [`stax top`](@/guide/inspecting-a-run.md#stax-top), sortable by self or
  total. Its bar and primary duration follow the same display metric as the
  flamegraph, with target span counts on cooperating rows.
- **A tabbed detail pane** — *disassembly* (cost-annotated, source-headed,
  the same data as [`stax annotate`](@/guide/inspecting-a-run.md#stax-annotate)),
  *family tree* (callers/callees around a symbol), and *intervals* (the
  individual off-CPU intervals, with reasons and wakers), and *target spans*
  (grouped target work by lane/span/origin, plus the most recent individual
  spans with duration and origin thread/link status). If a selected CPU thread
  has no linked spans while synthetic lanes do have work, the target-span pane
  points at those lanes directly; if origins are linked, clicking the origin
  switches to the CPU thread and opens the origin symbol's family tree.

The UI and the CLI are interchangeable: a run started from the CLI shows up
in the browser, and vice versa. They are both just clients of the same
daemon.

## Checking Target UI Behavior

For visual or target-span UI changes, run the browser smoke against a
checkout-local server instead of whatever daemon is installed on the machine:

```bash
just web-target-smoke
```

That records the blessed `stax-target/examples/corpus.rs` workload through a
temporary checkout-local `stax-server`, saves and reopens the archive, starts
Vite, then drives a real browser through the target-time and target-span
details surfaces at desktop and mobile widths.

## Regenerating the RPC bindings

The frontend talks to `stax-server` through generated TypeScript bindings in
`frontend/src/generated/` — vox generates them from the Rust service
definitions, so the types never drift. After changing the protocol:

```bash
pnpm codegen      # runs `cargo run -p xtask -- codegen`
```

> **Generated code is generated.** Don't hand-edit anything under
> `frontend/src/generated/` — change the Rust protocol and re-run codegen.
> See [Programmatic Usage](@/reference/rpc-services.md).
