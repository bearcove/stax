//! Forward `LiveSink` events to `stax-server` over a vox local
//! socket. Async-trait callbacks are intentionally tiny: each one
//! pushes an owned `IngestEvent` into a sync-friendly tokio mpsc
//! and returns immediately. A separate forwarder task drains the
//! mpsc and pumps events through `vox::Tx::send` at whatever rate
//! the wire allows.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stax_live_proto::{
    IngestEvent, RunId, RunIngestClient, WireBinaryLoaded, WireBinaryUnloaded, WireMachOSymbol,
    WireOffCpuInterval, WireOnCpuInterval, WireSampleEvent, WireWakeup,
};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::live_sink::{
    BinaryLoadedEvent, BinaryUnloadedEvent, CpuIntervalEvent, CpuIntervalKind, LiveSink,
    SampleEvent, TargetAttached, ThreadName, WakeupEvent,
};

#[cfg(target_os = "macos")]
use crate::live_sink::MachOByteSource;

/// `LiveSink` impl that drops every event into a channel which a
/// forwarder task drains and pushes into a vox `Tx<IngestEvent>`.
///
/// `stop_requested` flips to `true` when the forwarder sees the
/// vox `Tx` reject a send — typically because stax-server dropped
/// its `Rx<IngestEvent>` after a `RunControl::stop_active`. The
/// recorder loop polls `LiveSink::stop_requested()` to break out
/// of `drive_session` cleanly.
pub struct IngestSink {
    tx: UnboundedSender<IngestEvent>,
    reliable_tx: std::sync::mpsc::Sender<ReliableIngest>,
    stop_requested: Arc<AtomicBool>,
}

impl IngestSink {
    fn new(
        tx: UnboundedSender<IngestEvent>,
        reliable_tx: std::sync::mpsc::Sender<ReliableIngest>,
        stop_requested: Arc<AtomicBool>,
    ) -> Self {
        Self {
            tx,
            reliable_tx,
            stop_requested,
        }
    }

    fn reliable_call(&self, msg: ReliableIngestMsg) {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        if self
            .reliable_tx
            .send(ReliableIngest {
                msg,
                reply: reply_tx,
            })
            .is_err()
        {
            self.stop_requested.store(true, Ordering::Relaxed);
            return;
        }
        match reply_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                log::warn!("reliable ingest call failed: {err}");
                self.stop_requested.store(true, Ordering::Relaxed);
            }
            Err(err) => {
                log::warn!("reliable ingest reply channel closed: {err}");
                self.stop_requested.store(true, Ordering::Relaxed);
            }
        }
    }
}

#[async_trait::async_trait]
impl LiveSink for IngestSink {
    fn stop_flag(&self) -> Option<Arc<AtomicBool>> {
        Some(self.stop_requested.clone())
    }

    async fn on_sample(&self, ev: &SampleEvent) {
        let user_backtrace = ev.user_backtrace.iter().map(|f| f.address).collect();
        let _ = self.tx.send(IngestEvent::Sample(WireSampleEvent {
            timestamp_ns: ev.timestamp,
            pid: ev.pid,
            tid: ev.tid,
            kernel_backtrace: ev.kernel_backtrace.to_vec(),
            user_backtrace,
            cycles: ev.cycles,
            instructions: ev.instructions,
            l1d_misses: ev.l1d_misses,
            branch_mispreds: ev.branch_mispreds,
        }));
    }

    async fn on_target_attached(&self, ev: &TargetAttached) {
        self.reliable_call(ReliableIngestMsg::TargetAttached {
            pid: ev.pid,
            task_port: ev.task_port,
        });
    }

