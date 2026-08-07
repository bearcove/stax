//! Target-side latch for stax.
//!
//! A profiled app links this crate to put execution lanes the CPU
//! sampler cannot see — GPU command queues, accelerators, runtime
//! queues, worker pools — on a live stax recording's timeline (see
//! `stax-server`'s `TargetIngest`: spans become a synthetic thread
//! with named frames in the existing views).
//! Targets can also attach a [`TargetSpanOrigin`] captured with
//! [`current_span_origin`] so stax can link target work back to the
//! CPU stack that queued it. That origin is provenance: target work
//! still renders as a parallel synthetic lane unless a future richer
//! integration also reports the CPU wait/completion side of the
//! relationship.
//!
//! The contract has two halves:
//!
//! - **Capture gating (polling)** — the background worker polls the
//!   server (~1s) with `should_report(pid)`; the answer drives
//!   [`reporting_active`], which the app reads (one relaxed atomic
//!   load) wherever it decides whether to pay its span-capture cost.
//!   Attach (`stax record --pid`) turns capture on within one poll
//!   period; stop/detach turns it off the same way. No server, no
//!   socket, server restart — all degrade to "off" and recover on a
//!   later poll.
//! - **Data ([`report`])** — fire-and-forget batches, bounded queue,
//!   drop-newest. The server is the authority: it drops batches whose
//!   pid doesn't match the active run's target. Lossy by design. Local
//!   queue drops are counted in [`reporter_stats`] and sent to
//!   `stax diagnose` while capture is active. [`reporter_stats`] also
//!   exposes target-local worker/gate/connection state for integration
//!   health checks.
//!
//! Span timestamps are absolute mach-derived nanoseconds — on Apple
//! platforms `mach_absolute_time` converted to ns (Apple Silicon GPU
//! timestamps share that timebase), the same clock domain the sampler
//! records in, so no correlation step exists anywhere.
//!
//! ## Executor-style integration
//!
//! ```no_run
//! let lane = stax_target::Lane::new("decoder worker");
//! let _gpu_lane = stax_target::Lane::metal("GPU tq1s");
//!
//! // Queue side: capture provenance only while stax is recording us.
//! let origin = lane.capture_origin();
//!
//! // Worker side: time the work where it actually runs.
//! if let Some(open) = lane.begin_span_with_captured_origin("decode chunk", origin) {
//!     // decode_chunk();
//!     open.finish_and_report(&lane);
//! }
//!
//! let stats = lane.reporter_stats();
//! if stats.batches_dropped_queue_full > 0 {
//!     // Consider batching spans more coarsely.
//! }
//! ```
//!
//! For APIs that already return exact timestamps, use
//! [`Lane::span_with_captured_origin`] and [`Lane::report_one`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender, error::TrySendError};

/// Per-call timeout: a half-dead connection (e.g. server dropped us for
/// missing keepalive pongs) must never wedge the worker — time out,
/// drop the client, reconnect on a later poll.
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

pub use stax_live_proto::{
    OffCpuReason, TargetAttachmentId, TargetAttachmentKind, TargetAttachmentRecord,
    TargetCommandBufferId, TargetCommandBufferRecord, TargetContractDuty, TargetContractId,
    TargetContractKind, TargetContractRecord, TargetContractSeverity, TargetCounterDefinition,
    TargetCounterSampleId, TargetCounterSamplePoint, TargetCounterSampleRecord,
    TargetCounterScalar, TargetCounterSetId, TargetCounterSetRecord, TargetCounterUnit,
    TargetDispatchId, TargetDispatchRecord, TargetEventFieldDefinition, TargetEventId,
    TargetEventKindId, TargetEventKindRecord, TargetEventRecord, TargetLaneId, TargetLaneKind,
    TargetLaneRecord, TargetQueueId, TargetQueueRecord, TargetRecordBatch, TargetRuntimeId,
    TargetRuntimeRecord, TargetShaderId, TargetShaderRecord, TargetSignalBatch,
    TargetSignalSelector, TargetSourceId, TargetSourceRecord, TargetSpan, TargetSpanOrigin,
};
use stax_live_proto::{TargetIngestClient, TargetReporterStats, TargetSpanBatch};

/// Bounded queue between reporting threads and the worker. Each entry
/// is one batch; overflow drops the newest batch (profiling telemetry,
/// not ledger data).
const QUEUE_DEPTH: usize = 64;

/// Capture-gate poll period. Attach/detach latency is bounded by this.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Whether the app's pid is currently being recorded.
static REPORTING_ACTIVE: AtomicBool = AtomicBool::new(false);
static WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static CONNECTED_TO_SERVER: AtomicBool = AtomicBool::new(false);
static BATCHES_DROPPED_QUEUE_FULL: AtomicU64 = AtomicU64::new(0);
static SPANS_DROPPED_QUEUE_FULL: AtomicU64 = AtomicU64::new(0);
static BATCHES_DROPPED_WORKER_DISCONNECTED: AtomicU64 = AtomicU64::new(0);
static SPANS_DROPPED_WORKER_DISCONNECTED: AtomicU64 = AtomicU64::new(0);
static SIGNAL_BATCHES_DROPPED_QUEUE_FULL: AtomicU64 = AtomicU64::new(0);
static SIGNALS_DROPPED_QUEUE_FULL: AtomicU64 = AtomicU64::new(0);
static SIGNAL_BATCHES_DROPPED_WORKER_DISCONNECTED: AtomicU64 = AtomicU64::new(0);
static SIGNALS_DROPPED_WORKER_DISCONNECTED: AtomicU64 = AtomicU64::new(0);
static NEXT_EVENT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_COUNTER_SAMPLE_ID: AtomicU64 = AtomicU64::new(1);

