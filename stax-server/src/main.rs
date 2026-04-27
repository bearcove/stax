//! `stax-server` — long-running unprivileged daemon.
//!
//! Hosts the run registry (one active + history) plus the live
//! aggregator + binary registry. Three vox services are exposed over
//! the local socket:
//!
//! - `RunControl` — agent-facing lifecycle (status / wait / stop / list).
//! - `RunIngest` — recorder pushes IngestEvents into the active run.
//! - `Profiler`  — query the live aggregator (top, flamegraph, annotate, …).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use stax_live::source::SourceResolver;
use stax_live::{
    Aggregator, BinaryRegistry, IntervalKind, LiveServer, LiveSymbolOwned, LoadedBinary,
    PmuSample,
};
use stax_live_proto::{
    IngestEvent, ProfilerDispatcher, RunConfig, RunControl, RunControlDispatcher, RunId,
    RunIngest, RunIngestDispatcher, RunState, RunSummary, ServerStatus, StopReason,
    WaitCondition, WaitOutcome,
};

const DEFAULT_SOCK_NAME: &str = "stax-server.sock";
const DEFAULT_WS_BIND: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> eyre::Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var("RUST_LOG", "info,stax_server=info");
        }
    }
    env_logger::init();

    let socket = resolve_socket_path();
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }

    let server = ServerState::new();

    let local_listener = vox::transport::local::LocalLinkAcceptor::bind(
        socket.to_string_lossy().into_owned(),
    )?;
    log::info!("stax-server listening on local://{}", socket.display());

    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));

    let ws_addr = std::env::var("STAX_SERVER_WS_BIND")
        .unwrap_or_else(|_| DEFAULT_WS_BIND.to_owned());
    let ws_listener = vox::WsListener::bind(&ws_addr).await?;
    let ws_local = ws_listener.local_addr()?;
    log::info!("stax-server listening on ws://{ws_local}");

    let local_loop = tokio::spawn({
        let server = server.clone();
        async move {
            loop {
                let link = match local_listener.accept().await {
                    Ok(l) => l,
                    Err(e) => {
                        log::warn!("stax-server: local accept failed: {e}");
                        continue;
                    }
                };
                spawn_session_local(server.clone(), link);
            }
        }
    });
    let ws_loop = tokio::spawn({
        let server = server.clone();
        async move {
            if let Err(e) = vox::serve_listener(ws_listener, factory(server)).await {
                log::error!("stax-server: ws serve exited: {e}");
            }
        }
    });

    tokio::select! {
        _ = local_loop => {},
        _ = ws_loop => {},
        _ = tokio::signal::ctrl_c() => {
            log::info!("stax-server: SIGINT, shutting down");
        }
    }
    Ok(())
}

/// Build the multi-service routing factory shared by both transports.
fn factory(server: ServerState) -> impl vox::ConnectionAcceptor + 'static {
    vox::acceptor_fn(move |request: &vox::ConnectionRequest,
                            connection: vox::PendingConnection|
                            -> Result<(), vox::Metadata<'static>> {
        match request.service() {
            "RunControl" => {
                connection.handle_with(RunControlDispatcher::new(server.clone()));
                Ok(())
            }
            "RunIngest" => {
                connection.handle_with(RunIngestDispatcher::new(server.clone()));
                Ok(())
            }
            "Profiler" => {
                connection.handle_with(ProfilerDispatcher::new(server.profiler()));
                Ok(())
            }
            other => {
                log::warn!("stax-server: rejecting unknown service {other:?}");
                Err(vec![])
            }
        }
    })
}

/// Local-socket accept path uses non_resumable so the daemon notices
/// when the recorder process disappears (resumable would keep the
/// session in recovery mode and the per-channel send would silently
/// succeed into a void).
fn spawn_session_local(server: ServerState, link: vox::transport::local::LocalLink) {
    tokio::spawn(async move {
        let result = vox::acceptor_on(link)
            .non_resumable()
            .on_connection(factory(server))
            .establish::<vox::NoopClient>()
            .await;
        match result {
            Ok(client) => client.caller.closed().await,
            Err(e) => log::warn!("stax-server: local session establish failed: {e:?}"),
        }
    });
}

fn resolve_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("STAX_SERVER_SOCKET") {
        return PathBuf::from(p);
    }
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join(DEFAULT_SOCK_NAME);
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/stax-server-{uid}.sock"))
}

