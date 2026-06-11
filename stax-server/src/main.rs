//! `stax-server` — long-running unprivileged daemon.
//!
//! Hosts the run registry (one active + history) plus the live
//! aggregator + binary registry. Two vox services are exposed over
//! the local socket:
//!
//! - `RunControl` — agent-facing lifecycle (status / wait / stop / list).
//! - `Profiler`  — query the live aggregator (top, flamegraph, annotate, …).
//!
//! Recording happens in-process on a per-run tokio task driven by
//! `recorder::spawn_attach` / `recorder::spawn_launch`. The old
//! `stax-shade` companion process has been deleted.

mod recorder;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use stax_live::source::SourceResolver;
use stax_live::{Aggregator, BinaryRegistry, LiveServer};
use stax_live_proto::{
    DiagnosticsSnapshot, ProfilerDispatcher, RunConfig, RunControl, RunControlDispatcher,
    RunControlError, RunId, RunState, RunSummary, ServerStatus, StopReason, WaitCondition,
    WaitOutcome,
};
use vox::VoxListener;

const DEFAULT_SOCK_NAME: &str = "stax-server.sock";
const DEFAULT_WS_BIND: &str = "127.0.0.1:8080";
const STAX_SERVER_CHANNEL_CAPACITY: u32 = 64;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    init_logging();
    let _vox_sigusr1_dump = stax_vox_observe::install_global_sigusr1_dump("stax-server");

    let socket = resolve_socket_path();
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }

    let server = ServerState::new(socket.clone());
    server.attach_local_shared_cache();

    let local_listener =
        vox::transport::local::LocalLinkAcceptor::bind(socket.to_string_lossy().into_owned())?;
    tracing::info!("stax-server listening on local://{}", socket.display());

    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));

    let ws_addr =
        std::env::var("STAX_SERVER_WS_BIND").unwrap_or_else(|_| DEFAULT_WS_BIND.to_owned());
    let mut ws_listener = vox::WsListener::bind(&ws_addr).await?;
    let ws_local = ws_listener.local_addr()?;
    tracing::info!("stax-server listening on ws://{ws_local}");

    let local_loop = tokio::spawn({
        let server = server.clone();
        async move {
            loop {
                let link = match local_listener.accept().await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("stax-server: local accept failed: {e}");
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
            loop {
                let link = match ws_listener.accept().await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("stax-server: ws accept failed: {e}");
                        continue;
                    }
                };
                spawn_session_ws(server.clone(), link);
            }
        }
    });

    tokio::select! {
        _ = local_loop => {},
        _ = ws_loop => {},
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("stax-server: SIGINT, shutting down");
        }
    }
    Ok(())
}

fn build_factory(server: ServerState) -> impl vox::ConnectionAcceptor + 'static {
    vox::acceptor_fn(
        move |request: &vox::ConnectionRequest,
              connection: vox::PendingConnection|
              -> Result<(), vox::Metadata> {
            match request.service() {
                "RunControl" => {
                    connection.handle_with(RunControlDispatcher::new(server.clone()));
                    Ok(())
                }
                "Profiler" => {
                    connection.handle_with(ProfilerDispatcher::new(server.profiler()));
                    Ok(())
                }
                other => {
                    tracing::warn!("stax-server: rejecting unknown service {other:?}");
                    Err(vox::Metadata::default())
                }
            }
        },
    )
}

fn spawn_session_local(server: ServerState, link: vox::transport::local::LocalLink) {
    let factory = build_factory(server.clone());
    let observer = stax_vox_observe::VoxObserverLogger::new("stax-server", "local");
    tokio::spawn(async move {
        let result = vox::acceptor_on(link)
            .channel_capacity(STAX_SERVER_CHANNEL_CAPACITY)
            .observer(observer)
            .keepalive(vox::SessionKeepaliveConfig {
                ping_interval: std::time::Duration::from_secs(5),
                pong_timeout: std::time::Duration::from_secs(30),
            })
            .on_connection(factory)
            .establish::<vox::NoopClient>()
            .await;
        match result {
            Ok(client) => {
                let _debug_registration = stax_vox_observe::register_global_caller(
                    "stax-server",
                    "local",
                    "root",
                    &client.caller,
                );
                client.caller.closed().await;
            }
            Err(e) => tracing::warn!("stax-server: local session establish failed: {e:?}"),
        }
    });
}

