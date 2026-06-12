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
mod target_ingest;

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use stax_live::source::SourceResolver;
use stax_live::{Aggregator, BinaryRegistry, LiveServer};
use stax_live_proto::{
    AnnotatedView, CfgUpdate, DiagnosticsSnapshot, FlamegraphUpdate, IntervalListUpdate,
    NeighborsUpdate, PetSampleListUpdate, Profiler, ProfilerDispatcher, RunConfig, RunControl,
    RunControlDispatcher, RunControlError, RunId, RunState, RunSummary, RunViewParams,
    SavedAggregator, SavedArchiveBlob, SavedBinaryRegistry, SavedEventLogEntry, SavedRunArchive,
    SavedRunArchiveBundle, SavedRunArchiveFiles, SavedRunArchiveManifest,
    SavedRunArchiveProvenance, ServerStatus, StopReason, TargetIngestDiagnostics,
    TargetIngestDispatcher, TargetSpanListUpdate, ThreadsUpdate, TimelineParams, TimelineUpdate,
    TopEntry, TopSort, TopUpdate, ViewParams, WaitCondition, WaitOutcome, WakersUpdate,
};

use crate::target_ingest::{TargetIngestService, TargetLaneRegistry};
use vox::VoxListener;

const DEFAULT_SOCK_NAME: &str = "stax-server.sock";
const DEFAULT_WS_BIND: &str = "127.0.0.1:8080";
const STAX_SERVER_CHANNEL_CAPACITY: u32 = 64;
const ARCHIVE_FORMAT_VERSION: u32 = 2;
const ARCHIVE_V1_FORMAT_VERSION: u32 = 1;
const ARCHIVE_V1_FILE_NAME: &str = "archive.json";
const ARCHIVE_MANIFEST_FILE_NAME: &str = "manifest.json";
const ARCHIVE_AGGREGATOR_FILE_NAME: &str = "aggregator.json";
const ARCHIVE_BINARIES_FILE_NAME: &str = "binaries.json";
const ARCHIVE_TARGET_INGEST_FILE_NAME: &str = "target-ingest.json";
const ARCHIVE_EVENTS_FILE_NAME: &str = "events.jsonl";
const ARCHIVE_BLOBS_DIR_NAME: &str = "blobs";
const ARCHIVE_SINGLE_FILE_EXTENSION: &str = "stax";

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

    let local_loop = spawn_accept_loop_local(server.clone(), local_listener);
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

fn build_factory(server: ServerState) -> impl vox::ConnectionRouter + 'static {
    vox::router_fn(
        move |request: &vox::ConnectionRequest| -> Result<vox::ConnectionRoute, vox::Metadata> {
            match request.service() {
                "Noop" => Ok(vox::ConnectionRoute::handle(())),
                "RunControl" => Ok(vox::ConnectionRoute::handle(RunControlDispatcher::new(
                    server.clone(),
                ))),
                "Profiler" => Ok(vox::ConnectionRoute::handle(ProfilerDispatcher::new(
                    server.profiler(),
                ))),
                "TargetIngest" => Ok(vox::ConnectionRoute::handle(TargetIngestDispatcher::new(
                    TargetIngestService::new(server.clone()),
                ))),
                other => {
                    tracing::warn!("stax-server: rejecting unknown service {other:?}");
                    Err(vox::Metadata::default())
                }
            }
        },
    )
}

/// Accept loop for the local socket, factored so tests can stand up the
/// production routing (`build_factory`) on a scratch socket.
pub(crate) fn spawn_accept_loop_local(
    server: ServerState,
    listener: vox::transport::local::LocalLinkAcceptor,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let link = match listener.accept().await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("stax-server: local accept failed: {e}");
                    continue;
                }
            };
            spawn_session_local(server.clone(), link);
        }
    })
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

/// Shared state. The current aggregator + binary registry are the live query
/// state. Stopped runs keep in-memory snapshots; `ViewParams.run` and
/// `RunViewParams.run` query those snapshots without replacing the current
/// state.
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
    /// Synthetic-lane bookkeeping for `TargetIngest` (pseudo-tids +
    /// span-name symbols). See `target_ingest`.
    target_lanes: Arc<Mutex<TargetLaneRegistry>>,
}

struct Inner {
    active: Option<RunSummary>,
    /// Summary for the run currently loaded into the query surfaces when
    /// there is no active recording. Usually the most recently stopped run,
    /// but `select_run` and `open_saved` can point it at older history.
    selected: Option<RunSummary>,
    /// Stop signal for the active recording task. Flipped by
    /// `stop_active`; polled from `recorder`'s `should_stop` hook.
    recording_stop: Option<Arc<AtomicBool>>,
    history: Vec<RunSnapshot>,
}

#[derive(Clone)]
struct RunSnapshot {
    summary: RunSummary,
    archive: SavedRunArchive,
}

impl ServerState {
    #[cfg(test)]
    pub(crate) fn new_for_tests() -> Self {
        Self::new(PathBuf::from("/dev/null"))
    }

