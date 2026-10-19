import {
  useEffect,
  useId,
  useMemo,
  useState,
  type DependencyList,
  type ReactNode,
} from "react";
import {
  connectEvalGraph,
  type EvalGraphClient,
  type GraphEdge,
  type GraphNeighborhood,
  type GraphNode,
  type GraphNodePage,
  type GraphSite,
  type GraphSummary,
} from "./generated/evalgraph.generated.ts";
import "./evalGraph.css";

const PAGE_SIZE = 25;
const RELATION_PAGE_SIZE = 12;
const PATH_KEY = "stax-eval-graph-path";
const count = (n: bigint | number) => n.toLocaleString();
const siteId = (s: GraphSite) => `${s.module}:${s.id}`;
const message = (error: unknown) => error instanceof Error ? error.message : String(error);

function unwrap<T>(result: { ok: true; value: T } | { ok: false; error: string }): T {
  if (!result.ok) throw new Error(result.error);
  return result.value;
}

// All list and neighborhood requests are bounded. Ignore responses belonging to
// an old selection/query, rather than letting a slow response replace newer UI.
function useQuery<T>(load: () => Promise<T>, deps: DependencyList) {
  const [state, setState] = useState<{ data: T | null; error: string | null; loading: boolean }>({
    data: null, error: null, loading: true,
  });
  useEffect(() => {
    let current = true;
    setState({ data: null, error: null, loading: true });
    load().then(
      (data) => { if (current) setState({ data, error: null, loading: false }); },
      (error) => { if (current) setState({ data: null, error: message(error), loading: false }); },
    );
    return () => { current = false; };
  }, deps);
  return state;
}

function ErrorNotice({ children }: { children: ReactNode }) {
  return <div className="eg-error" role="alert">{children}</div>;
}

function QueryNotice({ loading, error }: { loading: boolean; error: string | null }) {
  return error ? <ErrorNotice>{error}</ErrorNotice> : loading ? <p className="eg-muted" role="status">Loading…</p> : null;
}

function Pagination({ offset, total, size, length, loading, onChange, noun = "items" }: {
  offset: bigint; total: bigint; size: number; length: number; loading: boolean;
  onChange: (offset: bigint) => void; noun?: string;
}) {
  const end = offset + BigInt(length);
  return <div className="eg-pagination" aria-label={`${noun} pagination`}>
    <span>{length ? `${count(offset + 1n)}–${count(end)}` : "0"} of {count(total)} {noun} · at most {size}/page</span>
    <button disabled={loading || offset === 0n} onClick={() => onChange(offset > BigInt(size) ? offset - BigInt(size) : 0n)}>Previous</button>
    <button disabled={loading || end >= total} onClick={() => onChange(offset + BigInt(size))}>Next</button>
  </div>;
}

function initialPath() {
  try { return localStorage.getItem(PATH_KEY) ?? ""; } catch { return ""; }
}

/** Independent service lane: no profiler run, sample, symbol or timing dependency. */
export function EvalGraphPanel({ url }: { url: string }) {
  const [retry, setRetry] = useState(0);
  const connection = useQuery(() => connectEvalGraph(url), [url, retry]);
  return <div className="eval-graph">
    <header className="eg-heading">
      <div><h2>Lazy dataflow explorer</h2><p>Recorded dependencies and identities — not a temporal call tree or a time profile.</p></div>
      <span className="eg-badge">DATAFLOW</span>
    </header>
    <QueryNotice {...connection} />
    {connection.error && <button onClick={() => setRetry((n) => n + 1)}>Reconnect graph service</button>}
    {connection.data && <CaptureBrowser key={`${url}:${retry}`} client={connection.data} />}
  </div>;
}