/// Target-local reporter health snapshot.
///
/// This is intentionally passive: unlike [`reporting_active`], reading
/// stats does not arm the background worker. Use it for status logs,
/// admin endpoints, or test assertions around integration health.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReporterStats {
    /// Whether the background worker has been armed at least once.
    pub worker_started: bool,
    /// Last capture-gate state observed by the worker.
    pub reporting_active: bool,
    /// Whether the worker currently has a live stax-server connection.
    pub connected_to_server: bool,
    /// Batches dropped because the bounded target-local queue was full.
    pub batches_dropped_queue_full: u64,
    /// Spans in batches dropped because the bounded target-local queue
    /// was full.
    pub spans_dropped_queue_full: u64,
    /// Batches dropped because the background worker disconnected.
    pub batches_dropped_worker_disconnected: u64,
    /// Spans in batches dropped because the background worker
    /// disconnected.
    pub spans_dropped_worker_disconnected: u64,
    pub signal_batches_dropped_queue_full: u64,
    pub signals_dropped_queue_full: u64,
    pub signal_batches_dropped_worker_disconnected: u64,
    pub signals_dropped_worker_disconnected: u64,
}

/// Capture gate: `true` while a stax recording of this process is
/// active. One relaxed atomic load — safe to read on hot paths. The
/// first call arms the background worker (and thus the polling), so
/// apps need no explicit init: read the gate where capture is decided.
pub fn reporting_active() -> bool {
    let _ = worker_sender();
    REPORTING_ACTIVE.load(Ordering::Relaxed)
}

/// Return a passive snapshot of target-side reporter health.
///
/// This does not start the background worker. The capture gate is still
/// [`reporting_active`]; call that from instrumentation sites when you
/// want stax-target to begin polling for an active recording.
pub fn reporter_stats() -> ReporterStats {
    ReporterStats {
        worker_started: WORKER_STARTED.load(Ordering::Relaxed),
        reporting_active: REPORTING_ACTIVE.load(Ordering::Relaxed),
        connected_to_server: CONNECTED_TO_SERVER.load(Ordering::Relaxed),
        batches_dropped_queue_full: BATCHES_DROPPED_QUEUE_FULL.load(Ordering::Relaxed),
        spans_dropped_queue_full: SPANS_DROPPED_QUEUE_FULL.load(Ordering::Relaxed),
        batches_dropped_worker_disconnected: BATCHES_DROPPED_WORKER_DISCONNECTED
            .load(Ordering::Relaxed),
        spans_dropped_worker_disconnected: SPANS_DROPPED_WORKER_DISCONNECTED
            .load(Ordering::Relaxed),
        signal_batches_dropped_queue_full: SIGNAL_BATCHES_DROPPED_QUEUE_FULL
            .load(Ordering::Relaxed),
        signals_dropped_queue_full: SIGNALS_DROPPED_QUEUE_FULL.load(Ordering::Relaxed),
        signal_batches_dropped_worker_disconnected: SIGNAL_BATCHES_DROPPED_WORKER_DISCONNECTED
            .load(Ordering::Relaxed),
        signals_dropped_worker_disconnected: SIGNALS_DROPPED_WORKER_DISCONNECTED
            .load(Ordering::Relaxed),
    }
}

fn reset_reporter_stats() {
    BATCHES_DROPPED_QUEUE_FULL.store(0, Ordering::Relaxed);
    SPANS_DROPPED_QUEUE_FULL.store(0, Ordering::Relaxed);
    BATCHES_DROPPED_WORKER_DISCONNECTED.store(0, Ordering::Relaxed);
    SPANS_DROPPED_WORKER_DISCONNECTED.store(0, Ordering::Relaxed);
    SIGNAL_BATCHES_DROPPED_QUEUE_FULL.store(0, Ordering::Relaxed);
    SIGNALS_DROPPED_QUEUE_FULL.store(0, Ordering::Relaxed);
    SIGNAL_BATCHES_DROPPED_WORKER_DISCONNECTED.store(0, Ordering::Relaxed);
    SIGNALS_DROPPED_WORKER_DISCONNECTED.store(0, Ordering::Relaxed);
}

fn reporter_stats_for_pid(pid: u32) -> TargetReporterStats {
    let stats = reporter_stats();
    TargetReporterStats {
        pid,
        batches_dropped_queue_full: stats.batches_dropped_queue_full,
        spans_dropped_queue_full: stats.spans_dropped_queue_full,
        batches_dropped_worker_disconnected: stats.batches_dropped_worker_disconnected,
        spans_dropped_worker_disconnected: stats.spans_dropped_worker_disconnected,
        signal_batches_dropped_queue_full: stats.signal_batches_dropped_queue_full,
        signals_dropped_queue_full: stats.signals_dropped_queue_full,
        signal_batches_dropped_worker_disconnected: stats
            .signal_batches_dropped_worker_disconnected,
        signals_dropped_worker_disconnected: stats.signals_dropped_worker_disconnected,
    }
}

fn record_queue_full_drop(spans: u64) {
    BATCHES_DROPPED_QUEUE_FULL.fetch_add(1, Ordering::Relaxed);
    SPANS_DROPPED_QUEUE_FULL.fetch_add(spans, Ordering::Relaxed);
}

fn record_worker_disconnected_drop(spans: u64) {
    BATCHES_DROPPED_WORKER_DISCONNECTED.fetch_add(1, Ordering::Relaxed);
    SPANS_DROPPED_WORKER_DISCONNECTED.fetch_add(spans, Ordering::Relaxed);
}
fn record_signal_queue_full_drop(signals: u64) {
    SIGNAL_BATCHES_DROPPED_QUEUE_FULL.fetch_add(1, Ordering::Relaxed);
    SIGNALS_DROPPED_QUEUE_FULL.fetch_add(signals, Ordering::Relaxed);
}

fn record_signal_worker_disconnected_drop(signals: u64) {
    SIGNAL_BATCHES_DROPPED_WORKER_DISCONNECTED.fetch_add(1, Ordering::Relaxed);
    SIGNALS_DROPPED_WORKER_DISCONNECTED.fetch_add(signals, Ordering::Relaxed);
}

