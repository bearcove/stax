//! In-process recording driver. Replaces the per-attachment
//! `stax-shade` companion process.
//!
//! Only one entry point: `spawn_attach`. The CLI now owns
//! `posix_spawn(SUSPENDED)` + PTY for `stax record -- <argv>` runs
//! and hands us a PID via `RunControl::start_attach` once the
//! target is spawned. Per-target staxd sampling is the same path
//! whether the CLI spawned the PID or attached to an existing one.

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use stax_core::cmd_record_mac::LiveOnlySink;
use stax_core::live_sink::{
    BinaryLoadedEvent as LiveBinaryLoaded, BinaryUnloadedEvent as LiveBinaryUnloaded,
    CpuIntervalEvent as LiveCpuInterval, CpuIntervalKind as LiveCpuIntervalKind, LiveSink,
    MachOByteSource, SampleEvent as LiveSampleEvent, TargetAttached, ThreadName,
    WakeupEvent as LiveWakeup,
};
use stax_live::{IntervalKind, LiveSymbolOwned, LoadedBinary, PmuSample};
use stax_live_proto::{RunId, StopReason};

use crate::ServerState;

/// Spawn the recording task for an attach run. The task drives
/// `staxd-client` on a dedicated OS thread with its own
/// current-thread tokio runtime and finalises the run when it
/// exits.
pub fn spawn_attach(
    server: ServerState,
    run_id: RunId,
    pid: u32,
    frequency_hz: u32,
    daemon_socket: String,
    time_limit: Option<Duration>,
) {
    let stop_flag = Arc::new(AtomicBool::new(false));
    server.set_recording_stop_flag(run_id, stop_flag.clone());

    spawn_on_dedicated_runtime(move || async move {
        let result =
            run_attach(server.clone(), run_id, pid, frequency_hz, daemon_socket, time_limit, stop_flag)
                .await;
        finalize(&server, run_id, result);
    });
}

/// Run the recording loop on a dedicated OS thread with its own
/// current-thread tokio runtime. `staxd-client` internally builds
/// futures (via `vox::connect`, etc.) that are not `Send`, so we
/// can't `tokio::spawn` it onto the multi-thread runtime that's
/// hosting the vox handlers; a current-thread runtime sidesteps the
/// `Send` requirement entirely.
fn spawn_on_dedicated_runtime<F, Fut>(make_future: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("recorder: build runtime failed: {e}");
                return;
            }
        };
        rt.block_on(make_future());
    });
}

fn finalize(server: &ServerState, run_id: RunId, result: eyre::Result<StopReason>) {
    let reason = match result {
        Ok(reason) => reason,
        Err(err) => {
            tracing::warn!(run_id = run_id.0, error = ?err, "recording failed");
            StopReason::RecorderError {
                message: format!("{err:?}"),
            }
        }
    };
    server.finalize_run(run_id, reason);
}

async fn run_attach(
    server: ServerState,
    run_id: RunId,
    pid: u32,
    frequency_hz: u32,
    daemon_socket: String,
    time_limit: Option<Duration>,
    stop_flag: Arc<AtomicBool>,
) -> eyre::Result<StopReason> {
    let opts = staxd_client::RemoteOptions {
        daemon_socket,
        pid,
        frequency_hz,
        duration: time_limit,
        ..Default::default()
    };

    let recording_start = Instant::now();
    tracing::info!(
        run_id = run_id.0,
        pid,
        frequency_hz = opts.frequency_hz,
        daemon_socket = %opts.daemon_socket,
        "recording lifecycle starting"
    );

    let sink = LiveOnlySink::new(Some(Box::new(ServerLiveSink {
        server: server.clone(),
        run_id,
    })));
    sink.notify_target_attached(pid);

    let stop_flag_for_should_stop = stop_flag.clone();
    let mut stop_reason_logged = false;
    let should_stop = move || {
        if stop_flag_for_should_stop.load(Ordering::Relaxed) {
            if !stop_reason_logged {
                stop_reason_logged = true;
                tracing::info!("recording stop requested");
            }
            return true;
        }
        false
    };

    let on_first_batch = move || {
        tracing::info!(run_id = run_id.0, "staxd-client first batch observed");
    };

    let result = staxd_client::drive_session_with_hooks(
        opts,
        sink,
        should_stop,
        on_first_batch,
        |_, _| {},
    )
    .await;

    match &result {
        Ok(()) => tracing::info!(
            run_id = run_id.0,
            elapsed = ?recording_start.elapsed(),
            "drive_session completed"
        ),
        Err(e) => tracing::warn!(
            run_id = run_id.0,
            elapsed = ?recording_start.elapsed(),
            error = %e,
            "drive_session failed"
        ),
    }

    match result {
        Ok(()) => Ok(StopReason::UserStop),
        Err(e) => Ok(StopReason::RecorderError {
            message: format!("staxd-client failed: {e}"),
        }),
    }
}