function CaptureBrowser({ client }: { client: EvalGraphClient }) {
  const [path, setPath] = useState(initialPath);
  const [revision, setRevision] = useState(0);
  const [active, setActive] = useState<GraphSummary | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const captures = useQuery(() => client.captures(), [client, revision]);

  async function importPath(source: string) {
    if (!source.trim() || busy) return;
    setBusy(true); setError(null); setNotice(null);
    try {
      const summary = unwrap(await client.importPath(source.trim()));
      setActive(summary);
      setPath(source.trim());
      try { localStorage.setItem(PATH_KEY, source.trim()); } catch { /* optional convenience */ }
      setRevision((n) => n + 1);
    } catch (err) { setError(message(err)); }
    finally { setBusy(false); }
  }

  async function closeCapture(summary: GraphSummary) {
    if (busy) return;
    setBusy(true); setError(null); setNotice(null);
    try {
      unwrap(await client.close(summary.capture_id));
      setActive((current) => current?.capture_id === summary.capture_id ? null : current);
      setNotice(`Closed capture #${summary.capture_id}. The source JSONL file was not deleted.`);
      setRevision((n) => n + 1);
    } catch (err) { setError(message(err)); }
    finally { setBusy(false); }
  }

  return <>
    <form className="eg-import" onSubmit={(event) => { event.preventDefault(); void importPath(path); }}>
      <label htmlFor="eg-import-path">Server-local JSONL path</label>
      <div className="eg-toolbar">
        <input id="eg-import-path" value={path} onChange={(event) => setPath(event.target.value)} placeholder="/path/on/server/eval-graph.jsonl" spellCheck={false} disabled={busy} required />
        <button type="submit" disabled={busy || !path.trim()}>{busy ? "Working…" : "Import capture"}</button>
      </div>
      <p className="eg-muted">Read on the connected server, not uploaded from this browser. Captures are immutable and process-local; restarting the server loses imported captures. Keep the JSONL file to import it again. Closing releases a capture, never its source file.</p>
    </form>
    {error && <ErrorNotice>{error}</ErrorNotice>}
    {notice && <p role="status">{notice}</p>}
    <section className="eg-section" aria-label="Imported captures">
      <div className="eg-toolbar"><h3>Imported captures</h3><button disabled={busy || captures.loading} onClick={() => setRevision((n) => n + 1)}>Refresh list</button></div>
      <QueryNotice {...captures} />
      {captures.data?.length === 0 && <p className="eg-muted">No captures imported. Import a server-local JSONL file above; no active recording is required.</p>}
      {captures.data && captures.data.length > 0 && <div className="eg-table-wrap eg-captures"><table>
        <thead><tr><th>Capture / source path</th><th>Stream</th><th>Nodes</th><th>Diagnostics</th><th>Actions</th></tr></thead>
        <tbody>{captures.data.map((capture) => <tr key={String(capture.capture_id)} className={active?.capture_id === capture.capture_id ? "eg-selected" : ""}>
          <td><button className="eg-link" aria-pressed={active?.capture_id === capture.capture_id} onClick={() => setActive(capture)}>#{String(capture.capture_id)} · {capture.path}</button></td>
          <td><span className={`eg-badge ${capture.complete ? "" : "eg-warning"}`}>{capture.complete ? "complete" : "partial"}</span></td>
          <td>{count(capture.nodes)}</td><td>{count(capture.diagnostic_count)}</td>
          <td><div className="eg-actions"><button disabled={busy} title="Read the source path again as a new immutable capture" onClick={() => void importPath(capture.path)}>Reimport path</button><button disabled={busy} aria-label={`Close capture ${capture.capture_id}`} onClick={() => void closeCapture(capture)}>Close</button></div></td>
        </tr>)}</tbody>
      </table></div>}
    </section>
    {active ? <CaptureExplorer key={String(active.capture_id)} client={client} summary={active} /> : <div className="eg-empty">Select an imported capture to reopen its explorer, or import a new one.</div>}
  </>;
}

