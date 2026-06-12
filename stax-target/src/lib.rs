//! Target-side latch for stax.
//!
//! A profiled app links this crate to put execution lanes the CPU
//! sampler cannot see — GPU command queues, accelerators — on a live
//! stax recording's timeline (see `stax-server`'s `TargetIngest`: spans
//! become a synthetic thread with named frames in the existing views).
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

/// Report one lane's spans. Cheap and non-blocking; safe to call from
/// hot paths. `lane` names the synthetic thread (e.g. "GPU tq1s").
/// Batches sent while no recording is active are dropped server-side
/// (and capture should be gated on [`reporting_active`] anyway, so the
/// steady-state idle cost is nil).
pub fn report(lane: &str, spans: Vec<TargetSpan>) {
    if spans.is_empty() {
        return;
    }
    let sender = worker_sender();
    let batch = TargetSpanBatch {
        pid: std::process::id(),
        lane: lane.to_owned(),
        spans,
    };
    match sender.try_send(batch) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            tracing::debug!("stax-target queue full; dropping span batch");
        }
        Err(TrySendError::Disconnected(_)) => {}
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
