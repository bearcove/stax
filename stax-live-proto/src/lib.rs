//! Schema for the stax live RPC service.
//!
//! This crate is intentionally tiny: it holds only the `#[vox::service]`
//! trait + the wire types. Both `stax-live` (the runtime that implements
//! and serves the trait) and `xtask` (which generates TypeScript bindings
//! from the trait) depend on this crate. Keeping the schema in its own
//! crate lets `xtask` skip the heavy runtime deps (tokio, transports, etc.)
//! that `stax-live` pulls in.

use std::collections::BTreeMap;

use facet::Facet;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum TargetLaneKind {
    #[default]
    Generic = 0,
    Metal = 1,
}

macro_rules! target_id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Facet)]
        pub struct $name {
            pub raw: u64,
        }

        impl $name {
            pub const fn new(raw: u64) -> Self {
                Self { raw }
            }
        }
    };
}

target_id_type!(TargetRuntimeId);
target_id_type!(TargetLaneId);
target_id_type!(TargetQueueId);
target_id_type!(TargetCommandBufferId);
target_id_type!(TargetDispatchId);
target_id_type!(TargetShaderId);
target_id_type!(TargetSourceId);
target_id_type!(TargetAttachmentId);
target_id_type!(TargetCounterSetId);
target_id_type!(TargetCounterSampleId);

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
    /// Present for target-span synthetic symbols. This is explicit
    /// metadata from the cooperating target, not inferred from names.
    pub target_kind: Option<TargetLaneKind>,

    /// Active time attributed to this symbol as a leaf frame, ns.
    /// For real threads this is CPU time; for cooperating target
    /// lanes it also includes target-reported execution spans.
    pub self_on_cpu_ns: u64,
    /// Active time attributed to this symbol as any frame on the
    /// stack, ns.
    pub total_on_cpu_ns: u64,
    /// Portion of `self_on_cpu_ns` that came from target-reported
    /// execution spans, ns.
    pub self_target_ns: u64,
    /// Portion of `total_on_cpu_ns` that came from target-reported
    /// execution spans, ns.
    pub total_target_ns: u64,
    /// Off-CPU breakdown attributed as a leaf.
    pub self_off_cpu: OffCpuBreakdown,
    /// Off-CPU breakdown attributed as any frame on the stack.
    pub total_off_cpu: OffCpuBreakdown,
    /// PET stack-walk hits where this symbol was the leaf.
    pub self_pet_samples: u64,
    /// PET stack-walk hits where this symbol appeared anywhere.
    pub total_pet_samples: u64,
    /// Target-reported spans where this symbol was the leaf.
    pub self_target_spans: u64,
    /// Target-reported spans where this symbol appeared anywhere.
    pub total_target_spans: u64,
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
    /// Total active time across every entry in this snapshot, ns.
    /// Real CPU intervals plus cooperating target execution spans.
    pub total_on_cpu_ns: u64,
    /// Portion of `total_on_cpu_ns` contributed by target-reported
    /// execution spans.
    pub total_target_ns: u64,
    /// Count of target-reported execution spans included in this
    /// snapshot.
    pub total_target_spans: u64,
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
/// Each node carries active time, target-executor time, and off-CPU
/// time separately, with the off-CPU portion broken down by reason.
/// Children sum to (or are less than, after pruning) the parent's
/// totals, per-field. The UI picks which field drives flame-box width
/// and can color-segment a box across the off-CPU breakdown.
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
    pub target_kind: Option<TargetLaneKind>,

    /// Active time at (or under) this stack, in nanoseconds. For real
    /// CPU work this comes from SCHED on-CPU intervals; for
    /// cooperating target lanes this includes exact target-reported
    /// span durations.
    pub on_cpu_ns: u64,
    /// Portion of `on_cpu_ns` that came from target-reported execution
    /// spans.
    pub target_ns: u64,
    /// Off-CPU time at this stack, by reason. Computed from SCHED
    /// off-CPU intervals using the leaf frame at the moment the
    /// thread blocked.
    pub off_cpu: OffCpuBreakdown,
    /// Number of PET stack-walk hits at (or under) this node. Lets
    /// the UI tell apart "10ms × 1 sample" (low confidence) from
    /// "10ms × 10 samples" (high confidence) for the same on-cpu
    /// number.
    pub pet_samples: u64,
    /// Number of target-reported spans at (or under) this node.
    pub target_spans: u64,
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
    /// Total active time covered by this snapshot's intervals, ns.
    /// Equals `root.on_cpu_ns`.
    pub total_on_cpu_ns: u64,
    /// Portion of `total_on_cpu_ns` contributed by target-reported
    /// execution spans. Equals `root.target_ns`.
    pub total_target_ns: u64,
    /// Count of target-reported spans included in the tree.
    pub total_target_spans: u64,
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
    /// Active time for this row, ns. For CPU thread rows this includes
    /// real on-CPU time plus any origin-linked target duration for
    /// compatibility with aggregate active-time views; subtract
    /// `target_ns` for real CPU-busy time. For synthetic target lanes
    /// this is lane execution time.
    pub on_cpu_ns: u64,
    /// Portion of `on_cpu_ns` that came from target-reported execution
    /// spans.
    pub target_ns: u64,
    /// Off-CPU breakdown for this thread.
    pub off_cpu: OffCpuBreakdown,
    /// Total PET stack-walk hits we caught for this thread.
    pub pet_samples: u64,
    /// Target-reported spans included in this thread/lane row.
    pub target_spans: u64,
    /// Present for synthetic target lanes. This is explicit metadata
    /// from the cooperating target, not inferred from the lane name.
    pub target_kind: Option<TargetLaneKind>,
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
    /// Active time attributed to this bucket from SCHED on-CPU
    /// intervals and target spans that overlapped it.
    pub on_cpu_ns: u64,
    /// Portion of `on_cpu_ns` that came from target-reported spans.
    pub target_ns: u64,
    /// Off-CPU time, summed across all reasons.
    pub off_cpu_ns: u64,
}

