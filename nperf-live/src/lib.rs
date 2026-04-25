//! Live serving of nperf samples over a vox WebSocket RPC service.
//!
//! Architecture: the (sync) sampler thread pushes events into an unbounded
//! tokio channel via `LiveSinkImpl`. A drainer task on the tokio side updates
//! a shared `Aggregator`, which the vox service queries.

use std::sync::Arc;

use eyre::Result;
use parking_lot::RwLock;
use tokio::sync::mpsc;

use nperf_core::live_sink::{LiveSink, SampleEvent};
use nperf_live_proto::{
    AnnotatedLine, AnnotatedView, Profiler, ProfilerDispatcher, TopEntry, TopUpdate,
};

mod aggregator;
mod highlight;

pub use aggregator::Aggregator;

/// What the sampler thread pushes into tokio. Owned data so we can move
/// across the thread boundary cheaply.
pub(crate) struct OwnedSample {
    pub user_addrs: Vec<u64>,
}

#[derive(Clone)]
pub struct LiveSinkImpl {
    tx: mpsc::UnboundedSender<OwnedSample>,
}

impl LiveSink for LiveSinkImpl {
    fn on_sample(&self, event: &SampleEvent) {
        let user_addrs: Vec<u64> = event.user_backtrace.iter().map(|f| f.address).collect();
        let _ = self.tx.send(OwnedSample { user_addrs });
    }
}

#[derive(Clone)]
pub struct LiveServer {
    pub aggregator: Arc<RwLock<Aggregator>>,
}

impl Profiler for LiveServer {
    async fn top(&self, limit: u32) -> Vec<TopEntry> {
        self.aggregator.read().top(limit as usize)
    }

    async fn subscribe_top(&self, limit: u32, output: vox::Tx<TopUpdate>) {
        let aggregator = self.aggregator.clone();
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            interval.tick().await;
            let snapshot = {
                let agg = aggregator.read();
                TopUpdate {
                    total_samples: agg.total_samples(),
                    entries: agg.top(limit as usize),
                }
            };
            if output.send(snapshot).await.is_err() {
                break;
            }
        }
    }

    async fn total_samples(&self) -> u64 {
        self.aggregator.read().total_samples()
    }

    async fn subscribe_annotated(&self, address: u64, output: vox::Tx<AnnotatedView>) {
        // TODO: real disassembly path requires plumbing the address-space /
        // binary-map state from the sampler into the live aggregator and
        // re-using cmd_annotate's symbol resolution + yaxpeax disassembly.
        //
        // For now we emit a small synthetic disassembly around the queried
        // address, highlighted via arborium. This proves the wire format end
        // to end and lets the frontend iterate on the annotation UI before
        // the binary-lookup integration lands.
        let aggregator = self.aggregator.clone();
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            interval.tick().await;
            // Build the view in a sync block so neither the parking_lot guard
            // nor the (non-Send) arborium Highlighter cross an await.
            let view = build_annotated_view(&aggregator, address);
            if output.send(view).await.is_err() {
                break;
            }
        }
    }
}

fn build_annotated_view(
    aggregator: &Arc<RwLock<Aggregator>>,
    address: u64,
) -> AnnotatedView {
    let mut hl = highlight::AsmHighlighter::new();
    let agg = aggregator.read();
    let lines: Vec<AnnotatedLine> = (0..8)
        .map(|i| {
            let addr = address.wrapping_add(i * 4);
            let asm = match i % 4 {
                0 => "push    rbp".to_string(),
                1 => "mov     rbp, rsp".to_string(),
                2 => format!("mov     eax, dword ptr [rdi + {:#x}]", i * 8),
                _ => "ret".to_string(),
            };
            AnnotatedLine {
                address: addr,
                html: hl.highlight_line(&asm),
                self_count: agg.self_count(addr),
            }
        })
        .collect();
    AnnotatedView {
        function_name: format!("fn@{:#x}", address),
        base_address: address,
        queried_address: address,
        lines,
    }
}

/// Spawn the live-serving infrastructure on the current tokio runtime.
///
/// Returns the `LiveSinkImpl` to install on `ProfilingController` and a
/// JoinHandle for the server task.
pub async fn start(addr: &str) -> Result<(LiveSinkImpl, tokio::task::JoinHandle<()>)> {
    let aggregator = Arc::new(RwLock::new(Aggregator::default()));
    let (tx, mut rx) = mpsc::unbounded_channel::<OwnedSample>();

    {
        let aggregator = aggregator.clone();
        tokio::spawn(async move {
            while let Some(sample) = rx.recv().await {
                aggregator.write().record(&sample.user_addrs);
            }
        });
    }

    let listener = vox::WsListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!("nperf-live listening on ws://{}", local);
    eprintln!("nperf-live: listening on ws://{}", local);

    let server = LiveServer { aggregator };
    let dispatcher = ProfilerDispatcher::new(server);
    let handle = tokio::spawn(async move {
        if let Err(e) = vox::serve_listener(listener, dispatcher).await {
            tracing::error!("vox serve_listener exited: {e}");
        }
    });

    Ok((LiveSinkImpl { tx }, handle))
}