/// Shared state. The aggregator + binary registry persist across
/// runs (a new run resets them); historical Profiler queries aren't
/// addressable yet — that ships when `Profiler` learns to take a
/// `RunId`.
#[derive(Clone)]
struct ServerState {
    inner: Arc<Mutex<Inner>>,
    aggregator: Arc<RwLock<Aggregator>>,
    binaries: Arc<RwLock<BinaryRegistry>>,
    source: Arc<Mutex<SourceResolver>>,
    paused: Arc<AtomicBool>,
    started_at_unix_ns: u64,
    next_run_id: Arc<AtomicU64>,
}

struct Inner {
    active: Option<RunSummary>,
    history: Vec<RunSummary>,
}

impl ServerState {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                active: None,
                history: Vec::new(),
            })),
            aggregator: Arc::new(RwLock::new(Aggregator::default())),
            binaries: Arc::new(RwLock::new(BinaryRegistry::new())),
            source: Arc::new(Mutex::new(SourceResolver::new())),
            paused: Arc::new(AtomicBool::new(false)),
            started_at_unix_ns: now_unix_ns(),
            next_run_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// View suitable for hosting the existing `Profiler` impl.
    fn profiler(&self) -> LiveServer {
        LiveServer {
            aggregator: self.aggregator.clone(),
            binaries: self.binaries.clone(),
            source: self.source.clone(),
            paused: self.paused.clone(),
        }
    }
}

fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

impl RunControl for ServerState {
    async fn status(&self) -> ServerStatus {
        let inner = self.inner.lock();
        ServerStatus {
            server_started_at_unix_ns: self.started_at_unix_ns,
            active: inner.active.clone().into_iter().collect(),
        }
    }

    async fn list_runs(&self) -> Vec<RunSummary> {
        let inner = self.inner.lock();
        let mut out = inner.history.clone();
        if let Some(active) = inner.active.clone() {
            out.push(active);
        }
        out
    }

