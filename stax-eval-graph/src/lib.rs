//! File-based Vix v1 import. IDs are capture-local identities, never previews or pointers.
//! No graph records are evicted. Limits reject the import rather than truncate its graph.
use serde_json::Value;
pub use stax_live_proto::{
    GraphAttribute, GraphCallable, GraphCallablePage, GraphEdge, GraphExecution, GraphNeighborhood,
    GraphNode, GraphNodePage, GraphSite, GraphSummary,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

type SiteKey = (u64, u64);
type EdgeKey = (String, String, String, Option<u64>, Option<i64>);

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub bytes: u64,
    pub record_bytes: usize,
    pub records: u64,
    pub nodes: usize,
    pub relations: usize,
    pub executions: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: 256 * 1024 * 1024,
            record_bytes: 1024 * 1024,
            records: 2_000_000,
            nodes: 1_000_000,
            relations: 2_000_000,
            executions: 1000,
        }
    }
}

pub struct Graph {
    pub summary: GraphSummary,
    nodes: BTreeMap<String, GraphNode>,
    sites: BTreeMap<SiteKey, GraphSite>,
    modules: BTreeSet<u64>,
    counts: BTreeMap<SiteKey, u64>,
    edges: BTreeMap<EdgeKey, GraphEdge>,
    adjacency: BTreeMap<String, Vec<EdgeKey>>,
}

fn num(v: &Value, key: &str) -> Result<u64, String> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{key} must be an unsigned integer"))
}
fn id(v: &Value, key: &str) -> Result<u64, String> {
    let n = num(v, key)?;
    if n == 0 {
        return Err(format!("{key} must be nonzero"));
    }
    Ok(n)
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    let s = v
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} must be a string"))?;
    if s.len() > 16 * 1024 {
        return Err(format!("{key} exceeds 16 KiB text limit"));
    }
    Ok(s)
}
fn attr(node: &mut GraphNode, key: &str, value: impl ToString) {
    if let Some(a) = node.attributes.iter_mut().find(|a| a.name == key) {
        a.value = value.to_string();
    } else {
        node.attributes.push(GraphAttribute {
            name: key.into(),
            value: value.to_string(),
        });
    }
}
fn page(offset: u64, limit: u32) -> Result<(usize, usize), String> {
    if limit == 0 || limit > 200 {
        return Err("limit must be 1..=200".into());
    }
    Ok((
        usize::try_from(offset).map_err(|_| "offset too large")?,
        limit as usize,
    ))
}