function CaptureSummary({ summary: s }: { summary: GraphSummary }) {
  const stats: [string, bigint][] = [
    ["records", s.records], ["unique nodes", s.nodes], ["aggregated relations", s.relations],
    ["recorded body starts / invocations", s.invocations], ["force requests", s.force_requests],
    ["cache compute/miss outcomes", s.cache_computes], ["cache hits", s.cache_hits],
    ["cache waits", s.cache_waits], ["missing endpoints", s.missing_endpoints], ["diagnostics", s.diagnostic_count],
  ];
  return <section className="eg-section">
    <div className="eg-toolbar"><h3>Capture #{String(s.capture_id)}</h3><span className={`eg-badge ${s.complete ? "" : "eg-warning"}`}>{s.complete ? "Complete stream" : "Partial stream"}</span></div>
    <p className="eg-path">{s.path}</p>
    <p><strong>Semantics:</strong> {s.semantics}</p>
    <p><strong>Scope:</strong> {s.scope}</p>
    <p><strong>Recorded selection:</strong> {s.selection === null ? "No selector metadata recorded" : <code>{s.selection}</code>}</p>
    <p><strong>Recorded byte budget:</strong> {s.max_bytes === null ? "No byte budget metadata recorded" : `${count(s.max_bytes)} bytes`}</p>
    {s.partial_reason !== null && <p className="eg-warning"><strong>Recorded partial / stop reason:</strong> {s.partial_reason} <span className="eg-muted">(verbatim; an intentional stop is not necessarily malformed input)</span></p>}
    <p className="eg-muted">Stream completeness is not evaluation success. Counts describe recorded events only, not all logical calls. Cache compute/miss outcomes are not thunk evaluation counts. Timestamps are raw observation metadata, not durations.</p>
    <dl className="eg-stats">{stats.map(([label, value]) => <div key={label}><dt>{label}</dt><dd>{count(value)}</dd></div>)}</dl>
    <details className="eg-details" open={s.diagnostic_count > 0n}>
      <summary>Import diagnostics · {count(s.diagnostic_count)} total · {s.diagnostics.length} examples</summary>
      {s.diagnostics.length ? <ul>{s.diagnostics.map((text, index) => <li key={index}>{text}</li>)}</ul> : <p className="eg-muted">No diagnostic examples recorded.</p>}
      {s.diagnostic_count > BigInt(s.diagnostics.length) && <p className="eg-muted">Examples are bounded; the total includes additional occurrences.</p>}
    </details>
    <details className="eg-details" open>
      <summary>Recorded execution statuses · {s.executions.length}</summary>
      {s.executions.length ? <div className="eg-table-wrap eg-status-table"><table><thead><tr><th>Status (verbatim)</th><th>Workers</th><th>Sequence</th><th>Observed ts_ns</th></tr></thead><tbody>{s.executions.map((execution, index) => <tr key={index}><td><code>{execution.status}</code></td><td>{count(execution.workers)}</td><td>{String(execution.seq)}</td><td>{String(execution.ts_ns)}</td></tr>)}</tbody></table></div> : <p className="eg-muted">No execution status records.</p>}
    </details>
  </section>;
}

type BrowserTab = "callables" | "invocations" | "nodes";
function CaptureExplorer({ client, summary }: { client: EvalGraphClient; summary: GraphSummary }) {
  const [tab, setTab] = useState<BrowserTab>("callables");
  const [selected, setSelected] = useState<string | null>(null);
  const [callable, setCallable] = useState<GraphSite | null>(null);
  const [history, setHistory] = useState<string[]>([]);
  function selectNode(id: string) {
    if (id === selected) return;
    if (selected) setHistory((previous) => [...previous.slice(-49), selected]);
    setSelected(id);
  }
  function back() {
    const previous = history[history.length - 1];
    if (previous) { setSelected(previous); setHistory((items) => items.slice(0, -1)); }
  }
  return <>
    <CaptureSummary summary={summary} />
    <div className="eg-workspace">
      <section className="eg-section eg-browser" aria-label="Graph lists">
        <div className="eg-tabs" role="tablist" aria-label="Graph entities">
          {([ ["callables", "Callable counts"], ["invocations", "Invocations"], ["nodes", "All nodes"] ] as const).map(([value, label]) => <button key={value} role="tab" aria-selected={tab === value} onClick={() => setTab(value)}>{label}</button>)}
        </div>
        {tab === "callables" ? <CallableList client={client} captureId={summary.capture_id} onSelect={(site) => { setCallable(site); setTab("invocations"); }} /> : tab === "invocations" ? <InvocationList key={callable ? siteId(callable) : "all"} client={client} captureId={summary.capture_id} site={callable} selected={selected} onSelect={selectNode} clearSite={() => setCallable(null)} /> : <NodeList client={client} captureId={summary.capture_id} selected={selected} onSelect={selectNode} />}
      </section>
      <section className="eg-section eg-inspector" aria-label="Node inspector">
        <div className="eg-toolbar"><h3>Node inspector</h3><button disabled={!history.length} onClick={back}>Back to previous node</button></div>
        {selected ? <NodeInspector key={selected} client={client} captureId={summary.capture_id} id={selected} onSelect={selectNode} onCallable={(site) => { setCallable(site); setTab("invocations"); }} /> : <p className="eg-empty">Choose a callable to browse its individual recorded body starts, or select an invocation or thunk from All nodes. Follow recorded relations to move between a thunk and its invocation.</p>}
      </section>
    </div>
  </>;
}