/// Current timestamp in the same nanosecond clock domain stax uses for
/// target spans.
///
/// On macOS this is `mach_absolute_time` converted to nanoseconds; on
/// Linux it is `CLOCK_MONOTONIC`. Returns `None` on unsupported
/// platforms.
pub fn now_ns() -> Option<u64> {
    clock_now_ns()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetPoint {
    pub timestamp_ns: u64,
    pub pid: u32,
    pub tid: Option<u32>,
}

pub fn current_point() -> Option<TargetPoint> {
    Some(TargetPoint {
        timestamp_ns: now_ns()?,
        pid: std::process::id(),
        tid: current_thread_id(),
    })
}

/// Capture the current target-side queue/dispatch origin.
///
/// Use this at the point where work is submitted to an executor the CPU
/// sampler cannot see directly (for example, immediately before a GPU
/// dispatch is encoded). Attach the returned origin to each
/// [`TargetSpan`] that represents that work; stax-server will use it to
/// borrow the nearest sampled CPU stack on the same thread.
pub fn current_span_origin() -> Option<TargetSpanOrigin> {
    Some(TargetSpanOrigin {
        tid: current_thread_id()?,
        timestamp_ns: clock_now_ns()?,
    })
}

/// Construct a span with the current target-side origin attached when
/// the platform exposes one.
pub fn span_with_current_origin(name: impl Into<String>, start_ns: u64, end_ns: u64) -> TargetSpan {
    let span = TargetSpan::new(name, start_ns, end_ns);
    match current_span_origin() {
        Some(origin) => span.with_origin(origin),
        None => span,
    }
}

/// Error returned by fallible reporting helpers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportError {
    /// The bounded in-process queue is full; this batch was dropped.
    QueueFull,
    /// The background worker has stopped, so no further batches can be
    /// delivered.
    WorkerDisconnected,
}

/// Report one lane's spans. Cheap and non-blocking; safe to call from
/// hot paths. `lane` names the synthetic thread (e.g. "GPU tq1s").
/// Batches sent while no recording is active are dropped server-side
/// (and capture should be gated on [`reporting_active`] anyway, so the
/// steady-state idle cost is nil).
pub fn report(lane: &str, spans: Vec<TargetSpan>) {
    let _ = try_report(lane, spans);
}

pub fn report_with_kind(lane: &str, lane_kind: TargetLaneKind, spans: Vec<TargetSpan>) {
    let _ = try_report_with_kind(lane, lane_kind, spans);
}

pub fn report_records(lane: &str, records: TargetRecordBatch) {
    let _ = try_report_records(lane, records);
}

pub fn report_records_with_kind(lane: &str, lane_kind: TargetLaneKind, records: TargetRecordBatch) {
    let _ = try_report_batch_with_kind(lane, lane_kind, Vec::new(), records);
}

/// Fallible variant of [`report`] for integrations that want to count
/// local queue drops.
pub fn try_report(lane: &str, spans: Vec<TargetSpan>) -> Result<(), ReportError> {
    try_report_with_kind(lane, TargetLaneKind::Generic, spans)
}

pub fn try_report_records(lane: &str, records: TargetRecordBatch) -> Result<(), ReportError> {
    try_report_batch_with_kind(lane, TargetLaneKind::Generic, Vec::new(), records)
}

pub fn try_report_with_kind(
    lane: &str,
    lane_kind: TargetLaneKind,
    spans: Vec<TargetSpan>,
) -> Result<(), ReportError> {
    try_report_batch_with_kind(lane, lane_kind, spans, TargetRecordBatch::default())
}

pub fn try_report_batch_with_kind(
    lane: &str,
    lane_kind: TargetLaneKind,
    spans: Vec<TargetSpan>,
    records: TargetRecordBatch,
) -> Result<(), ReportError> {
    if spans.is_empty() && records.is_empty() {
        return Ok(());
    }
    let span_count = spans.len() as u64;
    let sender = worker_sender();
    let batch = TargetSpanBatch {
        pid: std::process::id(),
        lane: lane.to_owned(),
        lane_kind,
        spans,
        records,
    };
    match sender.try_send(WorkerMessage::Spans(batch)) {
        Ok(()) => Ok(()),

        Err(TrySendError::Full(_)) => {
            record_queue_full_drop(span_count);
            tracing::debug!("stax-target queue full; dropping span batch");
            Err(ReportError::QueueFull)
        }
        Err(TrySendError::Closed(_)) => {
            record_worker_disconnected_drop(span_count);
            Err(ReportError::WorkerDisconnected)
        }
    }
}
pub fn report_signals(batch: TargetSignalBatch) {
    let _ = try_report_signals(batch);
}

pub fn try_report_signals(mut batch: TargetSignalBatch) -> Result<(), ReportError> {
    if batch.is_empty() {
        return Ok(());
    }
    batch.pid = std::process::id();
    let signal_count = (batch.event_kinds.len()
        + batch.events.len()
        + batch.counter_sets.len()
        + batch.counter_samples.len()
        + batch.contracts.len()) as u64;
    match worker_sender().try_send(WorkerMessage::Signals(batch)) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => {
            record_signal_queue_full_drop(signal_count);
            Err(ReportError::QueueFull)
        }
        Err(TrySendError::Closed(_)) => {
            record_signal_worker_disconnected_drop(signal_count);
            Err(ReportError::WorkerDisconnected)
        }
    }
}

#[derive(Clone, Debug, Default)]
struct SignalDefinitions {
    event_kinds: HashMap<TargetEventKindId, TargetEventKindRecord>,
    counter_sets: HashMap<TargetCounterSetId, TargetCounterSetRecord>,
    contracts: HashMap<TargetContractId, TargetContractRecord>,
}

fn signal_definitions() -> &'static Mutex<SignalDefinitions> {
    static DEFINITIONS: OnceLock<Mutex<SignalDefinitions>> = OnceLock::new();
    DEFINITIONS.get_or_init(|| Mutex::new(SignalDefinitions::default()))
}

fn definition_snapshot(pid: u32) -> TargetSignalBatch {
    let definitions = signal_definitions()
        .lock()
        .expect("stax signal registry poisoned");
    TargetSignalBatch {
        pid,
        event_kinds: definitions.event_kinds.values().cloned().collect(),
        events: Vec::new(),
        counter_sets: definitions.counter_sets.values().cloned().collect(),
        counter_samples: Vec::new(),
        contracts: definitions.contracts.values().cloned().collect(),
    }
}

#[derive(Clone, Debug)]
pub struct EventKind {
    definition: TargetEventKindRecord,
}