impl Graph {
    pub fn from_path(path: &Path, capture_id: u64, limits: Limits) -> Result<Self, String> {
        if !std::fs::metadata(path)
            .map_err(|e| e.to_string())?
            .is_file()
        {
            return Err("capture path must be a regular file".into());
        }
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("capture path must be a regular file".into());
        }
        if meta.len() > limits.bytes {
            return Err(format!("capture exceeds {} byte limit", limits.bytes));
        }
        Self::read(
            BufReader::new(file),
            capture_id,
            path.to_string_lossy().into_owned(),
            limits,
        )
    }

    pub fn read(
        mut reader: impl BufRead,
        capture_id: u64,
        path: String,
        limits: Limits,
    ) -> Result<Self, String> {
        let mut graph = Self {
            summary: GraphSummary {
                capture_id,
                path,
                semantics: "lazy-dataflow".into(),
                scope: "Recorded instrumented body starts and within-body force requests; not a complete call tree or per-call timing".into(),
                ..Default::default()
            },
            nodes: BTreeMap::new(),
            sites: BTreeMap::new(),
            modules: BTreeSet::new(),
            counts: BTreeMap::new(),
            edges: BTreeMap::new(),
            adjacency: BTreeMap::new(),
        };
        let mut bytes = 0u64;
        let mut last_seq = 0u64;
        let mut last_ts = 0u64;
        let mut footer = false;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader
                .by_ref()
                .take(limits.record_bytes as u64 + 1)
                .read_until(b'\n', &mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            if bytes > limits.bytes {
                return Err(format!("capture exceeds {} byte limit", limits.bytes));
            }
            if n > limits.record_bytes {
                return Err(format!("record exceeds {} byte limit", limits.record_bytes));
            }
            graph.summary.records += 1;
            if graph.summary.records > limits.records {
                return Err(format!("capture exceeds {} record limit", limits.records));
            }
            let v: Value = match serde_json::from_slice(&line) {
                Ok(v) => v,
                Err(e) if graph.summary.records == 1 => {
                    return Err(format!("invalid header JSON: {e}"));
                }
                Err(e) => {
                    graph.diagnostic(format!(
                        "line {}: invalid/truncated JSON: {e}",
                        graph.summary.records
                    ));
                    continue;
                }
            };
            if graph.summary.records == 1 {
                if text(&v, "event")? != "header"
                    || text(&v, "format")? != "vix-eval-graph"
                    || num(&v, "version")? != 1
                    || text(&v, "semantics")? != "lazy-dataflow"
                {
                    return Err("expected vix-eval-graph version 1 lazy-dataflow header".into());
                }
                if v.get("scope").is_some() {
                    graph.summary.scope = text(&v, "scope")?.into();
                }
                if v.get("include").is_some_and(|v| !v.is_null()) {
                    graph.summary.selection = Some(text(&v, "include")?.into());
                }
                if v.get("max_bytes").is_some() {
                    graph.summary.max_bytes = Some(num(&v, "max_bytes")?);
                }
                if num(&v, "records")? != 1 {
                    graph.diagnostic("header records must be 1".into());
                }
            }
            let result = (|| {
                let seq = id(&v, "seq")?;
                let ts = num(&v, "ts_ns")?;
                if seq != last_seq.saturating_add(1) || seq != graph.summary.records {
                    graph.diagnostic(format!(
                        "line {}: non-contiguous seq {seq}",
                        graph.summary.records
                    ));
                }
                if ts < last_ts {
                    graph.diagnostic(format!("seq {seq}: timestamp went backwards"));
                }
                last_seq = seq;
                last_ts = ts;
                if footer {
                    graph.diagnostic(format!("seq {seq}: data after footer"));
                }
                if text(&v, "event")? == "footer" {
                    if footer {
                        graph.diagnostic("duplicate footer".into());
                    }
                    footer = true;
                    match v.get("complete").and_then(Value::as_bool) {
                        Some(true) => {}
                        Some(false) => {
                            let reason = if v.get("reason").is_some() {
                                text(&v, "reason")?
                            } else {
                                "unspecified"
                            };
                            graph.summary.partial_reason = Some(reason.into());
                            graph.diagnostic(format!(
                                "producer intentionally stopped partial capture: {reason}"
                            ));
                        }
                        None => graph.diagnostic("footer complete must be boolean".into()),
                    }
                    if num(&v, "records")? != graph.summary.records || num(&v, "records")? != seq {
                        graph.diagnostic("footer record count mismatch".into());
                    }
                    return Ok(());
                }
                graph.record(&v, seq, ts)
            })();
            if let Err(e) = result {
                graph.diagnostic(format!("line {}: {e}", graph.summary.records));
            }
            if graph.nodes.len() > limits.nodes
                || graph.edges.len() > limits.relations
                || graph.sites.len() > limits.nodes
                || graph.modules.len() > limits.nodes
                || graph.summary.executions.len() > limits.executions
            {
                return Err("graph exceeds node/site/module/relation/execution limit; no partial import retained".into());
            }
        }
        if graph.summary.records == 0 {
            return Err("empty capture: missing header".into());
        }
        if !footer {
            graph.diagnostic("missing complete footer (capture may be truncated)".into());
        }
        graph.finish();
        graph.summary.complete = footer && graph.summary.diagnostic_count == 0;
        Ok(graph)
    }

    fn diagnostic(&mut self, message: String) {
        self.summary.diagnostic_count += 1;
        if self.summary.diagnostics.len() < 100 {
            let mut end = message.len().min(512);
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            self.summary.diagnostics.push(message[..end].to_owned());
        }
    }
    fn placeholder(&mut self, key: &str, kind: &str) {
        self.nodes.entry(key.into()).or_insert_with(|| GraphNode {
            id: key.into(),
            kind: kind.into(),
            label: format!("missing {key}"),
            ..Default::default()
        });
    }
    fn define(&mut self, mut node: GraphNode) -> bool {
        if self.nodes.get(&node.id).is_some_and(|n| n.defined) {
            self.diagnostic(format!("duplicate node definition {}", node.id));
            return false;
        }
        if let Some(old) = self.nodes.get(&node.id) {
            node.preview.clone_from(&old.preview);
            node.preview_truncated = old.preview_truncated;
            node.attributes.extend(old.attributes.clone());
        }
        self.nodes.insert(node.id.clone(), node);
        true
    }
    fn thunk(v: &Value, seq: u64) -> Result<(String, bool), String> {
        let t = v.get("thunk").ok_or("missing thunk")?;
        let identity = text(t, "identity")?;
        match identity {
            "allocated" => Ok((format!("thunk:{}", id(t, "id")?), true)),
            "unknown_external" | "null" if t.get("id").is_some_and(Value::is_null) => {
                Ok((format!("unresolved:{seq}:{identity}"), false))
            }
            _ => Err("invalid thunk identity/id combination".into()),
        }
    }
    fn record(&mut self, v: &Value, seq: u64, ts: u64) -> Result<(), String> {
        match text(v, "event")? {
            "header" if seq == 1 => {}
            "header" => return Err("unexpected header".into()),
            "module" => {
                let module = id(v, "module")?;
                if !self.modules.insert(module) {
                    self.diagnostic(format!("duplicate module {module}"));
                }
            }
            "site" => {
                let key = (id(v, "module")?, id(v, "site")?);
                let params = v
                    .get("params")
                    .and_then(Value::as_array)
                    .ok_or("params must be an array")?;
                if params.len() > 256 {
                    return Err("site has more than 256 parameters".into());
                }
                let params = params
                    .iter()
                    .map(|p| {
                        p.as_str()
                            .filter(|s| s.len() <= 1024)
                            .map(String::from)
                            .ok_or("params must contain strings of <=1024 bytes".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let site = GraphSite {
                    module: key.0,
                    id: key.1,
                    defined: true,
                    kind: text(v, "kind")?.into(),
                    name: text(v, "name")?.into(),
                    file: text(v, "file")?.into(),
                    start: num(v, "start")?,
                    end: num(v, "end")?,
                    line: num(v, "line")?,
                    column: num(v, "column")?,
                    params,
                };
                if site.end < site.start {
                    return Err("source end precedes start".into());
                }
                if self.sites.contains_key(&key) {
                    self.diagnostic(format!("duplicate site {}:{}", key.0, key.1));
                } else {
                    self.sites.insert(key, site);
                }
            }
            "allocation" => {
                let thunk = id(v, "thunk")?;
                let words = num(v, "words")?;
                let born = match v.get("born_ns") {
                    Some(value) => Some(
                        value
                            .as_number()
                            .ok_or("born_ns must be an unsigned integer")?
                            .to_string()
                            .parse::<u128>()
                            .map_err(|_| "born_ns must be an unsigned u128 integer")?,
                    ),
                    None => None,
                };
                let mut node = GraphNode {
                    id: format!("thunk:{thunk}"),
                    kind: "thunk".into(),
                    label: format!("thunk {thunk}"),
                    defined: true,
                    seq,
                    ts_ns: ts,
                    ..Default::default()
                };
                attr(&mut node, "words", words);
                if let Some(born) = born {
                    attr(&mut node, "born_ns", born);
                }
                self.define(node);
            }
            "invocation" => {
                let inv = id(v, "invocation")?;
                let key = (id(v, "module")?, id(v, "site")?);
                let arity = num(v, "arity")?;
                let mut node = GraphNode {
                    id: format!("invocation:{inv}"),
                    kind: "invocation".into(),
                    label: format!("invocation {inv}"),
                    defined: true,
                    seq,
                    ts_ns: ts,
                    site: Some(GraphSite {
                        module: key.0,
                        id: key.1,
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                attr(&mut node, "arity", arity);
                if self.define(node) {
                    *self.counts.entry(key).or_default() += 1;
                    self.summary.invocations += 1;
                }
            }
            relation @ ("argument" | "force" | "created") => {
                let inv = format!("invocation:{}", id(v, "invocation")?);
                let (thunk, allocated) = Self::thunk(v, seq)?;
                let index = if relation == "argument" {
                    Some(num(v, "index")?)
                } else {
                    None
                };
                let state = if relation == "argument" || relation == "force" {
                    let n = v
                        .get("state")
                        .and_then(Value::as_i64)
                        .ok_or("state must be a signed integer")?;
                    Some(n)
                } else {
                    None
                };
                let preview = if relation == "argument" {
                    match v.get("preview") {
                        Some(p) if p.is_null() => None,
                        Some(p) => {
                            let t = text(p, "text")?;
                            if t.len() > 256 {
                                return Err("preview exceeds 256 UTF-8 bytes".into());
                            }
                            let truncated = p
                                .get("truncated")
                                .and_then(Value::as_bool)
                                .ok_or("preview.truncated must be boolean")?;
                            Some((t.to_owned(), truncated))
                        }
                        None => return Err("argument missing preview (use null)".into()),
                    }
                } else {
                    None
                };
                self.placeholder(&inv, "invocation");
                self.placeholder(&thunk, if allocated { "thunk" } else { "unresolved" });
                if let Some((text, truncated)) = preview {
                    let n = self.nodes.get_mut(&thunk).unwrap();
                    if n.preview.is_none() {
                        n.preview = Some(text);
                        n.preview_truncated = truncated;
                    }
                }
                let key = (inv.clone(), thunk.clone(), relation.into(), index, state);
                let edge = self.edges.entry(key).or_insert(GraphEdge {
                    from: inv,
                    to: thunk,
                    relation: relation.into(),
                    argument_index: index,
                    state,
                    count: 0,
                    first_seq: seq,
                    last_seq: seq,
                    first_ts_ns: ts,
                    last_ts_ns: ts,
                });
                edge.count += 1;
                edge.last_seq = seq;
                edge.last_ts_ns = ts;
                if relation == "force" {
                    self.summary.force_requests += 1;
                }
            }
            "cache" => {
                let inv = format!("invocation:{}", id(v, "invocation")?);
                let result = num(v, "result")?;
                let name = match result {
                    0 => {
                        self.summary.cache_computes += 1;
                        "cache_compute"
                    }
                    1 => {
                        self.summary.cache_hits += 1;
                        "cache_hit"
                    }
                    2 => {
                        self.summary.cache_waits += 1;
                        "cache_wait"
                    }
                    _ => return Err("unknown cache result".into()),
                };
                self.placeholder(&inv, "invocation");
                let node = self.nodes.get_mut(&inv).unwrap();
                let n = node
                    .attributes
                    .iter()
                    .find(|a| a.name == name)
                    .and_then(|a| a.value.parse::<u64>().ok())
                    .unwrap_or(0)
                    + 1;
                attr(node, name, n);
            }
            "execution" => {
                self.summary.executions.push(GraphExecution {
                    seq,
                    ts_ns: ts,
                    workers: num(v, "workers")?,
                    status: text(v, "status")?.into(),
                });
            }
            other => return Err(format!("unknown event {other:?}")),
        }
        Ok(())
    }
    fn finish(&mut self) {
        let missing_modules: Vec<_> = self
            .sites
            .keys()
            .map(|(m, _)| *m)
            .chain(self.counts.keys().map(|(m, _)| *m))
            .filter(|m| !self.modules.contains(m))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for m in missing_modules {
            self.summary.missing_endpoints += 1;
            self.diagnostic(format!("missing module {m}"));
        }
        let missing_sites: Vec<_> = self
            .counts
            .keys()
            .filter(|k| !self.sites.contains_key(k))
            .copied()
            .collect();
        for (m, s) in missing_sites {
            self.summary.missing_endpoints += 1;
            self.diagnostic(format!("missing site {m}:{s}"));
        }
        let missing: Vec<_> = self
            .nodes
            .values()
            .filter(|n| !n.defined)
            .map(|n| n.id.clone())
            .collect();
        self.summary.missing_endpoints += missing.len() as u64;
        for n in missing {
            self.diagnostic(format!("missing endpoint {n}"));
        }
        for node in self.nodes.values_mut() {
            if let Some(key) = node.site.as_ref().map(|s| (s.module, s.id)) {
                if let Some(site) = self.sites.get(&key) {
                    node.label = format!(
                        "{} · {}",
                        site.name.chars().take(80).collect::<String>(),
                        node.id
                    );
                }
            }
        }
        for key in self.edges.keys() {
            self.adjacency
                .entry(key.0.clone())
                .or_default()
                .push(key.clone());
            if key.1 != key.0 {
                self.adjacency
                    .entry(key.1.clone())
                    .or_default()
                    .push(key.clone());
            }
        }
        self.summary.nodes = self.nodes.len() as u64;
        self.summary.relations = self.edges.len() as u64;
    }
    fn decorated(&self, node: &GraphNode) -> GraphNode {
        let mut node = node.clone();
        if let Some(site) = node
            .site
            .as_ref()
            .and_then(|s| self.sites.get(&(s.module, s.id)))
        {
            node.site = Some(site.clone());
        }
        node
    }
    pub fn inspect(&self, node: &str) -> Result<GraphNode, String> {
        self.nodes
            .get(node)
            .map(|n| self.decorated(n))
            .ok_or_else(|| format!("unknown node {node:?}"))
    }
    pub fn search(
        &self,
        query: &str,
        kind: &str,
        offset: u64,
        limit: u32,
    ) -> Result<GraphNodePage, String> {
        if !matches!(kind, "" | "all" | "invocation" | "thunk" | "unresolved") {
            return Err("invalid node kind".into());
        }
        let (skip, take) = page(offset, limit)?;
        let query = query.to_lowercase();
        let matched = self.nodes.values().filter(|n| {
            (kind.is_empty() || kind == "all" || kind == n.kind)
                && (node_matches(n, &query)
                    || n.site
                        .as_ref()
                        .and_then(|s| self.sites.get(&(s.module, s.id)))
                        .is_some_and(|s| {
                            s.file.to_lowercase().contains(&query)
                                || s.name.to_lowercase().contains(&query)
                        }))
        });
        let total = matched.clone().count() as u64;
        Ok(GraphNodePage {
            total,
            offset,
            nodes: matched
                .skip(skip)
                .take(take)
                .map(|n| self.decorated(n))
                .collect(),
        })
    }
    pub fn callables(
        &self,
        query: &str,
        offset: u64,
        limit: u32,
    ) -> Result<GraphCallablePage, String> {
        let (skip, take) = page(offset, limit)?;
        let query = query.to_lowercase();
        let mut rows: Vec<_> = self
            .counts
            .iter()
            .map(|(&(m, s), &count)| GraphCallable {
                site: self.sites.get(&(m, s)).cloned().unwrap_or(GraphSite {
                    module: m,
                    id: s,
                    name: format!("missing site {m}:{s}"),
                    ..Default::default()
                }),
                invocations: count,
            })
            .filter(|c| {
                format!(
                    "{} {} {}:{}",
                    c.site.name, c.site.file, c.site.module, c.site.id
                )
                .to_lowercase()
                .contains(&query)
            })
            .collect();
        rows.sort_by(|a, b| {
            b.invocations
                .cmp(&a.invocations)
                .then((a.site.module, a.site.id).cmp(&(b.site.module, b.site.id)))
        });
        Ok(GraphCallablePage {
            total: rows.len() as u64,
            offset,
            callables: rows.into_iter().skip(skip).take(take).collect(),
        })
    }
    pub fn invocations(
        &self,
        module: u64,
        site: u64,
        offset: u64,
        limit: u32,
    ) -> Result<GraphNodePage, String> {
        let (skip, take) = page(offset, limit)?;
        let rows = self.nodes.values().filter(|n| {
            n.kind == "invocation"
                && n.site
                    .as_ref()
                    .is_some_and(|s| s.module == module && s.id == site)
        });
        Ok(GraphNodePage {
            total: rows.clone().count() as u64,
            offset,
            nodes: rows
                .skip(skip)
                .take(take)
                .map(|n| self.decorated(n))
                .collect(),
        })
    }
    pub fn neighborhood(
        &self,
        node: &str,
        direction: &str,
        offset: u64,
        limit: u32,
    ) -> Result<GraphNeighborhood, String> {
        let (skip, take) = page(offset, limit)?;
        if !matches!(direction, "both" | "incoming" | "outgoing") {
            return Err("invalid direction".into());
        }
        let center = self.inspect(node)?;
        let rows = self.adjacency.get(node).into_iter().flatten().filter(|k| {
            direction == "both"
                || (direction == "incoming" && k.1 == node)
                || (direction == "outgoing" && k.0 == node)
        });
        let total_relations = rows.clone().count() as u64;
        let edges: Vec<_> = rows
            .skip(skip)
            .take(take)
            .map(|k| self.edges[k].clone())
            .collect();
        let ids: BTreeSet<_> = std::iter::once(node.to_owned())
            .chain(edges.iter().flat_map(|e| [e.from.clone(), e.to.clone()]))
            .collect();
        Ok(GraphNeighborhood {
            center,
            nodes: ids
                .iter()
                .map(|id| self.decorated(&self.nodes[id]))
                .collect(),
            edges,
            total_relations,
            offset,
        })
    }
}
fn node_matches(n: &GraphNode, q: &str) -> bool {
    n.id.to_lowercase().contains(q)
        || n.label.to_lowercase().contains(q)
        || n.preview
            .as_ref()
            .is_some_and(|s| s.to_lowercase().contains(q))
        || n.site
            .as_ref()
            .is_some_and(|s| s.file.to_lowercase().contains(q))
}

#[cfg(test)]
mod tests;
