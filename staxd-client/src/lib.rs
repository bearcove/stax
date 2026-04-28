//! Client-side driver for the staxd RPC.
//!
//! Connects to staxd over a vox local socket, asks it to start a
//! kperf+kdebug session for the target pid, and consumes the streaming
//! `KdBufBatch`es it sends back. Each record runs through the shared
//! [`Pipeline`] in `stax-mac-kperf-parse` — the same parser, off-CPU
//! tracker, libproc image / thread scanner, kernel-symbol slide
//! estimator, and jitdump tailer the in-process recorder uses, so
//! the daemon-driven path emits exactly the same `SampleSink` event
//! sequence as the in-process kperf path.

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use stax_mac_capture::{ProbeResultEvent, SampleSink};
use stax_mac_kperf_parse::pipeline::{Pipeline, PipelineConfig};
use stax_mac_kperf_sys::bindings::sampler;
use stax_mac_kperf_sys::kdebug::{self, KdBuf, DBG_MACH, DBG_MACH_SCHED, DBG_PERF};
use staxd_proto::{KdBufBatch, KdBufWire, StaxdClient, SessionConfig};
use log::{info, warn};

/// User-facing options. Mirrors the shape of
/// `stax_mac_kperf::RecordOptions` so plumbing through the existing
/// CLI is mechanical.
#[derive(Clone, Debug)]
pub struct RemoteOptions {
    /// `local://` URL or path of the daemon socket. Either
    /// `local:///var/run/staxd.sock` or just `/var/run/staxd.sock`
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
            daemon_socket: "/tmp/staxd.sock".into(),
            pid: 0,
            frequency_hz: 1000,
            duration: None,
            buf_records: 1_000_000,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("connecting to staxd at {url}: {source}")]
    Connect { url: String, source: Box<dyn std::error::Error + Send + Sync> },

    #[error("staxd record() RPC failed: {0:?}")]
    Rpc(staxd_proto::RecordError),

    #[error("vox call returned an error: {0}")]
    VoxCall(String),
}