impl EventKind {
    pub fn new(
        id: TargetEventKindId,
        name: impl Into<String>,
        description: Option<String>,
        fields: Vec<TargetEventFieldDefinition>,
    ) -> Self {
        let definition = TargetEventKindRecord {
            event_kind_id: id,
            name: name.into(),
            description,
            fields,
        };
        signal_definitions()
            .lock()
            .expect("stax signal registry poisoned")
            .event_kinds
            .insert(id, definition.clone());
        Self { definition }
    }

    pub fn id(&self) -> TargetEventKindId {
        self.definition.event_kind_id
    }

    pub fn emit(&self, correlation_id: Option<u64>, values: Vec<TargetCounterScalar>) {
        let Some(point) = current_point() else { return };
        self.emit_at(
            point.timestamp_ns,
            point.pid,
            point.tid,
            correlation_id,
            values,
        );
    }

    pub fn emit_at(
        &self,
        timestamp_ns: u64,
        source_pid: u32,
        tid: Option<u32>,
        correlation_id: Option<u64>,
        values: Vec<TargetCounterScalar>,
    ) {
        if !reporting_active() || (source_pid != std::process::id() && tid.is_some()) {
            return;
        }
        let event = TargetEventRecord {
            event_id: TargetEventId::new(NEXT_EVENT_ID.fetch_add(1, Ordering::Relaxed)),
            event_kind_id: self.definition.event_kind_id,
            timestamp_ns,
            source_pid,
            tid,
            correlation_id,
            values,
        };
        let _ = try_report_signals(TargetSignalBatch {
            pid: std::process::id(),
            events: vec![event],
            ..TargetSignalBatch::default()
        });
    }
}

#[derive(Clone, Debug)]
pub struct CounterSet {
    definition: TargetCounterSetRecord,
}

impl CounterSet {
    pub fn new(
        id: TargetCounterSetId,
        name: impl Into<String>,
        counters: Vec<TargetCounterDefinition>,
    ) -> Self {
        let definition = TargetCounterSetRecord {
            counter_set_id: id,
            name: name.into(),
            counters,
        };
        signal_definitions()
            .lock()
            .expect("stax signal registry poisoned")
            .counter_sets
            .insert(id, definition.clone());
        Self { definition }
    }

    pub fn id(&self) -> TargetCounterSetId {
        self.definition.counter_set_id
    }

    pub fn sample(&self, values: Vec<TargetCounterScalar>) {
        let Some(point) = current_point() else { return };
        self.sample_at(point.timestamp_ns, point.pid, point.tid, values);
    }

    pub fn sample_at(
        &self,
        timestamp_ns: u64,
        source_pid: u32,
        tid: Option<u32>,
        values: Vec<TargetCounterScalar>,
    ) {
        if !reporting_active() || (source_pid != std::process::id() && tid.is_some()) {
            return;
        }
        let sample = TargetCounterSampleRecord {
            counter_sample_id: TargetCounterSampleId::new(
                NEXT_COUNTER_SAMPLE_ID.fetch_add(1, Ordering::Relaxed),
            ),
            counter_set_id: self.definition.counter_set_id,
            dispatch_id: None,
            command_buffer_id: None,
            sample_point: TargetCounterSamplePoint::TimeSeries,
            timestamp_ns: Some(timestamp_ns),
            source_pid,
            tid,
            values,
            error: None,
        };
        let _ = try_report_signals(TargetSignalBatch {
            pid: std::process::id(),
            counter_samples: vec![sample],
            ..TargetSignalBatch::default()
        });
    }
}

#[derive(Clone, Debug)]
pub struct Contract {
    definition: TargetContractRecord,
}

impl Contract {
    pub fn new(definition: TargetContractRecord) -> Self {
        signal_definitions()
            .lock()
            .expect("stax signal registry poisoned")
            .contracts
            .insert(definition.contract_id, definition.clone());
        Self { definition }
    }

    pub fn max_off_cpu_current_thread(
        id: TargetContractId,
        name: impl Into<String>,
        description: Option<String>,
        severity: TargetContractSeverity,
        duty: TargetContractDuty,
        max_ns: u64,
        reasons: Vec<OffCpuReason>,
    ) -> Option<Self> {
        Some(Self::new(TargetContractRecord {
            contract_id: id,
            name: name.into(),
            description,
            severity,
            duty,
            kind: TargetContractKind::MaxOffCpuInterval {
                tid: current_thread_id()?,
                max_ns,
                reasons,
            },
        }))
    }

    pub fn max_signal_gap(
        id: TargetContractId,
        name: impl Into<String>,
        description: Option<String>,
        severity: TargetContractSeverity,
        duty: TargetContractDuty,
        signal: TargetSignalSelector,
        max_ns: u64,
    ) -> Self {
        Self::new(TargetContractRecord {
            contract_id: id,
            name: name.into(),
            description,
            severity,
            duty,
            kind: TargetContractKind::MaxSignalGap { signal, max_ns },
        })
    }

    pub fn max_latency(
        id: TargetContractId,
        name: impl Into<String>,
        description: Option<String>,
        severity: TargetContractSeverity,
        duty: TargetContractDuty,
        start_event: TargetEventKindId,
        end_event: TargetEventKindId,
        max_ns: u64,
    ) -> Self {
        Self::new(TargetContractRecord {
            contract_id: id,
            name: name.into(),
            description,
            severity,
            duty,
            kind: TargetContractKind::MaxLatency {
                start_event,
                end_event,
                max_ns,
            },
        })
    }

    pub fn definition(&self) -> &TargetContractRecord {
        &self.definition
    }
}

/// Origin captured at a CPU-side queue/dispatch point and carried with
/// work until it starts running on a target lane.
///
/// This token remembers both "capture was active" and the optional OS
/// thread/timestamp origin. Some supported platforms may be able to
/// report spans while failing to capture a thread origin; in that case
/// the token stays active but has no origin, so lane-only views still
/// work.
#[derive(Clone, Copy, Debug, Default)]
pub struct CapturedOrigin {
    active: bool,
    origin: Option<TargetSpanOrigin>,
}

impl CapturedOrigin {
    /// Token representing "stax was not recording this process".
    pub fn inactive() -> Self {
        Self::default()
    }

    /// Token representing an active capture where no OS-thread origin
    /// was available.
    pub fn active_without_origin() -> Self {
        Self {
            active: true,
            origin: None,
        }
    }