/// Per-lane target time for a timeline row. The `buckets` vector has
/// the same length and bucket size as `TimelineUpdate.buckets`; each
/// entry is target-span nanoseconds for this lane in that bucket.
#[derive(Clone, Debug, Facet)]
pub struct TargetLaneTimeline {
    pub tid: u32,
    pub lane_name: Option<u32>,
    pub target_kind: TargetLaneKind,
    pub total_target_ns: u64,
    pub target_spans: u64,
    pub buckets: Vec<u64>,
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
    /// Optional historical run id to query without changing the server's
    /// selected query state. `None` means the live/current query state.
    pub run: Option<RunId>,
    /// Filter to one thread's samples; `None` aggregates across all.
    pub tid: Option<u32>,
    pub filter: LiveFilter,
}

#[derive(Clone, Copy, Debug, Facet)]
pub struct RunViewParams {
    /// Optional historical run id to query without changing the server's
    /// selected query state. `None` means the live/current query state.
    pub run: Option<RunId>,
}

#[derive(Clone, Copy, Debug, Facet)]
pub struct TimelineParams {
    /// Optional historical run id to query without changing the server's
    /// selected query state. `None` means the live/current query state.
    pub run: Option<RunId>,
    /// Filter to one thread or synthetic lane; `None` aggregates all lanes.
    pub tid: Option<u32>,
}

#[derive(Clone, Debug, Facet)]
pub struct TimelineUpdate {
    /// Shared string table for timeline lane names.
    pub strings: Vec<String>,
    /// Width of each bucket in nanoseconds.
    pub bucket_size_ns: u64,
    /// Recording duration so the UI can show "Xs elapsed" without
    /// computing it client-side.
    pub recording_duration_ns: u64,
    /// Total active time across the timeline.
    pub total_on_cpu_ns: u64,
    /// Portion of `total_on_cpu_ns` contributed by target-reported
    /// execution spans.
    pub total_target_ns: u64,
    /// Total off-CPU time across the timeline (all reasons summed).
    pub total_off_cpu_ns: u64,
    /// Buckets in chronological order, dense (zero buckets in the
    /// middle are emitted so the UI can map x-position → time
    /// directly).
    pub buckets: Vec<TimelineBucket>,
    /// Top target lanes by target duration, each with a dense
    /// per-bucket target-time series.
    pub target_lanes: Vec<TargetLaneTimeline>,
    /// Agent/user-placed markers, in timestamp order. The timeline
    /// renders these as vertical anchors so a stall can be labelled
    /// (`stax mark freeze`) and later queried (`--window freeze..`)
    /// without converting wall-clock to recording time by hand.
    pub markers: Vec<RunMarker>,
}

/// A named point in recording time, dropped by an agent or user to
/// anchor later queries. `timestamp_ns` is recording-relative (ns
/// since the first sample), matching `TimeRange` and the timeline, so
/// a marker can directly seed a `--window` bound.
#[derive(Clone, Debug, Facet)]
pub struct RunMarker {
    /// Recording-relative timestamp (ns since the first sample).
    pub timestamp_ns: u64,
    /// Free-form label, e.g. "freeze", "click", "attach".
    pub label: String,
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

/// One classified run of text in highlighted output. Adjacent tokens
/// with the same `class_` are coalesced server-side; gaps the
/// highlighter didn't classify (whitespace, plain identifiers, …) ride
/// in their own `Plain` token. Frontends translate `class_` → their
/// own styling.
#[derive(Clone, Debug, Facet)]
pub struct Token {
    pub text: String,
    pub kind: TokenClass,
}

/// Canonical syntax-highlight class. Mirrors arborium's `ThemeSlot`
/// vocabulary; `Plain` is the implicit "no styling" class for text
/// between styled spans.
///
/// `repr(u8)` and append-only — older clients should treat unknown
/// variants as `Plain`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum TokenClass {
    Plain = 0,
    Keyword,
    Function,
    String,
    Comment,
    Type,
    Variable,
    Constant,
    Number,
    Operator,
    Punctuation,
    Property,
    Attribute,
    Tag,
    Macro,
    Label,
    Namespace,
    Constructor,
    Title,
    Strong,
    Emphasis,
    Link,
    Literal,
    Strikethrough,
    DiffAdd,
    DiffDelete,
    Embedded,
    Error,
}

/// Source-line header attached to the first instruction generated from
/// a given (file, line) pair. The frontend renders one of these as a
/// banner row above the asm row whenever the source location changes
/// between consecutive instructions.
#[derive(Clone, Debug, Facet)]
pub struct SourceHeader {
    pub file: String,
    pub line: u32,
    /// Highlighted source-line snippet, as classified token runs.
    /// Empty when the file couldn't be loaded (build-machine-relative
    /// paths, missing source on this box, etc.).
    pub tokens: Vec<Token>,
}