    async fn on_binary_loaded(&self, ev: &BinaryLoadedEvent) {
        let symbols = ev
            .symbols
            .iter()
            .map(|s| WireMachOSymbol {
                start_svma: s.start_svma,
                end_svma: s.end_svma,
                name: s.name.to_vec(),
            })
            .collect();
        self.reliable_call(ReliableIngestMsg::BinaryLoaded(WireBinaryLoaded {
            path: ev.path.to_owned(),
            base_avma: ev.base_avma,
            vmsize: ev.vmsize,
            text_svma: ev.text_svma,
            arch: ev.arch.map(|s| s.to_owned()),
            is_executable: ev.is_executable,
            symbols,
            text_bytes: ev.text_bytes.map(|b| b.to_vec()),
        }));
    }

    async fn on_binary_unloaded(&self, ev: &BinaryUnloadedEvent) {
        self.reliable_call(ReliableIngestMsg::BinaryUnloaded(WireBinaryUnloaded {
            path: ev.path.to_owned(),
            base_avma: ev.base_avma,
        }));
    }

    async fn on_thread_name(&self, ev: &ThreadName) {
        let _ = self.tx.send(IngestEvent::ThreadName {
            pid: ev.pid,
            tid: ev.tid,
            name: ev.name.to_owned(),
        });
    }

    async fn on_wakeup(&self, ev: &WakeupEvent) {
        let _ = self.tx.send(IngestEvent::Wakeup(WireWakeup {
            timestamp_ns: ev.timestamp,
            waker_tid: ev.waker_tid,
            wakee_tid: ev.wakee_tid,
            waker_user_stack: ev.waker_user_stack.to_vec(),
            waker_kernel_stack: ev.waker_kernel_stack.to_vec(),
        }));
    }

    async fn on_probe_result<'a>(&self, ev: &crate::live_sink::ProbeResultEvent<'a>) {
        let _ = self
            .tx
            .send(IngestEvent::ProbeResult(stax_live_proto::WireProbeResult {
                tid: ev.tid,
                timing: ev.timing.into(),
                queue: ev.queue.into(),
                mach_pc: ev.mach_pc,
                mach_lr: ev.mach_lr,
                mach_fp: ev.mach_fp,
                mach_sp: ev.mach_sp,
                mach_walked: ev.mach_walked.to_vec(),
                used_framehop: ev.used_framehop,
            }));
    }

    async fn on_cpu_interval(&self, ev: &CpuIntervalEvent) {
        match &ev.kind {
            CpuIntervalKind::OnCpu => {
                let _ = self.tx.send(IngestEvent::OnCpuInterval(WireOnCpuInterval {
                    tid: ev.tid,
                    start_ns: ev.start_ns,
                    end_ns: ev.end_ns,
                }));
            }
            CpuIntervalKind::OffCpu {
                stack,
                waker_tid,
                waker_user_stack,
            } => {
                let _ = self
                    .tx
                    .send(IngestEvent::OffCpuInterval(WireOffCpuInterval {
                        tid: ev.tid,
                        start_ns: ev.start_ns,
                        end_ns: ev.end_ns,
                        stack: stack.iter().map(|f| f.address).collect(),
                        waker_tid: *waker_tid,
                        waker_user_stack: waker_user_stack.map(|s| s.to_vec()),
                    }));
            }
        }
    }

    #[cfg(target_os = "macos")]
    async fn on_macho_byte_source(&self, _source: Arc<dyn MachOByteSource>) {
        // The shared-cache mmap can't cross the vox boundary as an
        // Arc<dyn Trait>; the server will open it itself by path
        // (follow-up). For now, drop silently.
        let _ = _source;
    }
}

impl From<crate::live_sink::ProbeTiming> for stax_live_proto::ProbeTiming {
    fn from(t: crate::live_sink::ProbeTiming) -> Self {
        Self {
            kperf_ts: t.kperf_ts,
            enqueued: t.enqueued,
            worker_started: t.worker_started,
            thread_lookup_done: t.thread_lookup_done,
            state_done: t.state_done,
            resume_done: t.resume_done,
            walk_done: t.walk_done,
        }
    }
}

impl From<crate::live_sink::ProbeQueueStats> for stax_live_proto::ProbeQueueStats {
    fn from(q: crate::live_sink::ProbeQueueStats) -> Self {
        Self {
            coalesced_requests: q.coalesced_requests,
            worker_batch_len: q.worker_batch_len,
        }
    }
}

