//! Target-side latch for stax.
//!
//! A profiled app links this crate to put execution lanes the CPU
//! sampler cannot see — GPU command queues, accelerators, runtime
//! queues, worker pools — on a live stax recording's timeline (see
//! `stax-server`'s `TargetIngest`: spans become a synthetic thread
//! with named frames in the existing views).
//! Targets can also attach a [`TargetSpanOrigin`] captured with
//! [`current_span_origin`] so stax can place accelerator work under the
//! CPU stack that queued it.
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
//!   pid doesn't match the active run's target. Lossy by design.
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
//!
//! // Queue side: capture provenance only while stax is recording us.
//! let origin = lane.capture_origin();
//!
//! // Worker side: time the work where it actually runs.
//! if let Some(open) = lane.begin_span_with_captured_origin("decode chunk", origin) {
//!     // decode_chunk();
//!     open.finish_and_report(&lane);
//! }
//! ```
//!
//! For APIs that already return exact timestamps, use
//! [`Lane::span_with_captured_origin`] and [`Lane::report_one`].

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

/// Per-call timeout: a half-dead connection (e.g. server dropped us for
/// missing keepalive pongs) must never wedge the worker — time out,
/// drop the client, reconnect on a later poll.
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

use stax_live_proto::{TargetIngestClient, TargetSpanBatch};
pub use stax_live_proto::{TargetSpan, TargetSpanOrigin};

/// Bounded queue between reporting threads and the worker. Each entry
/// is one batch; overflow drops the newest batch (profiling telemetry,
/// not ledger data).
const QUEUE_DEPTH: usize = 64;

/// Capture-gate poll period. Attach/detach latency is bounded by this.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Whether the app's pid is currently being recorded.
static REPORTING_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Capture gate: `true` while a stax recording of this process is
/// active. One relaxed atomic load — safe to read on hot paths. The
/// first call arms the background worker (and thus the polling), so
/// apps need no explicit init: read the gate where capture is decided.
pub fn reporting_active() -> bool {
    let _ = worker_sender();
    REPORTING_ACTIVE.load(Ordering::Relaxed)
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

/// Fallible variant of [`report`] for integrations that want to count
/// local queue drops.
pub fn try_report(lane: &str, spans: Vec<TargetSpan>) -> Result<(), ReportError> {
    if spans.is_empty() {
        return Ok(());
    }
    let sender = worker_sender();
    let batch = TargetSpanBatch {
        pid: std::process::id(),
        lane: lane.to_owned(),
        spans,
    };
    match sender.try_send(batch) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => {
            tracing::debug!("stax-target queue full; dropping span batch");
            Err(ReportError::QueueFull)
        }
        Err(TrySendError::Disconnected(_)) => Err(ReportError::WorkerDisconnected),
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

    /// Validate and construct the reportable span.
    pub fn build(self) -> Option<TargetSpan> {
        if !self.active {
            return None;
        }
        if self.end_ns <= self.start_ns {
            return None;
        }
        let span = TargetSpan::new(self.name, self.start_ns, self.end_ns);
        Some(match self.origin {
            Some(origin) => span.with_origin(origin),
            None => span,
        })
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
}

impl Lane {
    /// Create a lane handle. The name is what `stax threads` prints.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// Lane name as it will appear in stax.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Same capture gate as [`reporting_active`], scoped for call sites
    /// that already hold a lane handle.
    pub fn reporting_active(&self) -> bool {
        reporting_active()
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
        report(&self.name, spans);
    }

    /// Fallible variant of [`Lane::report`] for integrations that want
    /// to count local queue drops.
    pub fn try_report(&self, spans: Vec<TargetSpan>) -> Result<(), ReportError> {
        try_report(&self.name, spans)
    }

    /// Report a batch only while the capture gate is active.
    pub fn report_if_active(&self, spans: Vec<TargetSpan>) -> Result<(), ReportError> {
        if !self.reporting_active() {
            return Ok(());
        }
        self.try_report(spans)
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

fn worker_sender() -> &'static SyncSender<TargetSpanBatch> {
    static SENDER: OnceLock<SyncSender<TargetSpanBatch>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
        std::thread::Builder::new()
            .name("stax-target".to_owned())
            .spawn(move || worker(rx))
            .expect("spawn stax-target worker thread");
        tx
    })
}

fn worker(rx: Receiver<TargetSpanBatch>) {
    // Multi-thread runtime (one background worker) so vox keepalive
    // pongs are answered while this thread is parked in recv_timeout —
    // stax-server pings every 5s and drops links that miss pongs for
    // 30s, which a current-thread runtime (driven only inside block_on)
    // cannot answer in time.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("stax-target-io")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::warn!("stax-target: no tokio runtime, span reporting disabled: {e}");
            for _ in rx {}
            return;
        }
    };
    let pid = std::process::id();
    let mut client: Option<TargetIngestClient> = None;
    let mut next_poll = Instant::now();
    loop {
        // Poll the capture gate when due.
        let now = Instant::now();
        if now >= next_poll {
            next_poll = now + POLL_INTERVAL;
            runtime.block_on(async {
                if client.is_none() {
                    client = connect().await;
                }
                let active = match client.as_ref() {
                    Some(live) => {
                        match tokio::time::timeout(CALL_TIMEOUT, live.should_report(pid)).await {
                            Ok(Ok(active)) => active,
                            Ok(Err(e)) => {
                                tracing::debug!(
                                    "stax-target: gate poll failed, dropping connection: {e}"
                                );
                                client = None;
                                false
                            }
                            Err(_) => {
                                tracing::debug!(
                                    "stax-target: gate poll timed out, dropping connection"
                                );
                                client = None;
                                false
                            }
                        }
                    }
                    None => false,
                };
                let was = REPORTING_ACTIVE.swap(active, Ordering::Relaxed);
                if was != active {
                    tracing::debug!(active, "stax-target: capture gate flipped");
                }
            });
        }
        // Pump batches until the next poll is due.
        let wait = next_poll.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(batch) => runtime.block_on(async {
                if client.is_none() {
                    client = connect().await;
                }
                let Some(live) = client.as_ref() else {
                    return;
                };
                match tokio::time::timeout(CALL_TIMEOUT, live.ingest(batch)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::debug!("stax-target: ingest failed, dropping connection: {e}");
                        client = None;
                    }
                    Err(_) => {
                        tracing::debug!("stax-target: ingest timed out, dropping connection");
                        client = None;
                    }
                }
            }),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

async fn connect() -> Option<TargetIngestClient> {
    let socket = stax_server_socket()?;
    let url = format!("local://{}", socket.display());
    match vox::connect(&url).await {
        Ok(client) => {
            tracing::debug!("stax-target: connected to {url}");
            Some(client)
        }
        Err(e) => {
            tracing::debug!("stax-target: connect to {url} failed: {e}");
            None
        }
    }
}

/// Same resolution order as the stax CLI: explicit override, XDG
/// runtime dir, per-uid /tmp fallback. `None` when no socket exists
/// (server not running) — polling then costs one `stat` per period.
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
fn current_thread_id() -> Option<u32> {
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
fn current_thread_id() -> Option<u32> {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    if tid <= 0 || tid > u32::MAX as libc::c_long {
        return None;
    }
    Some(tid as u32)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn current_thread_id() -> Option<u32> {
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
    use super::{CapturedOrigin, Lane, SpanBuilder, TargetSpanOrigin};

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
}