    /// Token with a concrete CPU-side queue/dispatch origin.
    pub fn from_origin(origin: TargetSpanOrigin) -> Self {
        Self {
            active: true,
            origin: Some(origin),
        }
    }

    /// Whether this token was captured while stax was recording this
    /// process.
    pub fn is_active(self) -> bool {
        self.active
    }

    /// The captured OS-thread/timestamp origin, if available.
    pub fn origin(self) -> Option<TargetSpanOrigin> {
        self.origin
    }
}

/// Builder for a target span whose timestamps came from an external
/// API.
#[derive(Clone, Debug)]
pub struct SpanBuilder {
    name: String,
    start_ns: u64,
    end_ns: u64,
    active: bool,
    origin: Option<TargetSpanOrigin>,
    dispatch_id: Option<TargetDispatchId>,
    shader_id: Option<TargetShaderId>,
    source_id: Option<TargetSourceId>,
    wait_origin: Option<TargetSpanOrigin>,
    completion_origin: Option<TargetSpanOrigin>,
    attachment_ids: Vec<TargetAttachmentId>,
    counter_sample_ids: Vec<TargetCounterSampleId>,
}

impl SpanBuilder {
    /// Create a span builder. The builder is active by default; use
    /// [`with_captured_origin`](Self::with_captured_origin) when the
    /// span should be gated by a queue/dispatch token.
    pub fn new(name: impl Into<String>, start_ns: u64, end_ns: u64) -> Self {
        Self {
            name: name.into(),
            start_ns,
            end_ns,
            active: true,
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

    /// Attach an explicit CPU-side queue/dispatch origin.
    pub fn with_origin(mut self, origin: TargetSpanOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    /// Attach an origin carried in a [`CapturedOrigin`] token when one
    /// was available, and make [`build`](Self::build) return `None`
    /// when the token was captured while stax was inactive.
    pub fn with_captured_origin(mut self, captured: CapturedOrigin) -> Self {
        self.active = captured.active;
        self.origin = captured.origin;
        self
    }

    /// Attach the current thread/timestamp as origin when available.
    pub fn with_current_origin(self) -> Self {
        match current_span_origin() {
            Some(origin) => self.with_origin(origin),
            None => self,
        }
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

    /// Validate and construct the reportable span.
    pub fn build(self) -> Option<TargetSpan> {
        if !self.active {
            return None;
        }
        if self.end_ns <= self.start_ns {
            return None;
        }
        let mut span = TargetSpan::new(self.name, self.start_ns, self.end_ns);
        if let Some(dispatch_id) = self.dispatch_id {
            span = span.with_dispatch_id(dispatch_id);
        }
        if let Some(shader_id) = self.shader_id {
            span = span.with_shader_id(shader_id);
        }
        if let Some(source_id) = self.source_id {
            span = span.with_source_id(source_id);
        }
        if let Some(origin) = self.wait_origin {
            span = span.with_wait_origin(origin);
        }
        if let Some(origin) = self.completion_origin {
            span = span.with_completion_origin(origin);
        }
        for attachment_id in self.attachment_ids {
            span = span.with_attachment_id(attachment_id);
        }
        for counter_sample_id in self.counter_sample_ids {
            span = span.with_counter_sample_id(counter_sample_id);
        }
        Some(match self.origin {
            Some(origin) => span.with_origin(origin),
            None => span,
        })
    }
}

#[derive(Clone, Debug)]
pub struct DispatchBuilder {
    name: String,
    start_ns: Option<u64>,
    end_ns: Option<u64>,
    active: bool,
    dispatch_origin: Option<TargetSpanOrigin>,
    wait_origin: Option<TargetSpanOrigin>,
    completion_origin: Option<TargetSpanOrigin>,
    dispatch_id: Option<TargetDispatchId>,
    shader_id: Option<TargetShaderId>,
    source_id: Option<TargetSourceId>,
    attachment_ids: Vec<TargetAttachmentId>,
    counter_sample_ids: Vec<TargetCounterSampleId>,
}

impl DispatchBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            start_ns: None,
            end_ns: None,
            active: true,
            dispatch_origin: None,
            wait_origin: None,
            completion_origin: None,
            dispatch_id: None,
            shader_id: None,
            source_id: None,
            attachment_ids: Vec::new(),
            counter_sample_ids: Vec::new(),
        }
    }

    pub fn timestamps(mut self, start_ns: u64, end_ns: u64) -> Self {
        self.start_ns = Some(start_ns);
        self.end_ns = Some(end_ns);
        self
    }

    pub fn with_captured_origin(mut self, captured: CapturedOrigin) -> Self {
        self.active = captured.active;
        self.dispatch_origin = captured.origin;
        self
    }

    pub fn with_dispatch_origin(mut self, origin: TargetSpanOrigin) -> Self {
        self.dispatch_origin = Some(origin);
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

    pub fn with_attachment_id(mut self, attachment_id: TargetAttachmentId) -> Self {
        self.attachment_ids.push(attachment_id);
        self
    }

    pub fn with_counter_sample_id(mut self, counter_sample_id: TargetCounterSampleId) -> Self {
        self.counter_sample_ids.push(counter_sample_id);
        self
    }

    pub fn build(self) -> Option<TargetSpan> {
        if !self.active {
            return None;
        }
        let start_ns = self.start_ns?;
        let end_ns = self.end_ns?;
        let mut builder = SpanBuilder::new(self.name, start_ns, end_ns);
        if let Some(origin) = self.dispatch_origin {
            builder = builder.with_origin(origin);
        }
        if let Some(origin) = self.wait_origin {
            builder = builder.with_wait_origin(origin);
        }
        if let Some(origin) = self.completion_origin {
            builder = builder.with_completion_origin(origin);
        }
        if let Some(dispatch_id) = self.dispatch_id {
            builder = builder.with_dispatch_id(dispatch_id);
        }
        if let Some(shader_id) = self.shader_id {
            builder = builder.with_shader_id(shader_id);
        }
        if let Some(source_id) = self.source_id {
            builder = builder.with_source_id(source_id);
        }
        for attachment_id in self.attachment_ids {
            builder = builder.with_attachment_id(attachment_id);
        }
        for counter_sample_id in self.counter_sample_ids {
            builder = builder.with_counter_sample_id(counter_sample_id);
        }
        builder.build()
    }
}

/// Reusable handle for one cooperating target lane.
///
/// A lane becomes one synthetic thread in stax, so create one per queue,
/// executor, GPU command stream, worker pool, or other logical place
/// work runs away from the sampled CPU stack.
#[derive(Clone, Debug)]
pub struct Lane {
    name: String,
    kind: TargetLaneKind,
}

impl Lane {
    /// Create a lane handle. The name is what `stax threads` prints.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: TargetLaneKind::Generic,
        }
    }

