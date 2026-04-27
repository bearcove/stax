// Hydration layer between the on-the-wire string-table-encoded
// FlameNode (function_name / binary / language are u32 indices into
// FlamegraphUpdate.strings) and what the rest of the frontend wants
// to see (those fields as inline strings).
//
// Each node carries on-CPU time + off-CPU breakdown (10 reasons) +
// PET sample count + off-CPU interval count separately. The UI picks
// which dimension drives flame box width and can color-segment a
// single box across the off-CPU reasons.

import type {
  FlameNode as WireFlameNode,
  FlamegraphUpdate as WireFlamegraphUpdate,
  NeighborsUpdate as WireNeighborsUpdate,
  OffCpuBreakdown as WireOffCpuBreakdown,
} from "./generated/profiler.generated.ts";

/// Re-export so non-wire code paths can describe their own breakdowns.
export type OffCpuBreakdown = WireOffCpuBreakdown;

export interface FlameView {
  address: bigint;
  /// Real CPU time at (or under) this stack, in ns.
  on_cpu_ns: bigint;
  /// Off-CPU time at this stack, by reason. Sum across fields = total
  /// off-CPU time. UI can color-segment a flame box by reason.
  off_cpu: OffCpuBreakdown;
  /// PET stack-walk hits at (or under) this node.
  pet_samples: bigint;
  /// Off-CPU intervals attributed to this stack.
  off_cpu_intervals: bigint;
  function_name: string | null;
  binary: string | null;
  is_main: boolean;
  language: string;
  cycles: bigint;
  instructions: bigint;
  l1d_misses: bigint;
  branch_mispreds: bigint;
  children: FlameView[];
}

export interface FlamegraphView {
  total_on_cpu_ns: bigint;
  total_off_cpu: OffCpuBreakdown;
  root: FlameView;
}

export interface NeighborsView {
  function_name: string | null;
  binary: string | null;
  is_main: boolean;
  language: string;
  own_on_cpu_ns: bigint;
  own_off_cpu: OffCpuBreakdown;
  own_pet_samples: bigint;
  own_off_cpu_intervals: bigint;
  callers_tree: FlameView;
  callees_tree: FlameView;
}

function lookup(strings: string[], idx: number | null): string | null {
  return idx == null ? null : strings[idx];
}

function hydrateNode(node: WireFlameNode, strings: string[]): FlameView {
  return {
    address: node.address,
    on_cpu_ns: node.on_cpu_ns,
    off_cpu: node.off_cpu,
    pet_samples: node.pet_samples,
    off_cpu_intervals: node.off_cpu_intervals,
    function_name: lookup(strings, node.function_name),
    binary: lookup(strings, node.binary),
    is_main: node.is_main,
    language: strings[node.language],
    cycles: node.cycles,
    instructions: node.instructions,
    l1d_misses: node.l1d_misses,
    branch_mispreds: node.branch_mispreds,
    children: node.children.map((c) => hydrateNode(c, strings)),
  };
}

export function hydrateFlamegraph(u: WireFlamegraphUpdate): FlamegraphView {
  return {
    total_on_cpu_ns: u.total_on_cpu_ns,
    total_off_cpu: u.total_off_cpu,
    root: hydrateNode(u.root, u.strings),
  };
}

export function hydrateNeighbors(u: WireNeighborsUpdate): NeighborsView {
  return {
    function_name: lookup(u.strings, u.function_name),
    binary: lookup(u.strings, u.binary),
    is_main: u.is_main,
    language: u.strings[u.language],
    own_on_cpu_ns: u.own_on_cpu_ns,
    own_off_cpu: u.own_off_cpu,
    own_pet_samples: u.own_pet_samples,
    own_off_cpu_intervals: u.own_off_cpu_intervals,
    callers_tree: hydrateNode(u.callers_tree, u.strings),
    callees_tree: hydrateNode(u.callees_tree, u.strings),
  };
}

/// Sum of every off-CPU reason in a breakdown.
export function offCpuTotal(b: OffCpuBreakdown): bigint {
  return (
    b.idle_ns +
    b.lock_ns +
    b.semaphore_ns +
    b.ipc_ns +
    b.io_read_ns +
    b.io_write_ns +
    b.readiness_ns +
    b.sleep_ns +
    b.connect_ns +
    b.other_ns
  );
}

/// One entry per reason that is non-zero, in display order.
export type ReasonKey =
  | "idle"
  | "lock"
  | "semaphore"
  | "ipc"
  | "io_read"
  | "io_write"
  | "readiness"
  | "sleep"
  | "connect"
  | "other";

export const REASON_ORDER: ReasonKey[] = [
  "lock",
  "semaphore",
  "ipc",
  "io_read",
  "io_write",
  "readiness",
  "connect",
  "sleep",
  "idle",
  "other",
];

export const REASON_LABEL: Record<ReasonKey, string> = {
  idle: "idle",
  lock: "lock",
  semaphore: "sema",
  ipc: "ipc",
  io_read: "read",
  io_write: "write",
  readiness: "ready",
  sleep: "sleep",
  connect: "connect",
  other: "other",
};

export function reasonNs(b: OffCpuBreakdown, k: ReasonKey): bigint {
  switch (k) {
    case "idle":
      return b.idle_ns;
    case "lock":
      return b.lock_ns;
    case "semaphore":
      return b.semaphore_ns;
    case "ipc":
      return b.ipc_ns;
    case "io_read":
      return b.io_read_ns;
    case "io_write":
      return b.io_write_ns;
    case "readiness":
      return b.readiness_ns;
    case "sleep":
      return b.sleep_ns;
    case "connect":
      return b.connect_ns;
    case "other":
      return b.other_ns;
  }
}

/// Extract non-zero reasons in display order. Used by flame box
/// stripes, the topbar legend, and any per-row breakdown rendering.
export function reasonSegments(
  b: OffCpuBreakdown,
): { reason: ReasonKey; ns: bigint }[] {
  const out: { reason: ReasonKey; ns: bigint }[] = [];
  for (const k of REASON_ORDER) {
    const ns = reasonNs(b, k);
    if (ns > 0n) out.push({ reason: k, ns });
  }
  return out;
}

/// Map a wire `OffCpuReason` tag to the local `ReasonKey`.
export function reasonKeyOfTag(
  tag:
    | "Idle"
    | "LockWait"
    | "SemaphoreWait"
    | "IpcWait"
    | "IoRead"
    | "IoWrite"
    | "Readiness"
    | "Sleep"
    | "ConnectionSetup"
    | "Other",
): ReasonKey {
  switch (tag) {
    case "Idle":
      return "idle";
    case "LockWait":
      return "lock";
    case "SemaphoreWait":
      return "semaphore";
    case "IpcWait":
      return "ipc";
    case "IoRead":
      return "io_read";
    case "IoWrite":
      return "io_write";
    case "Readiness":
      return "readiness";
    case "Sleep":
      return "sleep";
    case "ConnectionSetup":
      return "connect";
    case "Other":
      return "other";
  }
}

/// Format a nanosecond duration as a human-readable string.
export function formatDuration(ns: bigint): string {
  if (ns === 0n) return "0";
  const n = Number(ns);
  if (n < 1_000) return `${n}ns`;
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}µs`;
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(1)}ms`;
  if (n < 60_000_000_000) return `${(n / 1_000_000_000).toFixed(2)}s`;
  const minutes = Math.floor(n / 60_000_000_000);
  const seconds = (n % 60_000_000_000) / 1_000_000_000;
  return `${minutes}m${seconds.toFixed(1)}s`;
}
