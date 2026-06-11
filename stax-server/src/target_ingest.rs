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

    /// Synthetic tid for a lane, allocating + naming it on first sight.
    fn lane_tid(&self, pid: u32, lane: &str) -> u32 {
        let mut lanes = self.server.target_lanes().lock();
        if let Some(&tid) = lanes.lane_tids.get(&(pid, lane.to_owned())) {
            return tid;
        }
        let tid = SYNTH_TID_BASE + lanes.lane_tids.len() as u32;
        lanes.lane_tids.insert((pid, lane.to_owned()), tid);
        drop(lanes);
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
}