    fn new(_socket_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                active: None,
                selected: None,
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
            target_lanes: Arc::new(Mutex::new(TargetLaneRegistry::default())),
        }
    }

    /// Open the host's dyld shared cache and plug it into the
    /// binary registry as a Mach-O byte source. The recorder ships
    /// `BinaryLoaded` events with symbols for cache-resident images
    /// but doesn't ship bytes; the server has to open the cache
    /// itself to back disassembly.
    #[cfg(target_os = "macos")]
    fn attach_local_shared_cache_to_registry(binaries: &mut BinaryRegistry) {
        match stax_mac_shared_cache::SharedCache::for_host() {
            Some(cache) => {
                let cache = Arc::new(cache);
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
    fn attach_local_shared_cache_to_registry(_binaries: &mut BinaryRegistry) {}

    fn attach_local_shared_cache(&self) {
        Self::attach_local_shared_cache_to_registry(&mut self.binaries.write());
    }

    fn current_profiler(&self) -> LiveServer {
        LiveServer {
            aggregator: self.aggregator.clone(),
            binaries: self.binaries.clone(),
            revision: self.revision.clone(),
            source: self.source.clone(),
            paused: self.paused.clone(),
        }
    }

    fn profiler(&self) -> ServerProfiler {
        ServerProfiler {
            server: self.clone(),
        }
    }

    fn snapshot_for_run(&self, run_id: RunId) -> Option<RunSnapshot> {
        let inner = self.inner.lock();
        inner
            .history
            .iter()
            .find(|snapshot| snapshot.summary.id == run_id)
            .cloned()
    }

    fn is_active_run(&self, run_id: RunId) -> bool {
        self.inner
            .lock()
            .active
            .as_ref()
            .is_some_and(|active| active.id == run_id)
    }

    fn should_stream_run(&self, run: Option<RunId>) -> bool {
        match run {
            None => true,
            Some(run_id) => self.is_active_run(run_id),
        }
    }

    fn profiler_for_run(&self, run: Option<RunId>) -> LiveServer {
        match run {
            None => self.current_profiler(),
            Some(run_id) if self.is_active_run(run_id) => self.current_profiler(),
            Some(run_id) => self
                .snapshot_for_run(run_id)
                .map(|snapshot| self.profiler_from_archive(snapshot.archive))
                .unwrap_or_else(|| self.empty_profiler()),
        }
    }

    fn profiler_from_archive(&self, archive: SavedRunArchive) -> LiveServer {
        let mut aggregator = Aggregator::default();
        aggregator.replace_from_saved(archive.aggregator);
        let mut binaries = BinaryRegistry::new();
        binaries.replace_from_saved(archive.binaries);
        Self::attach_local_shared_cache_to_registry(&mut binaries);
        LiveServer {
            aggregator: Arc::new(RwLock::new(aggregator)),
            binaries: Arc::new(RwLock::new(binaries)),
            revision: Arc::new(AtomicU64::new(1)),
            source: self.source.clone(),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    fn empty_profiler(&self) -> LiveServer {
        LiveServer {
            aggregator: Arc::new(RwLock::new(Aggregator::default())),
            binaries: Arc::new(RwLock::new(BinaryRegistry::new())),
            revision: Arc::new(AtomicU64::new(1)),
            source: self.source.clone(),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    fn diagnostics_for_run(&self, run: Option<RunId>) -> DiagnosticsSnapshot {
        match run {
            None => {
                let inner = self.inner.lock();
                DiagnosticsSnapshot {
                    server_started_at_unix_ns: self.started_at_unix_ns,
                    active: inner.active.clone().into_iter().collect(),
                    target_ingest: self.target_lanes.lock().diagnostics(),
                }
            }
            Some(run_id) if self.is_active_run(run_id) => {
                let inner = self.inner.lock();
                DiagnosticsSnapshot {
                    server_started_at_unix_ns: self.started_at_unix_ns,
                    active: inner.active.clone().into_iter().collect(),
                    target_ingest: self.target_lanes.lock().diagnostics(),
                }
            }
            Some(run_id) => self
                .snapshot_for_run(run_id)
                .map(|snapshot| DiagnosticsSnapshot {
                    server_started_at_unix_ns: self.started_at_unix_ns,
                    active: Vec::new(),
                    target_ingest: snapshot.archive.target_ingest,
                })
                .unwrap_or_else(|| DiagnosticsSnapshot {
                    server_started_at_unix_ns: self.started_at_unix_ns,
                    active: Vec::new(),
                    target_ingest: TargetIngestDiagnostics::default(),
                }),
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

    pub(crate) fn target_lanes(&self) -> &Arc<Mutex<TargetLaneRegistry>> {
        &self.target_lanes
    }

    /// PID of the active run's target, if a run is active and attached.
    pub(crate) fn active_target_pid(&self) -> Option<u32> {
        self.inner.lock().active.as_ref()?.target_pid
    }

    /// Install a fake active run for unit tests of pid-gated surfaces.
    #[cfg(test)]
    pub(crate) fn set_active_run_for_tests(&self, target_pid: u32) {
        self.inner.lock().active = Some(RunSummary {
            id: RunId(1),
            state: RunState::Recording,
            stop_reason: None,
            started_at_unix_ns: 1,
            stopped_at_unix_ns: None,
            target_pid: Some(target_pid),
            label: "test".to_owned(),
            pet_samples: 0,
            off_cpu_intervals: 0,
        });
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

    fn queryable_run_summary(&self) -> Option<RunSummary> {
        let inner = self.inner.lock();
        inner
            .active
            .clone()
            .or_else(|| inner.selected.clone())
            .or_else(|| {
                inner
                    .history
                    .last()
                    .map(|snapshot| snapshot.summary.clone())
            })
    }

    fn archive_from_query_state(&self, run: RunSummary) -> SavedRunArchive {
        SavedRunArchive {
            format_version: ARCHIVE_FORMAT_VERSION,
            saved_at_unix_ns: now_unix_ns(),
            runs: vec![run],
            aggregator: self.aggregator.read().to_saved(),
            binaries: self.binaries.read().to_saved(),
            target_ingest: self.target_lanes.lock().diagnostics(),
        }
    }

    fn snapshot_from_query_state(&self, summary: RunSummary) -> RunSnapshot {
        RunSnapshot {
            archive: self.archive_from_query_state(summary.clone()),
            summary,
        }
    }

    fn upsert_history_snapshot(inner: &mut Inner, snapshot: RunSnapshot) {
        if let Some(existing) = inner
            .history
            .iter_mut()
            .find(|existing| existing.summary.id == snapshot.summary.id)
        {
            *existing = snapshot;
        } else {
            inner.history.push(snapshot);
        }
    }

    fn store_current_query_snapshot(&self, summary: RunSummary) {
        let snapshot = self.snapshot_from_query_state(summary.clone());
        let mut inner = self.inner.lock();
        Self::upsert_history_snapshot(&mut inner, snapshot);
        inner.selected = Some(summary);
    }

    fn sweep_stopped_active_into_history(&self) -> Result<(), RunControlError> {
        let summary = {
            let mut inner = self.inner.lock();
            match inner.active.as_ref() {
                Some(active) if active.state == RunState::Recording => {
                    return Err(RunControlError::AlreadyActive);
                }
                Some(_) => {
                    let active = inner.active.take().expect("checked above");
                    inner.recording_stop = None;
                    Some(active)
                }
                None => None,
            }
        };
        if let Some(summary) = summary {
            self.store_current_query_snapshot(summary);
        }
        Ok(())
    }

    fn restore_archive_to_query_state(
        &self,
        archive: SavedRunArchive,
        summary: RunSummary,
    ) -> Result<RunSummary, RunControlError> {
        if !is_supported_archive_version(archive.format_version) {
            return Err(RunControlError::Internal {
                message: format!(
                    "unsupported stax archive version {} (supported: {}, {})",
                    archive.format_version, ARCHIVE_V1_FORMAT_VERSION, ARCHIVE_FORMAT_VERSION
                ),
            });
        }

        self.aggregator
            .write()
            .replace_from_saved(archive.aggregator.clone());
        self.binaries
            .write()
            .replace_from_saved(archive.binaries.clone());
        {
            let mut target_lanes = self.target_lanes.lock();
            *target_lanes = TargetLaneRegistry::default();
            target_lanes.restore_saved_diagnostics(archive.target_ingest.clone());
        }
        self.attach_local_shared_cache();

        {
            let mut inner = self.inner.lock();
            inner.active = None;
            inner.recording_stop = None;
            inner.selected = Some(summary.clone());
            Self::upsert_history_snapshot(
                &mut inner,
                RunSnapshot {
                    summary: summary.clone(),
                    archive,
                },
            );
        }

        self.bump_revision();
        Ok(summary)
    }

    fn save_current_archive(&self, path: PathBuf) -> Result<(), String> {
        let run = self
            .queryable_run_summary()
            .ok_or_else(|| "no run is available to save".to_owned())?;
        let archive = self.archive_from_query_state(run);
        write_archive(&path, &archive)
    }

    fn open_saved_archive(&self, path: PathBuf) -> Result<(), RunControlError> {
        let archive =
            read_archive(&path).map_err(|message| RunControlError::Internal { message })?;
        if !is_supported_archive_version(archive.format_version) {
            return Err(RunControlError::Internal {
                message: format!(
                    "unsupported stax archive version {} (supported: {}, {})",
                    archive.format_version, ARCHIVE_V1_FORMAT_VERSION, ARCHIVE_FORMAT_VERSION
                ),
            });
        }
        let Some(mut summary) = archive.runs.last().cloned() else {
            return Err(RunControlError::Internal {
                message: "archive has no run summary".to_owned(),
            });
        };
        summary.state = RunState::Stopped;
        if summary.stopped_at_unix_ns.is_none() {
            summary.stopped_at_unix_ns = Some(archive.saved_at_unix_ns);
        }

        self.sweep_stopped_active_into_history()?;
        self.restore_archive_to_query_state(archive, summary.clone())?;
        self.advance_next_run_id(summary.id.0.saturating_add(1));
        Ok(())
    }

    fn select_run_archive(&self, run_id: RunId) -> Result<RunSummary, RunControlError> {
        self.sweep_stopped_active_into_history()?;
        let snapshot = {
            let inner = self.inner.lock();
            inner
                .history
                .iter()
                .find(|snapshot| snapshot.summary.id == run_id)
                .cloned()
        };
        let Some(snapshot) = snapshot else {
            return Err(RunControlError::Internal {
                message: format!("run {} is not in stax-server history", run_id.0),
            });
        };
        self.restore_archive_to_query_state(snapshot.archive, snapshot.summary)
    }

    fn advance_next_run_id(&self, next: u64) {
        let mut current = self.next_run_id.load(Ordering::Relaxed);
        while current < next {
            match self.next_run_id.compare_exchange(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn begin_run(&self, config: RunConfig) -> Result<RunId, String> {
        self.sweep_stopped_active_into_history().map_err(|_| {
            "another run is already active; \
                    call RunControl::stop_active or wait_active first"
                .to_owned()
        })?;
        {
            let mut inner = self.inner.lock();
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
            inner.selected = None;
            inner.recording_stop = None;

            *self.aggregator.write() = Aggregator::default();
            *self.binaries.write() = BinaryRegistry::new();
            *self.target_lanes.lock() = TargetLaneRegistry::default();
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
        let summary = {
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
            summary
        };
        tracing::info!(
            "stax-server: run {} stopped after {} samples / {} intervals",
            summary.id.0,
            summary.pet_samples,
            summary.off_cpu_intervals
        );
        self.store_current_query_snapshot(summary);
    }
}

#[derive(Clone)]
struct ServerProfiler {
    server: ServerState,
}

impl Profiler for ServerProfiler {
    async fn top(&self, limit: u32, sort: TopSort, params: ViewParams) -> Vec<TopEntry> {
        self.top_update(limit, sort, params).await.entries
    }

    async fn top_update(&self, limit: u32, sort: TopSort, params: ViewParams) -> TopUpdate {
        self.server
            .profiler_for_run(params.run)
            .top_update(limit, sort, params)
            .await
    }

    async fn subscribe_top(
        &self,
        limit: u32,
        sort: TopSort,
        params: ViewParams,
        output: vox::Tx<TopUpdate>,
    ) {
        let run = params.run;
        if self.server.should_stream_run(run) {
            self.server
                .profiler_for_run(run)
                .subscribe_top(limit, sort, params, output)
                .await;
        } else {
            let update = self.top_update(limit, sort, params).await;
            let _ = output.send(update).await;
        }
    }

    async fn total_on_cpu_ns(&self, params: RunViewParams) -> u64 {
        self.server
            .profiler_for_run(params.run)
            .total_on_cpu_ns(params)
            .await
    }

    async fn annotated(&self, address: u64, params: ViewParams) -> AnnotatedView {
        self.server
            .profiler_for_run(params.run)
            .annotated(address, params)
            .await
    }

    async fn subscribe_annotated(
        &self,
        address: u64,
        params: ViewParams,
        output: vox::Tx<AnnotatedView>,
    ) {
        let run = params.run;
        if self.server.should_stream_run(run) {
            self.server
                .profiler_for_run(run)
                .subscribe_annotated(address, params, output)
                .await;
        } else {
            let update = self.annotated(address, params).await;
            let _ = output.send(update).await;
        }
    }

    async fn cfg(&self, address: u64, params: ViewParams) -> CfgUpdate {
        self.server
            .profiler_for_run(params.run)
            .cfg(address, params)
            .await
    }

    async fn subscribe_cfg(&self, address: u64, params: ViewParams, output: vox::Tx<CfgUpdate>) {
        let run = params.run;
        if self.server.should_stream_run(run) {
            self.server
                .profiler_for_run(run)
                .subscribe_cfg(address, params, output)
                .await;
        } else {
            let update = self.cfg(address, params).await;
            let _ = output.send(update).await;
        }
    }

    async fn flamegraph(&self, params: ViewParams) -> FlamegraphUpdate {
        self.server
            .profiler_for_run(params.run)
            .flamegraph(params)
            .await
    }

    async fn subscribe_flamegraph(&self, params: ViewParams, output: vox::Tx<FlamegraphUpdate>) {
        let run = params.run;
        if self.server.should_stream_run(run) {
            self.server
                .profiler_for_run(run)
                .subscribe_flamegraph(params, output)
                .await;
        } else {
            let update = self.flamegraph(params).await;
            let _ = output.send(update).await;
        }
    }

    async fn threads(&self, params: RunViewParams) -> ThreadsUpdate {
        self.server
            .profiler_for_run(params.run)
            .threads(params)
            .await
    }

    async fn subscribe_threads(&self, params: RunViewParams, output: vox::Tx<ThreadsUpdate>) {
        if self.server.should_stream_run(params.run) {
            self.server
                .profiler_for_run(params.run)
                .subscribe_threads(params, output)
                .await;
        } else {
            let update = self.threads(params).await;
            let _ = output.send(update).await;
        }
    }

    async fn timeline(&self, params: TimelineParams) -> TimelineUpdate {
        self.server
            .profiler_for_run(params.run)
            .timeline(params)
            .await
    }

    async fn subscribe_timeline(&self, params: TimelineParams, output: vox::Tx<TimelineUpdate>) {
        if self.server.should_stream_run(params.run) {
            self.server
                .profiler_for_run(params.run)
                .subscribe_timeline(params, output)
                .await;
        } else {
            let update = self.timeline(params).await;
            let _ = output.send(update).await;
        }
    }

    async fn neighbors(&self, address: u64, params: ViewParams) -> NeighborsUpdate {
        self.server
            .profiler_for_run(params.run)
            .neighbors(address, params)
            .await
    }

    async fn subscribe_neighbors(
        &self,
        address: u64,
        params: ViewParams,
        output: vox::Tx<NeighborsUpdate>,
    ) {
        let run = params.run;
        if self.server.should_stream_run(run) {
            self.server
                .profiler_for_run(run)
                .subscribe_neighbors(address, params, output)
                .await;
        } else {
            let update = self.neighbors(address, params).await;
            let _ = output.send(update).await;
        }
    }

    async fn wakers(&self, wakee_tid: u32, params: RunViewParams) -> WakersUpdate {
        self.server
            .profiler_for_run(params.run)
            .wakers(wakee_tid, params)
            .await
    }

    async fn subscribe_wakers(
        &self,
        wakee_tid: u32,
        params: RunViewParams,
        output: vox::Tx<WakersUpdate>,
    ) {
        if self.server.should_stream_run(params.run) {
            self.server
                .profiler_for_run(params.run)
                .subscribe_wakers(wakee_tid, params, output)
                .await;
        } else {
            let update = self.wakers(wakee_tid, params).await;
            let _ = output.send(update).await;
        }
    }

    async fn intervals(&self, flame_key: String, params: ViewParams) -> IntervalListUpdate {
        self.server
            .profiler_for_run(params.run)
            .intervals(flame_key, params)
            .await
    }

    async fn subscribe_intervals(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<IntervalListUpdate>,
    ) {
        let run = params.run;
        if self.server.should_stream_run(run) {
            self.server
                .profiler_for_run(run)
                .subscribe_intervals(flame_key, params, output)
                .await;
        } else {
            let update = self.intervals(flame_key, params).await;
            let _ = output.send(update).await;
        }
    }

    async fn pet_samples(&self, flame_key: String, params: ViewParams) -> PetSampleListUpdate {
        self.server
            .profiler_for_run(params.run)
            .pet_samples(flame_key, params)
            .await
    }

    async fn subscribe_pet_samples(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<PetSampleListUpdate>,
    ) {
        let run = params.run;
        if self.server.should_stream_run(run) {
            self.server
                .profiler_for_run(run)
                .subscribe_pet_samples(flame_key, params, output)
                .await;
        } else {
            let update = self.pet_samples(flame_key, params).await;
            let _ = output.send(update).await;
        }
    }

    async fn target_spans(&self, flame_key: String, params: ViewParams) -> TargetSpanListUpdate {
        self.server
            .profiler_for_run(params.run)
            .target_spans(flame_key, params)
            .await
    }

    async fn subscribe_target_spans(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<TargetSpanListUpdate>,
    ) {
        let run = params.run;
        if self.server.should_stream_run(run) {
            self.server
                .profiler_for_run(run)
                .subscribe_target_spans(flame_key, params, output)
                .await;
        } else {
            let update = self.target_spans(flame_key, params).await;
            let _ = output.send(update).await;
        }
    }

    async fn set_paused(&self, paused: bool) {
        self.server.current_profiler().set_paused(paused).await;
    }

    async fn is_paused(&self) -> bool {
        self.server.current_profiler().is_paused().await
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
        let mut out: Vec<RunSummary> = inner
            .history
            .iter()
            .map(|snapshot| snapshot.summary.clone())
            .collect();
        if let Some(active) = inner.active.clone() {
            out.push(active);
        }
        out
    }

    async fn diagnostics(&self, params: RunViewParams) -> DiagnosticsSnapshot {
        self.diagnostics_for_run(params.run)
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

    async fn save_current(&self, path: String) -> Result<(), RunControlError> {
        self.save_current_archive(PathBuf::from(path))
            .map_err(|message| RunControlError::Internal { message })
    }

    async fn open_saved(&self, path: String) -> Result<(), RunControlError> {
        self.open_saved_archive(PathBuf::from(path))
    }

    async fn select_run(&self, run_id: RunId) -> Result<RunSummary, RunControlError> {
        self.select_run_archive(run_id)
    }
}

fn write_archive(path: &Path, archive: &SavedRunArchive) -> Result<(), String> {
    if is_single_file_archive_path(path) {
        return write_archive_bundle(path, archive);
    }
    write_archive_directory(path, archive)
}

fn write_archive_directory(path: &Path, archive: &SavedRunArchive) -> Result<(), String> {
    if path.exists() && !path.is_dir() {
        return Err(format!(
            "archive path {} exists and is not a directory",
            path.display()
        ));
    }
    fs::create_dir_all(path).map_err(|e| format!("create archive dir {}: {e}", path.display()))?;
    remove_legacy_archive_file(path)?;
    let (archive, blobs) = archive_for_storage(archive);
    let manifest = SavedRunArchiveManifest {
        format_version: ARCHIVE_FORMAT_VERSION,
        saved_at_unix_ns: archive.saved_at_unix_ns,
        provenance: archive_provenance(),
        runs: archive.runs.clone(),
        files: SavedRunArchiveFiles {
            aggregator: ARCHIVE_AGGREGATOR_FILE_NAME.to_owned(),
            binaries: ARCHIVE_BINARIES_FILE_NAME.to_owned(),
            target_ingest: ARCHIVE_TARGET_INGEST_FILE_NAME.to_owned(),
        },
    };
    write_manifest(path.join(ARCHIVE_MANIFEST_FILE_NAME), &manifest)?;
    write_aggregator(path.join(ARCHIVE_AGGREGATOR_FILE_NAME), &archive.aggregator)?;
    write_binaries(path.join(ARCHIVE_BINARIES_FILE_NAME), &archive.binaries)?;
    write_target_ingest(
        path.join(ARCHIVE_TARGET_INGEST_FILE_NAME),
        &archive.target_ingest,
    )?;
    write_archive_blobs(path, &blobs)?;
    write_event_log(path.join(ARCHIVE_EVENTS_FILE_NAME), &archive)
}

fn write_archive_bundle(path: &Path, archive: &SavedRunArchive) -> Result<(), String> {
    if path.exists() && path.is_dir() {
        return Err(format!(
            "archive package path {} exists and is a directory",
            path.display()
        ));
    }
    let (archive, blobs) = archive_for_storage(archive);
    let bundle = SavedRunArchiveBundle {
        format_version: archive.format_version,
        saved_at_unix_ns: archive.saved_at_unix_ns,
        provenance: archive_provenance(),
        runs: archive.runs.clone(),
        aggregator: archive.aggregator.clone(),
        binaries: archive.binaries.clone(),
        target_ingest: archive.target_ingest.clone(),
        events: archive_event_log_entries(&archive),
        blobs,
    };
    let bytes = facet_json::to_vec_pretty(&bundle)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

fn archive_for_storage(archive: &SavedRunArchive) -> (SavedRunArchive, Vec<SavedArchiveBlob>) {
    let mut archive = archive.clone();
    let mut blobs = Vec::new();
    for (index, binary) in archive.binaries.binaries.iter_mut().enumerate() {
        let Some(bytes) = binary.text_bytes.take() else {
            continue;
        };
        blobs.push(SavedArchiveBlob {
            path: binary_text_blob_member(index, binary),
            bytes,
        });
    }
    (archive, blobs)
}

fn write_archive_blobs(base: &Path, blobs: &[SavedArchiveBlob]) -> Result<(), String> {
    let blobs_dir = base.join(ARCHIVE_BLOBS_DIR_NAME);
    if blobs_dir.exists() {
        if !blobs_dir.is_dir() {
            return Err(format!(
                "archive blobs path {} exists and is not a directory",
                blobs_dir.display()
            ));
        }
        fs::remove_dir_all(&blobs_dir)
            .map_err(|e| format!("remove stale archive blobs {}: {e}", blobs_dir.display()))?;
    }
    if blobs.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(&blobs_dir)
        .map_err(|e| format!("create archive blobs dir {}: {e}", blobs_dir.display()))?;
    for blob in blobs {
        let path = archive_member_path(base, &blob.path)?;
        fs::write(&path, &blob.bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

fn archive_provenance() -> SavedRunArchiveProvenance {
    SavedRunArchiveProvenance {
        producer: env!("CARGO_PKG_NAME").to_owned(),
        producer_version: env!("CARGO_PKG_VERSION").to_owned(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    }
}

fn read_archive(path: &Path) -> Result<SavedRunArchive, String> {
    if path.is_dir() {
        let manifest_path = path.join(ARCHIVE_MANIFEST_FILE_NAME);
        if manifest_path.exists() {
            return read_archive_manifest(&manifest_path);
        }
        return read_archive_v1(&path.join(ARCHIVE_V1_FILE_NAME));
    }

    if is_single_file_archive_path(path) {
        read_archive_bundle(path)
    } else if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ARCHIVE_MANIFEST_FILE_NAME)
    {
        read_archive_manifest(path)
    } else {
        read_archive_v1(path)
    }
}

fn remove_legacy_archive_file(path: &Path) -> Result<(), String> {
    let legacy_path = path.join(ARCHIVE_V1_FILE_NAME);
    if !legacy_path.exists() {
        return Ok(());
    }
    if !legacy_path.is_file() {
        return Err(format!(
            "legacy archive path {} exists and is not a file",
            legacy_path.display()
        ));
    }
    fs::remove_file(&legacy_path).map_err(|e| format!("remove {}: {e}", legacy_path.display()))
}

fn write_manifest(path: PathBuf, value: &SavedRunArchiveManifest) -> Result<(), String> {
    let bytes = facet_json::to_vec_pretty(value)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_aggregator(path: PathBuf, value: &SavedAggregator) -> Result<(), String> {
    let bytes = facet_json::to_vec_pretty(value)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_binaries(path: PathBuf, value: &SavedBinaryRegistry) -> Result<(), String> {
    let bytes = facet_json::to_vec_pretty(value)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_target_ingest(path: PathBuf, value: &TargetIngestDiagnostics) -> Result<(), String> {
    let bytes = facet_json::to_vec_pretty(value)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_event_log(path: PathBuf, archive: &SavedRunArchive) -> Result<(), String> {
    let file = fs::File::create(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for entry in archive_event_log_entries(archive) {
        write_event_log_entry(&mut writer, &path, entry)?;
    }
    writer
        .flush()
        .map_err(|e| format!("flush {}: {e}", path.display()))
}

fn archive_event_log_entries(archive: &SavedRunArchive) -> Vec<SavedEventLogEntry> {
    let mut entries = Vec::new();
    entries.push(SavedEventLogEntry::ArchiveSaved {
        saved_at_unix_ns: archive.saved_at_unix_ns,
    });
    entries.extend(
        archive
            .runs
            .iter()
            .cloned()
            .map(|run| SavedEventLogEntry::RunSummary { run }),
    );
    entries.push(SavedEventLogEntry::AggregatorClock {
        session_start_ns: archive.aggregator.session_start_ns,
        last_event_ns: archive.aggregator.last_event_ns,
    });
    entries.extend(archive.aggregator.thread_names.iter().map(|thread_name| {
        SavedEventLogEntry::ThreadName {
            tid: thread_name.tid,
            name: thread_name.name.clone(),
        }
    }));
    entries.extend(
        archive
            .binaries
            .binaries
            .iter()
            .cloned()
            .map(|binary| SavedEventLogEntry::BinaryLoaded { binary }),
    );
    entries.push(SavedEventLogEntry::TargetIngestDiagnostics {
        diagnostics: archive.target_ingest.clone(),
    });

    let mut timed = Vec::new();
    for thread in &archive.aggregator.threads {
        for sample in &thread.pet_samples {
            timed.push((
                sample.timestamp_ns,
                0_u8,
                thread.tid,
                SavedEventLogEntry::PetSample {
                    tid: thread.tid,
                    sample: sample.clone(),
                },
            ));
        }
        for interval in &thread.intervals {
            timed.push((
                interval.start_ns,
                1_u8,
                thread.tid,
                SavedEventLogEntry::Interval {
                    tid: thread.tid,
                    interval: interval.clone(),
                },
            ));
        }
        for wakeup in &thread.wakeups {
            timed.push((
                wakeup.timestamp_ns,
                2_u8,
                thread.tid,
                SavedEventLogEntry::Wakeup {
                    tid: thread.tid,
                    wakeup: wakeup.clone(),
                },
            ));
        }
    }
    timed.sort_by_key(|(timestamp_ns, order, tid, _)| (*timestamp_ns, *order, *tid));
    entries.extend(timed.into_iter().map(|(_, _, _, entry)| entry));
    entries
}

fn write_event_log_entry(
    writer: &mut impl Write,
    path: &Path,
    entry: SavedEventLogEntry,
) -> Result<(), String> {
    let bytes =
        facet_json::to_vec(&entry).map_err(|e| format!("serialize {}: {e}", path.display()))?;
    writer
        .write_all(&bytes)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    writer
        .write_all(b"\n")
        .map_err(|e| format!("write {}: {e}", path.display()))
}

fn read_archive_v1(archive_path: &Path) -> Result<SavedRunArchive, String> {
    let bytes =
        fs::read(&archive_path).map_err(|e| format!("read {}: {e}", archive_path.display()))?;
    facet_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", archive_path.display()))
}

fn read_archive_bundle(bundle_path: &Path) -> Result<SavedRunArchive, String> {
    let bytes =
        fs::read(bundle_path).map_err(|e| format!("read {}: {e}", bundle_path.display()))?;
    let bundle: SavedRunArchiveBundle = facet_json::from_slice(&bytes)
        .map_err(|e| format!("parse {}: {e}", bundle_path.display()))?;
    if !bundle.events.is_empty() {
        let mut archive = SavedRunArchive::from_event_log_entries(
            bundle.format_version,
            bundle.saved_at_unix_ns,
            bundle.events,
        );
        if archive.runs.is_empty() {
            archive.runs = bundle.runs;
        }
        restore_bundle_blobs(&mut archive.binaries, &bundle.blobs)?;
        return Ok(archive);
    }
    let mut archive = SavedRunArchive {
        format_version: bundle.format_version,
        saved_at_unix_ns: bundle.saved_at_unix_ns,
        runs: bundle.runs,
        aggregator: bundle.aggregator,
        binaries: bundle.binaries,
        target_ingest: bundle.target_ingest,
    };
    restore_bundle_blobs(&mut archive.binaries, &bundle.blobs)?;
    Ok(archive)
}

fn read_archive_manifest(manifest_path: &Path) -> Result<SavedRunArchive, String> {
    let bytes =
        fs::read(manifest_path).map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
    let manifest: SavedRunArchiveManifest = facet_json::from_slice(&bytes)
        .map_err(|e| format!("parse {}: {e}", manifest_path.display()))?;
    if !is_supported_archive_version(manifest.format_version) {
        return Err(format!(
            "unsupported stax archive version {} in {} (supported: {}, {})",
            manifest.format_version,
            manifest_path.display(),
            ARCHIVE_V1_FORMAT_VERSION,
            ARCHIVE_FORMAT_VERSION
        ));
    }
    let base = manifest_path.parent().ok_or_else(|| {
        format!(
            "manifest {} has no parent directory",
            manifest_path.display()
        )
    })?;
    let event_log_path = base.join(ARCHIVE_EVENTS_FILE_NAME);
    if event_log_path.exists() {
        let entries = read_event_log(&event_log_path)?;
        if !entries.is_empty() {
            let mut archive = SavedRunArchive::from_event_log_entries(
                manifest.format_version,
                manifest.saved_at_unix_ns,
                entries,
            );
            if archive.runs.is_empty() {
                archive.runs = manifest.runs;
            }
            restore_directory_blobs(base, &mut archive.binaries)?;
            return Ok(archive);
        }
    }
    let aggregator = read_aggregator(archive_member_path(base, &manifest.files.aggregator)?)?;
    let mut binaries = read_binaries(archive_member_path(base, &manifest.files.binaries)?)?;
    restore_directory_blobs(base, &mut binaries)?;
    let target_ingest =
        read_target_ingest(archive_member_path(base, &manifest.files.target_ingest)?)?;
    Ok(SavedRunArchive {
        format_version: manifest.format_version,
        saved_at_unix_ns: manifest.saved_at_unix_ns,
        runs: manifest.runs,
        aggregator,
        binaries,
        target_ingest,
    })
}

fn read_aggregator(path: PathBuf) -> Result<SavedAggregator, String> {
    let bytes = fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    facet_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn read_binaries(path: PathBuf) -> Result<SavedBinaryRegistry, String> {
    let bytes = fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    facet_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn read_target_ingest(path: PathBuf) -> Result<TargetIngestDiagnostics, String> {
    let bytes = fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    facet_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn read_event_log(path: &Path) -> Result<Vec<SavedEventLogEntry>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut entries = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry = facet_json::from_slice(line.as_bytes()).map_err(|e| {
            format!(
                "parse {} line {}: {e}",
                path.display(),
                line_index.saturating_add(1)
            )
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

fn restore_directory_blobs(base: &Path, binaries: &mut SavedBinaryRegistry) -> Result<(), String> {
    for (index, binary) in binaries.binaries.iter_mut().enumerate() {
        let member = binary_text_blob_member(index, binary);
        let path = archive_member_path(base, &member)?;
        if !path.exists() {
            continue;
        }
        binary.text_bytes =
            Some(fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?);
    }
    Ok(())
}

fn restore_bundle_blobs(
    binaries: &mut SavedBinaryRegistry,
    blobs: &[SavedArchiveBlob],
) -> Result<(), String> {
    for (index, binary) in binaries.binaries.iter_mut().enumerate() {
        let member = binary_text_blob_member(index, binary);
        let Some(blob) = blobs.iter().find(|blob| blob.path == member) else {
            continue;
        };
        binary.text_bytes = Some(blob.bytes.clone());
    }
    Ok(())
}

fn binary_text_blob_member(index: usize, binary: &stax_live_proto::SavedLoadedBinary) -> String {
    format!(
        "{ARCHIVE_BLOBS_DIR_NAME}/binary-text-{index:06}-{:016x}.bin",
        binary.base_avma
    )
}

fn archive_member_path(base: &Path, member: &str) -> Result<PathBuf, String> {
    let member_path = Path::new(member);
    let mut has_component = false;
    for component in member_path.components() {
        match component {
            std::path::Component::Normal(_) => has_component = true,
            _ => {
                return Err(format!(
                    "archive member path {member:?} must stay inside {}",
                    base.display()
                ));
            }
        }
    }
    if !has_component {
        return Err(format!("archive member path {member:?} must not be empty"));
    }
    Ok(base.join(member_path))
}

fn is_supported_archive_version(version: u32) -> bool {
    matches!(version, ARCHIVE_V1_FORMAT_VERSION | ARCHIVE_FORMAT_VERSION)
}

fn is_single_file_archive_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension == ARCHIVE_SINGLE_FILE_EXTENSION)
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,stax_server=info,vox::server=debug"));

    // macOS: fan out to os_log (Console.app / `log stream`). Linux:
    // plain stderr — journald captures it when run under systemd.
    // `STAX_SERVER_STDERR_LOG=1` adds a stderr layer on macOS too, for
    // dev-server runs where os_log is inconvenient to tail.
    #[cfg(target_os = "macos")]
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_oslog::OsLogger::new(
            "eu.bearcove.stax-server",
            "default",
        ))
        .with(
            std::env::var("STAX_SERVER_STDERR_LOG")
                .is_ok()
                .then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr)),
        )
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

#[cfg(test)]
mod tests {
    use stax_live_proto::{
        LiveFilter, Profiler as _, RunControl as _, RunViewParams, SavedAggregator,
        SavedBinaryRegistry, SavedEventLogEntry, SavedLoadedBinary, TargetIngest as _,
        TargetReporterStats, TargetSpan, TargetSpanBatch, TopSort, ViewParams,
    };

    use super::*;

    const SYNTH_TID_BASE: u32 = 0xFFF0_0000;

    #[test]
    fn archive_member_path_rejects_paths_outside_archive() {
        let base = Path::new("/tmp/stax-archive");

        assert_eq!(
            archive_member_path(base, "aggregator.json").expect("valid member"),
            base.join("aggregator.json")
        );
        assert!(archive_member_path(base, "../outside.json").is_err());
        assert!(archive_member_path(base, "/tmp/outside.json").is_err());
        assert!(archive_member_path(base, "").is_err());
    }

    #[test]
    fn archive_writes_binary_text_bytes_as_blobs() {
        let archive_dir = temp_archive_dir("blobs");
        let _ = std::fs::remove_dir_all(&archive_dir);
        let package_path = archive_dir.with_extension(ARCHIVE_SINGLE_FILE_EXTENSION);
        let _ = std::fs::remove_file(&package_path);
        let archive = SavedRunArchive {
            format_version: ARCHIVE_FORMAT_VERSION,
            saved_at_unix_ns: 42,
            runs: Vec::new(),
            aggregator: SavedAggregator::default(),
            binaries: SavedBinaryRegistry {
                binaries: vec![test_saved_binary()],
            },
            target_ingest: TargetIngestDiagnostics::default(),
        };

        write_archive_directory(&archive_dir, &archive).expect("write directory archive");
        let stored_binaries =
            read_binaries(archive_dir.join(ARCHIVE_BINARIES_FILE_NAME)).expect("read binaries");
        assert!(stored_binaries.binaries[0].text_bytes.is_none());
        let event_log = read_event_log_for_test(&archive_dir.join(ARCHIVE_EVENTS_FILE_NAME));
        assert!(event_log.iter().any(|entry| {
            matches!(
                entry,
                SavedEventLogEntry::BinaryLoaded { binary }
                    if binary.text_bytes.is_none()
            )
        }));
        let blob_member = binary_text_blob_member(0, &stored_binaries.binaries[0]);
        assert_eq!(
            std::fs::read(archive_dir.join(&blob_member)).expect("read directory blob"),
            vec![1, 2, 3, 4]
        );
        let restored = read_archive(&archive_dir).expect("read directory archive");
        assert_eq!(
            restored.binaries.binaries[0].text_bytes.as_deref(),
            Some(&[1, 2, 3, 4][..])
        );

        write_archive_bundle(&package_path, &archive).expect("write package archive");
        let bundle_bytes = std::fs::read(&package_path).expect("read package");
        let bundle: SavedRunArchiveBundle =
            facet_json::from_slice(&bundle_bytes).expect("parse package");
        assert!(bundle.binaries.binaries[0].text_bytes.is_none());
        assert!(bundle.events.iter().any(|entry| {
            matches!(
                entry,
                SavedEventLogEntry::BinaryLoaded { binary }
                    if binary.text_bytes.is_none()
            )
        }));
        assert_eq!(bundle.blobs.len(), 1);
        assert_eq!(bundle.blobs[0].path, blob_member);
        assert_eq!(bundle.blobs[0].bytes, vec![1, 2, 3, 4]);
        let restored = read_archive(&package_path).expect("read package archive");
        assert_eq!(
            restored.binaries.binaries[0].text_bytes.as_deref(),
            Some(&[1, 2, 3, 4][..])
        );

        let _ = std::fs::remove_dir_all(&archive_dir);
        let _ = std::fs::remove_file(&package_path);
    }

    #[tokio::test]
    async fn save_open_restores_query_state_and_target_diagnostics() {
        let server = ServerState::new_for_tests();
        let service = TargetIngestService::new(server.clone());
        let run_id = server
            .begin_run(test_run_config("archive-source"))
            .expect("begin source run");
        server.apply_target_attached_in_process(run_id, 4242, 0);

        service
            .ingest(TargetSpanBatch {
                pid: 4242,
                lane: "GPU archive".to_owned(),
                spans: vec![TargetSpan::new("archive_kernel", 10_000_000, 16_000_000)],
            })
            .await;
        service
            .reporter_stats(TargetReporterStats {
                pid: 4242,
                batches_dropped_queue_full: 2,
                spans_dropped_queue_full: 9,
                batches_dropped_worker_disconnected: 1,
                spans_dropped_worker_disconnected: 4,
            })
            .await;

        let archive_dir = temp_archive_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&archive_dir);
        std::fs::create_dir_all(&archive_dir).expect("create archive dir");
        std::fs::write(archive_dir.join(ARCHIVE_V1_FILE_NAME), b"stale v1 archive")
            .expect("write stale legacy archive");
        server
            .save_current_archive(archive_dir.clone())
            .expect("save current archive");
        assert!(archive_dir.join(ARCHIVE_MANIFEST_FILE_NAME).exists());
        assert!(archive_dir.join(ARCHIVE_AGGREGATOR_FILE_NAME).exists());
        assert!(archive_dir.join(ARCHIVE_BINARIES_FILE_NAME).exists());
        assert!(archive_dir.join(ARCHIVE_TARGET_INGEST_FILE_NAME).exists());
        assert!(archive_dir.join(ARCHIVE_EVENTS_FILE_NAME).exists());
        assert!(!archive_dir.join(ARCHIVE_V1_FILE_NAME).exists());
        let event_log = read_event_log_for_test(&archive_dir.join(ARCHIVE_EVENTS_FILE_NAME));
        assert!(event_log.iter().any(|entry| {
            matches!(
                entry,
                SavedEventLogEntry::ArchiveSaved {
                    saved_at_unix_ns: _
                }
            )
        }));
        assert!(event_log.iter().any(|entry| {
            matches!(
                entry,
                SavedEventLogEntry::RunSummary { run } if run.label == "archive-source"
            )
        }));
        assert!(event_log.iter().any(|entry| {
            matches!(
                entry,
                SavedEventLogEntry::TargetIngestDiagnostics { diagnostics }
                    if diagnostics.spans_recorded == 1
            )
        }));
        assert!(event_log.iter().any(|entry| {
            matches!(
                entry,
                SavedEventLogEntry::Interval { tid, interval }
                    if *tid == SYNTH_TID_BASE
                        && interval.start_ns == 10_000_000
                        && interval.end_ns == 16_000_000
            )
        }));
        let package_path = archive_dir.with_extension(ARCHIVE_SINGLE_FILE_EXTENSION);
        let _ = std::fs::remove_file(&package_path);
        server
            .save_current_archive(package_path.clone())
            .expect("save single-file archive");
        assert!(package_path.is_file());
        let bundle_bytes = std::fs::read(&package_path).expect("read bundle");
        let mut bundle: SavedRunArchiveBundle =
            facet_json::from_slice(&bundle_bytes).expect("parse bundle");
        assert_eq!(bundle.format_version, ARCHIVE_FORMAT_VERSION);
        assert_eq!(bundle.provenance.producer, env!("CARGO_PKG_NAME"));
        assert!(bundle.events.iter().any(|entry| {
            matches!(
                entry,
                SavedEventLogEntry::Interval { tid, interval }
                    if *tid == SYNTH_TID_BASE
                        && interval.start_ns == 10_000_000
                        && interval.end_ns == 16_000_000
            )
        }));
        bundle.aggregator = SavedAggregator::default();
        bundle.binaries = SavedBinaryRegistry::default();
        bundle.target_ingest = TargetIngestDiagnostics::default();
        std::fs::write(
            &package_path,
            facet_json::to_vec_pretty(&bundle).expect("serialize aggregate-blanked bundle"),
        )
        .expect("write aggregate-blanked bundle");
        let bundle_archive = read_archive(&package_path).expect("read single-file archive");
        assert_eq!(bundle_archive.format_version, ARCHIVE_FORMAT_VERSION);
        assert_eq!(
            bundle_archive.runs.last().map(|run| run.label.as_str()),
            Some("archive-source")
        );
        assert_eq!(bundle_archive.target_ingest.spans_recorded, 1);
        assert_eq!(bundle_archive.aggregator.threads.len(), 1);
        let manifest_bytes =
            std::fs::read(archive_dir.join(ARCHIVE_MANIFEST_FILE_NAME)).expect("read manifest");
        let manifest: SavedRunArchiveManifest =
            facet_json::from_slice(&manifest_bytes).expect("parse manifest");
        assert_eq!(manifest.format_version, ARCHIVE_FORMAT_VERSION);
        assert_eq!(manifest.provenance.producer, env!("CARGO_PKG_NAME"));
        assert_eq!(
            manifest.provenance.producer_version,
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(manifest.provenance.os, std::env::consts::OS);
        assert_eq!(manifest.provenance.arch, std::env::consts::ARCH);
        assert_eq!(manifest.files.aggregator, ARCHIVE_AGGREGATOR_FILE_NAME);
        assert_eq!(manifest.files.binaries, ARCHIVE_BINARIES_FILE_NAME);
        assert_eq!(
            manifest.files.target_ingest,
            ARCHIVE_TARGET_INGEST_FILE_NAME
        );
        write_aggregator(
            archive_dir.join(ARCHIVE_AGGREGATOR_FILE_NAME),
            &SavedAggregator::default(),
        )
        .expect("blank aggregate chunk");
        write_binaries(
            archive_dir.join(ARCHIVE_BINARIES_FILE_NAME),
            &SavedBinaryRegistry::default(),
        )
        .expect("blank binary chunk");
        write_target_ingest(
            archive_dir.join(ARCHIVE_TARGET_INGEST_FILE_NAME),
            &TargetIngestDiagnostics::default(),
        )
        .expect("blank target-ingest chunk");
        let manifest_archive = read_archive(&archive_dir.join(ARCHIVE_MANIFEST_FILE_NAME))
            .expect("read archive through manifest path");
        assert_eq!(manifest_archive.format_version, ARCHIVE_FORMAT_VERSION);
        assert_eq!(
            manifest_archive.runs.last().map(|run| run.label.as_str()),
            Some("archive-source")
        );
        assert_eq!(manifest_archive.target_ingest.spans_recorded, 1);
        assert_eq!(manifest_archive.aggregator.threads.len(), 1);

        let busy = ServerState::new_for_tests();
        busy.set_active_run_for_tests(9000);
        assert!(matches!(
            busy.open_saved_archive(archive_dir.clone()),
            Err(RunControlError::AlreadyActive)
        ));

        let restored = ServerState::new_for_tests();
        restored
            .open_saved_archive(archive_dir.clone())
            .expect("open saved archive");

        let runs = restored.list_runs().await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, RunState::Stopped);
        assert_eq!(runs[0].target_pid, Some(4242));
        assert_eq!(runs[0].label, "archive-source");

        let profiler = restored.profiler();
        let top = profiler
            .top(10, TopSort::BySelf, view_params(Some(SYNTH_TID_BASE)))
            .await;
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].function_name.as_deref(), Some("archive_kernel"));
        assert_eq!(top[0].self_on_cpu_ns, 6_000_000);
        assert_eq!(top[0].self_target_ns, 6_000_000);
        assert_eq!(top[0].self_pet_samples, 1);
        assert_eq!(top[0].self_target_spans, 1);

        let flame = profiler.flamegraph(view_params(Some(SYNTH_TID_BASE))).await;
        assert_eq!(flame.total_on_cpu_ns, 6_000_000);
        assert_eq!(flame.total_target_ns, 6_000_000);
        assert_eq!(flame.total_target_spans, 1);
        assert_eq!(flame.root.children.len(), 1);
        let lane = &flame.root.children[0];
        assert_eq!(flame_node_name(lane, &flame.strings), Some("GPU archive"));
        assert_eq!(lane.children.len(), 1);
        assert_eq!(
            flame_node_name(&lane.children[0], &flame.strings),
            Some("archive_kernel")
        );

        let threads = profiler.threads(run_view_params(None)).await;
        let thread = threads
            .threads
            .iter()
            .find(|thread| thread.tid == SYNTH_TID_BASE)
            .expect("restored synthetic lane");
        assert_eq!(thread.name.as_deref(), Some("GPU archive"));
        assert_eq!(thread.on_cpu_ns, 6_000_000);
        assert_eq!(thread.target_ns, 6_000_000);
        assert_eq!(thread.pet_samples, 1);
        assert_eq!(thread.target_spans, 1);

        let diagnostics = restored
            .diagnostics(run_view_params(None))
            .await
            .target_ingest;
        assert_eq!(diagnostics.batches, 1);
        assert_eq!(diagnostics.spans_recorded, 1);
        assert_eq!(diagnostics.total_duration_ns, 6_000_000);
        assert_eq!(diagnostics.batches_dropped_target_queue_full, 2);
        assert_eq!(diagnostics.spans_dropped_target_queue_full, 9);
        assert_eq!(diagnostics.batches_dropped_target_worker_disconnected, 1);
        assert_eq!(diagnostics.spans_dropped_target_worker_disconnected, 4);
        assert_eq!(diagnostics.lanes.len(), 1);
        assert_eq!(diagnostics.lanes[0].tid, SYNTH_TID_BASE);
        assert_eq!(diagnostics.lanes[0].name, "GPU archive");

        let _ = std::fs::remove_dir_all(&archive_dir);
        let _ = std::fs::remove_file(&package_path);
    }

    #[tokio::test]
    async fn select_run_restores_stopped_run_query_snapshot() {
        let server = ServerState::new_for_tests();
        let service = TargetIngestService::new(server.clone());

        let first = server
            .begin_run(test_run_config("first-run"))
            .expect("begin first run");
        server.apply_target_attached_in_process(first, 1111, 0);
        service
            .ingest(TargetSpanBatch {
                pid: 1111,
                lane: "GPU first".to_owned(),
                spans: vec![TargetSpan::new("first_kernel", 1_000_000, 3_000_000)],
            })
            .await;
        server.finalize_run(first, StopReason::TargetExited);

        let second = server
            .begin_run(test_run_config("second-run"))
            .expect("begin second run");
        server.apply_target_attached_in_process(second, 2222, 0);
        service
            .ingest(TargetSpanBatch {
                pid: 2222,
                lane: "GPU second".to_owned(),
                spans: vec![TargetSpan::new("second_kernel", 10_000_000, 16_000_000)],
            })
            .await;
        server.finalize_run(second, StopReason::TargetExited);

        let runs = server.list_runs().await;
        assert_eq!(
            runs.iter().map(|run| run.id).collect::<Vec<_>>(),
            vec![first, second,]
        );

        let profiler = server.profiler();
        let top = profiler
            .top(10, TopSort::BySelf, view_params(Some(SYNTH_TID_BASE)))
            .await;
        assert_eq!(top[0].function_name.as_deref(), Some("second_kernel"));

        let selected = server
            .select_run_archive(first)
            .expect("select first run snapshot");
        assert_eq!(selected.id, first);
        assert_eq!(selected.label, "first-run");

        let top = profiler
            .top(10, TopSort::BySelf, view_params(Some(SYNTH_TID_BASE)))
            .await;
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].function_name.as_deref(), Some("first_kernel"));
        assert_eq!(top[0].self_target_ns, 2_000_000);
        assert_eq!(top[0].self_target_spans, 1);

        let diagnostics = server
            .diagnostics(run_view_params(None))
            .await
            .target_ingest;
        assert_eq!(diagnostics.lanes.len(), 1);
        assert_eq!(diagnostics.lanes[0].name, "GPU first");
        assert_eq!(diagnostics.total_duration_ns, 2_000_000);
    }

    #[tokio::test]
    async fn run_params_query_history_without_selecting_it() {
        let server = ServerState::new_for_tests();
        let service = TargetIngestService::new(server.clone());

        let first = server
            .begin_run(test_run_config("first-run"))
            .expect("begin first run");
        server.apply_target_attached_in_process(first, 1111, 0);
        service
            .ingest(TargetSpanBatch {
                pid: 1111,
                lane: "GPU first".to_owned(),
                spans: vec![TargetSpan::new("first_kernel", 1_000_000, 3_000_000)],
            })
            .await;
        server.finalize_run(first, StopReason::TargetExited);

        let second = server
            .begin_run(test_run_config("second-run"))
            .expect("begin second run");
        server.apply_target_attached_in_process(second, 2222, 0);
        service
            .ingest(TargetSpanBatch {
                pid: 2222,
                lane: "GPU second".to_owned(),
                spans: vec![TargetSpan::new("second_kernel", 10_000_000, 16_000_000)],
            })
            .await;
        server.finalize_run(second, StopReason::TargetExited);

        let profiler = server.profiler();
        let current_top = profiler
            .top(10, TopSort::BySelf, view_params(Some(SYNTH_TID_BASE)))
            .await;
        assert_eq!(
            current_top[0].function_name.as_deref(),
            Some("second_kernel")
        );

        let first_top = profiler
            .top(
                10,
                TopSort::BySelf,
                view_params_for_run(Some(first), Some(SYNTH_TID_BASE)),
            )
            .await;
        assert_eq!(first_top.len(), 1);
        assert_eq!(first_top[0].function_name.as_deref(), Some("first_kernel"));
        assert_eq!(first_top[0].self_target_ns, 2_000_000);

        let first_threads = profiler.threads(run_view_params(Some(first))).await;
        let first_lane = first_threads
            .threads
            .iter()
            .find(|thread| thread.tid == SYNTH_TID_BASE)
            .expect("first synthetic lane");
        assert_eq!(first_lane.name.as_deref(), Some("GPU first"));

        let first_diagnostics = server
            .diagnostics(run_view_params(Some(first)))
            .await
            .target_ingest;
        assert_eq!(first_diagnostics.lanes[0].name, "GPU first");

        let current_top = profiler
            .top(10, TopSort::BySelf, view_params(Some(SYNTH_TID_BASE)))
            .await;
        assert_eq!(
            current_top[0].function_name.as_deref(),
            Some("second_kernel")
        );
        let current_diagnostics = server
            .diagnostics(run_view_params(None))
            .await
            .target_ingest;
        assert_eq!(current_diagnostics.lanes[0].name, "GPU second");
    }

    fn test_run_config(label: &str) -> RunConfig {
        RunConfig {
            label: label.to_owned(),
            frequency_hz: 900,
            dwarf_unwind: false,
        }
    }

    fn view_params(tid: Option<u32>) -> ViewParams {
        view_params_for_run(None, tid)
    }

    fn view_params_for_run(run: Option<RunId>, tid: Option<u32>) -> ViewParams {
        ViewParams {
            run,
            tid,
            filter: LiveFilter {
                time_range: None,
                exclude_symbols: Vec::new(),
            },
        }
    }

    fn run_view_params(run: Option<RunId>) -> RunViewParams {
        RunViewParams { run }
    }

    fn flame_node_name<'a>(
        node: &stax_live_proto::FlameNode,
        strings: &'a [String],
    ) -> Option<&'a str> {
        node.function_name
            .and_then(|index| strings.get(index as usize))
            .map(String::as_str)
    }

    fn temp_archive_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "stax-archive-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn read_event_log_for_test(path: &Path) -> Vec<SavedEventLogEntry> {
        let text = std::fs::read_to_string(path).expect("read event log");
        text.lines()
            .map(|line| facet_json::from_slice(line.as_bytes()).expect("parse event log entry"))
            .collect()
    }

    fn test_saved_binary() -> SavedLoadedBinary {
        SavedLoadedBinary {
            path: "/tmp/jit-code".to_owned(),
            base_avma: 0x1234_0000,
            avma_end: 0x1234_1000,
            text_svma: 0,
            arch: Some(std::env::consts::ARCH.to_owned()),
            is_executable: false,
            symbols: Vec::new(),
            text_bytes: Some(vec![1, 2, 3, 4]),
        }
    }
}