/// Connect to stax-server, register a run, return:
///   - the assigned `RunId`
///   - a `LiveSink` to hand to the recorder
///   - a join handle that resolves once the forwarder task drains
///     the channel and closes the vox Tx.
pub async fn connect_and_register(
    server_socket: &str,
    config: stax_live_proto::RunConfig,
) -> eyre::Result<(
    stax_live_proto::RunId,
    IngestSink,
    tokio::task::JoinHandle<()>,
)> {
    let url = format!("local://{server_socket}");
    let client: RunIngestClient = vox::connect(&url).await?;
    let client = client.with_middleware(vox::ClientLogging::default());

    let (vox_tx, vox_rx) = vox::channel::<IngestEvent>();
    let run_id = match client.start_run(config, vox_rx).await {
        Ok(id) => id,
        Err(vox::VoxError::User(err)) => {
            return Err(eyre::eyre!("server rejected start_run: {err:?}"));
        }
        Err(e) => return Err(eyre::eyre!("vox start_run failed: {e:?}")),
    };

    let (sync_tx, sync_rx) = mpsc::unbounded_channel::<IngestEvent>();
    let (reliable_tx, reliable_rx) = std::sync::mpsc::channel::<ReliableIngest>();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let forwarder = spawn_forwarders(
        client,
        run_id,
        vox_tx,
        sync_rx,
        reliable_rx,
        stop_requested.clone(),
    );

    Ok((
        run_id,
        IngestSink::new(sync_tx, reliable_tx, stop_requested),
        forwarder,
    ))
}

/// Connect an ingest channel to a run that stax-server already
/// created via RunControl. Used by stax-shade in the server-owned
/// lifecycle path.
pub async fn connect_to_existing_run(
    server_socket: &str,
    run_id: stax_live_proto::RunId,
) -> eyre::Result<(IngestSink, tokio::task::JoinHandle<()>)> {
    let url = format!("local://{server_socket}");
    let client: RunIngestClient = vox::connect(&url).await?;
    let client = client.with_middleware(vox::ClientLogging::default());

    let (vox_tx, vox_rx) = vox::channel::<IngestEvent>();
    match client.attach_run(run_id, vox_rx).await {
        Ok(()) => {}
        Err(vox::VoxError::User(err)) => {
            return Err(eyre::eyre!("server rejected attach_run: {err:?}"));
        }
        Err(e) => return Err(eyre::eyre!("vox attach_run failed: {e:?}")),
    }

    let (sync_tx, sync_rx) = mpsc::unbounded_channel::<IngestEvent>();
    let (reliable_tx, reliable_rx) = std::sync::mpsc::channel::<ReliableIngest>();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let forwarder = spawn_forwarders(
        client,
        run_id,
        vox_tx,
        sync_rx,
        reliable_rx,
        stop_requested.clone(),
    );

    Ok((
        IngestSink::new(sync_tx, reliable_tx, stop_requested),
        forwarder,
    ))
}

fn spawn_forwarders(
    client: RunIngestClient,
    run_id: RunId,
    vox_tx: vox::Tx<IngestEvent>,
    sync_rx: mpsc::UnboundedReceiver<IngestEvent>,
    reliable_rx: std::sync::mpsc::Receiver<ReliableIngest>,
    stop_requested: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let event_stop = stop_requested.clone();
    let event_forwarder = tokio::spawn(forward_events(vox_tx, sync_rx, event_stop));
    let reliable_stop = stop_requested.clone();
    let reliable_forwarder = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("reliable ingest runtime");
        for request in reliable_rx {
            let result = rt.block_on(async {
                match request.msg {
                    ReliableIngestMsg::TargetAttached { pid, task_port } => client
                        .publish_target_attached(run_id, pid, task_port)
                        .await
                        .map_err(|e| format!("{e:?}"))?,
                    ReliableIngestMsg::BinaryLoaded(binary) => client
                        .publish_binaries_loaded(run_id, vec![binary])
                        .await
                        .map_err(|e| format!("{e:?}"))?,
                    ReliableIngestMsg::BinaryUnloaded(binary) => client
                        .publish_binaries_unloaded(run_id, vec![binary])
                        .await
                        .map_err(|e| format!("{e:?}"))?,
                }
                Ok::<(), String>(())
            });
            if result.is_err() {
                reliable_stop.store(true, Ordering::Relaxed);
            }
            let _ = request.reply.send(result);
        }
    });
    tokio::spawn(async move {
        let _ = event_forwarder.await;
        let _ = reliable_forwarder.await;
    })
}

