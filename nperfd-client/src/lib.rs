//! Client-side driver for the nperfd RPC.
//!
//! Connects to nperfd over a vox local socket, asks it to start a
//! kperf+kdebug session for the target pid, and consumes the streaming
//! `KdBufBatch`es it sends back. Each record runs through the same
//! parser + off-CPU + libproc image scanner the in-process recorder
//! uses; output goes to a caller-provided `SampleSink`, which is the
//! same trait the in-process flow already feeds. Live UI / archive
//! sinks plug in unchanged — the only thing that's different is where
//! the kdebug records came from.
//!
//! v0 scope (deliberately incomplete):
//! - parser, off-CPU tracker, image scan, thread name scan: ✅
//! - jitdump tailing, kernel symbols / slide estimation: ⏭ deferred.
//!   Plumbing in once the trinity end-to-end is validated.

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use nerf_mac_capture::proc_maps::MachOSymbol;
use nerf_mac_capture::recorder::ThreadNameCache;
use nerf_mac_capture::sample_sink::{CpuIntervalEvent, CpuIntervalKind};
use nerf_mac_capture::{
    BinaryLoadedEvent, JitdumpEvent, SampleEvent, SampleSink, ThreadNameEvent, WakeupEvent,
};
use nerf_mac_kperf_parse::image_scan::ImageScanner;
use nerf_mac_kperf_parse::libproc;
use nerf_mac_kperf_parse::offcpu::{CpuIntervalTracker, PendingKind};
use nerf_mac_kperf_parse::parser::Parser;
use nerf_mac_kperf_sys::bindings::sampler;
use nerf_mac_kperf_sys::kdebug::{
    self, kdbg_class, kdbg_subclass, KdBuf, DBG_MACH, DBG_MACH_SCHED, DBG_PERF,
};
use nperfd_proto::{KdBufBatch, KdBufWire, NperfdClient, SessionConfig};
use log::{info, warn};

/// User-facing options. Mirrors the shape of
/// `nerf_mac_kperf::RecordOptions` so plumbing through the existing
/// CLI is mechanical.
#[derive(Clone, Debug)]
pub struct RemoteOptions {
    /// `local://` URL or path of the daemon socket. Either
    /// `local:///var/run/nperfd.sock` or just `/var/run/nperfd.sock`
    /// works; the latter is normalised below.
    pub daemon_socket: String,
    /// Target pid.
    pub pid: u32,
    /// Sampling frequency in Hz.
    pub frequency_hz: u32,
    /// If `Some`, stop after this duration. The daemon's drain loop
    /// continues until we close the records channel; we do that when
    /// the duration elapses or `should_stop` returns `true`.
    pub duration: Option<Duration>,
    /// kdebug ringbuffer size in records. Mirrors the in-process default.
    pub buf_records: u32,
}

