//! Immutable, capture-scoped lazy dataflow graph queries. Not a temporal call tree.
use facet::Facet;

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphSite {
    pub module: u64,
    pub id: u64,
    pub defined: bool,
    pub kind: String,
    pub name: String,
    pub file: String,
    pub start: u64,
    pub end: u64,
    pub line: u64,
    pub column: u64,
    pub params: Vec<String>,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphAttribute {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphNode {
    /// Capture-local typed ID, e.g. invocation:1 or thunk:1. Never an address.
    pub id: String,
    pub kind: String,
    pub label: String,
    pub defined: bool,
    pub seq: u64,
    pub ts_ns: u64,
    pub site: Option<GraphSite>,
    /// Already-observed text only. Equal text does not imply equal identity.
    pub preview: Option<String>,
    pub preview_truncated: bool,
    pub attributes: Vec<GraphAttribute>,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub relation: String,
    pub argument_index: Option<u64>,
    pub state: Option<i64>,
    pub count: u64,
    pub first_seq: u64,
    pub last_seq: u64,
    pub first_ts_ns: u64,
    pub last_ts_ns: u64,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphExecution {
    pub seq: u64,
    pub ts_ns: u64,
    pub workers: u64,
    pub status: String,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphSummary {
    /// Import identity, distinct even when reopening the same path.
    pub capture_id: u64,
    pub path: String,
    pub semantics: String,
    pub scope: String,
    /// Exact optional producer include selector; completeness covers only this selected stream.
    pub selection: Option<String>,
    pub max_bytes: Option<u64>,
    /// Intentional producer stop (e.g. byte_budget), distinct from malformed input.
    pub partial_reason: Option<String>,
    pub complete: bool,
    pub records: u64,
    pub nodes: u64,
    pub relations: u64,
    pub invocations: u64,
    pub force_requests: u64,
    /// Cache result=0 outcomes, not a thunk evaluation/claim count.
    pub cache_computes: u64,
    pub cache_hits: u64,
    pub cache_waits: u64,
    pub missing_endpoints: u64,
    /// Execution outcome is separate from stream completeness. Status is producer-defined.
    pub executions: Vec<GraphExecution>,
    pub diagnostic_count: u64,
    /// Bounded diagnostic examples; diagnostic_count includes all occurrences.
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphNodePage {
    pub total: u64,
    pub offset: u64,
    pub nodes: Vec<GraphNode>,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphCallable {
    pub site: GraphSite,
    /// Exact recorded entries, not estimated evaluations or sampled time.
    pub invocations: u64,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphCallablePage {
    pub total: u64,
    pub offset: u64,
    pub callables: Vec<GraphCallable>,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphNeighborhood {
    pub center: GraphNode,
    /// Unique nodes, shared between incoming and outgoing relations.
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Pagination is over aggregated relations, not over requests.
    pub total_relations: u64,
    pub offset: u64,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct GraphSearchQuery {
    pub capture_id: u64,
    pub query: String,
    pub kind: String,
    pub offset: u64,
    pub limit: u32,
}
#[derive(Clone, Debug, Default, Facet)]
pub struct GraphInvocationQuery {
    pub capture_id: u64,
    pub module: u64,
    pub site: u64,
    pub offset: u64,
    pub limit: u32,
}
#[derive(Clone, Debug, Default, Facet)]
pub struct GraphNeighborhoodQuery {
    pub capture_id: u64,
    pub node: String,
    pub direction: String,
    pub offset: u64,
    pub limit: u32,
}

#[vox::service]
pub trait EvalGraph {
    /// Read a server-local JSONL file. Does not change active recordings.
    async fn import_path(&self, path: String) -> Result<GraphSummary, String>;
    async fn captures(&self) -> Vec<GraphSummary>;
    /// Explicit release only; graph data is never silently evicted.
    async fn close(&self, capture_id: u64) -> Result<(), String>;
    async fn summary(&self, capture_id: u64) -> Result<GraphSummary, String>;
    /// kind: empty/all, invocation, thunk, or unresolved. limit <= 200.
    async fn search(&self, params: GraphSearchQuery) -> Result<GraphNodePage, String>;
    async fn callables(
        &self,
        capture_id: u64,
        query: String,
        offset: u64,
        limit: u32,
    ) -> Result<GraphCallablePage, String>;
    /// site is module:id; filters recorded invocation nodes by exact source site.
    async fn invocations(&self, params: GraphInvocationQuery) -> Result<GraphNodePage, String>;
    async fn inspect(&self, capture_id: u64, node: String) -> Result<GraphNode, String>;
    /// direction: both/incoming/outgoing. limit <= 200 aggregated relations.
    async fn neighborhood(
        &self,
        params: GraphNeighborhoodQuery,
    ) -> Result<GraphNeighborhood, String>;
}