    /// Create a lane handle with an explicit target kind.
    pub fn with_kind(name: impl Into<String>, kind: TargetLaneKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }

    /// Create a Metal execution lane.
    pub fn metal(name: impl Into<String>) -> Self {
        Self::with_kind(name, TargetLaneKind::Metal)
    }

    /// Lane name as it will appear in stax.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Explicit lane kind reported to stax.
    pub fn kind(&self) -> TargetLaneKind {
        self.kind
    }

    /// Same capture gate as [`reporting_active`], scoped for call sites
    /// that already hold a lane handle.
    pub fn reporting_active(&self) -> bool {
        reporting_active()
    }

    /// Passive snapshot of target-side reporter health.
    pub fn reporter_stats(&self) -> ReporterStats {
        reporter_stats()
    }

    /// Capture the current CPU-side origin for work that will later run
    /// on this lane.
    pub fn current_origin(&self) -> Option<TargetSpanOrigin> {
        current_span_origin()
    }

    /// Capture the current CPU-side origin only when this process is
    /// actively being recorded.
    pub fn origin_if_active(&self) -> Option<TargetSpanOrigin> {
        self.reporting_active()
            .then(|| self.current_origin())
            .flatten()
    }

    /// Capture a typed token at the queue/dispatch site, to carry with
    /// work until it starts running on this lane.
    pub fn capture_origin(&self) -> CapturedOrigin {
        if !self.reporting_active() {
            return CapturedOrigin::inactive();
        }
        match self.current_origin() {
            Some(origin) => CapturedOrigin::from_origin(origin),
            None => CapturedOrigin::active_without_origin(),
        }
    }

    /// Construct a span for this lane without an origin.
    pub fn span(&self, name: impl Into<String>, start_ns: u64, end_ns: u64) -> TargetSpan {
        TargetSpan::new(name, start_ns, end_ns)
    }

    /// Construct a validating builder for a span on this lane.
    pub fn span_builder(&self, name: impl Into<String>, start_ns: u64, end_ns: u64) -> SpanBuilder {
        SpanBuilder::new(name, start_ns, end_ns)
    }

    /// Construct a richer target dispatch builder with optional ids,
    /// shader/source metadata links, attachments, counters, and wait or
    /// completion origins.
    pub fn dispatch_builder(&self, name: impl Into<String>) -> DispatchBuilder {
        DispatchBuilder::new(name)
    }

    /// Construct a span for this lane with an explicit queue/dispatch
    /// origin.
    pub fn span_with_origin(
        &self,
        name: impl Into<String>,
        start_ns: u64,
        end_ns: u64,
        origin: TargetSpanOrigin,
    ) -> TargetSpan {
        TargetSpan::new(name, start_ns, end_ns).with_origin(origin)
    }

    /// Construct a span from a captured queue/dispatch token. Returns
    /// `None` when capture was inactive or the duration is invalid.
    pub fn span_with_captured_origin(
        &self,
        name: impl Into<String>,
        start_ns: u64,
        end_ns: u64,
        captured: CapturedOrigin,
    ) -> Option<TargetSpan> {
        if !captured.is_active() {
            return None;
        }
        self.span_builder(name, start_ns, end_ns)
            .with_captured_origin(captured)
            .build()
    }

    /// Construct a span for this lane with the current thread/timestamp
    /// as origin when the platform exposes one.
    pub fn span_with_current_origin(
        &self,
        name: impl Into<String>,
        start_ns: u64,
        end_ns: u64,
    ) -> TargetSpan {
        span_with_current_origin(name, start_ns, end_ns)
    }

    /// Begin timing a span with no origin. Useful for executor work
    /// whose natural start/end timestamps are CPU-side.
    pub fn begin_span(&self, name: impl Into<String>) -> Option<OpenSpan> {
        OpenSpan::new(name, None)
    }

    /// Begin timing a span that was queued from a known CPU-side
    /// origin.
    pub fn begin_span_with_origin(
        &self,
        name: impl Into<String>,
        origin: TargetSpanOrigin,
    ) -> Option<OpenSpan> {
        OpenSpan::new(name, Some(origin))
    }

    /// Begin timing work that carries a queue/dispatch origin token.
    pub fn begin_span_with_captured_origin(
        &self,
        name: impl Into<String>,
        captured: CapturedOrigin,
    ) -> Option<OpenSpan> {
        if !captured.is_active() {
            return None;
        }
        OpenSpan::new(name, captured.origin())
    }

    /// Report a batch of spans on this lane.
    pub fn report(&self, spans: Vec<TargetSpan>) {
        report_with_kind(&self.name, self.kind, spans);
    }

    /// Report typed target metadata records on this lane without spans.
    pub fn report_records(&self, records: TargetRecordBatch) {
        report_records_with_kind(&self.name, self.kind, records);
    }

    /// Report spans and typed target metadata records in one batch.
    pub fn report_batch(&self, spans: Vec<TargetSpan>, records: TargetRecordBatch) {
        let _ = self.try_report_batch(spans, records);
    }

    /// Fallible variant of [`Lane::report`] for integrations that want
    /// to count local queue drops.
    pub fn try_report(&self, spans: Vec<TargetSpan>) -> Result<(), ReportError> {
        try_report_with_kind(&self.name, self.kind, spans)
    }

    pub fn try_report_records(&self, records: TargetRecordBatch) -> Result<(), ReportError> {
        try_report_batch_with_kind(&self.name, self.kind, Vec::new(), records)
    }