async fn forward_events(
    vox_tx: vox::Tx<IngestEvent>,
    mut sync_rx: mpsc::UnboundedReceiver<IngestEvent>,
    stop_requested: Arc<AtomicBool>,
) {
    let mut forwarded: u64 = 0;
    let mut counts = ForwardCounts::default();
    let mut last_log = std::time::Instant::now();
    while let Some(event) = sync_rx.recv().await {
        counts.record(&event);
        match vox_tx.send(event).await {
            Ok(()) => {
                forwarded = forwarded.saturating_add(1);
                if last_log.elapsed() >= std::time::Duration::from_secs(2) {
                    log::info!(
                        "ingest_sink: forwarder progress: forwarded={} queued={} {}",
                        forwarded,
                        sync_rx.len(),
                        counts.summary(),
                    );
                    last_log = std::time::Instant::now();
                }
            }
            Err(e) => {
                log::warn!(
                    "ingest_sink: vox send failed (server dropped Rx?) after forwarded={} queued={} err={:?}",
                    forwarded,
                    sync_rx.len(),
                    e
                );
                stop_requested.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
    log::info!(
        "ingest_sink: forwarder exiting (sync_rx closed) after forwarded={} {}; flushing vox",
        forwarded,
        counts.summary(),
    );
    let _ = vox_tx.close(Default::default()).await;
    stop_requested.store(true, Ordering::Relaxed);
}

struct ReliableIngest {
    msg: ReliableIngestMsg,
    reply: std::sync::mpsc::SyncSender<Result<(), String>>,
}

enum ReliableIngestMsg {
    TargetAttached { pid: u32, task_port: u64 },
    BinaryLoaded(WireBinaryLoaded),
    BinaryUnloaded(WireBinaryUnloaded),
}

#[derive(Default)]
struct ForwardCounts {
    samples: u64,
    probe_results: u64,
    on_cpu: u64,
    off_cpu: u64,
    binaries_loaded: u64,
    binaries_unloaded: u64,
    target_attached: u64,
    thread_names: u64,
    wakeups: u64,
}

impl ForwardCounts {
    fn record(&mut self, event: &IngestEvent) {
        match event {
            IngestEvent::Sample(_) => self.samples += 1,
            IngestEvent::ProbeResult(_) => self.probe_results += 1,
            IngestEvent::OnCpuInterval(_) => self.on_cpu += 1,
            IngestEvent::OffCpuInterval(_) => self.off_cpu += 1,
            IngestEvent::BinaryLoaded(_) => self.binaries_loaded += 1,
            IngestEvent::BinaryUnloaded(_) => self.binaries_unloaded += 1,
            IngestEvent::TargetAttached { .. } => self.target_attached += 1,
            IngestEvent::ThreadName { .. } => self.thread_names += 1,
            IngestEvent::Wakeup(_) => self.wakeups += 1,
        }
    }

    fn summary(&self) -> String {
        format!(
            "samples={} probes={} on_cpu={} off_cpu={} bin_load={} bin_unload={} target={} threads={} wakeups={}",
            self.samples,
            self.probe_results,
            self.on_cpu,
            self.off_cpu,
            self.binaries_loaded,
            self.binaries_unloaded,
            self.target_attached,
            self.thread_names,
            self.wakeups,
        )
    }
}