impl Default for RemoteOptions {
    fn default() -> Self {
        Self {
            daemon_socket: "/tmp/nperfd.sock".into(),
            pid: 0,
            frequency_hz: 1000,
            duration: None,
            buf_records: 1_000_000,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("connecting to nperfd at {url}: {source}")]
    Connect { url: String, source: Box<dyn std::error::Error + Send + Sync> },

    #[error("nperfd record() RPC failed: {0:?}")]
    Rpc(nperfd_proto::RecordError),

    #[error("vox call returned an error: {0}")]
    VoxCall(String),
}

/// Run a remote recording session. Blocks until `should_stop` returns
/// `true`, the duration elapses, or the daemon closes the channel
/// (typically because it errored out, e.g. lost ktrace ownership).
///
/// The caller's `sink` receives the same events the in-process
/// recorder emits — `on_sample`, `on_thread_name`, `on_binary_loaded`,
/// `on_wakeup`, etc. — so live aggregators / archive writers plug in
/// without changes.
pub async fn drive_session<S: SampleSink>(
    opts: RemoteOptions,
    sink: &mut S,
    mut should_stop: impl FnMut() -> bool,
) -> Result<(), Error> {
    let url = if opts.daemon_socket.starts_with("local://") {
        opts.daemon_socket.clone()
    } else {
        format!("local://{}", opts.daemon_socket)
    };

    info!("nperfd-client: connecting to {url}");
    let client: NperfdClient = match vox::connect(&url).await {
        Ok(c) => c,
        Err(e) => {
            // The "no such file" case dominates — the user forgot to
            // start the daemon. Render an actionable hint instead of
            // bare io::ErrorKind::NotFound so they know what to do.
            let socket_missing = !std::path::Path::new(&opts.daemon_socket).exists()
                && !opts.daemon_socket.starts_with("local://");
            let hint = if socket_missing {
                " (daemon socket doesn't exist — is nperfd running? \
                 try `sudo nperf setup` to install it as a LaunchDaemon, \
                 or `sudo nperfd --socket <path>` for a one-off)"
            } else {
                ""
            };
            return Err(Error::Connect {
                url: format!("{url}{hint}"),
                source: Box::new(e),
            });
        }
    };

    let status = client
        .status()
        .await
        .map_err(|e| Error::VoxCall(format!("status: {e:?}")))?;
    info!(
        "nperfd-client: daemon v{} arch={} state={:?}",
        status.version, status.host_arch, status.state
    );

    // Plumb the SharedCache to the sink up front so disassembly /
    // symbol resolution for system frames works the same way it does
    // in the in-process flow. SharedCache::for_host opens
    // dyld_shared_cache_<arch>, parses it once, and shares it via Arc.
    if let Some(sc) = nperf_mac_shared_cache::SharedCache::for_host().map(Arc::new) {
        sink.on_macho_byte_source(sc);
    }
    let mut images = ImageScanner::new(None);
    let mut thread_names = ThreadNameCache::new();

    // Initial libproc walk: image regions + thread names. Both work
    // unprivileged for same-uid pids, so they happen client-side
    // without going through the broker yet.
    let t0 = Instant::now();
    images.rescan(opts.pid, sink);
    info!("initial image scan took {:?}", t0.elapsed());
    scan_thread_names(opts.pid, sink, &mut thread_names);

    // Build the session config the daemon expects. Filter range covers
    // DBG_MACH..DBG_PERF, mirroring the in-process recorder's default
    // (so context switches + kperf samples both flow through).
    let session_config = SessionConfig {
        target_pid: opts.pid,
        frequency_hz: opts.frequency_hz,
        buf_records: opts.buf_records,
        samplers: sampler::TH_INFO | sampler::USTACK | sampler::KSTACK | sampler::PMC_THREAD,
        // v0: no configurable PMU events. The daemon defaults to
        // FIXED-only counters (cycles + instructions retired on Apple
        // Silicon), which is the same as what `kpc_set_counting`
        // wants us to enable.
        pmu_event_configs: Vec::new(),
        class_mask: nerf_mac_kperf_sys::bindings::KPC_CLASS_FIXED_MASK,
        filter_range_value1: kdebug::kdbg_eventid(DBG_MACH, DBG_MACH_SCHED, 0),
        filter_range_value2: kdebug::kdbg_eventid(DBG_PERF, 0xff, 0x3fff),
    };

    // Server→client streaming: we construct the channel, hand `tx` to
    // the RPC, drain `rx` here. The RPC future doesn't resolve until
    // the daemon's record() returns (clean stop or error).
    let (tx, mut rx) = vox::channel::<KdBufBatch>();
    let record_fut = tokio::spawn({
        let client = client.clone();
        async move { client.record(session_config, tx).await }
    });

    let session_start = Instant::now();
    let image_period = Duration::from_millis(250);
    let thread_period = Duration::from_millis(50);
    let mut next_image = Instant::now() + image_period;
    let mut next_thread = Instant::now() + thread_period;

    let mut parser = Parser::new();
    let mut offcpu = CpuIntervalTracker::default();
    let mut total_drained: u64 = 0;
    // First-batch log: tells the user "yes, the daemon is streaming
    // records" without spamming the per-batch case. Once the first
    // batch lands we go quiet until the session ends.
    let mut seen_first_batch = false;
    // Track which side ended the session. On user / duration stop
    // we have to force-close the underlying connection (see end of
    // function); on daemon-initiated end we just await the RPC for
    // its `RecordSummary` / error.
    let mut user_stopped = false;

    loop {
        if should_stop() {
            user_stopped = true;
            info!("nperfd-client: stop requested");
            break;
        }
        if let Some(d) = opts.duration
            && session_start.elapsed() >= d
        {
            user_stopped = true;
            info!("nperfd-client: duration elapsed");
            break;
        }

        // Wake roughly every 50ms regardless of traffic so the
        // periodic libproc scans keep ticking even on idle targets.
        let recv_timeout = Duration::from_millis(50);
        let batch_sref = match tokio::time::timeout(recv_timeout, rx.recv()).await {
            Ok(Ok(Some(value))) => Some(value),
            Ok(Ok(None)) => {
                info!("nperfd-client: daemon closed records channel");
                break;
            }
            Ok(Err(e)) => {
                warn!("nperfd-client: recv error: {e:?}");
                break;
            }
            Err(_) => None, // Timeout — fall through to periodic work.
        };

        if Instant::now() >= next_image {
            images.rescan(opts.pid, sink);
            next_image = Instant::now() + image_period;
        }
        if Instant::now() >= next_thread {
            scan_thread_names(opts.pid, sink, &mut thread_names);
            next_thread = Instant::now() + thread_period;
        }

        let Some(batch_sref) = batch_sref else {
            continue;
        };
        // SelfRef<KdBufBatch> doesn't expose a borrowing accessor for
        // owned types without a `Reborrow` impl; `.map(...)` consumes
        // the SelfRef and lets us hold the owned value for the duration
        // of the closure, which is all we need.
        let pid = opts.pid;
        let _ = batch_sref.map(|batch| {
            if !seen_first_batch {
                seen_first_batch = true;
                info!(
                    "nperfd-client: first batch ({} records) arrived {:?} after session start",
                    batch.records.len(),
                    session_start.elapsed(),
                );
            }
            total_drained += batch.records.len() as u64;
            process_batch(&batch.records, &mut parser, &mut offcpu, sink, pid);
        });
    }

    if user_stopped {
        // Force the underlying connection closed so the daemon's
        // record() future actually unblocks.
        //
        // vox's per-channel close-on-drop does NOT currently
        // propagate to the server's Tx::send (only whole-connection
        // close does, see vox-core/src/driver.rs::send_payload), so
        // dropping `rx` alone leaves the daemon spinning in its
        // drain loop forever, sending into a void. With
        // `non_resumable` on the acceptor builder, killing the
        // connection makes the daemon's tx.send fail with
        // Transport("connection closed") and its loop breaks
        // cleanly. This stops being necessary once vox propagates
        // per-channel close itself.
        info!("nperfd-client: forcing connection close (workaround until vox propagates per-channel close)");
        record_fut.abort();
        drop(rx);
        drop(client);
        // Give the runtime a tick so the abort actually drops the
        // spawned task's client clone before we return — that's
        // what makes the connection drop and the daemon notice.
        tokio::task::yield_now().await;
    } else {
        // Daemon-initiated end (clean shutdown or error). Await
        // the RPC for its RecordSummary or error.
        drop(rx);
        let rpc_result = record_fut
            .await
            .map_err(|e| Error::VoxCall(format!("join: {e:?}")))?;
        match rpc_result {
            Ok(summary) => info!(
                "nperfd-client: session ended cleanly, daemon drained {} records ({:?} session)",
                summary.records_drained,
                Duration::from_nanos(summary.session_ns)
            ),
            Err(vox::VoxError::User(e)) => {
                warn!("nperfd-client: daemon returned error: {e:?}");
                return Err(Error::Rpc(e));
            }
            Err(e) => {
                return Err(Error::VoxCall(format!("record rpc: {e:?}")));
            }
        }
    }
    let s = &parser.stats;
    info!(
        "nperfd-client: locally drained {total_drained} records, parser \
         started/emitted/orphaned: {}/{}/{}, walk errors u/k: {}/{}",
        s.samples_started,
        s.samples_emitted,
        s.samples_orphaned,
        s.user_walk_errors,
        s.kernel_walk_errors,
    );
    Ok(())
}

fn process_batch<S: SampleSink>(
    records: &[KdBufWire],
    parser: &mut Parser,
    offcpu: &mut CpuIntervalTracker,
    sink: &mut S,
    pid: u32,
) {
    for wire in records {
        let rec = wire_to_kdbuf(wire);
        let class = kdbg_class(rec.debugid);
        if class == DBG_MACH && kdbg_subclass(rec.debugid) == DBG_MACH_SCHED {
            offcpu.feed(&rec);
            continue;
        }
        parser.feed(&rec, |sample| {
            // Cache the on-CPU stack on the offcpu tracker. This is
            // what the next off-CPU interval gets attributed to —
            // without it, every off-CPU interval surfaces with an
            // empty user stack and shows up under "(no stack)" in
            // the UI.
            offcpu.note_sample(
                sample.tid,
                sample.user_backtrace,
                sample.kernel_backtrace,
            );
            // Drop empty-user-stack samples for the same reason
            // the in-process recorder does (recorder.rs:615): with
            // lightweight_pet=0 the kernel emits a sample bracket
            // for every thread on every tick, including blocked
            // ones with no live user PC. Forwarding those just
            // inflates the in-kernel residue.
            if sample.user_backtrace.is_empty() {
                return;
            }
            sink.on_sample(SampleEvent {
                timestamp_ns: sample.timestamp_ns,
                pid,
                tid: sample.tid,
                backtrace: sample.user_backtrace,
                kernel_backtrace: sample.kernel_backtrace,
                cycles: sample.pmc.first().copied().unwrap_or(0),
                instructions: sample.pmc.get(1).copied().unwrap_or(0),
                l1d_misses: 0,
                branch_mispreds: 0,
            });
        });
    }

    // Wakeups + closed CPU intervals (on/off) fall out of the tracker
    // per batch, same shape the in-process driver emits. Without the
    // interval forwarding, the aggregator never gets time accounting
    // and the UI's on-CPU/off-CPU totals stay at 0 even though PET
    // samples are accumulating. recorder.rs:647-693 is the canonical
    // version of this; we mirror it here.
    for w in offcpu.drain_wakeups() {
        sink.on_wakeup(WakeupEvent {
            timestamp_ns: w.timestamp_ns,
            pid,
            waker_tid: w.waker_tid,
            wakee_tid: w.wakee_tid,
            waker_user_stack: &w.waker_user_stack,
            waker_kernel_stack: &w.waker_kernel_stack,
        });
    }
    for interval in offcpu.drain_pending() {
        match interval.kind {
            PendingKind::OnCpu => {
                sink.on_cpu_interval(CpuIntervalEvent {
                    pid,
                    tid: interval.tid,
                    start_ns: interval.start_ns,
                    end_ns: interval.end_ns,
                    kind: CpuIntervalKind::OnCpu,
                });
            }
            PendingKind::OffCpu {
                user_stack,
                kernel_stack: _,
                waker_tid,
                waker_user_stack,
            } => {
                sink.on_cpu_interval(CpuIntervalEvent {
                    pid,
                    tid: interval.tid,
                    start_ns: interval.start_ns,
                    end_ns: interval.end_ns,
                    kind: CpuIntervalKind::OffCpu {
                        stack: &user_stack,
                        waker_tid,
                        waker_user_stack: waker_user_stack.as_deref(),
                    },
                });
            }
        }
    }
}

fn wire_to_kdbuf(w: &KdBufWire) -> KdBuf {
    KdBuf {
        timestamp: w.timestamp,
        arg1: w.arg1,
        arg2: w.arg2,
        arg3: w.arg3,
        arg4: w.arg4,
        arg5: w.arg5,
        debugid: w.debugid,
        cpuid: w.cpuid,
        unused: w.unused,
    }
}

/// Same shape as the in-process recorder's helper — emit a
/// `ThreadNameEvent` for each (tid, name) the cache hasn't seen yet.
fn scan_thread_names<S: SampleSink>(
    pid: u32,
    sink: &mut S,
    cache: &mut ThreadNameCache,
) {
    let tids = match libproc::list_thread_ids(pid) {
        Ok(t) => t,
        Err(_) => return,
    };
    for tid64 in tids {
        let tid = tid64 as u32;
        if let Ok(Some(name)) = libproc::thread_name(pid, tid64)
            && cache.note_thread(tid, &name)
        {
            sink.on_thread_name(ThreadNameEvent { pid, tid, name: &name });
        }
    }
}

// Keep the `BinaryLoadedEvent` / `JitdumpEvent` / `MachOSymbol` types
// referenced so future iterations can re-add jitdump tailing + kernel
// symbol scanning without churning the import block.
#[allow(dead_code)]
fn _doc_anchors(_: BinaryLoadedEvent<'_>, _: JitdumpEvent<'_>, _: MachOSymbol) {}