function SearchBox({ placeholder, onSearch, children }: { placeholder: string; onSearch: (query: string) => void; children?: ReactNode }) {
  const [draft, setDraft] = useState("");
  return <form className="eg-toolbar eg-search" role="search" onSubmit={(event) => { event.preventDefault(); onSearch(draft.trim()); }}>
    <input aria-label={placeholder} value={draft} onChange={(event) => setDraft(event.target.value)} placeholder={placeholder} />
    {children}<button type="submit">Search</button>
  </form>;
}

function CallableList({ client, captureId, onSelect }: { client: EvalGraphClient; captureId: bigint; onSelect: (site: GraphSite) => void }) {
  const [query, setQuery] = useState("");
  const [offset, setOffset] = useState(0n);
  const result = useQuery(async () => unwrap(await client.callables(captureId, query, offset, PAGE_SIZE)), [client, captureId, query, offset]);
  return <>
    <h3>Exact recorded callable counts</h3>
    <p className="eg-muted">Ranked by recorded body starts (descending), grouped by source site module:ID. Not samples, timings, distinct inputs or estimated evaluations. Choose a row for individual invocations.</p>
    <SearchBox placeholder="Search callable name, source or site" onSearch={(text) => { setQuery(text); setOffset(0n); }} />
    <QueryNotice {...result} />
    {result.data && <>
      <div className="eg-table-wrap"><table><thead><tr><th>Rank</th><th>Callable / exact site</th><th>Recorded body starts</th></tr></thead><tbody>{result.data.callables.map((callable, index) => <tr key={siteId(callable.site)}><td>{String(offset + BigInt(index) + 1n)}</td><td><button className="eg-link" onClick={() => onSelect(callable.site)}>{callable.site.name || "(unnamed callable)"}<small>{siteId(callable.site)} · {callable.site.kind}{!callable.site.defined && " · missing definition"}</small><small>{callable.site.file || "No source metadata"}</small></button></td><td>{count(callable.invocations)}</td></tr>)}</tbody></table></div>
      {!result.data.callables.length && <p className="eg-empty">No recorded callables match.</p>}
      <Pagination offset={result.data.offset} total={result.data.total} length={result.data.callables.length} size={PAGE_SIZE} loading={result.loading} onChange={setOffset} noun="callables" />
    </>}
  </>;
}

