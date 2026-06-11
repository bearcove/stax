//! `TargetIngest` — the latch a profiled app uses to put its off-CPU
//! execution lanes (GPU command queues, accelerators) on the recording
//! timeline.
//!
//! There is deliberately NO span-specific storage or view. Reported
//! spans are translated into data the existing model already
//! understands, the same way jitdump turns JIT code into an ordinary
//! binary:
//!
//! - each `(pid, lane)` becomes a **synthetic thread**: a pseudo-tid in
//!   a high range no real thread occupies, named after the lane, so the
//!   threads/timeline views show "GPU tq1s" next to real threads;
//! - each distinct span name becomes a **synthetic symbol** inside a
//!   synthetic binary at a base address no real image occupies, so
//!   top/flame group and label by kernel name;
//! - each span is decomposed into **synthesized PET samples** at
//!   `SYNTH_PERIOD_NS` across `[start, end)` — exactly the evidence the
//!   sampler would have produced had the lane been a thread executing a
//!   function with the span's name. Duration weighting in every view
//!   follows for free.
//!
//! Timestamps arrive as absolute mach-derived nanoseconds (Apple
//! Silicon GPU timestamps share mach_absolute_time's epoch and rate),
//! so they need no correlation against sampled stacks.

use std::collections::HashMap;

use stax_live::{LiveSymbolOwned, LoadedBinary, PmuSample};
use stax_live_proto::{TargetIngest, TargetSpanBatch};

use crate::ServerState;

/// Pseudo-tid range for synthetic lanes. Real Mach thread ids are
/// kernel-allocated small-ish integers; anything at or above this base
/// is ours.
const SYNTH_TID_BASE: u32 = 0xFFF0_0000;

/// Base AVMA for the synthetic span-name "binary". Above any userspace
/// image on arm64 macOS (user VA tops out well below 2^47), below the
/// kernel collection's high range.
const SYNTH_BINARY_BASE_AVMA: u64 = 0xFFFF_0000_0000;

/// Each span name owns one 16-byte synthetic "function".
const SYNTH_SYMBOL_STRIDE: u64 = 16;

/// Synthesized-sample period: one sample per millisecond of span time.
/// Matches the order of magnitude of the real PET timer so lane weights
/// are comparable with CPU threads in top/flame.
const SYNTH_PERIOD_NS: u64 = 1_000_000;

/// Cap on samples synthesized from one span — a misreported span (e.g.
/// a stuck end timestamp) must not flood the aggregator.
const SYNTH_SAMPLES_PER_SPAN_CAP: u64 = 16_384;

#[derive(Default)]
pub(crate) struct TargetLaneRegistry {
    /// (pid, lane) → synthetic tid.
    lane_tids: HashMap<(u32, String), u32>,
    /// span name → synthetic symbol AVMA.
    symbol_addrs: HashMap<String, u64>,
}

#[derive(Clone)]
pub(crate) struct TargetIngestService {
    server: ServerState,
}

impl TargetIngestService {
    pub(crate) fn new(server: ServerState) -> Self {
        Self { server }
    }

    /// Synthetic tid for a lane, allocating on first sight. The lane
    /// NAME is re-armed on every batch: the aggregator resets on each
    /// new run, which would orphan a name set only once.
    fn lane_tid(&self, pid: u32, lane: &str) -> u32 {
        let tid = {
            let mut lanes = self.server.target_lanes().lock();
            match lanes.lane_tids.get(&(pid, lane.to_owned())) {
                Some(&tid) => tid,
                None => {
                    let tid = SYNTH_TID_BASE + lanes.lane_tids.len() as u32;
                    lanes.lane_tids.insert((pid, lane.to_owned()), tid);
                    tid
                }
            }
        };
        self.server
            .aggregator()
            .write()
            .set_thread_name(tid, lane.to_owned());
        tid
    }

