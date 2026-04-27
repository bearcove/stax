//! Schema for the nperf live RPC service.
//!
//! This crate is intentionally tiny: it holds only the `#[vox::service]`
//! trait + the wire types. Both `nperf-live` (the runtime that implements
//! and serves the trait) and `xtask` (which generates TypeScript bindings
//! from the trait) depend on this crate. Keeping the schema in its own
//! crate lets `xtask` skip the heavy runtime deps (tokio, transports, etc.)
//! that `nperf-live` pulls in.

use facet::Facet;

#[derive(Clone, Debug, Facet)]
pub struct TopEntry {
    pub address: u64,
    /// Wall-clock time attributed to this address as a leaf frame
    /// (sum across samples whose stack ended here), in nanoseconds.
    pub self_duration_ns: u64,
    /// Wall-clock time attributed to this address as any frame on
    /// the stack, in nanoseconds.
    pub total_duration_ns: u64,
    /// Demangled symbol name when the live binary registry has the
    /// containing image loaded. `None` for JIT'd code, kernel frames,
    /// or images that haven't been observed yet.
    pub function_name: Option<String>,
    /// Basename of the image (e.g. "libsystem_malloc.dylib"). Same
    /// availability semantics as `function_name`.
    pub binary: Option<String>,
    /// True when the containing binary is the main executable rather
    /// than a system / runtime dylib. The frontend uses this to colour
    /// target-code rows distinctly.
    pub is_main: bool,
    /// Source language inferred from demangling — `"rust"`, `"cpp"`,
    /// `"swift"`, etc. The frontend uses this to pick a glyph;
    /// `"unknown"` when no demangler claimed the symbol.
    pub language: String,
    /// CPU cycles attributed to this symbol's leaf samples, summed
    /// from Apple Silicon's fixed PMU counter 0. 0 on Linux / when
    /// PMC sampling is unavailable.
    pub self_cycles: u64,
    /// Same idea but for instructions retired (fixed counter 1).
    pub self_instructions: u64,
    /// L1D cache misses on loads attributed to leaf samples (from a
    /// configurable PMU counter). 0 when the event didn't resolve on
    /// this chip.
    pub self_l1d_misses: u64,
    /// Branch mispredicts attributed to leaf samples.
    pub self_branch_mispreds: u64,
    /// Cycles attributed to every sample that traversed this symbol
    /// (matches `total_duration_ns` semantics). Lets the frontend
    /// compute inclusive IPC.
    pub total_cycles: u64,
    pub total_instructions: u64,
    pub total_l1d_misses: u64,
    pub total_branch_mispreds: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct TopUpdate {
    /// Total wall-clock time covered by the underlying samples (sum
    /// of every sample's `duration_ns`), in nanoseconds. Use as the
    /// denominator for "X% of wall time" displays.
    pub total_duration_ns: u64,
    pub entries: Vec<TopEntry>,
}

/// Sort key for the top-N list. Truncation happens after sorting, so
/// `ByTotal` will surface rows that are pure inner frames (high total,
/// zero self) which `BySelf` would push past the limit.
#[derive(Clone, Copy, Debug, Facet)]
#[repr(u8)]
pub enum TopSort {
    BySelf = 0,
    ByTotal = 1,
}

/// One node in the call-tree flamegraph. Address 0 is reserved for the
/// synthetic root that aggregates all stacks. Children sum to (or are
/// less than, after pruning) the parent's `duration_ns`.
///
/// `function_name`, `binary`, and `language` are indices into the
/// containing `FlamegraphUpdate.strings` / `NeighborsUpdate.strings`
/// table — interning saves ~50 bytes per node on the wire when most
/// nodes resolve to the same handful of (function, binary) pairs.
#[derive(Clone, Debug, Facet)]
pub struct FlameNode {
    pub address: u64,
    /// Wall-clock time spent at (or under) this node, in nanoseconds.
    /// Sum of `duration_ns` across every sample whose stack passed
    /// through this node. Flame width is proportional to this.
    pub duration_ns: u64,
    pub function_name: Option<u32>,
    pub binary: Option<u32>,
    pub is_main: bool,
    pub language: u32,
    /// PMU counter sums across every sample that traversed this
    /// node. Lets the flamegraph colour-by-event mode fall straight
    /// out of the existing tree (cycles for IPC, l1d_misses for
    /// memory-stall heatmaps, branch_mispreds for control-flow
    /// pressure).
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,
    pub children: Vec<FlameNode>,
}

#[derive(Clone, Debug, Facet)]
pub struct FlamegraphUpdate {
    /// Total wall-clock time covered by samples in this snapshot, in
    /// nanoseconds. Equals `root.duration_ns` and acts as the
    /// denominator for percentage labels.
    pub total_duration_ns: u64,
    /// Deduplicated string table: `FlameNode.function_name`,
    /// `binary`, and `language` are indices into this. A typical
    /// session has on the order of ~50 unique (function, binary)
    /// pairs that would otherwise repeat across thousands of nodes.
    pub strings: Vec<String>,
    pub root: FlameNode,
}

/// One row in a "who woke this thread?" panel. Aggregated server-side
/// across the wakee's wakeup ledger, grouped by (waker_tid,
/// waker_function). The leaf frame is what gets named so a user sees
/// e.g. "tid 5103 / dispatch_async_f · 24 wakeups" -- the function
/// where the wake-up call was issued.
#[derive(Clone, Debug, Facet)]
pub struct WakerEntry {
    pub waker_tid: u32,
    pub waker_address: u64,
    pub waker_function_name: Option<String>,
    pub waker_binary: Option<String>,
    pub language: String,
    pub count: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct WakersUpdate {
    pub wakee_tid: u32,
    pub total_wakeups: u64,
    pub entries: Vec<WakerEntry>,
}

#[derive(Clone, Debug, Facet)]
pub struct ThreadInfo {
    pub tid: u32,
    pub name: Option<String>,
    /// Wall-clock time covered by this thread's samples, in
    /// nanoseconds. Used by the thread switcher to rank threads.
    pub duration_ns: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct ThreadsUpdate {
    pub threads: Vec<ThreadInfo>,
}

/// One time bucket on the timeline.
#[derive(Clone, Debug, Facet)]
pub struct TimelineBucket {
    /// Bucket start, in nanoseconds since the recording started (i.e.
    /// since the first sample).
    pub start_ns: u64,
    /// Wall-clock time attributed to this bucket, in nanoseconds.
    /// Sum of `duration_ns` across samples whose timestamp fell into
    /// this bucket. With samples weighted by their duration, this
    /// directly represents activity per bucket — high bars = busy
    /// intervals.
    pub duration_ns: u64,
}

/// A pair of (start, end) timestamps in ns, both relative to the
/// recording start (the timestamp of the first sample). End-exclusive.
#[derive(Clone, Debug, Facet)]
pub struct TimeRange {
    pub start_ns: u64,
    pub end_ns: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct SymbolRef {
    pub function_name: Option<String>,
    pub binary: Option<String>,
}

/// Which subset of samples to count.
///
/// `Both` is the default "wall-clock" view -- on-CPU PET samples plus
/// the synthesised off-CPU samples that fill in time the thread spent
/// blocked. `OnCpu` reproduces what samply / Instruments' Time
/// Profiler show; `OffCpu` is the inverted view where only blocked
/// stacks are counted.
#[derive(Clone, Copy, Debug, Facet)]
#[repr(u8)]
pub enum SampleMode {
    Both = 0,
    OnCpu = 1,
    OffCpu = 2,
}

/// Filter applied at query time over the raw sample log. When all
/// fields are at their defaults, the server hits the fast pre-aggregated
/// path; any non-default field forces re-aggregation.
#[derive(Clone, Debug, Facet)]
pub struct LiveFilter {
    pub time_range: Option<TimeRange>,
    /// Drop any sample whose stack contains *any* of these symbols.
    pub exclude_symbols: Vec<SymbolRef>,
    /// Restrict to on-CPU samples (what samply sees), off-CPU samples
    /// (where the thread was blocked), or both (wall-clock).
    pub sample_mode: SampleMode,
}

/// Bundle of "what to look at" knobs shared by every view
/// subscription. Bundled into one struct because vox/facet's tuple
/// bound caps method arities at 4.
#[derive(Clone, Debug, Facet)]
pub struct ViewParams {
    /// Filter to one thread's samples; `None` aggregates across all.
    pub tid: Option<u32>,
    pub filter: LiveFilter,
}

#[derive(Clone, Debug, Facet)]
pub struct TimelineUpdate {
    /// Width of each bucket in nanoseconds.
    pub bucket_size_ns: u64,
    /// Recording duration so the UI can show "Xs elapsed" without
    /// computing it client-side.
    pub recording_duration_ns: u64,
    /// Total wall-clock time covered by samples on the timeline
    /// (sum of `duration_ns` across `buckets`).
    pub total_duration_ns: u64,
    /// Buckets in chronological order, dense (zero buckets in the
    /// middle are emitted so the UI can map x-position → time
    /// directly).
    pub buckets: Vec<TimelineBucket>,
}

/// kcachegrind-style "family tree" of a symbol's neighbors.
///
/// `callers_tree` is rooted at the target. Its children are direct
/// callers (one level up the stack); their children are the callers'
/// callers; and so on. So the deeper you go, the further from the
/// target — i.e. the tree grows *outward toward main*.
///
/// `callees_tree` is also rooted at the target. Its children are
/// direct callees; its grandchildren are their callees. So the deeper
/// you go, the further into the call stack — i.e. the tree grows
/// *outward toward leaf frames*.
///
/// Both trees are keyed by symbol (multiple addresses inside the same
/// function merge), so recursion / multiple call sites all roll up.
/// Counts are pruned at ~0.5% of `own_count` to bound the wire size.
#[derive(Clone, Debug, Facet)]
pub struct NeighborsUpdate {
    /// Shared string table for all FlameNode references in this
    /// update plus the target's own symbol fields.
    pub strings: Vec<String>,
    /// Resolved name of the target symbol; index into `strings`.
    /// `None` for unresolved addresses (JIT, kernel frames, etc.).
    pub function_name: Option<u32>,
    pub binary: Option<u32>,
    pub is_main: bool,
    pub language: u32,
    /// Total wall-clock time attributed to this symbol, in
    /// nanoseconds (sum across every address resolving to it).
    pub own_duration_ns: u64,
    pub callers_tree: FlameNode,
    pub callees_tree: FlameNode,
}

/// Source-line header attached to the first instruction generated from
/// a given (file, line) pair. The frontend renders one of these as a
/// banner row above the asm row whenever the source location changes
/// between consecutive instructions.
#[derive(Clone, Debug, Facet)]
pub struct SourceHeader {
    pub file: String,
    pub line: u32,
    /// Highlighted source-line snippet (arborium custom-tag HTML); empty
    /// when the file couldn't be loaded (build-machine-relative paths,
    /// missing source on this box, etc.).
    pub html: String,
}

/// One disassembled instruction with its current sample count.
#[derive(Clone, Debug, Facet)]
pub struct AnnotatedLine {
    pub address: u64,
    /// HTML-highlighted assembly text. Uses arborium's default
    /// `CustomElements` format (`<a-k>mov</a-k>` etc.); the frontend
    /// styles those tags via the generated theme.css. Render with
    /// `dangerouslySetInnerHTML`.
    pub html: String,
    /// Wall-clock time attributed to this instruction as a leaf, in
    /// nanoseconds.
    pub self_duration_ns: u64,
    /// Set on the first instruction emitted for a new source location.
    /// `None` for instructions that share their source line with the
    /// previous instruction, and for binaries without DWARF.
    pub source_header: Option<SourceHeader>,
}

#[derive(Clone, Debug, Facet)]
pub struct AnnotatedView {
    /// Best-effort symbol name (or hex string fallback).
    pub function_name: String,
    pub language: String,
    /// Address the disassembly starts at. Used by the client to mark which
    /// line corresponds to the original query address.
    pub base_address: u64,
    pub queried_address: u64,
    pub lines: Vec<AnnotatedLine>,
}

#[vox::service]
pub trait Profiler {
    /// Snapshot of the top-N functions, ranked by `sort`. `params`
    /// bundles thread/time/exclude filters.
    async fn top(
        &self,
        limit: u32,
        sort: TopSort,
        params: ViewParams,
    ) -> Vec<TopEntry>;