function InvocationList({ client, captureId, site, selected, onSelect, clearSite }: {
  client: EvalGraphClient; captureId: bigint; site: GraphSite | null; selected: string | null; onSelect: (id: string) => void; clearSite: () => void;
}) {
  const [offset, setOffset] = useState(0n);
  const [query, setQuery] = useState("");
  const result = useQuery(async () => unwrap(site
    ? await client.invocations({ capture_id: captureId, module: site.module, site: site.id, offset, limit: PAGE_SIZE })
    : await client.search({ capture_id: captureId, query, kind: "invocation", offset, limit: PAGE_SIZE })), [client, captureId, site, query, offset]);
  return <>
    <h3>Individual recorded invocations</h3>
    {site ? <div className="eg-scope"><strong>{site.name || "(unnamed callable)"}</strong><code>{siteId(site)}</code><button onClick={clearSite}>Show / search all invocations</button><p className="eg-muted">Exact-site filter; the API paginates this list without text search.</p></div> : <SearchBox placeholder="Search invocation ID, label or preview" onSearch={(text) => { setQuery(text); setOffset(0n); }} />}
    <p className="eg-muted">One row per identity, not a count aggregate. Equal previews do not imply equal nodes or equal inputs.</p>
    <QueryNotice {...result} />
    {result.data && <NodePage page={result.data} selected={selected} onSelect={onSelect} offsetChanged={setOffset} loading={result.loading} noun="invocations" />}
  </>;
}

function NodeList({ client, captureId, selected, onSelect }: { client: EvalGraphClient; captureId: bigint; selected: string | null; onSelect: (id: string) => void }) {
  const [query, setQuery] = useState("");
  const [kind, setKind] = useState("");
  const [offset, setOffset] = useState(0n);
  const result = useQuery(async () => unwrap(await client.search({ capture_id: captureId, query, kind, offset, limit: PAGE_SIZE })), [client, captureId, query, kind, offset]);
  return <>
    <h3>Capture-local node identities</h3>
    <SearchBox placeholder="Search node ID, label or preview" onSearch={(text) => { setQuery(text); setOffset(0n); }}>
      <select aria-label="Node kind" value={kind} onChange={(event) => { setKind(event.target.value); setOffset(0n); }}><option value="">All kinds</option><option value="invocation">Invocations</option><option value="thunk">Thunks</option><option value="unresolved">Missing definitions</option></select>
    </SearchBox>
    <QueryNotice {...result} />
    {result.data && <NodePage page={result.data} selected={selected} onSelect={onSelect} offsetChanged={setOffset} loading={result.loading} noun="nodes" />}
  </>;
}

function NodePage({ page, selected, onSelect, offsetChanged, loading, noun }: {
  page: GraphNodePage; selected: string | null; onSelect: (id: string) => void; offsetChanged: (offset: bigint) => void; loading: boolean; noun: string;
}) {
  return <>
    <div className="eg-table-wrap"><table><thead><tr><th>Identity / kind</th><th>Recorded label / preview</th></tr></thead><tbody>{page.nodes.map((node) => <tr key={node.id} className={node.id === selected ? "eg-selected" : ""}>
      <td><button className="eg-link eg-node-id" onClick={() => onSelect(node.id)} aria-pressed={selected === node.id}>{node.id}</button><small>{node.kind}{!node.defined && <span className="eg-warning"> · missing definition</span>}</small></td>
      <td><button className="eg-link" onClick={() => onSelect(node.id)}>{node.label || "(no label)"}</button>{node.preview !== null && <pre className="eg-preview-inline">{node.preview}{node.preview_truncated && " … [truncated]"}</pre>}</td>
    </tr>)}</tbody></table></div>
    {!page.nodes.length && <p className="eg-empty">No {noun} match.</p>}
    <Pagination offset={page.offset} total={page.total} length={page.nodes.length} size={PAGE_SIZE} loading={loading} onChange={offsetChanged} noun={noun} />
  </>;
}

function SourceMetadata({ site }: { site: GraphSite }) {
  return <div className="eg-source">
    <h4>Recorded source metadata</h4>
    <dl className="eg-fields"><dt>Site (module:ID)</dt><dd><code>{siteId(site)}</code>{!site.defined && <span className="eg-warning"> · missing definition</span>}</dd><dt>Kind / name</dt><dd>{site.kind} · {site.name || "(unnamed)"}</dd><dt>File / origin</dt><dd><code>{site.file || "(not recorded)"}</code></dd><dt>Source offsets</dt><dd>{String(site.start)}–{String(site.end)}</dd><dt>Line / column</dt><dd>{String(site.line)} / {String(site.column)}</dd><dt>Parameters / captures</dt><dd>{site.params.length ? <ul>{site.params.map((param, index) => <li key={index}><code>{param}</code></li>)}</ul> : "None recorded"}</dd></dl>
    <p className="eg-muted">Metadata only, not verified on-disk source or the exact executed text. Runtime sites may have synthetic names and zero ranges. No source-file read is requested.</p>
  </div>;
}