    /// Synthetic symbol address for a span name, (re)publishing the
    /// synthetic binary when a new name appears.
    fn symbol_addr(&self, name: &str) -> u64 {
        let mut lanes = self.server.target_lanes().lock();
        if let Some(&addr) = lanes.symbol_addrs.get(name) {
            return addr;
        }
        let index = lanes.symbol_addrs.len() as u64;
        let addr = SYNTH_BINARY_BASE_AVMA + index * SYNTH_SYMBOL_STRIDE;
        lanes.symbol_addrs.insert(name.to_owned(), addr);
        // Republish the synthetic binary with the full symbol list;
        // `BinaryRegistry::insert` replaces by base AVMA. New names are
        // rare (one per distinct kernel), so the rebuild is cold.
        let mut symbols: Vec<LiveSymbolOwned> = lanes
            .symbol_addrs
            .iter()
            .map(|(name, &addr)| LiveSymbolOwned {
                start_svma: addr - SYNTH_BINARY_BASE_AVMA,
                end_svma: addr - SYNTH_BINARY_BASE_AVMA + SYNTH_SYMBOL_STRIDE,
                name: name.as_bytes().to_vec(),
            })
            .collect();
        symbols.sort_by_key(|s| s.start_svma);
        let avma_end =
            SYNTH_BINARY_BASE_AVMA + (lanes.symbol_addrs.len() as u64) * SYNTH_SYMBOL_STRIDE;
        drop(lanes);
        self.server.binaries().write().insert(LoadedBinary {
            path: "<target spans>".to_owned(),
            base_avma: SYNTH_BINARY_BASE_AVMA,
            avma_end,
            text_svma: 0,
            arch: None,
            is_executable: false,
            symbols,
            text_bytes: None,
        });
        addr
    }
}

impl TargetIngest for TargetIngestService {
    async fn ingest(&self, batch: TargetSpanBatch) {
        // Only the active run's target may land spans on the timeline.
        let Some(active_pid) = self.server.active_target_pid() else {
            return;
        };
        if active_pid != batch.pid {
            return;
        }
        if batch.spans.is_empty() {
            return;
        }
        let tid = self.lane_tid(batch.pid, &batch.lane);
        let mut synthesized = 0u64;
        for span in &batch.spans {
            if span.end_ns <= span.start_ns {
                continue;
            }
            let addr = self.symbol_addr(&span.name);
            let duration = span.end_ns - span.start_ns;
            let samples = (duration / SYNTH_PERIOD_NS)
                .max(1)
                .min(SYNTH_SAMPLES_PER_SPAN_CAP);
            let mut aggregator = self.server.aggregator().write();
            for k in 0..samples {
                let ts = span.start_ns + k * SYNTH_PERIOD_NS;
                aggregator.record_pet_sample(
                    tid,
                    ts,
                    &[addr],
                    &[],
                    PmuSample {
                        cycles: 0,
                        instructions: 0,
                        l1d_misses: 0,
                        branch_mispreds: 0,
                    },
                );
            }
            synthesized += samples;
        }
        if synthesized > 0 {
            self.server.bump_revision();
        }
        tracing::debug!(
            pid = batch.pid,
            lane = %batch.lane,
            spans = batch.spans.len(),
            synthesized,
            "target spans ingested"
        );
    }

    async fn should_report(&self, pid: u32) -> bool {
        self.server.active_target_pid() == Some(pid)
    }
}

#[cfg(test)]
mod tests {
    use stax_live_proto::{TargetIngest as _, TargetSpan, TargetSpanBatch};

    use super::*;

