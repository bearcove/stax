//! Schema for the stax live RPC service.
//!
//! This crate is intentionally tiny: it holds only the `#[vox::service]`
//! trait + the wire types. Both `stax-live` (the runtime that implements
//! and serves the trait) and `xtask` (which generates TypeScript bindings
//! from the trait) depend on this crate. Keeping the schema in its own
//! crate lets `xtask` skip the heavy runtime deps (tokio, transports, etc.)
//! that `stax-live` pulls in.

use facet::Facet;

/// Off-CPU time at a stack node, broken down by why the thread was
/// off-CPU. Sum across all fields = total off-CPU time.
///
/// The breakdown is the wire's main lever for "what is this thread
/// actually doing?": idle parking is uninteresting, lock contention
/// is usually the thing to chase, IO and IPC tell different stories.
/// The UI renders flame boxes color-segmented by these fields.
#[derive(Clone, Copy, Debug, Default, Facet)]
pub struct OffCpuBreakdown {
    /// Voluntarily parked waiting for new work
    /// (cond-vars, ulock, workq idle).
    pub idle_ns: u64,
    /// Blocked on a mutex / rwlock owned by another thread.
    pub lock_ns: u64,
    /// Blocked on a semaphore.
    pub semaphore_ns: u64,
    /// Blocked in mach_msg waiting for a reply.
    pub ipc_ns: u64,
    /// Blocking read syscall.
    pub io_read_ns: u64,
    /// Blocking write syscall.
    pub io_write_ns: u64,
    /// fd-readiness wait (poll/select/kevent).
    pub readiness_ns: u64,
    /// Explicit sleep.
    pub sleep_ns: u64,
    /// Connection-setup blocking (connect/accept/open).
    pub connect_ns: u64,
    /// Couldn't classify the leaf frame, or no PET stack was
    /// available to consult.
    pub other_ns: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct TopEntry {
    pub address: u64,
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
    /// `"swift"`, etc.
    pub language: String,

    /// On-CPU time attributed to this symbol as a leaf frame, ns.
    pub self_on_cpu_ns: u64,
    /// On-CPU time attributed to this symbol as any frame on the
    /// stack, ns.
    pub total_on_cpu_ns: u64,
    /// Off-CPU breakdown attributed as a leaf.
    pub self_off_cpu: OffCpuBreakdown,
    /// Off-CPU breakdown attributed as any frame on the stack.
    pub total_off_cpu: OffCpuBreakdown,
    /// PET stack-walk hits where this symbol was the leaf.
    pub self_pet_samples: u64,
    /// PET stack-walk hits where this symbol appeared anywhere.
    pub total_pet_samples: u64,
    /// Off-CPU intervals attributed to this symbol as a leaf.
    pub self_off_cpu_intervals: u64,
    /// Off-CPU intervals attributed to this symbol anywhere.
    pub total_off_cpu_intervals: u64,

    /// CPU cycles attributed to this symbol's leaf samples, summed
    /// from Apple Silicon's fixed PMU counter 0. 0 on Linux / when
    /// PMC sampling is unavailable. Off-CPU contributes nothing here.
    pub self_cycles: u64,
    pub self_instructions: u64,
    pub self_l1d_misses: u64,
    pub self_branch_mispreds: u64,
    pub total_cycles: u64,
    pub total_instructions: u64,
    pub total_l1d_misses: u64,
    pub total_branch_mispreds: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct TopUpdate {
    /// Total on-CPU time across every entry in this snapshot, ns.
    /// Bounded above by `cores × wall_time`.
    pub total_on_cpu_ns: u64,
    /// Total off-CPU time across every entry, ns. Per-reason
    /// breakdown across the whole snapshot.
    pub total_off_cpu: OffCpuBreakdown,
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
/// synthetic root that aggregates all stacks.
///
/// Each node carries on-CPU time and off-CPU time *separately*, with
/// the off-CPU portion broken down by reason. Children sum to (or are
/// less than, after pruning) the parent's totals, per-field. The UI
/// picks which field drives flame-box width and can color-segment a
/// box across the off-CPU breakdown.
///
/// `function_name`, `binary`, and `language` are indices into the
/// containing `FlamegraphUpdate.strings` / `NeighborsUpdate.strings`
/// table — interning saves ~50 bytes per node on the wire when most
/// nodes resolve to the same handful of (function, binary) pairs.
#[derive(Clone, Debug, Facet)]
pub struct FlameNode {
    pub address: u64,
    pub function_name: Option<u32>,
    pub binary: Option<u32>,
    pub is_main: bool,
    pub language: u32,

    /// Real CPU time at (or under) this stack, in nanoseconds.
    /// Computed from SCHED on-CPU intervals: each interval's duration
    /// is distributed evenly across the PET stack samples that fell
    /// inside it, then credited to every node on those stacks.
    pub on_cpu_ns: u64,
    /// Off-CPU time at this stack, by reason. Computed from SCHED
    /// off-CPU intervals using the leaf frame at the moment the
    /// thread blocked.
    pub off_cpu: OffCpuBreakdown,
    /// Number of PET stack-walk hits at (or under) this node. Lets
    /// the UI tell apart "10ms × 1 sample" (low confidence) from
    /// "10ms × 10 samples" (high confidence) for the same on-cpu
    /// number.
    pub pet_samples: u64,
    /// Number of off-CPU intervals attributed to this stack. Hot
    /// blocking-site indicator independent of total time.
    pub off_cpu_intervals: u64,

    /// PMU counter sums across PET samples that traversed this node.
    /// Off-CPU contributes nothing (no PMC during sleep). Lets the
    /// flamegraph colour-by-event mode fall straight out of the tree.
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,

    pub children: Vec<FlameNode>,
}

#[derive(Clone, Debug, Facet)]
pub struct FlamegraphUpdate {
    /// Total on-CPU time covered by this snapshot's intervals, ns.
    /// Equals `root.on_cpu_ns`.
    pub total_on_cpu_ns: u64,
    /// Total off-CPU time, by reason. Equals `root.off_cpu`.
    pub total_off_cpu: OffCpuBreakdown,
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
    /// On-CPU time for this thread, ns. Bounded by wall_time on a
    /// single core (≤ wall_time × cores in practice -- a thread can
    /// only be on one CPU at a time, so per-thread on_cpu_ns ≤
    /// wall_time).
    pub on_cpu_ns: u64,
    /// Off-CPU breakdown for this thread.
    pub off_cpu: OffCpuBreakdown,
    /// Total PET stack-walk hits we caught for this thread.
    pub pet_samples: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct ThreadsUpdate {
    pub threads: Vec<ThreadInfo>,
}

/// One time bucket on the timeline. On-CPU and off-CPU show up as
/// separately-stacked layers so the UI can distinguish "the system
/// was busy here" from "lots of threads were parked here."
#[derive(Clone, Debug, Facet)]
pub struct TimelineBucket {
    /// Bucket start, in nanoseconds since the recording started (i.e.
    /// since the first sample).
    pub start_ns: u64,
    /// On-CPU time attributed to this bucket from SCHED on-CPU
    /// intervals that overlapped it.
    pub on_cpu_ns: u64,
    /// Off-CPU time, summed across all reasons.
    pub off_cpu_ns: u64,
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

/// Why a thread was off-CPU. Classified at the moment the thread
/// blocked from the leaf user-space frame on its stack at that
/// instant. The 10 categories cover the macOS / pthread / BSD
/// surface area; anything that doesn't match a known leaf goes to
/// `Other`.
///
/// Order matters: variants are repr(u8) and serialised by index.
/// Append new variants at the end -- inserting in the middle would
/// renumber everything past the insert and break older clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum OffCpuReason {
    /// Voluntarily idle: thread parked waiting for new work.
    /// `__psynch_cvwait`, `__ulock_wait`, `__workq_kernreturn`.
    /// The thread isn't blocked ON anything -- it's waiting to be
    /// told there's work. Cheap and usually uninteresting unless
    /// it's the *target* code's path through it.
    Idle = 0,
    /// Lock contention: thread wants to run but is blocked on a
    /// mutex / rwlock / spinlock owned by someone else. This is
    /// usually the off-CPU you actually want to fix.
    /// `__psynch_mutexwait`, `__psynch_rw_*`.
    LockWait = 1,
    /// Semaphore wait (explicit count-based sync).
    /// `__semwait_signal`, `semaphore_wait_trap`.
    SemaphoreWait = 2,
    /// Mach IPC blocked in mach_msg waiting for a reply.
    /// `mach_msg2_trap`, `mach_msg_overwrite_trap`.
    IpcWait = 3,
    /// Read-side IO syscall: `read`, `recv`, `recvfrom`, `recvmsg`,
    /// `pread`. (Includes blocking-mode socket reads.)
    IoRead = 4,
    /// Write-side IO syscall: `write`, `send`, `sendmsg`, `pwrite`.
    IoWrite = 5,
    /// fd-readiness wait: `select`, `pselect`, `poll`, `ppoll`,
    /// `kevent`, `kevent_id`, `kevent_qos`.
    Readiness = 6,
    /// Explicit sleep: `nanosleep`, `usleep`.
    Sleep = 7,
    /// Connection-setup blocking: `connect`, `accept`, `__open_nocancel`,
    /// dyld lazy-bind faults, etc.
    ConnectionSetup = 8,
    /// Couldn't classify the leaf frame, or no PET stack was
    /// available before the thread went off-CPU.
    Other = 9,
}

/// Filter applied at query time over the raw event log. When all
/// fields are at their defaults, the server hits the fast pre-aggregated
/// path; any non-default field forces re-aggregation.
///
/// Note: there's no on-CPU / off-CPU mode flag here. Every flame node
/// carries on/off-CPU and per-reason durations as separate fields, so
/// "what to render as box width" is purely a frontend concern -- the
/// server always serves the full breakdown.
#[derive(Clone, Debug, Facet)]
pub struct LiveFilter {
    pub time_range: Option<TimeRange>,
    /// Drop any sample / interval whose stack contains *any* of these
    /// symbols.
    pub exclude_symbols: Vec<SymbolRef>,
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
    /// Total on-CPU time across the timeline.
    pub total_on_cpu_ns: u64,
    /// Total off-CPU time across the timeline (all reasons summed).
    pub total_off_cpu_ns: u64,
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
    /// On-CPU time attributed to this symbol (sum across every
    /// address resolving to it).
    pub own_on_cpu_ns: u64,
    /// Off-CPU breakdown for this symbol.
    pub own_off_cpu: OffCpuBreakdown,
    /// PET stack-walk hits at this symbol.
    pub own_pet_samples: u64,
    /// Off-CPU intervals attributed to this symbol.
    pub own_off_cpu_intervals: u64,
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

/// One disassembled instruction with its sampled hit data.
#[derive(Clone, Debug, Facet)]
pub struct AnnotatedLine {
    pub address: u64,
    /// HTML-highlighted assembly text. Uses arborium's default
    /// `CustomElements` format (`<a-k>mov</a-k>` etc.); the frontend
    /// styles those tags via the generated theme.css. Render with
    /// `dangerouslySetInnerHTML`.
    pub html: String,
    /// On-CPU time attributed to this instruction as a leaf, ns.
    /// Heatmap source.
    pub self_on_cpu_ns: u64,
    /// PET stack-walk hits at this instruction. With on_cpu_ns this
    /// gives both "how much time" and "how confident."
    pub self_pet_samples: u64,
    /// Set on the first instruction emitted for a new source location.
    /// `None` for instructions that share their source line with the
    /// previous instruction, and for binaries without DWARF.
    pub source_header: Option<SourceHeader>,
}

/// One off-CPU interval surfaced by `subscribe_intervals`.
/// Recording-relative timestamps (ns since the first sample).
#[derive(Clone, Debug, Facet)]
pub struct IntervalEntry {
    pub tid: u32,
    pub start_ns: u64,
    pub duration_ns: u64,
    pub reason: OffCpuReason,
    /// Who woke this thread out of the off-CPU interval, if
    /// MACH_MAKERUNNABLE caught it. None for intervals that closed
    /// without a captured wakeup edge (open at end-of-recording, or
    /// the wakeup batch hadn't drained when the interval ended).
    pub waker_tid: Option<u32>,
    pub waker_address: Option<u64>,
    pub waker_function_name: Option<u32>,
    pub waker_binary: Option<u32>,
}

#[derive(Clone, Debug, Facet)]
pub struct IntervalListUpdate {
    /// Shared string table for waker function/binary references.
    pub strings: Vec<String>,
    /// Total intervals matching the query (entries may be capped by
    /// the server before sending; this is the pre-cap count).
    pub total_intervals: u64,
    /// Sum of `duration_ns` across all matching intervals.
    pub total_duration_ns: u64,
    /// Per-reason breakdown of the total.
    pub by_reason: OffCpuBreakdown,
    pub entries: Vec<IntervalEntry>,
}

/// One PET stack-walk hit surfaced by `subscribe_pet_samples`.
#[derive(Clone, Debug, Facet)]
pub struct PetSampleEntry {
    pub tid: u32,
    /// Recording-relative ns.
    pub timestamp_ns: u64,
    /// Cycles delta since the previous PET tick on this thread (0
    /// when PMU sampling isn't available).
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct PetSampleListUpdate {
    pub total_samples: u64,
    pub entries: Vec<PetSampleEntry>,
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

    /// Total on-CPU time across every thread, in nanoseconds.
    /// Bounded by `cores × wall_time` (you can't be on more than one
    /// CPU at a time, and there are only so many CPUs). Useful for
    /// "X CPU-seconds across the recording" displays.
    async fn total_on_cpu_ns(&self) -> u64;

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

    /// Stream the off-CPU intervals attributed to a single stack
    /// node, in chronological order. Lets the UI drill into a flame
    /// box and see "this stack was blocked here for 12ms, here for
    /// 30ms..." with each interval colored by reason and clickable
    /// to surface the waker. `flame_key` matches the `r/2/1/0`
    /// addressing the frontend already uses for focus.
    async fn subscribe_intervals(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<IntervalListUpdate>,
    );

    /// Stream the PET stack-walk hits attributed to a single stack
    /// node, in chronological order. Symmetric counterpart to
    /// `subscribe_intervals` for the on-CPU side.
    async fn subscribe_pet_samples(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<PetSampleListUpdate>,
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

/// All service descriptors exposed by stax-live; the codegen iterates over
/// this list.
pub fn all_services() -> Vec<&'static vox::session::ServiceDescriptor> {
    vec![profiler_service_descriptor()]
}