function NodeInspector({ client, captureId, id, onSelect, onCallable }: { client: EvalGraphClient; captureId: bigint; id: string; onSelect: (id: string) => void; onCallable: (site: GraphSite) => void }) {
  const [retry, setRetry] = useState(0);
  const result = useQuery(async () => unwrap(await client.inspect(captureId, id)), [client, captureId, id, retry]);
  const node = result.data;
  return <>
    <QueryNotice {...result} />
    {result.error && <button onClick={() => setRetry((n) => n + 1)}>Retry inspection</button>}
    {node && <>
      <h3 className="eg-node-id">{node.id}</h3>
      {!node.defined && <p className="eg-warning">Missing endpoint definition: this identity was referenced, but its defining record is unavailable.</p>}
      <dl className="eg-fields"><dt>Kind</dt><dd>{node.kind}</dd><dt>Label</dt><dd>{node.label || "(none recorded)"}</dd><dt>Defined</dt><dd>{node.defined ? "yes" : "no"}</dd><dt>Recorded sequence</dt><dd>{String(node.seq)}</dd><dt>Observed ts_ns</dt><dd>{String(node.ts_ns)} <span className="eg-muted">(raw, not a duration)</span></dd></dl>
      <h4>Already-observed preview</h4>
      {node.preview === null ? <p className="eg-muted">No preview recorded. Inspecting does not force a value.</p> : <pre className="eg-preview">{node.preview}</pre>}
      {node.preview_truncated && <p className="eg-warning">Preview was truncated in the recording.</p>}
      <p className="eg-muted">Identity is {node.id}, not its preview text. Different IDs stay distinct even when previews are equal.</p>
      {node.site ? <><SourceMetadata site={node.site} /><button onClick={() => onCallable(node.site!)}>Browse recorded invocations at this source site</button></> : <p className="eg-muted">No source site recorded for this node.</p>}
      <h4>Recorded fields / bindings</h4>
      {node.attributes.length ? <div className="eg-table-wrap"><table><thead><tr><th>Field</th><th>Recorded value</th></tr></thead><tbody>{node.attributes.map((attribute, index) => <tr key={index}><td><code>{attribute.name}</code></td><td><code className="eg-wrap">{attribute.value}</code></td></tr>)}</tbody></table></div> : <p className="eg-muted">No additional fields recorded.</p>}
      <Neighborhood client={client} captureId={captureId} id={id} onSelect={onSelect} />
    </>}
  </>;
}

function Neighborhood({ client, captureId, id, onSelect }: { client: EvalGraphClient; captureId: bigint; id: string; onSelect: (id: string) => void }) {
  const [direction, setDirection] = useState("both");
  const [offset, setOffset] = useState(0n);
  const [retry, setRetry] = useState(0);
  const result = useQuery(async () => unwrap(await client.neighborhood({ capture_id: captureId, node: id, direction, offset, limit: RELATION_PAGE_SIZE })), [client, captureId, id, direction, offset, retry]);
  return <section className="eg-neighborhood">
    <div className="eg-toolbar"><h3>Bounded shared-node neighborhood</h3><label>Relations <select aria-label="Relation direction" value={direction} onChange={(event) => { setDirection(event.target.value); setOffset(0n); }}><option value="both">Both directions</option><option value="incoming">Incoming</option><option value="outgoing">Outgoing</option></select></label></div>
    <p className="eg-muted">One box per capture-local ID, shared by every relation on this page. Arrowheads follow recorded relation direction, not time or caller nesting. Multiplicity ×N is an aggregated relation count. This is only a local page, never a whole-program layout.</p>
    <QueryNotice {...result} />
    {result.error && <button onClick={() => setRetry((n) => n + 1)}>Retry relations</button>}
    {result.data && <>
      <Pagination offset={result.data.offset} total={result.data.total_relations} size={RELATION_PAGE_SIZE} length={result.data.edges.length} loading={result.loading} onChange={setOffset} noun={`${direction} aggregated relations`} />
      <NeighborhoodSvg graph={result.data} onSelect={onSelect} />
      {!result.data.edges.length && <p className="eg-empty">No {direction} relations for this node.</p>}
      <RelationTable edges={result.data.edges} nodes={result.data.nodes} center={result.data.center} onSelect={onSelect} />
    </>}
  </section>;
}