/// Run a remote recording session. Blocks until `should_stop` returns
/// `true`, the duration elapses, or the daemon closes the channel
/// (typically because it errored out, e.g. lost ktrace ownership).
///
/// The caller's `sink` receives the same events the in-process
/// recorder emits — `on_sample`, `on_thread_name`, `on_binary_loaded`,
/// `on_wakeup`, `on_cpu_interval`, `on_jitdump`, `on_kallsyms`, etc. —
/// so live aggregators / archive writers plug in without changes.
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

    info!("staxd-client: connecting to {url}");
    let client: StaxdClient = match vox::connect(&url).await {
        Ok(c) => c,
        Err(e) => {
            // The "no such file" case dominates — the user forgot to
            // start the daemon. Render an actionable hint instead of
            // bare io::ErrorKind::NotFound so they know what to do.
            let socket_missing = !std::path::Path::new(&opts.daemon_socket).exists()
                && !opts.daemon_socket.starts_with("local://");
            let hint = if socket_missing {
                " (daemon socket doesn't exist — is staxd running? \
                 try `sudo stax setup` to install it as a LaunchDaemon, \
                 or `sudo staxd --socket <path>` for a one-off)"
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
        "staxd-client: daemon v{} arch={} state={:?}",
        status.version, status.host_arch, status.state
    );

    // Pipeline owns the parser + off-CPU tracker + image scanner +
    // thread name cache + kernel image + slide estimator + jitdump
    // tailer, and runs all the periodic libproc scans. The in-process
    // recorder uses the same Pipeline, so the daemon-driven path
    // emits the same SampleSink event sequence — feature parity by
    // construction.
    //
    // v0 doesn't yet plumb configurable PMU events through the
    // SessionConfig wire surface, so the pmc indices stay None
    // (Apple Silicon FIXED counters — cycles + insns retired —
    // still flow through the kperf samplers).
    let shared_cache: Option<Arc<stax_mac_shared_cache::SharedCache>> =
        stax_mac_shared_cache::SharedCache::for_host().map(Arc::new);
    let mut pipeline = Pipeline::new(
        PipelineConfig {
            pid: opts.pid,
            frequency_hz: opts.frequency_hz,
            pmc_idx_l1d: None,
            pmc_idx_brmiss: None,
        },
        shared_cache,
        sink,
    );

    // Build the session config the daemon expects. Filter range covers
    // DBG_MACH..DBG_PERF, mirroring the in-process recorder's default
    // (so context switches + kperf samples both flow through).
    let session_config = SessionConfig {
        target_pid: opts.pid,
        frequency_hz: opts.frequency_hz,
        buf_records: opts.buf_records,
        samplers: sampler::TH_INFO | sampler::USTACK | sampler::KSTACK | sampler::PMC_THREAD,
        // v0: no configurable PMU events. Daemon falls back to FIXED.
        pmu_event_configs: Vec::new(),
        class_mask: stax_mac_kperf_sys::bindings::KPC_CLASS_FIXED_MASK,
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
    let mut total_drained: u64 = 0;
    let mut seen_first_batch = false;

    loop {
        if should_stop() {
            info!("staxd-client: stop requested");
            break;
        }
        if let Some(d) = opts.duration
            && session_start.elapsed() >= d
        {
            info!("staxd-client: duration elapsed");
            break;
        }

        // Wake roughly every 50ms regardless of traffic so the
        // pipeline's periodic scans (libproc images, thread names,
        // jitdump probe) keep ticking even on idle targets.
        let recv_timeout = Duration::from_millis(50);
        let batch_sref = match tokio::time::timeout(recv_timeout, rx.recv()).await {
            Ok(Ok(Some(value))) => Some(value),
            Ok(Ok(None)) => {
                info!("staxd-client: daemon closed records channel");
                break;
            }
            Ok(Err(e)) => {
                warn!("staxd-client: recv error: {e:?}");
                break;
            }
            Err(_) => None, // Timeout — fall through to periodic work.
        };

        pipeline.tick(sink);

        let Some(batch_sref) = batch_sref else {
            continue;
        };
        // SelfRef<KdBufBatch> doesn't expose a borrowing accessor for
        // owned types without a `Reborrow` impl; `.map(...)` consumes
        // the SelfRef and lets us hold the owned value for the
        // duration of the closure.
        let _ = batch_sref.map(|batch| {
            if !seen_first_batch {
                seen_first_batch = true;
                info!(
                    "staxd-client: first batch ({} records) arrived {:?} after session start",
                    batch.records.len(),
                    session_start.elapsed(),
                );
            }
            total_drained += batch.records.len() as u64;
            let kdbufs: Vec<KdBuf> = batch.records.iter().map(wire_to_kdbuf).collect();
            pipeline.process_records(&kdbufs, sink);

            // Probe results ride alongside the kperf records — emit
            // them through the same sink so they reach the same
            // symbolicator + aggregator + UI as kperf samples. The
            // pipeline doesn't need to correlate; downstream pairs
            // by (tid, kperf_ts == sample.timestamp_ns).
            for pr in &batch.probe_results {
                sink.on_probe_result(ProbeResultEvent {
                    tid: pr.tid,
                    kperf_ts_ns: pr.kperf_ts_mach,
                    probe_done_ns: pr.probe_done_mach,
                    mach_pc: pr.mach_pc,
                    mach_lr: pr.mach_lr,
                    mach_fp: pr.mach_fp,
                    mach_sp: pr.mach_sp,
                    mach_walked: &pr.mach_walked,
                    used_framehop: pr.used_framehop,
                });
            }
        });
    }

    // Drop our `Rx`. With vox propagating per-channel close to the
    // server's Tx::send (since the upstream patch), the daemon's
    // next send fails with Transport, its drain loop breaks, kperf
    // teardown runs, record() returns with a RecordSummary or error.
    drop(rx);
    let rpc_result = record_fut
        .await
        .map_err(|e| Error::VoxCall(format!("join: {e:?}")))?;
    match rpc_result {
        Ok(summary) => info!(
            "staxd-client: session ended cleanly, daemon drained {} records ({:?} session)",
            summary.records_drained,
            Duration::from_nanos(summary.session_ns)
        ),
        Err(vox::VoxError::User(e)) => {
            warn!("staxd-client: daemon returned error: {e:?}");
            return Err(Error::Rpc(e));
        }
        Err(e) => {
            return Err(Error::VoxCall(format!("record rpc: {e:?}")));
        }
    }

    info!("staxd-client: locally drained {total_drained} records");
    pipeline.finish(sink);
    Ok(())
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
