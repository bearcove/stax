import type { ReactNode } from "react";
import { LuCircuitBoard } from "react-icons/lu";
import type { TargetLaneKind } from "./generated/profiler.generated";

export const TARGET_SPANS_BINARY = "<target spans>";

export type TargetVisualKind = "target" | "metal";

export type TargetVisualInput = {
  function_name?: string | null;
  binary?: string | null;
  lane?: string | null;
  span?: string | null;
  target_spans?: bigint;
  target_kind?: TargetLaneKind | null;
};

export function isTargetFrame(o: TargetVisualInput): boolean {
  return (
    o.target_kind != null ||
    o.binary === TARGET_SPANS_BINARY ||
    o.lane != null ||
    o.span != null
  );
}

export function targetVisualKind(o: TargetVisualInput): TargetVisualKind | null {
  if (!isTargetFrame(o)) return null;
  if (o.target_kind?.tag === "Metal") {
    return "metal";
  }
  return "target";
}

export function hasTargetSignpost(o: TargetVisualInput): boolean {
  return !isTargetFrame(o) && (o.target_spans ?? 0n) > 0n;
}

export function targetClass(kind: TargetVisualKind | null): string {
  return kind ? `target-${kind}` : "";
}

export function targetTitle(kind: TargetVisualKind): string {
  switch (kind) {
    case "metal":
      return "Metal target span";
    case "target":
      return "target span";
  }
}

export function TargetMark({
  kind,
}: {
  kind: TargetVisualKind;
}): ReactNode {
  return (
    <span
      className={`target-mark ${targetClass(kind)}`}
      title={targetTitle(kind)}
      aria-label={targetTitle(kind)}
    >
      {kind === "metal" ? <MetalMark /> : <LuCircuitBoard aria-hidden="true" />}
    </span>
  );
}

function MetalMark() {
  // Apple Metal logo geometry from Wikimedia Commons:
  // https://commons.wikimedia.org/wiki/File:Apple_Metal_logo,_version_2.svg
  // Original file attributes it to Apple Inc.; Commons marks the simple
  // logo geometry as public-domain text/logo, with trademark caveats.
  return (
    <svg
      className="target-mark-metal-svg"
      viewBox="0 0 720 720"
      aria-hidden="true"
    >
      <defs>
        <linearGradient
          id="metalLogoGradient"
          x1="50%"
          y1="0%"
          x2="50%"
          y2="100%"
        >
          <stop stopColor="#0EFFDD" offset="0%" />
          <stop stopColor="#24FF74" offset="100%" />
        </linearGradient>
      </defs>
      <path
        d="M576 720H144C64.5 720 0 655.5 0 576V144C0 64.5 64.5 0 144 0h432c79.5 0 144 64.5 144 144v432c0 79.5-64.5 144-144 144Z"
        fill="url(#metalLogoGradient)"
      />
      <polygon
        fill="#000"
        points="141 132 334 368 334 195 651 545 569 545 398 364 396 545 205 309 205 545 141 545"
      />
    </svg>
  );
}