    async fn subscribe_top(
        &self,
        limit: u32,
        sort: TopSort,
        params: ViewParams,
        output: vox::Tx<TopUpdate>,
    );

    /// Total wall-clock time covered by samples, across every
    /// thread, in nanoseconds. The recorder's `T s elapsed` reading
    /// is `total_duration_ns / 1e9`.
    async fn total_duration_ns(&self) -> u64;

    async fn subscribe_annotated(
        &self,
        address: u64,
        params: ViewParams,
        output: vox::Tx<AnnotatedView>,
    );

    async fn subscribe_flamegraph(
        &self,
        params: ViewParams,
        output: vox::Tx<FlamegraphUpdate>,
    );

    async fn subscribe_threads(&self, output: vox::Tx<ThreadsUpdate>);

    /// Always relative to the full recording (no `filter`); brush
    /// selection happens on top of the unfiltered timeline.
    async fn subscribe_timeline(
        &self,
        tid: Option<u32>,
        output: vox::Tx<TimelineUpdate>,
    );

    async fn subscribe_neighbors(
        &self,
        address: u64,
        params: ViewParams,
        output: vox::Tx<NeighborsUpdate>,
    );

    /// Stream "who woke this thread?" updates: top wakers grouped by
    /// (waker_tid, waker_function), aggregated from the kperf
    /// MACH_MAKERUNNABLE wakeup edges. The wakee's tid is required;
    /// `None` produces an empty update (we don't aggregate across
    /// threads).
    async fn subscribe_wakers(
        &self,
        wakee_tid: u32,
        output: vox::Tx<WakersUpdate>,
    );

    /// Pause / resume live ingestion. While paused, new samples and
    /// wakeup edges from the recorder get dropped before reaching
    /// the aggregator -- frozen views, no client churn -- but the
    /// recorder keeps running underneath, the binary registry keeps
    /// updating, and disassembly / source / annotation queries
    /// continue to work against the existing (frozen) data.
    async fn set_paused(&self, paused: bool);
    async fn is_paused(&self) -> bool;
}

/// All service descriptors exposed by nperf-live; the codegen iterates over
/// this list.
pub fn all_services() -> Vec<&'static vox::session::ServiceDescriptor> {
    vec![profiler_service_descriptor()]
}