    pub fn try_report_batch(
        &self,
        spans: Vec<TargetSpan>,
        records: TargetRecordBatch,
    ) -> Result<(), ReportError> {
        try_report_batch_with_kind(&self.name, self.kind, spans, records)
    }

    /// Report a batch only while the capture gate is active.
    pub fn report_if_active(&self, spans: Vec<TargetSpan>) -> Result<(), ReportError> {
        if !self.reporting_active() {
            return Ok(());
        }
        self.try_report(spans)
    }

    pub fn report_batch_if_active(
        &self,
        spans: Vec<TargetSpan>,
        records: TargetRecordBatch,
    ) -> Result<(), ReportError> {
        if !self.reporting_active() {
            return Ok(());
        }
        self.try_report_batch(spans, records)
    }

    /// Report one span on this lane.
    pub fn report_one(&self, span: TargetSpan) {
        self.report(vec![span]);
    }

    /// Report one span only while the capture gate is active.
    pub fn report_one_if_active(&self, span: TargetSpan) -> Result<(), ReportError> {
        self.report_if_active(vec![span])
    }
}

/// In-progress target-side span timed with [`now_ns`].
///
/// This type is explicit rather than RAII-on-drop: integrators decide
/// when a span is complete and whether to report it, which matters for
/// queues whose completion timestamp arrives from another API.
#[derive(Debug)]
pub struct OpenSpan {
    name: String,
    start_ns: u64,
    origin: Option<TargetSpanOrigin>,
}

impl OpenSpan {
    fn new(name: impl Into<String>, origin: Option<TargetSpanOrigin>) -> Option<Self> {
        if !reporting_active() {
            return None;
        }
        Some(Self {
            name: name.into(),
            start_ns: now_ns()?,
            origin,
        })
    }

    /// Finish the span and return the reportable [`TargetSpan`].
    ///
    /// Returns `None` if the platform clock is unavailable or went
    /// backwards for this span.
    pub fn finish(self) -> Option<TargetSpan> {
        let end_ns = now_ns()?;
        if end_ns <= self.start_ns {
            return None;
        }
        let span = TargetSpan::new(self.name, self.start_ns, end_ns);
        Some(match self.origin {
            Some(origin) => span.with_origin(origin),
            None => span,
        })
    }

    /// Finish and report this span to `lane`.
    pub fn finish_and_report(self, lane: &Lane) {
        if let Some(span) = self.finish() {
            lane.report_one(span);
        }
    }
}

enum WorkerMessage {
    Spans(TargetSpanBatch),
    Signals(TargetSignalBatch),
}

fn worker_sender() -> &'static Sender<WorkerMessage> {
    static SENDER: OnceLock<Sender<WorkerMessage>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_DEPTH);
        std::thread::Builder::new()
            .name("stax-target".to_owned())
            .spawn(move || worker(rx))
            .expect("spawn stax-target worker thread");
        WORKER_STARTED.store(true, Ordering::Relaxed);
        tx
    })
}

fn worker(rx: Receiver<WorkerMessage>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            REPORTING_ACTIVE.store(false, Ordering::Relaxed);
            CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
            tracing::warn!("stax-target: no tokio runtime, reporting disabled: {e}");
            return;
        }
    };
    runtime.block_on(worker_loop(rx));
}

async fn worker_loop(mut rx: Receiver<WorkerMessage>) {
    let pid = std::process::id();
    let mut client: Option<TargetIngestClient> = None;
    let mut next_poll = tokio::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(next_poll) => {
                next_poll = tokio::time::Instant::now() + POLL_INTERVAL;
                poll_capture_gate(pid, &mut client).await;
            }
            message = rx.recv() => {
                let Some(message) = message else {
                    REPORTING_ACTIVE.store(false, Ordering::Relaxed);
                    CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
                    return;
                };
                match message {
                    WorkerMessage::Spans(batch) => ingest_batch(batch, &mut client).await,
                    WorkerMessage::Signals(batch) => ingest_signal_batch(batch, &mut client).await,
                }
            }
        }
    }
}

async fn poll_capture_gate(pid: u32, client: &mut Option<TargetIngestClient>) {
    if client.is_none() {
        *client = connect().await;
    }
    let active = match client.as_ref() {
        Some(live) => match tokio::time::timeout(CALL_TIMEOUT, live.should_report(pid)).await {
            Ok(Ok(active)) => active,
            Ok(Err(e)) => {
                tracing::debug!("stax-target: gate poll failed, dropping connection: {e}");
                REPORTING_ACTIVE.store(false, Ordering::Relaxed);
                CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
                *client = None;
                false
            }
            Err(_) => {
                tracing::debug!("stax-target: gate poll timed out, dropping connection");
                REPORTING_ACTIVE.store(false, Ordering::Relaxed);
                CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
                *client = None;
                false
            }
        },
        None => false,
    };
    let was = REPORTING_ACTIVE.swap(active, Ordering::Relaxed);
    if !was && active {
        reset_reporter_stats();
        let definitions = definition_snapshot(pid);
        if !definitions.is_empty() {
            ingest_signal_batch(definitions, client).await;
        }
    }
    if was != active {
        tracing::debug!(active, "stax-target: capture gate flipped");
    }
    if !active {
        return;
    }
    let Some(live) = client.as_ref() else {
        return;
    };
    let stats = reporter_stats_for_pid(pid);
    match tokio::time::timeout(CALL_TIMEOUT, live.reporter_stats(stats)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::debug!("stax-target: reporter stats failed, dropping connection: {e}");
            CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
            *client = None;
        }
        Err(_) => {
            tracing::debug!("stax-target: reporter stats timed out, dropping connection");
            CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
            *client = None;
        }
    }
}

async fn ingest_batch(batch: TargetSpanBatch, client: &mut Option<TargetIngestClient>) {
    if client.is_none() {
        *client = connect().await;
    }
    let Some(live) = client.as_ref() else {
        return;
    };
    match tokio::time::timeout(CALL_TIMEOUT, live.ingest(batch)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::debug!("stax-target: ingest failed, dropping connection: {e}");
            CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
            *client = None;
        }
        Err(_) => {
            tracing::debug!("stax-target: ingest timed out, dropping connection");
            CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
            *client = None;
        }
    }
}

