// Headless end-to-end check: connect to a live stax-server over the
// same generated vox client + vox-ws transport the React UI uses, and
// assert the three must-have surfaces return non-empty, symbolized
// data: flamegraph, top-N, and disassembly-with-cost-annotation
// (incl. the new structured `tokens`). Run while a recording is active:
//   node --experimental-strip-types scripts/smoke.ts [ws-url]
import { connectProfiler } from "../src/generated/profiler.generated.ts";
import type {
  ViewParams,
  TopSort,
} from "../src/generated/profiler.generated.ts";

const url = process.argv[2] ?? "ws://127.0.0.1:8080";
const params: ViewParams = {
  tid: null,
  filter: { time_range: null, exclude_symbols: [] },
};
const bySelf: TopSort = { tag: "BySelf" };

function die(msg: string): never {
  console.error("FAIL:", msg);
  process.exit(1);
}

const client = await connectProfiler(url);

// 1. threads
const threads = await client.threads();
console.log(`threads: ${threads.threads.length}`);
if (threads.threads.length === 0) die("no threads");

// 2. flamegraph — must have on-CPU time and a non-trivial tree
const flame = await client.flamegraph(params);
const kids = flame.root.children.length;
console.log(
  `flamegraph: total_on_cpu_ns=${flame.total_on_cpu_ns} root.children=${kids} strings=${flame.strings.length}`,
);
if (flame.total_on_cpu_ns <= 0n) die("flamegraph total_on_cpu_ns == 0");
if (kids === 0) die("flamegraph root has no children");

// 3. top-N — must be symbolized
const top = await client.top(20, bySelf, params);
console.log(`top: ${top.length} entries`);
if (top.length === 0) die("top is empty");
const named = top.filter((e) => e.function_name && e.function_name.length > 0);
console.log(
  `  top[0]: ${top[0].function_name} (${top[0].binary}) self=${top[0].self_on_cpu_ns}ns`,
);
if (named.length === 0) die("no top entry is symbolized");

// 4. disassembly with cost annotation + structured tokens
const target = named[0];
const ann = await client.annotated(target.address, params);
const withTokens = ann.lines.filter((l) => l.tokens.length > 0);
const withCost = ann.lines.filter((l) => l.self_on_cpu_ns > 0n);
console.log(
  `annotated ${ann.function_name}: lines=${ann.lines.length} tokenized=${withTokens.length} cost-annotated=${withCost.length}`,
);
if (ann.lines.length === 0) die("annotated view has no lines");
if (withTokens.length === 0) die("no annotated line carries syntax tokens");
if (withCost.length === 0) die("no annotated line carries a cost");
const sample = withTokens[0].tokens
  .slice(0, 8)
  .map((t) => `${t.text}<${t.kind.tag}>`)
  .join(" ");
console.log(`  sample tokens: ${sample}`);

console.log("\nPASS: flamegraph + top-N + annotated all live & symbolized");
process.exit(0);