/// One disassembled instruction with its sampled hit data.
#[derive(Clone, Debug, Facet)]
pub struct AnnotatedLine {
    pub address: u64,
    /// Highlighted assembly text, as classified token runs.
    pub tokens: Vec<Token>,
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

/// Aggregated target/executor work grouped by lane, span name, and origin.
#[derive(Clone, Debug, Facet)]
pub struct TargetSpanGroup {
    pub tid: u32,
    pub lane_name: Option<u32>,
    pub target_kind: TargetLaneKind,
    pub span_name: Option<u32>,
    pub origin_tid: Option<u32>,
    pub origin_linked: bool,
    pub origin_address: Option<u64>,
    pub origin_function_name: Option<u32>,
    pub origin_binary: Option<u32>,
    pub count: u64,
    pub total_duration_ns: u64,
    pub max_duration_ns: u64,
    /// Recording-relative ns for the newest span in this group.
    pub last_start_ns: u64,
}

/// One target/executor span surfaced by `subscribe_target_spans`.
#[derive(Clone, Debug, Facet)]
pub struct TargetSpanEntry {
    pub tid: u32,
    /// Recording-relative ns.
    pub start_ns: u64,
    pub duration_ns: u64,
    pub lane_name: Option<u32>,
    pub target_kind: TargetLaneKind,
    pub span_name: Option<u32>,
    pub origin_tid: Option<u32>,
    pub origin_linked: bool,
    pub origin_address: Option<u64>,
    pub origin_function_name: Option<u32>,
    pub origin_binary: Option<u32>,
}

#[derive(Clone, Debug, Facet)]
pub struct TargetSpanListUpdate {
    pub strings: Vec<String>,
    pub total_spans: u64,
    pub total_duration_ns: u64,
    pub groups: Vec<TargetSpanGroup>,
    pub entries: Vec<TargetSpanEntry>,
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

/// One basic block — a maximal straight-line sequence of instructions
/// that ends at a branch / return / call (or at a fallthrough into
/// another block's leader). `id` is dense (0..blocks.len) so edges
/// can index directly.
#[derive(Clone, Debug, Facet)]
pub struct BasicBlock {
    pub id: u32,
    /// Address of the first instruction.
    pub start_address: u64,
    /// Heatmap-bearing instructions, in program order. Same shape as
    /// `AnnotatedView.lines` so the renderer can reuse the row.
    pub lines: Vec<AnnotatedLine>,
}

/// One control-flow edge in the function-local CFG.
#[derive(Clone, Debug, Facet)]
pub struct CfgEdge {
    pub from_id: u32,
    pub to_id: u32,
    pub kind: CfgEdgeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum CfgEdgeKind {
    /// Unconditional control flow into the next block.
    Fallthrough = 0,
    /// Unconditional branch (e.g. `B`, `JMP`).
    Branch,
    /// Conditional branch's taken arm — the not-taken arm is a
    /// `Fallthrough` edge to the next block.
    ConditionalBranch,
    /// Recognised function call back to the next instruction. Only
    /// emitted when the call has a direct, in-function target — most
    /// calls leave the function and are not modeled.
    Call,
}

/// Function-scoped control-flow graph for `function_name @ entry_address`.
/// Returned by `Profiler::cfg`/`subscribe_cfg`; unlike `AnnotatedView`
/// the per-instruction stats are split across blocks the server has
/// already partitioned, so the client doesn't need to re-discover
/// branch boundaries.
#[derive(Clone, Debug, Facet)]
pub struct CfgUpdate {
    pub function_name: String,
    pub language: String,
    /// Address the function starts at. Block 0 begins here.
    pub base_address: u64,
    /// The address the client originally asked about; the renderer
    /// highlights whichever block contains it.
    pub queried_address: u64,
    /// Dense block list. The entry block is always `blocks[0]`; other
    /// blocks are in increasing-address order.
    pub blocks: Vec<BasicBlock>,
    pub edges: Vec<CfgEdge>,
}

/// Where target-side work was queued/submitted from.
///
/// This is optional provenance for execution lanes the CPU sampler
/// cannot directly observe. When a target can report the OS thread id
/// and timestamp at the moment it queued the work, stax can borrow the
/// nearest sampled CPU stack on that thread for provenance,
/// diagnostics, CPU-tid filtering, and web target-span details. Target
/// execution remains parallel lane work unless a richer integration
/// also reports matching CPU wait/completion evidence. `tid` is the same OS thread-id
/// namespace used by the recorder (Mach thread_id on macOS, gettid on
/// Linux). `timestamp_ns` is in the same clock domain as span
/// timestamps.
#[derive(Clone, Copy, Debug, Facet)]
pub struct TargetSpanOrigin {
    pub tid: u32,
    pub timestamp_ns: u64,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct TargetRuntimeRecord {
    pub runtime_id: TargetRuntimeId,
    pub name: String,
    pub kind: TargetLaneKind,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct TargetLaneRecord {
    pub lane_id: TargetLaneId,
    pub name: String,
    pub kind: TargetLaneKind,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct TargetQueueRecord {
    pub queue_id: TargetQueueId,
    pub runtime_id: Option<TargetRuntimeId>,
    pub lane_id: Option<TargetLaneId>,
    pub label: String,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct TargetCommandBufferRecord {
    pub command_buffer_id: TargetCommandBufferId,
    pub queue_id: Option<TargetQueueId>,
    pub label: String,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct TargetSourceRecord {
    pub source_id: TargetSourceId,
    /// Domain-neutral language label: "metal", "rust", "cpp", "shader-ir", ...
    pub language: String,
    pub path: Option<String>,
    pub content_hash: String,
    pub text: Option<String>,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct TargetShaderRecord {
    pub shader_id: TargetShaderId,
    pub name: String,
    pub display_name: Option<String>,
    pub source_id: Option<TargetSourceId>,
    pub source_start_line: Option<u32>,
    pub source_end_line: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum TargetAttachmentKind {
    #[default]
    Other = 0,
    Buffer = 1,
    Tensor = 2,
    Texture = 3,
    File = 4,
    Socket = 5,
    Request = 6,
    ModelLayer = 7,
    Batch = 8,
    RuntimeObject = 9,
}

#[derive(Clone, Debug, Facet)]
pub struct TargetAttachmentRecord {
    pub attachment_id: TargetAttachmentId,
    pub dispatch_id: Option<TargetDispatchId>,
    pub kind: TargetAttachmentKind,
    pub label: String,
    pub slot: Option<String>,
    pub size_bytes: Option<u64>,
    pub offset_bytes: Option<u64>,
    pub dtype: Option<String>,
    pub shape: Vec<u64>,
    pub role: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum TargetCounterUnit {
    #[default]
    Count = 0,
    Nanoseconds = 1,
    Ticks = 2,
    Cycles = 3,
    Bytes = 4,
    Percent = 5,
    Rate = 6,
    Other = 7,
}

#[derive(Clone, Debug, Facet)]
pub struct TargetCounterDefinition {
    pub name: String,
    pub unit: TargetCounterUnit,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Facet)]
pub struct TargetCounterSetRecord {
    pub counter_set_id: TargetCounterSetId,
    pub name: String,
    pub counters: Vec<TargetCounterDefinition>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum TargetCounterSamplePoint {
    #[default]
    RuntimeDefined = 0,
    BeforeDispatch = 1,
    AfterDispatch = 2,
    CommandBufferBegin = 3,
    CommandBufferEnd = 4,
    WaitBegin = 5,
    WaitEnd = 6,
}

#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum TargetCounterScalar {
    U64 { value: u64 },
    I64 { value: i64 },
    F64 { value: f64 },
}

#[derive(Clone, Debug, Facet)]
pub struct TargetCounterValue {
    pub name: String,
    pub unit: TargetCounterUnit,
    pub value: TargetCounterScalar,
}

#[derive(Clone, Debug, Facet)]
pub struct TargetCounterSampleRecord {
    pub counter_sample_id: TargetCounterSampleId,
    pub counter_set_id: TargetCounterSetId,
    pub dispatch_id: Option<TargetDispatchId>,
    pub command_buffer_id: Option<TargetCommandBufferId>,
    pub sample_point: TargetCounterSamplePoint,
    pub values: Vec<TargetCounterValue>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Facet)]
pub struct TargetDispatchRecord {
    pub dispatch_id: TargetDispatchId,
    pub lane_id: Option<TargetLaneId>,
    pub queue_id: Option<TargetQueueId>,
    pub command_buffer_id: Option<TargetCommandBufferId>,
    pub shader_id: Option<TargetShaderId>,
    pub source_id: Option<TargetSourceId>,
    pub name: String,
    pub start_ns: Option<u64>,
    pub end_ns: Option<u64>,
    pub dispatch_origin: Option<TargetSpanOrigin>,
    pub wait_origin: Option<TargetSpanOrigin>,
    pub completion_origin: Option<TargetSpanOrigin>,
    pub attachment_ids: Vec<TargetAttachmentId>,
    pub counter_sample_ids: Vec<TargetCounterSampleId>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct TargetRecordBatch {
    pub runtimes: Vec<TargetRuntimeRecord>,
    pub lanes: Vec<TargetLaneRecord>,
    pub queues: Vec<TargetQueueRecord>,
    pub command_buffers: Vec<TargetCommandBufferRecord>,
    pub dispatches: Vec<TargetDispatchRecord>,
    pub shaders: Vec<TargetShaderRecord>,
    pub sources: Vec<TargetSourceRecord>,
    pub attachments: Vec<TargetAttachmentRecord>,
    pub counter_sets: Vec<TargetCounterSetRecord>,
    pub counter_samples: Vec<TargetCounterSampleRecord>,
}

impl TargetRecordBatch {
    pub fn is_empty(&self) -> bool {
        self.runtimes.is_empty()
            && self.lanes.is_empty()
            && self.queues.is_empty()
            && self.command_buffers.is_empty()
            && self.dispatches.is_empty()
            && self.shaders.is_empty()
            && self.sources.is_empty()
            && self.attachments.is_empty()
            && self.counter_sets.is_empty()
            && self.counter_samples.is_empty()
    }
}

/// One execution span reported by an instrumented TARGET process —
/// e.g. a GPU kernel's execution window captured via Metal 4 timestamp
/// counters. Timestamps are absolute mach-derived nanoseconds, the same
/// clock domain as sampled stacks (Apple Silicon GPU timestamps share
/// mach_absolute_time's epoch and 24MHz rate), so spans land on the
/// recording timeline with no correlation step.
#[derive(Clone, Debug, Facet)]
pub struct TargetSpan {
    /// Symbolic name for the work (e.g. the Metal kernel function name).
    /// Becomes a synthetic symbol — top/flame/annotate group by it.
    pub name: String,
    pub start_ns: u64,
    pub end_ns: u64,
    /// Optional CPU-side queue/dispatch origin. When present and a
    /// nearby PET sample exists on that thread, stax can link the span
    /// back to the sampled CPU dispatch stack while keeping execution
    /// on its parallel synthetic lane.
    pub origin: Option<TargetSpanOrigin>,
    pub dispatch_id: Option<TargetDispatchId>,
    pub shader_id: Option<TargetShaderId>,
    pub source_id: Option<TargetSourceId>,
    pub wait_origin: Option<TargetSpanOrigin>,
    pub completion_origin: Option<TargetSpanOrigin>,
    pub attachment_ids: Vec<TargetAttachmentId>,
    pub counter_sample_ids: Vec<TargetCounterSampleId>,
}

impl TargetSpan {
    pub fn new(name: impl Into<String>, start_ns: u64, end_ns: u64) -> Self {
        Self {
            name: name.into(),
            start_ns,
            end_ns,
            origin: None,
            dispatch_id: None,
            shader_id: None,
            source_id: None,
            wait_origin: None,
            completion_origin: None,
            attachment_ids: Vec::new(),
            counter_sample_ids: Vec::new(),
        }
    }

    pub fn with_origin(mut self, origin: TargetSpanOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    pub fn with_dispatch_id(mut self, dispatch_id: TargetDispatchId) -> Self {
        self.dispatch_id = Some(dispatch_id);
        self
    }

    pub fn with_shader_id(mut self, shader_id: TargetShaderId) -> Self {
        self.shader_id = Some(shader_id);
        self
    }

    pub fn with_source_id(mut self, source_id: TargetSourceId) -> Self {
        self.source_id = Some(source_id);
        self
    }

    pub fn with_wait_origin(mut self, origin: TargetSpanOrigin) -> Self {
        self.wait_origin = Some(origin);
        self
    }

    pub fn with_completion_origin(mut self, origin: TargetSpanOrigin) -> Self {
        self.completion_origin = Some(origin);
        self
    }

    pub fn with_attachment_id(mut self, attachment_id: TargetAttachmentId) -> Self {
        self.attachment_ids.push(attachment_id);
        self
    }

    pub fn with_counter_sample_id(mut self, counter_sample_id: TargetCounterSampleId) -> Self {
        self.counter_sample_ids.push(counter_sample_id);
        self
    }
}

/// A batch of target-reported spans for one execution lane.
///
/// A lane is a GPU queue (or any other off-CPU executor) and maps onto a
/// SYNTHETIC THREAD in the existing aggregator model: the server
/// allocates a high pseudo-tid per (pid, lane), names it `lane`, and
/// records each reported span as one sample marker plus one attributed
/// synthetic execution interval. Every existing view (top, flamegraph,
/// timeline, threads) includes the lane with exact duration weighting
/// and span counts; no span-specific views exist.
#[derive(Clone, Debug, Facet)]
pub struct TargetSpanBatch {
    /// Reporting process id — spans are dropped unless this matches the
    /// active run's target.
    pub pid: u32,
    /// Execution lane name, e.g. "GPU tq1s".
    pub lane: String,
    pub lane_kind: TargetLaneKind,
    pub spans: Vec<TargetSpan>,
    pub records: TargetRecordBatch,
}

#[derive(Clone, Copy, Debug, Default, Facet)]
pub struct TargetReporterStats {
    /// Reporting process id — stats are ignored unless this matches the
    /// active run's target.
    pub pid: u32,
    pub batches_dropped_queue_full: u64,
    pub spans_dropped_queue_full: u64,
    pub batches_dropped_worker_disconnected: u64,
    pub spans_dropped_worker_disconnected: u64,
}

/// Target-facing ingest surface: the thing a profiled app latches onto.
/// Fire-and-forget; the target keeps running whether or not a recording
/// is active (the server drops batches with no matching run).
#[vox::service]
pub trait TargetIngest {
    async fn ingest(&self, batch: TargetSpanBatch);

    /// Snapshot of target-side reporter drops from stax-target's local
    /// bounded queue. Sent by the target worker while capture is active.
    async fn reporter_stats(&self, stats: TargetReporterStats);

    /// Capture gate, polled by targets (~1s): `true` iff an active run
    /// is recording `pid`. Targets use this to switch span capture on
    /// when a recording attaches and off (dropping the per-span capture
    /// cost) when it stops or detaches — the polling half of the
    /// target-latch contract.
    async fn should_report(&self, pid: u32) -> bool;
}

#[vox::service]
pub trait Profiler {
    /// Snapshot of the top-N functions, ranked by `sort`. `params`
    /// bundles thread/time/exclude filters.
    async fn top(&self, limit: u32, sort: TopSort, params: ViewParams) -> Vec<TopEntry>;

    /// One-shot top-function snapshot, including totals. UIs may poll
    /// this instead of opening a long-lived channel.
    async fn top_update(&self, limit: u32, sort: TopSort, params: ViewParams) -> TopUpdate;

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
    async fn total_on_cpu_ns(&self, params: RunViewParams) -> u64;

    async fn annotated(&self, address: u64, params: ViewParams) -> AnnotatedView;

    async fn subscribe_annotated(
        &self,
        address: u64,
        params: ViewParams,
        output: vox::Tx<AnnotatedView>,
    );

    /// Function-scoped CFG (basic blocks + edges) for the function
    /// containing `address`. Heatmap stats live on each block's
    /// `lines`, so subscribers can keep colours fresh as samples
    /// land.
    async fn cfg(&self, address: u64, params: ViewParams) -> CfgUpdate;

    async fn subscribe_cfg(&self, address: u64, params: ViewParams, output: vox::Tx<CfgUpdate>);

    async fn flamegraph(&self, params: ViewParams) -> FlamegraphUpdate;

    async fn subscribe_flamegraph(&self, params: ViewParams, output: vox::Tx<FlamegraphUpdate>);

    async fn threads(&self, params: RunViewParams) -> ThreadsUpdate;

    async fn subscribe_threads(&self, params: RunViewParams, output: vox::Tx<ThreadsUpdate>);

    /// Always relative to the full recording (no `filter`); brush
    /// selection happens on top of the unfiltered timeline.
    async fn timeline(&self, params: TimelineParams) -> TimelineUpdate;

    /// Always relative to the full recording (no `filter`); brush
    /// selection happens on top of the unfiltered timeline.
    async fn subscribe_timeline(&self, params: TimelineParams, output: vox::Tx<TimelineUpdate>);

    async fn neighbors(&self, address: u64, params: ViewParams) -> NeighborsUpdate;

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
    async fn wakers(&self, wakee_tid: u32, params: RunViewParams) -> WakersUpdate;

    /// Stream "who woke this thread?" updates: top wakers grouped by
    /// (waker_tid, waker_function), aggregated from the kperf
    /// MACH_MAKERUNNABLE wakeup edges. The wakee's tid is required;
    /// `None` produces an empty update (we don't aggregate across
    /// threads).
    async fn subscribe_wakers(
        &self,
        wakee_tid: u32,
        params: RunViewParams,
        output: vox::Tx<WakersUpdate>,
    );

    /// Stream the off-CPU intervals attributed to a single stack
    /// node, in chronological order. Lets the UI drill into a flame
    /// box and see "this stack was blocked here for 12ms, here for
    /// 30ms..." with each interval colored by reason and clickable
    /// to surface the waker. `flame_key` matches the `r/2/1/0`
    /// addressing the frontend already uses for focus.
    async fn intervals(&self, flame_key: String, params: ViewParams) -> IntervalListUpdate;

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
    async fn pet_samples(&self, flame_key: String, params: ViewParams) -> PetSampleListUpdate;

    /// Stream the PET stack-walk hits attributed to a single stack
    /// node, in chronological order. Symmetric counterpart to
    /// `subscribe_intervals` for the on-CPU side.
    async fn subscribe_pet_samples(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<PetSampleListUpdate>,
    );

    /// Stream target/executor spans attributed to a selected thread/lane
    /// and filter. This is the target-time counterpart to
    /// `subscribe_intervals`: each entry is one synthetic span interval
    /// with lane/span names and origin-link status.
    async fn target_spans(&self, flame_key: String, params: ViewParams) -> TargetSpanListUpdate;

    /// Stream target/executor spans attributed to a selected thread/lane
    /// and filter.
    async fn subscribe_target_spans(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<TargetSpanListUpdate>,
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

/// Stable handle for one run hosted by the server. Returned by
/// `RunControl::start_run` and accepted by every other run-scoped
/// query. New format / domain in the future; today it's just a u64
/// monotonically issued by the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Facet)]
pub struct RunId(pub u64);

/// Lifecycle phase of a hosted run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum RunState {
    /// Recording is in progress; samples are streaming in.
    Recording,
    /// The recorder reported it stopped (target exited, time limit hit,
    /// `stop_active` was called). Aggregator state is frozen but still
    /// queryable.
    Stopped,
}

/// Why a run stopped. Surfaced once the run transitions to
/// `RunState::Stopped`.
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum StopReason {
    /// The launched child exited (or the attached PID went away).
    TargetExited,
    /// `--time-limit` elapsed.
    TimeLimit,
    /// User Ctrl-C'd the recorder, or an agent called `stop_active`.
    UserStop,
    /// The recorder errored. `message` carries the human-readable
    /// detail.
    RecorderError { message: String },
}

#[derive(Clone, Debug, Facet)]
pub struct RunSummary {
    pub id: RunId,
    pub state: RunState,
    /// `None` while still recording.
    pub stop_reason: Option<StopReason>,
    /// Wall-clock start (unix nanos).
    pub started_at_unix_ns: u64,
    /// Wall-clock stop (unix nanos). `None` while still recording.
    pub stopped_at_unix_ns: Option<u64>,
    /// PID of the target process, if any. `None` for runs that
    /// haven't acquired a PID yet (very early in the lifecycle).
    pub target_pid: Option<u32>,
    /// Best-effort label derived from the launch command or attached
    /// PID's executable basename. Free-form; not guaranteed unique.
    pub label: String,
    /// PET stack-walk hits ingested so far. Sourced from kperf
    /// (kdebug PERF_CS_UHDR/UDATA), one per kernel-side sampling
    /// tick.
    pub pet_samples: u64,
    /// Off-CPU intervals ingested so far.
    pub off_cpu_intervals: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct ServerStatus {
    /// Wall-clock time the server itself started, unix nanos.
    pub server_started_at_unix_ns: u64,
    /// Empty when no run is active. The server hosts one run at a
    /// time; agents should `wait_active` or `stop_active` before
    /// starting another. (Modelled as `Vec<RunSummary>` rather than
    /// `Option<RunSummary>` because Option-of-struct trips
    /// vox-postcard at the moment.)
    pub active: Vec<RunSummary>,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct TargetLaneDiagnostics {
    pub tid: u32,
    pub name: String,
    pub records_received: u64,
    pub dispatch_records: u64,
    pub source_records: u64,
    pub shader_records: u64,
    pub attachment_records: u64,
    pub counter_set_records: u64,
    pub counter_sample_records: u64,
    pub spans_recorded: u64,
    pub spans_with_origin: u64,
    pub spans_linked_origin: u64,
    pub spans_unlinked_origin: u64,
    pub spans_origin_invalid_tid: u64,
    pub spans_origin_no_thread: u64,
    pub spans_origin_no_stack: u64,
    pub spans_origin_too_far: u64,
    pub origin_linked_distance_min_ns: u64,
    pub origin_linked_distance_avg_ns: u64,
    pub origin_linked_distance_max_ns: u64,
    pub origin_too_far_distance_min_ns: u64,
    pub origin_too_far_distance_avg_ns: u64,
    pub origin_too_far_distance_max_ns: u64,
    pub total_duration_ns: u64,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct TargetIngestDiagnostics {
    pub batches: u64,
    pub batches_dropped_no_active_run: u64,
    pub spans_dropped_no_active_run: u64,
    pub batches_dropped_wrong_pid: u64,
    pub spans_dropped_wrong_pid: u64,
    pub batches_dropped_target_queue_full: u64,
    pub spans_dropped_target_queue_full: u64,
    pub batches_dropped_target_worker_disconnected: u64,
    pub spans_dropped_target_worker_disconnected: u64,
    pub spans_received: u64,
    pub records_received: u64,
    pub dispatch_records: u64,
    pub source_records: u64,
    pub shader_records: u64,
    pub attachment_records: u64,
    pub counter_set_records: u64,
    pub counter_sample_records: u64,
    pub spans_recorded: u64,
    pub spans_dropped_bad_duration: u64,
    pub spans_with_origin: u64,
    pub spans_linked_origin: u64,
    pub spans_unlinked_origin: u64,
    pub spans_origin_invalid_tid: u64,
    pub spans_origin_no_thread: u64,
    pub spans_origin_no_stack: u64,
    pub spans_origin_too_far: u64,
    pub origin_stack_max_distance_ns: u64,
    pub origin_linked_distance_min_ns: u64,
    pub origin_linked_distance_avg_ns: u64,
    pub origin_linked_distance_max_ns: u64,
    pub origin_too_far_distance_min_ns: u64,
    pub origin_too_far_distance_avg_ns: u64,
    pub origin_too_far_distance_max_ns: u64,
    pub total_duration_ns: u64,
    pub lanes: Vec<TargetLaneDiagnostics>,
}

#[derive(Clone, Debug, Facet)]
pub struct DiagnosticsSnapshot {
    pub server_started_at_unix_ns: u64,
    pub active: Vec<RunSummary>,
    pub target_ingest: TargetIngestDiagnostics,
}

#[derive(Clone, Debug, Facet)]
pub struct SavedRunArchive {
    pub format_version: u32,
    pub saved_at_unix_ns: u64,
    pub runs: Vec<RunSummary>,
    pub aggregator: SavedAggregator,
    pub binaries: SavedBinaryRegistry,
    pub target_ingest: TargetIngestDiagnostics,
}

impl SavedRunArchive {
    pub fn from_event_log_entries(
        format_version: u32,
        saved_at_unix_ns: u64,
        entries: impl IntoIterator<Item = SavedEventLogEntry>,
    ) -> Self {
        let mut archive = Self {
            format_version,
            saved_at_unix_ns,
            runs: Vec::new(),
            aggregator: SavedAggregator::default(),
            binaries: SavedBinaryRegistry::default(),
            target_ingest: TargetIngestDiagnostics::default(),
        };
        let mut thread_names = BTreeMap::new();
        let mut threads: BTreeMap<u32, SavedThread> = BTreeMap::new();

        for entry in entries {
            match entry {
                SavedEventLogEntry::ArchiveSaved { saved_at_unix_ns } => {
                    archive.saved_at_unix_ns = saved_at_unix_ns;
                }
                SavedEventLogEntry::RunSummary { run } => {
                    archive.runs.push(run);
                }
                SavedEventLogEntry::AggregatorClock {
                    session_start_ns,
                    last_event_ns,
                } => {
                    archive.aggregator.session_start_ns = session_start_ns;
                    archive.aggregator.last_event_ns = last_event_ns;
                }
                SavedEventLogEntry::ThreadName { tid, name } => {
                    thread_names.insert(tid, name);
                }
                SavedEventLogEntry::BinaryLoaded { binary } => {
                    archive.binaries.binaries.push(binary);
                }
                SavedEventLogEntry::PetSample { tid, sample } => {
                    threads.entry(tid).or_insert_with(|| SavedThread {
                        tid,
                        pet_samples: Vec::new(),
                        intervals: Vec::new(),
                        wakeups: Vec::new(),
                    });
                    if let Some(thread) = threads.get_mut(&tid) {
                        thread.pet_samples.push(sample);
                    }
                }
                SavedEventLogEntry::Interval { tid, interval } => {
                    threads.entry(tid).or_insert_with(|| SavedThread {
                        tid,
                        pet_samples: Vec::new(),
                        intervals: Vec::new(),
                        wakeups: Vec::new(),
                    });
                    if let Some(thread) = threads.get_mut(&tid) {
                        thread.intervals.push(interval);
                    }
                }
                SavedEventLogEntry::Wakeup { tid, wakeup } => {
                    threads.entry(tid).or_insert_with(|| SavedThread {
                        tid,
                        pet_samples: Vec::new(),
                        intervals: Vec::new(),
                        wakeups: Vec::new(),
                    });
                    if let Some(thread) = threads.get_mut(&tid) {
                        thread.wakeups.push(wakeup);
                    }
                }
                SavedEventLogEntry::TargetIngestDiagnostics { diagnostics } => {
                    archive.target_ingest = diagnostics;
                }
            }
        }

        archive.aggregator.thread_names = thread_names
            .into_iter()
            .map(|(tid, name)| SavedThreadName { tid, name })
            .collect();
        archive.aggregator.threads = threads.into_values().collect();
        archive
    }
}

#[derive(Clone, Debug, Facet)]
pub struct SavedRunArchiveBundle {
    pub format_version: u32,
    pub saved_at_unix_ns: u64,
    pub provenance: SavedRunArchiveProvenance,
    pub runs: Vec<RunSummary>,
    pub aggregator: SavedAggregator,
    pub binaries: SavedBinaryRegistry,
    pub target_ingest: TargetIngestDiagnostics,
    pub events: Vec<SavedEventLogEntry>,
    pub blobs: Vec<SavedArchiveBlob>,
}

#[derive(Clone, Debug, Facet)]
pub struct SavedArchiveBlob {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Facet)]
pub struct SavedRunArchiveManifest {
    pub format_version: u32,
    pub saved_at_unix_ns: u64,
    pub provenance: SavedRunArchiveProvenance,
    pub runs: Vec<RunSummary>,
    pub files: SavedRunArchiveFiles,
}

#[derive(Clone, Debug, Facet)]
pub struct SavedRunArchiveProvenance {
    pub producer: String,
    pub producer_version: String,
    pub os: String,
    pub arch: String,
}

#[derive(Clone, Debug, Facet)]
pub struct SavedRunArchiveFiles {
    pub aggregator: String,
    pub binaries: String,
    pub target_ingest: String,
}

/// One append-friendly record in `events.jsonl`, the saved-run stream written
/// next to the v2 aggregate chunks/blobs and embedded in `.stax` packages.
/// New readers replay this stream when it is present; the aggregate chunks
/// remain a fast inspection and compatibility path.
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum SavedEventLogEntry {
    ArchiveSaved {
        saved_at_unix_ns: u64,
    },
    RunSummary {
        run: RunSummary,
    },
    AggregatorClock {
        session_start_ns: Option<u64>,
        last_event_ns: Option<u64>,
    },
    ThreadName {
        tid: u32,
        name: String,
    },
    BinaryLoaded {
        binary: SavedLoadedBinary,
    },
    PetSample {
        tid: u32,
        sample: SavedPetSample,
    },
    Interval {
        tid: u32,
        interval: SavedInterval,
    },
    Wakeup {
        tid: u32,
        wakeup: SavedWakeup,
    },
    TargetIngestDiagnostics {
        diagnostics: TargetIngestDiagnostics,
    },
}

#[derive(Clone, Debug, Default, Facet)]
pub struct SavedAggregator {
    pub session_start_ns: Option<u64>,
    pub last_event_ns: Option<u64>,
    pub thread_names: Vec<SavedThreadName>,
    pub threads: Vec<SavedThread>,
    /// Agent/user-placed markers for this run. Persisted so an opened
    /// archive keeps its `--window <marker>..` anchors.
    pub markers: Vec<RunMarker>,
}

#[derive(Clone, Debug, Facet)]
pub struct SavedThreadName {
    pub tid: u32,
    pub name: String,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct SavedThread {
    pub tid: u32,
    pub pet_samples: Vec<SavedPetSample>,
    pub intervals: Vec<SavedInterval>,
    pub wakeups: Vec<SavedWakeup>,
}

#[derive(Clone, Debug, Facet)]
pub struct SavedPetSample {
    pub timestamp_ns: u64,
    pub stack: Vec<u64>,
    pub kernel_stack: Vec<u64>,
    pub pmc: SavedPmuSample,
}

#[derive(Clone, Copy, Debug, Default, Facet)]
pub struct SavedPmuSample {
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct SavedInterval {
    pub start_ns: u64,
    pub end_ns: u64,
    pub kind: SavedIntervalKind,
}

#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum SavedIntervalKind {
    OnCpu,
    SyntheticSpan {
        stack: Vec<u64>,
        origin_tid: Option<u32>,
        lane_kind: TargetLaneKind,
    },
    OffCpu {
        stack: Vec<u64>,
        waker_tid: Option<u32>,
        waker_user_stack: Option<Vec<u64>>,
    },
}

#[derive(Clone, Debug, Facet)]
pub struct SavedWakeup {
    pub timestamp_ns: u64,
    pub waker_tid: u32,
    pub waker_user_stack: Vec<u64>,
    pub waker_kernel_stack: Vec<u64>,
}

#[derive(Clone, Debug, Default, Facet)]
pub struct SavedBinaryRegistry {
    pub binaries: Vec<SavedLoadedBinary>,
}

#[derive(Clone, Debug, Facet)]
pub struct SavedLoadedBinary {
    pub path: String,
    pub base_avma: u64,
    pub avma_end: u64,
    pub text_svma: u64,
    pub arch: Option<String>,
    pub is_executable: bool,
    pub symbols: Vec<SavedLiveSymbol>,
    pub text_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Facet)]
pub struct SavedLiveSymbol {
    pub start_svma: u64,
    pub end_svma: u64,
    pub name: Vec<u8>,
}

/// Agent-side wait condition: which event makes `wait_active` return.
/// First-fired wins; `wait_active` always also returns once the run
/// transitions to `Stopped`, regardless of which condition was set.
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum WaitCondition {
    /// Block until the active run transitions to `Stopped`. The
    /// natural choice for "let the recording finish, then I'll
    /// query."
    UntilStopped,
    /// Return as soon as the run has ingested at least `count` PET
    /// samples (returns immediately if already past). Useful for
    /// "give me enough data to be statistically meaningful, then
    /// look."
    ForSamples { count: u64 },
    /// Return after `seconds` of wall-clock time inside `wait_active`,
    /// even if the run is still recording.
    ForSeconds { seconds: u64 },
    /// Return as soon as a symbol whose demangled name contains
    /// `needle` (case-sensitive substring match) has been observed
    /// in the binary registry. Useful for "wait until the JIT has
    /// produced the function I want to look at."
    UntilSymbolSeen { needle: String },
}

/// Outcome of a `wait_active` call.
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum WaitOutcome {
    /// The wait condition fired. `summary` is the run's snapshot
    /// at the moment the condition fired (still `Recording` if the
    /// condition was, e.g., `ForSamples`).
    ConditionMet { summary: RunSummary },
    /// The run reached `Stopped`. Always returned for `UntilStopped`,
    /// and pre-empts any other condition for the other variants.
    Stopped { summary: RunSummary },
    /// The caller-supplied `timeout_ms` elapsed first. `summary` is
    /// the run's snapshot at that moment (still `Recording`).
    TimedOut { summary: RunSummary },
    /// No run was active when `wait_active` was called.
    NoActiveRun,
}

#[derive(Clone, Debug, Facet)]
pub struct RunConfig {
    /// Free-form label (typically the launch command's basename).
    pub label: String,
    /// PET sampling frequency the recorder requested, Hz. Surfaced in
    /// `RunSummary` so the UI can label samples.
    pub frequency_hz: u32,
    /// Whether to replay user stacks via `.eh_frame` DWARF unwinding
    /// rather than the kernel's frame-pointer walk, so call chains
    /// stay complete through `-fomit-frame-pointer` code. Already
    /// resolved by the `stax` CLI — on by default on x86_64 Linux,
    /// off elsewhere, overridable with `--no-dwarf-unwind` /
    /// `STAX_DWARF_UNWIND`. Ignored by the recorder on macOS (kperf
    /// already walks full stacks).
    pub dwarf_unwind: bool,
}

/// Errors the server-side run-control plane can surface to a client.
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum RunControlError {
    /// No run is currently active.
    NoActiveRun,
    /// A run is already active; only one run at a time is supported.
    AlreadyActive,
    /// Spawning the in-process recorder failed (posix_spawn of the
    /// `--launch` target, staxd handshake, etc.).
    SpawnFailed { detail: String },
    /// Catch-all for errors not yet promoted to a typed variant.
    Internal { message: String },
}

impl From<String> for RunControlError {
    fn from(message: String) -> Self {
        Self::Internal { message }
    }
}

/// Agent-facing control plane. One service instance per server; runs
/// are addressed by `RunId`. The web UI uses the existing `Profiler`
/// trait for view subscriptions; agents use `RunControl` for
/// lifecycle + the same `Profiler` for queries (with `subscribe_*`
/// returning a single update being equivalent to a unary call).
#[vox::service]
pub trait RunControl {
    /// Snapshot the server. Returns the active run (if any) plus
    /// server-wide info. Used by `stax status`.
    async fn status(&self) -> ServerStatus;

    /// All runs the server has ever hosted (active + historical
    /// in-memory archive). Bounded by the server's eviction policy
    /// (in-memory only for now; on-disk persistence is a follow-up).
    async fn list_runs(&self) -> Vec<RunSummary>;

    /// Point-in-time diagnostics: current run plus target-span ingest
    /// counters and origin-link health, or a stopped in-memory run when
    /// `params.run` is set.
    async fn diagnostics(&self, params: RunViewParams) -> DiagnosticsSnapshot;

    /// Start a recording by attaching to an existing pid. For
    /// `stax record -- <argv>`, the CLI `posix_spawn`s the target
    /// suspended and hands the PID to this call before resuming it.
    async fn start_attach(
        &self,
        pid: u32,
        config: RunConfig,
        daemon_socket: String,
        time_limit_secs: Option<u64>,
    ) -> Result<RunId, RunControlError>;

    /// Block until `condition` fires, the active run stops, or
    /// `timeout_ms` elapses (whichever comes first). Returns
    /// `NoActiveRun` immediately when nothing is recording.
    async fn wait_active(&self, condition: WaitCondition, timeout_ms: Option<u64>) -> WaitOutcome;

    /// Ask the recorder to stop the active run cleanly. Returns the
    /// final `RunSummary` once the run has transitioned to `Stopped`.
    /// Errors if no run is active.
    async fn stop_active(&self) -> Result<RunSummary, RunControlError>;

    /// Save the current or most recent queryable run into a v2 archive at
    /// `path`. Paths ending in `.stax` create a single-file package; other
    /// paths create a directory with aggregate chunks, blobs, and
    /// `events.jsonl`.
    async fn save_current(&self, path: String) -> Result<(), RunControlError>;

    /// Open a saved archive into the server's current query state. Accepts
    /// v2 archive directories/manifests, `.stax` packages, and legacy v1
    /// archive.json files. V2 archives replay saved event records when
    /// present.
    /// Fails while a recording is active.
    async fn open_saved(&self, path: String) -> Result<(), RunControlError>;

    /// Restore one stopped in-memory run into the server's current query
    /// state. Reporting commands prefer non-mutating `RunViewParams` /
    /// `ViewParams.run`; keep this for explicit "make this the current run"
    /// workflows and older clients. Fails while a recording is active, because
    /// the live aggregator belongs to that recording.
    async fn select_run(&self, run_id: RunId) -> Result<RunSummary, RunControlError>;

    /// Drop a named marker into the active run at the current
    /// recording time. The point of the feature is stall forensics:
    /// `stax mark freeze` when the user reports a stall, then
    /// `stax flame --window freeze..` reads exactly what the process
    /// was doing from that moment on. Errors with `NoActiveRun` when
    /// nothing is recording.
    async fn mark(&self, label: String) -> Result<RunMarker, RunControlError>;

    /// All markers recorded in the current/most-recent query state,
    /// in timestamp order. The CLI uses these to resolve a
    /// `--window <marker>..` anchor without re-reading the timeline.
    async fn markers(&self, params: RunViewParams) -> Vec<RunMarker>;
}

/// All service descriptors exposed by stax-live; the codegen iterates over
/// this list.
pub fn all_services() -> Vec<&'static vox::ServiceDescriptor> {
    vec![
        profiler_service_descriptor(),
        run_control_service_descriptor(),
        target_ingest_service_descriptor(),
    ]
}