async fn ingest_signal_batch(batch: TargetSignalBatch, client: &mut Option<TargetIngestClient>) {
    if client.is_none() {
        *client = connect().await;
    }
    let Some(live) = client.as_ref() else {
        return;
    };
    match tokio::time::timeout(CALL_TIMEOUT, live.ingest_signals(batch)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::debug!("stax-target: signal ingest failed, dropping connection: {e}");
            CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
            *client = None;
        }
        Err(_) => {
            tracing::debug!("stax-target: signal ingest timed out, dropping connection");
            CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
            *client = None;
        }
    }
}

async fn connect() -> Option<TargetIngestClient> {
    let Some(socket) = stax_server_socket() else {
        CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
        return None;
    };
    let url = format!("local://{}", socket.display());
    match vox::connect_lane(&url).await {
        Ok(client) => {
            CONNECTED_TO_SERVER.store(true, Ordering::Relaxed);
            tracing::debug!("stax-target: connected to {url}");
            Some(client)
        }
        Err(e) => {
            CONNECTED_TO_SERVER.store(false, Ordering::Relaxed);
            tracing::debug!("stax-target: connect to {url} failed: {e}");
            None
        }
    }
}

/// Same resolution order as the stax CLI: explicit override, XDG
/// runtime dir, per-uid /tmp fallback. `None` when no socket exists.
fn stax_server_socket() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STAX_SERVER_SOCKET") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt).join("stax-server.sock");
        if p.exists() {
            return Some(p);
        }
    }
    let uid = unsafe { libc::getuid() };
    let p = PathBuf::from(format!("/tmp/stax-server-{uid}.sock"));
    p.exists().then_some(p)
}

#[cfg(target_os = "macos")]
pub fn current_thread_id() -> Option<u32> {
    unsafe extern "C" {
        fn pthread_threadid_np(thread: *mut libc::c_void, thread_id: *mut u64) -> libc::c_int;
    }
    let mut tid = 0u64;
    let rc = unsafe { pthread_threadid_np(std::ptr::null_mut(), &mut tid) };
    if rc != 0 || tid > u32::MAX as u64 {
        return None;
    }
    Some(tid as u32)
}

#[cfg(target_os = "linux")]
pub fn current_thread_id() -> Option<u32> {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    if tid <= 0 || tid > u32::MAX as libc::c_long {
        return None;
    }
    Some(tid as u32)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn current_thread_id() -> Option<u32> {
    None
}

#[cfg(target_os = "macos")]
fn clock_now_ns() -> Option<u64> {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
    }
    static TIMEBASE: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    let (numer, denom) = (*TIMEBASE.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        let rc = unsafe { mach_timebase_info(&mut info) };
        if rc != 0 || info.denom == 0 {
            return None;
        }
        Some((u64::from(info.numer), u64::from(info.denom)))
    }))?;
    let ticks = unsafe { mach_absolute_time() };
    Some(((ticks as u128) * (numer as u128) / (denom as u128)) as u64)
}

#[cfg(target_os = "linux")]
fn clock_now_ns() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 || ts.tv_sec < 0 || ts.tv_nsec < 0 {
        return None;
    }
    let secs = u64::try_from(ts.tv_sec).ok()?;
    let nanos = u64::try_from(ts.tv_nsec).ok()?;
    secs.checked_mul(1_000_000_000)?.checked_add(nanos)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clock_now_ns() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::{
        CapturedOrigin, Lane, SpanBuilder, TargetSpanOrigin, record_queue_full_drop,
        record_worker_disconnected_drop, reporter_stats, reset_reporter_stats,
    };

    #[test]
    fn captured_origin_distinguishes_inactive_from_no_origin() {
        let inactive = CapturedOrigin::inactive();
        assert!(!inactive.is_active());
        assert!(inactive.origin().is_none());

        let active = CapturedOrigin::active_without_origin();
        assert!(active.is_active());
        assert!(active.origin().is_none());
    }

    #[test]
    fn span_builder_rejects_invalid_duration() {
        assert!(SpanBuilder::new("bad", 10, 10).build().is_none());
        assert!(SpanBuilder::new("backwards", 11, 10).build().is_none());
    }

    #[test]
    fn span_builder_attaches_captured_origin() {
        let origin = TargetSpanOrigin {
            tid: 123,
            timestamp_ns: 456,
        };
        let span = SpanBuilder::new("work", 1, 2)
            .with_captured_origin(CapturedOrigin::from_origin(origin))
            .build()
            .expect("valid span");

        let got = span.origin.expect("origin attached");
        assert_eq!(got.tid, origin.tid);
        assert_eq!(got.timestamp_ns, origin.timestamp_ns);
    }

    #[test]
    fn lane_span_with_captured_origin_gates_on_active_token() {
        let lane = Lane::new("test lane");
        assert!(
            lane.span_with_captured_origin("work", 1, 2, CapturedOrigin::inactive())
                .is_none()
        );

        let span = lane
            .span_with_captured_origin("work", 1, 2, CapturedOrigin::active_without_origin())
            .expect("active token without origin still reports lane span");
        assert_eq!(span.name, "work");
        assert!(span.origin.is_none());
    }

    #[test]
    fn span_builder_with_captured_origin_honors_inactive_token() {
        assert!(
            SpanBuilder::new("work", 1, 2)
                .with_captured_origin(CapturedOrigin::inactive())
                .build()
                .is_none()
        );
    }

    #[test]
    fn reporter_stats_count_target_side_drops() {
        reset_reporter_stats();

        record_queue_full_drop(3);
        record_worker_disconnected_drop(5);

        let stats = reporter_stats();
        assert_eq!(stats.batches_dropped_queue_full, 1);
        assert_eq!(stats.spans_dropped_queue_full, 3);
        assert_eq!(stats.batches_dropped_worker_disconnected, 1);
        assert_eq!(stats.spans_dropped_worker_disconnected, 5);

        reset_reporter_stats();
    }

    #[test]
    fn lane_reporter_stats_matches_global_snapshot() {
        reset_reporter_stats();
        record_queue_full_drop(2);

        let lane = Lane::new("stats lane");
        assert_eq!(lane.reporter_stats(), reporter_stats());

        reset_reporter_stats();
    }
}