function RelationTable({ edges, nodes, center, onSelect }: { edges: GraphEdge[]; nodes: GraphNode[]; center: GraphNode; onSelect: (id: string) => void }) {
  const known = new Map(nodes.map((node) => [node.id, node]));
  known.set(center.id, center);
  const endpoint = (id: string) => <><button className="eg-link eg-node-id" onClick={() => onSelect(id)}>{id}</button>{!known.get(id)?.defined && <small className="eg-warning">missing definition</small>}</>;
  return <div className="eg-table-wrap"><table className="eg-relations"><caption>Recorded relations · exact endpoints and multiplicities</caption><thead><tr><th>From</th><th>Relation</th><th>To</th><th>Multiplicity</th><th>Argument / state</th><th>Raw observations</th></tr></thead><tbody>{edges.map((edge, index) => <tr key={index}><td>{endpoint(edge.from)}</td><td><code>{edge.relation}</code></td><td>{endpoint(edge.to)}</td><td>×{count(edge.count)}</td><td>arg: {edge.argument_index === null ? "—" : String(edge.argument_index)}<br />state: {edge.state === null ? "—" : String(edge.state)}</td><td>seq {String(edge.first_seq)}–{String(edge.last_seq)}<br />ts_ns {String(edge.first_ts_ns)}–{String(edge.last_ts_ns)}</td></tr>)}</tbody></table></div>;
}

