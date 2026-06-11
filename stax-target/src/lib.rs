//! Target-side latch for stax.
//!
//! A profiled app links this crate and calls [`report`] with execution
//! spans from lanes the CPU sampler cannot see — GPU command queues,
//! accelerators. When a stax recording of this process is active, the
//! spans land on the recording timeline as a synthetic thread next to
//! the real ones (see `stax-server`'s `TargetIngest`); when no server
//! or no recording is around, reporting is a cheap no-op.
//!
//! Fire-and-forget by design: a background worker owns the connection,
//! batches are dropped (never block the caller) when the queue is full
//! or the server is away, and reconnection happens lazily per batch.
//!
//! Span timestamps are absolute mach-derived nanoseconds — on Apple
//! platforms `mach_absolute_time` converted to ns (Apple Silicon GPU
//! timestamps share that timebase), which is the same clock domain the
//! sampler records in.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

pub use stax_live_proto::TargetSpan;
use stax_live_proto::{TargetIngestClient, TargetSpanBatch};

/// Bounded queue between reporting threads and the worker. Each entry
/// is one batch; overflow drops the newest batch (profiling telemetry,
/// not ledger data).
const QUEUE_DEPTH: usize = 64;

/// Report one lane's spans. Cheap and non-blocking; safe to call from
/// hot paths. `lane` names the synthetic thread (e.g. "GPU tq1s").
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
    let runtime = match tokio::runtime::Builder::new_current_thread()
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
    let mut client: Option<TargetIngestClient> = None;
    while let Ok(batch) = rx.recv() {
        runtime.block_on(async {
            if client.is_none() {
                client = connect().await;
            }
            let Some(live) = client.as_ref() else {
                // No server right now; drop the batch, retry on the next.
                return;
            };
            if let Err(e) = live.ingest(batch).await {
                tracing::debug!("stax-target: ingest failed, dropping connection: {e}");
                client = None;
            }
        });
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
/// (server not running) — reporting is then a no-op.
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
