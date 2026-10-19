// Focused rendering checks, with no browser, network, server or new test dependencies.
// Transpile the existing UI in-memory and expose its private render units only here.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

const require = createRequire(import.meta.url);
const source = readFileSync(new URL('../src/EvalGraph.tsx', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, { compilerOptions: {
  module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022, jsx: ts.JsxEmit.ReactJSX,
}});
const module = { exports: {} };
vm.runInNewContext(outputText + `
exports.CaptureSummary = CaptureSummary;
exports.CaptureExplorer = CaptureExplorer;
exports.setQueryResult = (data) => { useQuery = () => ({ data, error: null, loading: false }); };
`, {
  module, exports: module.exports,
  require: (name) => name.endsWith('.css') ? {} : name.startsWith('./generated/') ? {} : require(name),
});
const { CaptureSummary, CaptureExplorer, setQueryResult } = module.exports;
const counts = { body_starts: 9n, force_requests: 18n, created: 7n,
  cache_compute: 3n, cache_hit: 5n, cache_wait: 2n, overflow: false };
const site = { module: 1n, id: 42n, defined: true, kind: 'function', name: 'example',
  file: 'counts.vix', start: 0n, end: 20n, line: 1n, column: 1n, params: ['x'] };
const summary = { capture_id: 1n, path: 'counts.jsonl', semantics: 'source-counts',
  capture_mode: 'counts', counts, counts_exact: true, scope: 'selected source counters',
  selection: 'example', max_bytes: null, partial_reason: null, complete: true,
  records: 8n, nodes: 0n, relations: 0n, invocations: 9n, force_requests: 18n,
  cache_computes: 3n, cache_hits: 5n, cache_waits: 2n, missing_endpoints: 0n,
  executions: [{ execution: 1n, module: 1n, workers: 4n, status: 'out_of_fuel', seq: 7n, ts_ns: 6n }],
  diagnostic_count: 0n, diagnostics: [] };
const render = (component, props) => renderToStaticMarkup(React.createElement(component, props));
function explorer(s = summary, rowCounts = counts) {
  setQueryResult({ total: 1n, offset: 0n, callables: [{ site, invocations: rowCounts.body_starts, counts: rowCounts }] });
  return render(CaptureExplorer, { client: {}, summary: s });
}

test('counts explorer shows all metrics and metadata only, never graph navigation', () => {
  const html = explorer();
  assert.match(html, /Exact aggregate counts for the selected stream/);
  assert.match(html, /No invocation\/argument detail/);
  assert.match(html, /Source metadata only/);
  assert.match(html, /example/);
  for (const label of ['Force requests', 'Created', 'Cache compute', 'Cache hit', 'Cache wait']) {
    assert.ok(html.includes(label), label);
  }
  assert.match(html, /out_of_fuel/);
  assert.match(html, /Complete stream/); // Execution failure is not stream incompleteness.
  assert.doesNotMatch(html, /role="tab"|<svg|<h3>Node inspector|Individual recorded invocations/);
});

test('overflow renders saturated lower bounds without an exact-total claim', () => {
  const overflow = { ...counts, body_starts: 18446744073709551615n, overflow: true };
  const html = explorer({ ...summary, counts: overflow, counts_exact: false }, overflow);
  assert.match(html, /Complete stream/);
  assert.match(html, /Overflow: saturated counters are lower bounds, not exact totals/);
  assert.match(html, /Overflow · lower bounds/);
  assert.doesNotMatch(html, /Exact aggregate counts|Exact recorded callable counts|Exact selected site/);
});

test('partial row streams render incomplete totals rather than exact counts', () => {
  const html = explorer({ ...summary, complete: false, counts_exact: false,
    diagnostic_count: 1n, diagnostics: ['execution 1 missing counts rows'] });
  assert.match(html, /Partial stream/);
  assert.match(html, /Incomplete counts/);
  assert.match(html, /missing counts rows/);
  assert.doesNotMatch(html, /Exact aggregate counts|Exact recorded callable counts|Exact selected site/);
});

test('legacy detail explorer preserves invocation and node navigation', () => {
  const detail = { ...summary, semantics: 'lazy-dataflow', capture_mode: 'detail', counts: null,
    nodes: 3n, relations: 2n, executions: [] };
  const html = explorer(detail);
  assert.match(html, /role="tab"[^>]*>Invocations/);
  assert.match(html, /role="tab"[^>]*>All nodes/);
  assert.match(html, /<h3>Node inspector/);
  assert.doesNotMatch(html, /Source metadata only|No invocation\/argument detail/);
  assert.match(render(CaptureSummary, { summary: detail }), /unique nodes/);
});