// A fixed, one-hop layout only. Map keys are the wire IDs: an incoming and an
// outgoing edge can NEVER cause a shared neighbor to become two visual nodes.
function NeighborhoodSvg({ graph, onSelect }: { graph: GraphNeighborhood; onSelect: (id: string) => void }) {
  const markerId = `eg-arrow-${useId().replace(/:/g, "")}`;
  const layout = useMemo(() => {
    const nodes = new Map<string, GraphNode | undefined>();
    nodes.set(graph.center.id, graph.center);
    for (const node of graph.nodes) nodes.set(node.id, node);
    for (const edge of graph.edges) {
      if (!nodes.has(edge.from)) nodes.set(edge.from, undefined);
      if (!nodes.has(edge.to)) nodes.set(edge.to, undefined);
    }
    const neighbors = [...nodes.keys()].filter((id) => id !== graph.center.id).sort();
    const positions = new Map<string, { x: number; y: number }>([[graph.center.id, { x: 440, y: 275 }]]);
    neighbors.forEach((id, index) => {
      const angle = (2 * Math.PI * index) / Math.max(1, neighbors.length) - Math.PI / 2;
      positions.set(id, { x: 440 + Math.cos(angle) * 338, y: 275 + Math.sin(angle) * 225 });
    });
    const pairs = new Map<string, number[]>();
    graph.edges.forEach((edge, index) => {
      const key = JSON.stringify([edge.from, edge.to].sort());
      pairs.set(key, [...(pairs.get(key) ?? []), index]);
    });
    const slots = new Map<number, { index: number; total: number }>();
    for (const indexes of pairs.values()) {
      indexes.forEach((edgeIndex, index) => slots.set(edgeIndex, { index, total: indexes.length }));
    }
    return { nodes, positions, slots };
  }, [graph]);
  const short = (text: string, length: number) => text.length > length ? `${text.slice(0, length - 1)}…` : text;
  return <div className="eg-svg-wrap"><svg className="eg-svg" viewBox="0 0 880 550" role="group" aria-label={`Dataflow relation page centered on ${graph.center.id}; ${layout.nodes.size} unique identities`}>
    <title>Shared-node dataflow neighborhood. The relation table contains full labels and endpoints.</title>
    <defs><marker id={markerId} viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z" /></marker></defs>
    {graph.edges.map((edge, index) => {
      const from = layout.positions.get(edge.from)!;
      const to = layout.positions.get(edge.to)!;
      const dx = to.x - from.x, dy = to.y - from.y;
      const distance = Math.hypot(dx, dy);
      const isLoop = edge.from === edge.to;
      const ux = distance ? dx / distance : 0, uy = distance ? dy / distance : 0;
      // Intersect the node rectangles (half-width 79, half-height 28).
      const inset = distance ? Math.min(80 / (Math.abs(ux) || 1e-9), 29 / (Math.abs(uy) || 1e-9)) : 0;
      const sx = from.x + ux * inset, sy = from.y + uy * inset;
      const tx = to.x - ux * inset, ty = to.y - uy * inset;
      const slot = layout.slots.get(index)!;
      // Normalize orientation so reverse and parallel relations get distinct
      // curves without duplicating either endpoint's identity.
      const bend = (slot.index - (slot.total - 1) / 2) * 30 * (edge.from < edge.to ? 1 : -1);
      const cx = (sx + tx) / 2 - uy * bend, cy = (sy + ty) / 2 + ux * bend;
      const loopRise = 125 + slot.index * 10;
      const path = isLoop ? `M ${from.x - 35} ${from.y - 28} C ${from.x - 115} ${from.y - loopRise}, ${from.x + 115} ${from.y - loopRise}, ${from.x + 35} ${from.y - 28}` : `M ${sx} ${sy} Q ${cx} ${cy} ${tx} ${ty}`;
      const t = slot.total > 1 ? .25 + .5 * slot.index / (slot.total - 1) : .5;
      const labelX = isLoop ? from.x : (1 - t) ** 2 * sx + 2 * (1 - t) * t * cx + t ** 2 * tx;
      const labelY = isLoop ? from.y - 102 - slot.index * 7.5 : (1 - t) ** 2 * sy + 2 * (1 - t) * t * cy + t ** 2 * ty;
      return <g key={index} className="eg-svg-edge"><path d={path} markerEnd={`url(#${markerId})`} /><title>{`${edge.from} → ${edge.to}: ${edge.relation} ×${edge.count}${edge.argument_index !== null ? `, argument ${edge.argument_index}` : ""}${edge.state !== null ? `, state ${edge.state}` : ""}`}</title><text x={labelX} y={labelY - 4} textAnchor="middle">{short(edge.relation, 23)} ×{String(edge.count)}</text></g>;
    })}
    {[...layout.nodes].map(([id, node]) => {
      const position = layout.positions.get(id)!;
      return <g key={id} transform={`translate(${position.x},${position.y})`} className={`eg-svg-node${id === graph.center.id ? " eg-center" : ""}${!node?.defined ? " eg-missing" : ""}`} role="button" tabIndex={0} aria-label={`Inspect ${id}${!node?.defined ? ", missing definition" : ""}`} onClick={() => onSelect(id)} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); onSelect(id); } }}>
        <title>{`${id} · ${node?.kind ?? "missing endpoint"} · ${node?.label ?? "no definition"}${node?.preview !== null && node?.preview !== undefined ? ` · ${node.preview}` : ""}`}</title><rect x="-79" y="-28" width="158" height="56" rx="6" /><text className="eg-svg-id" textAnchor="middle" y="-7" textLength={id.length > 23 ? 145 : undefined} lengthAdjust="spacingAndGlyphs">{id}</text><text textAnchor="middle" y="10">{short(node?.label || node?.kind || "missing endpoint", 25)}</text>{!node?.defined && <text textAnchor="middle" y="23" className="eg-svg-missing-label">missing definition</text>}
      </g>;
    })}
  </svg></div>;
}
