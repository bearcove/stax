// Headless end-to-end check: connect to a live stax-server over the
// same generated vox client + vox-ws transport the React UI uses, and
// assert the three must-have surfaces return non-empty, symbolized
// data: flamegraph, top-N, and disassembly-with-cost-annotation
// (incl. the new structured `tokens`). Run while a recording is active:
//   node --experimental-strip-types scripts/smoke.ts [ws-url]
import { connectProfiler } from "../src/generated/profiler.generated.ts";
import type {
  AnnotatedLine,
  AnnotatedView,
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

// 2b. off-CPU breakdown — must be populated and reason-classified
const o = flame.total_off_cpu;
const offTotal =
  o.idle_ns + o.lock_ns + o.semaphore_ns + o.ipc_ns + o.io_read_ns +
  o.io_write_ns + o.readiness_ns + o.sleep_ns + o.connect_ns + o.other_ns;
const ms = (n: bigint) => (Number(n) / 1e6).toFixed(1);
console.log(
  `off-CPU: total=${ms(offTotal)}ms  idle=${ms(o.idle_ns)} lock=${ms(o.lock_ns)} ` +
    `sem=${ms(o.semaphore_ns)} ioR=${ms(o.io_read_ns)} ioW=${ms(o.io_write_ns)} ` +
    `ready=${ms(o.readiness_ns)} sleep=${ms(o.sleep_ns)} other=${ms(o.other_ns)}`,
);
if (offTotal <= 0n) die("off-CPU total is 0 (context-switch ring not working)");
// The workload deliberately sleeps and futex/cond-waits, so the
// kallsyms+wchan classifier must bucket most of it into real reasons,
// not 'Other'.
if (o.other_ns * 2n >= offTotal) {
  die(`off-CPU mostly 'Other' (${ms(o.other_ns)}ms/${ms(offTotal)}ms) — classifier not biting`);
}

// 3. top-N — must be symbolized
const top = await client.top(20, bySelf, params);
console.log(`top: ${top.length} entries`);
if (top.length === 0) die("top is empty");
const named = top.filter((e) => e.function_name && e.function_name.length > 0);
console.log(
  `  top[0]: ${top[0].function_name} (${top[0].binary}) self=${top[0].self_on_cpu_ns}ns`,
);
if (named.length === 0) die("no top entry is symbolized");

// 4. disassembly with cost annotation + structured tokens. Try
// *user-space* symbols until one actually maps to text bytes: kernel
// frames and some system/stripped images have no on-disk text to
// disassemble — expected.
const KERNEL = 0xffff_0000_0000_0000n;
const candidates = named.filter((e) => e.address < KERNEL);
if (candidates.length === 0) die("no user-space symbol in top to annotate");
let ann: AnnotatedView | null = null;
let withTokens: AnnotatedLine[] = [];
let withCost: AnnotatedLine[] = [];
for (const target of candidates) {
  const candidate = await client.annotated(target.address, params);
  const candidateTokens = candidate.lines.filter((l) => l.tokens.length > 0);
  const candidateCost = candidate.lines.filter((l) => l.self_on_cpu_ns > 0n);
  if (candidate.lines.length > 0 && candidateTokens.length > 0 && candidateCost.length > 0) {
    ann = candidate;
    withTokens = candidateTokens;
    withCost = candidateCost;
    break;
  }
}
if (!ann) {
  die(`no annotated user-space symbol among ${candidates.length} top entries`);
}
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