    /// The whole latch path server-side: pid gate, synthetic thread
    /// (named lane), synthetic symbol resolution, and duration-weighted
    /// sample synthesis into the EXISTING aggregator — no span-specific
    /// storage anywhere.
    #[tokio::test]
    async fn ingest_lands_spans_as_synthetic_thread_with_named_symbols() {
        let server = ServerState::new_for_tests();
        let service = TargetIngestService::new(server.clone());

        // No active run: gate closed.
        assert!(!service.should_report(42).await);
        service
            .ingest(TargetSpanBatch {
                pid: 42,
                lane: "GPU test".to_owned(),
                spans: vec![span("kernel_a", 1_000_000, 4_000_000)],
            })
            .await;
        assert!(server.aggregator().read().session_start_ns().is_none());

        // Active run targeting pid 42: gate open, spans land.
        server.set_active_run_for_tests(42);
        assert!(service.should_report(42).await);
        assert!(!service.should_report(43).await);
        service
            .ingest(TargetSpanBatch {
                pid: 42,
                lane: "GPU test".to_owned(),
                spans: vec![
                    span("kernel_a", 1_000_000, 4_000_000), // 3ms -> 3 samples
                    span("kernel_b", 4_000_000, 4_500_000), // 0.5ms -> 1 sample
                ],
            })
            .await;
        // Wrong pid still drops.
        service
            .ingest(TargetSpanBatch {
                pid: 43,
                lane: "GPU other".to_owned(),
                spans: vec![span("kernel_c", 1_000_000, 9_000_000)],
            })
            .await;

        let aggregator = server.aggregator().read();
        let tid = SYNTH_TID_BASE;
        assert_eq!(aggregator.thread_name(tid), Some("GPU test"));
        assert_eq!(aggregator.session_start_ns(), Some(1_000_000));
        assert_eq!(aggregator.last_event_ns(), Some(4_000_000));

        // Span names resolve as symbols through the ordinary registry.
        let binaries = server.binaries().read();
        let a = binaries
            .lookup_symbol(SYNTH_BINARY_BASE_AVMA)
            .expect("kernel_a resolves");
        assert_eq!(a.function_name, "kernel_a");
        let b = binaries
            .lookup_symbol(SYNTH_BINARY_BASE_AVMA + SYNTH_SYMBOL_STRIDE)
            .expect("kernel_b resolves");
        assert_eq!(b.function_name, "kernel_b");
        assert!(
            binaries
                .lookup_symbol(SYNTH_BINARY_BASE_AVMA + 2 * SYNTH_SYMBOL_STRIDE)
                .is_none(),
            "dropped wrong-pid span must not register symbols"
        );
    }

    fn span(name: &str, start_ns: u64, end_ns: u64) -> TargetSpan {
        TargetSpan {
            name: name.to_owned(),
            start_ns,
            end_ns,
        }
    }

    /// Same assertions as above, but through the REAL wire: a local
    /// vox acceptor with the production routing factory, and a real
    /// `TargetIngestClient` like stax-target's worker uses. Guards the
    /// dispatch path (service routing + method dispatch + facet codec),
    /// not just the service impl.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_over_local_socket_lands_spans() {
        use stax_live_proto::TargetIngestClient;

        let server = ServerState::new_for_tests();
        server.set_active_run_for_tests(4242);
        let socket = std::env::temp_dir().join(format!(
            "stax-target-ingest-test-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let acceptor = vox::transport::local::LocalLinkAcceptor::bind(
            socket.to_string_lossy().into_owned(),
        )
        .expect("bind test socket");
        crate::spawn_accept_loop_local(server.clone(), acceptor);

        let url = format!("local://{}", socket.display());
        let client: TargetIngestClient = vox::connect(&url).await.expect("connect");
        assert!(client.should_report(4242).await.expect("should_report"));
        client
            .ingest(TargetSpanBatch {
                pid: 4242,
                lane: "GPU wire".to_owned(),
                spans: vec![span("wire_kernel", 10_000_000, 16_000_000)],
            })
            .await
            .expect("ingest");

        // ingest is fire-and-forget at the service level; give the
        // dispatcher a beat to run.
        for _ in 0..50 {
            if server
                .aggregator()
                .read()
                .thread_name(SYNTH_TID_BASE)
                .is_some()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let aggregator = server.aggregator().read();
        assert_eq!(aggregator.thread_name(SYNTH_TID_BASE), Some("GPU wire"));
        assert_eq!(aggregator.session_start_ns(), Some(10_000_000));
        let _ = std::fs::remove_file(&socket);
    }
}