/// `LiveSink` impl that records events straight into the
/// in-process aggregator + binary registry on `ServerState`. No
/// IngestBatch encoding, no IPC, no drainer task.
struct ServerLiveSink {
    server: ServerState,
    run_id: RunId,
}

#[async_trait::async_trait]
impl LiveSink for ServerLiveSink {
    async fn on_sample(&self, event: &LiveSampleEvent) {
        self.server.note_sample(self.run_id);
        self.server.aggregator().write().record_pet_sample(
            event.tid,
            event.timestamp,
            event.user_backtrace,
            event.kernel_backtrace,
            PmuSample {
                cycles: event.cycles,
                instructions: event.instructions,
                l1d_misses: event.l1d_misses,
                branch_mispreds: event.branch_mispreds,
            },
        );
        self.server.bump_revision();
    }

    async fn on_target_attached(&self, event: &TargetAttached) {
        self.server
            .apply_target_attached_in_process(self.run_id, event.pid, event.task_port);
    }

    async fn on_binary_loaded(&self, event: &LiveBinaryLoaded) {
        let symbols = event
            .symbols
            .iter()
            .map(|s| LiveSymbolOwned {
                start_svma: s.start_svma,
                end_svma: s.end_svma,
                name: s.name.to_vec(),
            })
            .collect();
        let binary = LoadedBinary {
            path: event.path.to_owned(),
            base_avma: event.base_avma,
            avma_end: event.base_avma.saturating_add(event.vmsize),
            text_svma: event.text_svma,
            arch: event.arch.map(|s| s.to_owned()),
            is_executable: event.is_executable,
            symbols,
            text_bytes: event.text_bytes.map(|b| b.to_vec()),
        };
        self.server.binaries().write().insert(binary);
        self.server.bump_revision();
    }

    async fn on_binary_unloaded(&self, _event: &LiveBinaryUnloaded) {
        self.server.bump_revision();
    }

    async fn on_thread_name(&self, event: &ThreadName) {
        self.server
            .aggregator()
            .write()
            .set_thread_name(event.tid, event.name.to_owned());
        self.server.bump_revision();
    }

    async fn on_wakeup(&self, event: &LiveWakeup) {
        self.server.aggregator().write().record_wakeup(
            event.timestamp,
            event.waker_tid,
            event.wakee_tid,
            event.waker_user_stack.to_vec(),
            event.waker_kernel_stack.to_vec(),
        );
        self.server.bump_revision();
    }

    async fn on_cpu_interval(&self, event: &LiveCpuInterval) {
        let kind = match &event.kind {
            LiveCpuIntervalKind::OnCpu => IntervalKind::OnCpu,
            LiveCpuIntervalKind::OffCpu {
                stack,
                waker_tid,
                waker_user_stack,
            } => {
                self.server.note_off_cpu(self.run_id);
                IntervalKind::OffCpu {
                    stack: stack.to_vec().into_boxed_slice(),
                    waker_tid: *waker_tid,
                    waker_user_stack: waker_user_stack.map(|s| s.to_vec().into_boxed_slice()),
                }
            }
        };
        self.server.aggregator().write().record_interval(
            event.tid,
            event.start_ns,
            event.end_ns,
            kind,
        );
        self.server.bump_revision();
    }

    async fn on_macho_byte_source(&self, source: Arc<dyn MachOByteSource>) {
        self.server.binaries().write().set_macho_byte_source(source);
    }
}