fn spawn_session_ws(server: ServerState, link: <vox::WsListener as vox::VoxListener>::Link) {
    let factory = build_factory(server.clone());
    let observer = stax_vox_observe::VoxObserverLogger::new("stax-server", "ws");
    tokio::spawn(async move {
        let result = vox::acceptor_on(link)
            .channel_capacity(STAX_SERVER_CHANNEL_CAPACITY)
            .observer(observer)
            .on_connection(factory)
            .establish::<vox::NoopClient>()
            .await;
        match result {
            Ok(client) => {
                let _debug_registration = stax_vox_observe::register_global_caller(
                    "stax-server",
                    "ws",
                    "root",
                    &client.caller,
                );
                client.caller.closed().await;
            }
            Err(e) => tracing::warn!("stax-server: ws session establish failed: {e:?}"),
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
pub(crate) struct ServerState {
    inner: Arc<Mutex<Inner>>,
    aggregator: Arc<RwLock<Aggregator>>,
    binaries: Arc<RwLock<BinaryRegistry>>,
    revision: Arc<AtomicU64>,
    source: Arc<Mutex<SourceResolver>>,
    paused: Arc<AtomicBool>,
    started_at_unix_ns: u64,
    next_run_id: Arc<AtomicU64>,
}

struct Inner {
    active: Option<RunSummary>,
    /// Stop signal for the active recording task. Flipped by
    /// `stop_active`; polled from `recorder`'s `should_stop` hook.
    recording_stop: Option<Arc<AtomicBool>>,
    history: Vec<RunSummary>,
}

impl ServerState {
    fn new(_socket_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                active: None,
                recording_stop: None,
                history: Vec::new(),
            })),
            aggregator: Arc::new(RwLock::new(Aggregator::default())),
            binaries: Arc::new(RwLock::new(BinaryRegistry::new())),
            revision: Arc::new(AtomicU64::new(1)),
            source: Arc::new(Mutex::new(SourceResolver::new())),
            paused: Arc::new(AtomicBool::new(false)),

            started_at_unix_ns: now_unix_ns(),
            next_run_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Open the host's dyld shared cache and plug it into the
    /// binary registry as a Mach-O byte source. The recorder ships
    /// `BinaryLoaded` events with symbols for cache-resident images
    /// but doesn't ship bytes; the server has to open the cache
    /// itself to back disassembly.
    #[cfg(target_os = "macos")]
    fn attach_local_shared_cache(&self) {
        match stax_mac_shared_cache::SharedCache::for_host() {
            Some(cache) => {
                let cache = Arc::new(cache);
                let mut binaries = self.binaries.write();
                binaries.set_macho_byte_source(cache.clone());
                binaries.set_shared_cache(cache);
                tracing::info!(
                    "stax-server: dyld shared cache mapped for symbol lookup + disassembly fallback"
                );
            }
            None => {
                tracing::warn!(
                    "stax-server: no dyld shared cache available; \
                     dyld-resident symbols will surface as <unresolved>"
                );
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn attach_local_shared_cache(&self) {}

    fn profiler(&self) -> LiveServer {
        LiveServer {
            aggregator: self.aggregator.clone(),
            binaries: self.binaries.clone(),
            revision: self.revision.clone(),
            source: self.source.clone(),
            paused: self.paused.clone(),
        }
    }

    pub(crate) fn aggregator(&self) -> &Arc<RwLock<Aggregator>> {
        &self.aggregator
    }

    pub(crate) fn binaries(&self) -> &Arc<RwLock<BinaryRegistry>> {
        &self.binaries
    }

    pub(crate) fn bump_revision(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }

    /// Record an in-process target-attached event. Sets the
    /// `RunSummary::target_pid` and registers the pid with the
    /// binary registry. `task_port` is `0` on the staxd path.
    pub(crate) fn apply_target_attached_in_process(&self, run_id: RunId, pid: u32, task_port: u64) {
        {
            let mut inner = self.inner.lock();
            let Some(active) = inner.active.as_mut() else {
                return;
            };
            if active.id != run_id {
                return;
            }
            active.target_pid = Some(pid);
        }
        self.binaries.write().set_target(pid, task_port);
        self.bump_revision();
    }

    pub(crate) fn note_sample(&self, run_id: RunId) {
        let mut inner = self.inner.lock();
        if let Some(active) = inner.active.as_mut()
            && active.id == run_id
        {
            active.pet_samples += 1;
        }
    }

    pub(crate) fn note_off_cpu(&self, run_id: RunId) {
        let mut inner = self.inner.lock();
        if let Some(active) = inner.active.as_mut()
            && active.id == run_id
        {
            active.off_cpu_intervals += 1;
        }
    }

    pub(crate) fn set_recording_stop_flag(&self, run_id: RunId, flag: Arc<AtomicBool>) {
        let mut inner = self.inner.lock();
        if let Some(active) = inner.active.as_ref()
            && active.id == run_id
        {
            inner.recording_stop = Some(flag);
        }
    }

    fn begin_run(&self, config: RunConfig) -> Result<RunId, String> {
        {
            let mut inner = self.inner.lock();
            // If there's a stale "stopped" run still occupying the
            // active slot, sweep it into history before starting
            // the new one.
            if let Some(active) = inner.active.as_ref()
                && active.state == RunState::Stopped
            {
                let active = inner.active.take().expect("checked above");
                tracing::warn!(
                    run_id = active.id.0,
                    "stale stopped run was still marked active; clearing it before new run"
                );
                if !inner.history.iter().any(|run| run.id == active.id) {
                    inner.history.push(active);
                }
                inner.recording_stop = None;
            }
            if inner.active.is_some() {
                return Err("another run is already active; \
                    call RunControl::stop_active or wait_active first"
                    .to_owned());
            }
            let id = RunId(self.next_run_id.fetch_add(1, Ordering::Relaxed));
            inner.active = Some(RunSummary {
                id,
                state: RunState::Recording,
                stop_reason: None,
                started_at_unix_ns: now_unix_ns(),
                stopped_at_unix_ns: None,
                target_pid: None,
                label: config.label.clone(),
                pet_samples: 0,
                off_cpu_intervals: 0,
            });
            inner.recording_stop = None;

            *self.aggregator.write() = Aggregator::default();
            *self.binaries.write() = BinaryRegistry::new();
            self.bump_revision();
            self.attach_local_shared_cache();

            tracing::info!(
                run_id = id.0,
                label = %config.label,
                frequency_hz = config.frequency_hz,
                dwarf_unwind = config.dwarf_unwind,
                "run started"
            );
            Ok(id)
        }
    }

    /// Called by the recorder when the recording task finishes
    /// (cleanly or with an error). Moves the active run into
    /// history with the given `stop_reason`.
    pub(crate) fn finalize_run(&self, run_id: RunId, default_reason: StopReason) {
        let mut inner = self.inner.lock();
        let Some(active) = inner.active.as_ref() else {
            return;
        };
        if active.id != run_id {
            return;
        }
        let mut summary = inner.active.take().expect("checked above");
        // `stop_active` may have already set state + reason +
        // timestamp; only fill in defaults when the recorder
        // finished on its own.
        if summary.state != RunState::Stopped {
            summary.state = RunState::Stopped;
            summary.stop_reason = Some(default_reason);
            summary.stopped_at_unix_ns = Some(now_unix_ns());
        }
        inner.recording_stop = None;
        tracing::info!(
            "stax-server: run {} stopped after {} samples / {} intervals",
            summary.id.0,
            summary.pet_samples,
            summary.off_cpu_intervals
        );
        inner.history.push(summary);
    }
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

    async fn diagnostics(&self) -> DiagnosticsSnapshot {
        let inner = self.inner.lock();
        DiagnosticsSnapshot {
            server_started_at_unix_ns: self.started_at_unix_ns,
            active: inner.active.clone().into_iter().collect(),
        }
    }

    async fn start_attach(
        &self,
        pid: u32,
        config: RunConfig,
        daemon_socket: String,
        time_limit_secs: Option<u64>,
    ) -> Result<RunId, RunControlError> {
        let frequency_hz = config.frequency_hz;
        let dwarf_unwind = config.dwarf_unwind;
        let time_limit = time_limit_secs.map(Duration::from_secs);
        let run_id = self.begin_run(config)?;
        recorder::spawn_attach(
            self.clone(),
            run_id,
            pid,
            frequency_hz,
            dwarf_unwind,
            daemon_socket,
            time_limit,
        );
        Ok(run_id)
    }

    async fn wait_active(&self, condition: WaitCondition, timeout_ms: Option<u64>) -> WaitOutcome {
        let deadline = timeout_ms.map(|ms| std::time::Instant::now() + Duration::from_millis(ms));
        let condition_deadline = match &condition {
            WaitCondition::ForSeconds { seconds } => {
                Some(std::time::Instant::now() + Duration::from_secs(*seconds))
            }
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
                WaitCondition::UntilSymbolSeen { needle } => {
                    self.binaries.read().any_symbol_contains(needle)
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

    async fn stop_active(&self) -> Result<RunSummary, RunControlError> {
        let (snapshot, stop_flag) = {
            let mut inner = self.inner.lock();
            let snapshot = match inner.active.as_mut() {
                Some(summary) => {
                    summary.state = RunState::Stopped;
                    summary.stop_reason = Some(StopReason::UserStop);
                    summary.stopped_at_unix_ns = Some(now_unix_ns());
                    summary.clone()
                }
                None => return Err(RunControlError::NoActiveRun),
            };
            (snapshot, inner.recording_stop.clone())
        };
        if let Some(flag) = stop_flag {
            flag.store(true, Ordering::Relaxed);
        }
        Ok(snapshot)
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,stax_server=info,vox::server=debug"));

    // macOS: fan out to os_log (Console.app / `log stream`). Linux:
    // plain stderr — journald captures it when run under systemd.
    #[cfg(target_os = "macos")]
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_oslog::OsLogger::new(
            "eu.bearcove.stax-server",
            "default",
        ))
        .init();

    #[cfg(not(target_os = "macos"))]
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
}

fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