    async fn wait_active(
        &self,
        condition: WaitCondition,
        timeout_ms: Option<u64>,
    ) -> WaitOutcome {
        // Polling implementation while we sketch the lifecycle. Will
        // graduate to event-driven (notify on state-transition) once
        // the rest of the daemon stabilises.
        let deadline = timeout_ms.map(|ms| {
            std::time::Instant::now() + Duration::from_millis(ms)
        });
        let condition_deadline = match &condition {
            WaitCondition::ForSeconds { seconds } => Some(
                std::time::Instant::now() + Duration::from_secs(*seconds),
            ),
            _ => None,
        };
        loop {
            let active = self.inner.lock().active.clone();
            let Some(active) = active else {
                return WaitOutcome::NoActiveRun;
            };
            if active.state == RunState::Stopped {
                return WaitOutcome::Stopped { summary: active };
            }
            let condition_met = match &condition {
                WaitCondition::UntilStopped => false,
                WaitCondition::ForSamples { count } => active.pet_samples >= *count,
                WaitCondition::ForSeconds { .. } => condition_deadline
                    .map(|d| std::time::Instant::now() >= d)
                    .unwrap_or(false),
                WaitCondition::UntilSymbolSeen { needle: _ } => {
                    // Symbol watcher needs a BinaryRegistry helper that
                    // doesn't exist yet; deferred to a follow-up.
                    false
                }
            };
            if condition_met {
                return WaitOutcome::ConditionMet { summary: active };
            }
            if let Some(d) = deadline
                && std::time::Instant::now() >= d
            {
                return WaitOutcome::TimedOut { summary: active };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn stop_active(&self) -> Result<RunSummary, String> {
        let mut inner = self.inner.lock();
        match inner.active.as_mut() {
            Some(summary) => {
                summary.state = RunState::Stopped;
                summary.stop_reason = Some(StopReason::UserStop);
                summary.stopped_at_unix_ns = Some(now_unix_ns());
                let snapshot = summary.clone();
                inner.history.push(snapshot.clone());
                inner.active = None;
                Ok(snapshot)
            }
            None => Err("no active run".to_owned()),
        }
    }
}

impl RunIngest for ServerState {
    async fn start_run(
        &self,
        config: RunConfig,
        mut events: vox::Rx<IngestEvent>,
    ) -> Result<RunId, String> {
        let id = RunId(self.next_run_id.fetch_add(1, Ordering::Relaxed));
        let summary = RunSummary {
            id,
            state: RunState::Recording,
            stop_reason: None,
            started_at_unix_ns: now_unix_ns(),
            stopped_at_unix_ns: None,
            target_pid: None,
            label: config.label,
            pet_samples: 0,
            off_cpu_intervals: 0,
        };
        {
            let mut inner = self.inner.lock();
            if inner.active.is_some() {
                return Err("another run is already active; \
                    call RunControl::stop_active or wait_active first"
                    .to_owned());
            }
            inner.active = Some(summary);
        }

        // Reset aggregator + binary registry for this run. Historical
        // queries against previous runs are deferred (Profiler doesn't
        // take a RunId yet).
        *self.aggregator.write() = Aggregator::default();
        *self.binaries.write() = BinaryRegistry::new();

        log::info!(
            "stax-server: run {} started (frequency_hz={})",
            id.0,
            config.frequency_hz
        );

        let state = self.clone();
        tokio::spawn(async move {
            while let Ok(Some(event_sref)) = events.recv().await {
                let state = state.clone();
                let _ = event_sref.map(|event| {
                    state.apply_event(id, event);
                });
            }
            state.finalize_run(id, StopReason::TargetExited);
        });

        Ok(id)
    }
}

impl ServerState {
    /// Translate one ingest event into aggregator / binary-registry
    /// updates. Mirrors the in-process drainer in `stax-live::start`.
    fn apply_event(&self, run_id: RunId, event: IngestEvent) {
        // Update run summary counters first (under run-lock).
        {
            let mut inner = self.inner.lock();
            let Some(active) = inner.active.as_mut() else {
                return;
            };
            if active.id != run_id {
                return;
            }
            match &event {
                IngestEvent::Sample(_) => active.pet_samples += 1,
                IngestEvent::OffCpuInterval(_) => active.off_cpu_intervals += 1,
                IngestEvent::TargetAttached { pid, .. } => {
                    active.target_pid = Some(*pid);
                }
                _ => {}
            }
        }
        match event {
            IngestEvent::Sample(s) => {
                self.aggregator.write().record_pet_sample(
                    s.tid,
                    s.timestamp_ns,
                    &s.user_backtrace,
                    PmuSample {
                        cycles: s.cycles,
                        instructions: s.instructions,
                        l1d_misses: s.l1d_misses,
                        branch_mispreds: s.branch_mispreds,
                    },
                );
            }
            IngestEvent::OnCpuInterval(i) => {
                self.aggregator
                    .write()
                    .record_interval(i.tid, i.start_ns, i.end_ns, IntervalKind::OnCpu);
            }
            IngestEvent::OffCpuInterval(i) => {
                let stack = i.stack.into_boxed_slice();
                let waker_user_stack = i.waker_user_stack.map(|s| s.into_boxed_slice());
                self.aggregator.write().record_interval(
                    i.tid,
                    i.start_ns,
                    i.end_ns,
                    IntervalKind::OffCpu {
                        stack,
                        waker_tid: i.waker_tid,
                        waker_user_stack,
                    },
                );
            }
            IngestEvent::Wakeup(w) => {
                self.aggregator.write().record_wakeup(
                    w.timestamp_ns,
                    w.waker_tid,
                    w.wakee_tid,
                    w.waker_user_stack,
                    w.waker_kernel_stack,
                );
            }
            IngestEvent::ThreadName { tid, name, .. } => {
                self.aggregator.write().set_thread_name(tid, name);
            }
            IngestEvent::BinaryLoaded(b) => {
                let symbols = b
                    .symbols
                    .into_iter()
                    .map(|s| LiveSymbolOwned {
                        start_svma: s.start_svma,
                        end_svma: s.end_svma,
                        name: s.name,
                    })
                    .collect();
                self.binaries.write().insert(LoadedBinary {
                    path: b.path,
                    base_avma: b.base_avma,
                    avma_end: b.base_avma.saturating_add(b.vmsize),
                    text_svma: b.text_svma,
                    arch: b.arch,
                    is_executable: b.is_executable,
                    symbols,
                    text_bytes: b.text_bytes,
                });
            }
            IngestEvent::BinaryUnloaded { base_avma, .. } => {
                self.binaries.write().remove(base_avma);
            }
            IngestEvent::TargetAttached { pid, task_port } => {
                self.binaries.write().set_target(pid, task_port);
            }
        }
    }

    fn finalize_run(&self, run_id: RunId, reason: StopReason) {
        let mut inner = self.inner.lock();
        let Some(active) = inner.active.as_ref() else {
            return;
        };
        if active.id != run_id {
            return;
        }
        let mut summary = inner.active.take().expect("checked above");
        summary.state = RunState::Stopped;
        summary.stop_reason = Some(reason);
        summary.stopped_at_unix_ns = Some(now_unix_ns());
        log::info!(
            "stax-server: run {} stopped after {} samples / {} intervals",
            summary.id.0,
            summary.pet_samples,
            summary.off_cpu_intervals
        );
        inner.history.push(summary);
    }
}
