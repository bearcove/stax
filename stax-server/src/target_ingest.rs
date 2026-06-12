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
//! - each lane and each distinct span name becomes a **synthetic
//!   symbol** inside a synthetic binary at a base address no real image
//!   occupies, so flame can render `lane -> span name`;
//! - each span records one sample marker plus one attributed synthetic
//!   execution interval over `[start, end)`. The sample count is the
//!   span count; the credited time is the exact sum of span durations.
//!   If the span carries a CPU-side origin, the server also borrows the
//!   nearest sampled stack on that origin tid, so per-thread top/flame
//!   can render `CPU caller -> lane -> span name`.
//!
//! Timestamps arrive as absolute mach-derived nanoseconds (Apple
//! Silicon GPU timestamps share mach_absolute_time's epoch and rate),
//! so they need no correlation against sampled stacks.

use std::collections::HashMap;

use stax_live::{IntervalKind, LiveSymbolOwned, LoadedBinary, NearestPetStackError, PmuSample};
use stax_live_proto::{
    TargetIngest, TargetIngestDiagnostics, TargetLaneDiagnostics, TargetReporterStats,
    TargetSpanBatch, TargetSpanOrigin,
};

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

/// Max distance between a span's CPU-side origin timestamp and the PET
/// sample we borrow as its causal CPU stack. At the default 900Hz PET
/// rate this is intentionally generous, but still refuses stale stacks
/// from a different phase of work.
const ORIGIN_STACK_MAX_DISTANCE_NS: u64 = 50_000_000;

#[derive(Default)]
pub(crate) struct TargetLaneRegistry {
    /// (pid, lane) → synthetic tid.
    lane_tids: HashMap<(u32, String), u32>,
    /// lane/span symbol → synthetic symbol AVMA.
    symbol_addrs: HashMap<SyntheticSymbolKey, u64>,
    totals: TargetIngestCounters,
    lane_counters: HashMap<(u32, String), TargetIngestCounters>,
    reporter_stats: HashMap<u32, TargetReporterStats>,
    saved_diagnostics: Option<TargetIngestDiagnostics>,
}

#[derive(Clone)]
pub(crate) struct TargetIngestService {
    server: ServerState,
}

#[derive(Clone, Hash, PartialEq, Eq)]
enum SyntheticSymbolKey {
    Lane(String),
    Span(String),
}

struct SpanEvent {
    start_ns: u64,
    end_ns: u64,
    span_addr: u64,
    origin: Option<TargetSpanOrigin>,
}

#[derive(Clone, Copy, Default)]
struct TargetIngestCounters {
    batches: u64,
    batches_dropped_no_active_run: u64,
    spans_dropped_no_active_run: u64,
    batches_dropped_wrong_pid: u64,
    spans_dropped_wrong_pid: u64,
    spans_received: u64,
    spans_recorded: u64,
    spans_dropped_bad_duration: u64,
    spans_with_origin: u64,
    spans_linked_origin: u64,
    origin_links: OriginLinkCounters,
    total_duration_ns: u64,
}

#[derive(Clone, Copy, Default)]
struct TargetBatchCounters {
    received: u64,
    recorded: u64,
    dropped_bad_duration: u64,
    spans_with_origin: u64,
    spans_linked_origin: u64,
    origin_links: OriginLinkCounters,
    total_duration_ns: u64,
}

#[derive(Clone, Copy, Default)]
struct OriginLinkCounters {
    invalid_tid: u64,
    no_thread: u64,
    no_stack: u64,
    too_far: u64,
    linked_distance: DistanceCounters,
    too_far_distance: DistanceCounters,
}

#[derive(Clone, Copy, Default)]
struct DistanceCounters {
    count: u64,
    total_ns: u64,
    min_ns: u64,
    max_ns: u64,
}

impl DistanceCounters {
    fn record(&mut self, distance_ns: u64) {
        self.count = self.count.saturating_add(1);
        self.total_ns = self.total_ns.saturating_add(distance_ns);
        if self.count == 1 || distance_ns < self.min_ns {
            self.min_ns = distance_ns;
        }
        self.max_ns = self.max_ns.max(distance_ns);
    }

    fn add(&mut self, other: Self) {
        if other.count == 0 {
            return;
        }
        if self.count == 0 || other.min_ns < self.min_ns {
            self.min_ns = other.min_ns;
        }
        self.count = self.count.saturating_add(other.count);
        self.total_ns = self.total_ns.saturating_add(other.total_ns);
        self.max_ns = self.max_ns.max(other.max_ns);
    }

    fn avg_ns(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.total_ns / self.count
        }
    }
}

impl OriginLinkCounters {
    fn add(&mut self, other: Self) {
        self.invalid_tid = self.invalid_tid.saturating_add(other.invalid_tid);
        self.no_thread = self.no_thread.saturating_add(other.no_thread);
        self.no_stack = self.no_stack.saturating_add(other.no_stack);
        self.too_far = self.too_far.saturating_add(other.too_far);
        self.linked_distance.add(other.linked_distance);
        self.too_far_distance.add(other.too_far_distance);
    }
}

impl TargetIngestCounters {
    fn spans_unlinked_origin(&self) -> u64 {
        self.spans_with_origin
            .saturating_sub(self.spans_linked_origin)
    }

    fn record_batch(&mut self, batch: TargetBatchCounters) {
        self.batches = self.batches.saturating_add(1);
        self.spans_received = self.spans_received.saturating_add(batch.received);
        self.spans_recorded = self.spans_recorded.saturating_add(batch.recorded);
        self.spans_dropped_bad_duration = self
            .spans_dropped_bad_duration
            .saturating_add(batch.dropped_bad_duration);
        self.spans_with_origin = self
            .spans_with_origin
            .saturating_add(batch.spans_with_origin);
        self.spans_linked_origin = self
            .spans_linked_origin
            .saturating_add(batch.spans_linked_origin);
        self.origin_links.add(batch.origin_links);
        self.total_duration_ns = self
            .total_duration_ns
            .saturating_add(batch.total_duration_ns);
    }

    fn record_dropped_no_active_run(&mut self, spans: u64) {
        self.batches_dropped_no_active_run = self.batches_dropped_no_active_run.saturating_add(1);
        self.spans_dropped_no_active_run = self.spans_dropped_no_active_run.saturating_add(spans);
    }

    fn record_dropped_wrong_pid(&mut self, spans: u64) {
        self.batches_dropped_wrong_pid = self.batches_dropped_wrong_pid.saturating_add(1);
        self.spans_dropped_wrong_pid = self.spans_dropped_wrong_pid.saturating_add(spans);
    }
}

impl TargetLaneRegistry {
    fn clear_saved_diagnostics(&mut self) {
        self.saved_diagnostics = None;
    }

    pub(crate) fn restore_saved_diagnostics(&mut self, diagnostics: TargetIngestDiagnostics) {
        self.saved_diagnostics = Some(diagnostics);
    }

    fn record_dropped_no_active_run(&mut self, spans: u64) {
        self.clear_saved_diagnostics();
        self.totals.record_dropped_no_active_run(spans);
    }

    fn record_dropped_wrong_pid(&mut self, spans: u64) {
        self.clear_saved_diagnostics();
        self.totals.record_dropped_wrong_pid(spans);
    }

    fn record_reporter_stats(&mut self, stats: TargetReporterStats) {
        self.clear_saved_diagnostics();
        self.reporter_stats.insert(stats.pid, stats);
    }

    fn record_batch(&mut self, pid: u32, lane: &str, batch: TargetBatchCounters) {
        self.clear_saved_diagnostics();
        self.totals.record_batch(batch);
        self.lane_counters
            .entry((pid, lane.to_owned()))
            .or_default()
            .record_batch(batch);
    }

    pub(crate) fn diagnostics(&self) -> TargetIngestDiagnostics {
        if let Some(diagnostics) = &self.saved_diagnostics {
            return diagnostics.clone();
        }
        let mut lanes: Vec<TargetLaneDiagnostics> = self
            .lane_counters
            .iter()
            .map(|((pid, lane), counters)| TargetLaneDiagnostics {
                tid: self
                    .lane_tids
                    .get(&(*pid, lane.clone()))
                    .copied()
                    .unwrap_or(0),
                name: lane.clone(),
                spans_recorded: counters.spans_recorded,
                spans_with_origin: counters.spans_with_origin,
                spans_linked_origin: counters.spans_linked_origin,
                spans_unlinked_origin: counters.spans_unlinked_origin(),
                spans_origin_invalid_tid: counters.origin_links.invalid_tid,
                spans_origin_no_thread: counters.origin_links.no_thread,
                spans_origin_no_stack: counters.origin_links.no_stack,
                spans_origin_too_far: counters.origin_links.too_far,
                origin_linked_distance_min_ns: counters.origin_links.linked_distance.min_ns,
                origin_linked_distance_avg_ns: counters.origin_links.linked_distance.avg_ns(),
                origin_linked_distance_max_ns: counters.origin_links.linked_distance.max_ns,
                origin_too_far_distance_min_ns: counters.origin_links.too_far_distance.min_ns,
                origin_too_far_distance_avg_ns: counters.origin_links.too_far_distance.avg_ns(),
                origin_too_far_distance_max_ns: counters.origin_links.too_far_distance.max_ns,
                total_duration_ns: counters.total_duration_ns,
            })
            .collect();
        lanes.sort_by(|a, b| {
            b.total_duration_ns
                .cmp(&a.total_duration_ns)
                .then_with(|| b.spans_recorded.cmp(&a.spans_recorded))
                .then_with(|| a.tid.cmp(&b.tid))
                .then_with(|| a.name.cmp(&b.name))
        });
        TargetIngestDiagnostics {
            batches: self.totals.batches,
            batches_dropped_no_active_run: self.totals.batches_dropped_no_active_run,
            spans_dropped_no_active_run: self.totals.spans_dropped_no_active_run,
            batches_dropped_wrong_pid: self.totals.batches_dropped_wrong_pid,
            spans_dropped_wrong_pid: self.totals.spans_dropped_wrong_pid,
            batches_dropped_target_queue_full: self
                .reporter_stats
                .values()
                .map(|stats| stats.batches_dropped_queue_full)
                .sum(),
            spans_dropped_target_queue_full: self
                .reporter_stats
                .values()
                .map(|stats| stats.spans_dropped_queue_full)
                .sum(),
            batches_dropped_target_worker_disconnected: self
                .reporter_stats
                .values()
                .map(|stats| stats.batches_dropped_worker_disconnected)
                .sum(),
            spans_dropped_target_worker_disconnected: self
                .reporter_stats
                .values()
                .map(|stats| stats.spans_dropped_worker_disconnected)
                .sum(),
            spans_received: self.totals.spans_received,
            spans_recorded: self.totals.spans_recorded,
            spans_dropped_bad_duration: self.totals.spans_dropped_bad_duration,
            spans_with_origin: self.totals.spans_with_origin,
            spans_linked_origin: self.totals.spans_linked_origin,
            spans_unlinked_origin: self.totals.spans_unlinked_origin(),
            spans_origin_invalid_tid: self.totals.origin_links.invalid_tid,
            spans_origin_no_thread: self.totals.origin_links.no_thread,
            spans_origin_no_stack: self.totals.origin_links.no_stack,
            spans_origin_too_far: self.totals.origin_links.too_far,
            origin_stack_max_distance_ns: ORIGIN_STACK_MAX_DISTANCE_NS,
            origin_linked_distance_min_ns: self.totals.origin_links.linked_distance.min_ns,
            origin_linked_distance_avg_ns: self.totals.origin_links.linked_distance.avg_ns(),
            origin_linked_distance_max_ns: self.totals.origin_links.linked_distance.max_ns,
            origin_too_far_distance_min_ns: self.totals.origin_links.too_far_distance.min_ns,
            origin_too_far_distance_avg_ns: self.totals.origin_links.too_far_distance.avg_ns(),
            origin_too_far_distance_max_ns: self.totals.origin_links.too_far_distance.max_ns,
            total_duration_ns: self.totals.total_duration_ns,
            lanes,
        }
    }
}

impl SyntheticSymbolKey {
    fn display_name(&self) -> &str {
        match self {
            Self::Lane(name) | Self::Span(name) => name,
        }
    }
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
    fn symbol_addr(&self, key: SyntheticSymbolKey) -> u64 {
        let mut lanes = self.server.target_lanes().lock();
        if let Some(&addr) = lanes.symbol_addrs.get(&key) {
            return addr;
        }
        let index = lanes.symbol_addrs.len() as u64;
        let addr = SYNTH_BINARY_BASE_AVMA + index * SYNTH_SYMBOL_STRIDE;
        lanes.symbol_addrs.insert(key, addr);
        // Republish the synthetic binary with the full symbol list;
        // `BinaryRegistry::insert` replaces by base AVMA. New names are
        // rare (one per distinct kernel), so the rebuild is cold.
        let mut symbols: Vec<LiveSymbolOwned> = lanes
            .symbol_addrs
            .iter()
            .map(|(key, &addr)| LiveSymbolOwned {
                start_svma: addr - SYNTH_BINARY_BASE_AVMA,
                end_svma: addr - SYNTH_BINARY_BASE_AVMA + SYNTH_SYMBOL_STRIDE,
                name: key.display_name().as_bytes().to_vec(),
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
        if batch.spans.is_empty() {
            return;
        }
        let spans_received = batch.spans.len() as u64;
        // Only the active run's target may land spans on the timeline.
        let Some(active_pid) = self.server.active_target_pid() else {
            self.server
                .target_lanes()
                .lock()
                .record_dropped_no_active_run(spans_received);
            return;
        };
        if active_pid != batch.pid {
            self.server
                .target_lanes()
                .lock()
                .record_dropped_wrong_pid(spans_received);
            return;
        }
        let tid = self.lane_tid(batch.pid, &batch.lane);
        let lane_addr = self.symbol_addr(SyntheticSymbolKey::Lane(batch.lane.clone()));
        let mut events = Vec::new();
        for span in &batch.spans {
            if span.end_ns <= span.start_ns {
                continue;
            }
            let span_addr = self.symbol_addr(SyntheticSymbolKey::Span(span.name.clone()));
            events.push(SpanEvent {
                start_ns: span.start_ns,
                end_ns: span.end_ns,
                span_addr,
                origin: span.origin,
            });
        }
        events.sort_by_key(|event| (event.start_ns, event.end_ns));
        let recorded_spans = events.len() as u64;
        let dropped_bad_duration = spans_received.saturating_sub(recorded_spans);
        let spans_with_origin = events.iter().filter(|event| event.origin.is_some()).count() as u64;
        let mut total_duration_ns = 0u64;
        let mut linked_origins = 0u64;
        let mut origin_links = OriginLinkCounters::default();
        if !events.is_empty() {
            let mut aggregator = self.server.aggregator().write();
            for event in &events {
                total_duration_ns = total_duration_ns.saturating_add(event.end_ns - event.start_ns);
                let origin_tid = event
                    .origin
                    .map(|origin| origin.tid)
                    .filter(|&tid| tid < SYNTH_TID_BASE);
                let origin_stack = event.origin.and_then(|origin| {
                    if origin.tid >= SYNTH_TID_BASE {
                        origin_links.invalid_tid = origin_links.invalid_tid.saturating_add(1);
                        return None;
                    }
                    match aggregator.nearest_pet_stack_with_distance(
                        origin.tid,
                        origin.timestamp_ns,
                        ORIGIN_STACK_MAX_DISTANCE_NS,
                    ) {
                        Ok(nearest) => {
                            origin_links.linked_distance.record(nearest.distance_ns);
                            Some(nearest.stack)
                        }
                        Err(NearestPetStackError::NoThread) => {
                            origin_links.no_thread = origin_links.no_thread.saturating_add(1);
                            None
                        }
                        Err(NearestPetStackError::NoUserStack) => {
                            origin_links.no_stack = origin_links.no_stack.saturating_add(1);
                            None
                        }
                        Err(NearestPetStackError::TooFar {
                            distance_ns,
                            max_distance_ns: _,
                        }) => {
                            origin_links.too_far = origin_links.too_far.saturating_add(1);
                            origin_links.too_far_distance.record(distance_ns);
                            None
                        }
                    }
                });
                let mut stack =
                    Vec::with_capacity(2 + origin_stack.as_ref().map_or(0, |stack| stack.len()));
                stack.push(event.span_addr);
                stack.push(lane_addr);
                if let Some(origin_stack) = origin_stack.as_deref() {
                    linked_origins += 1;
                    stack.extend_from_slice(origin_stack);
                }
                let stack = stack.into_boxed_slice();
                aggregator.record_pet_sample(
                    tid,
                    event.start_ns,
                    &stack,
                    &[],
                    PmuSample {
                        cycles: 0,
                        instructions: 0,
                        l1d_misses: 0,
                        branch_mispreds: 0,
                    },
                );
                aggregator.record_interval(
                    tid,
                    event.start_ns,
                    event.end_ns,
                    IntervalKind::SyntheticSpan { stack, origin_tid },
                );
            }
        }
        self.server.target_lanes().lock().record_batch(
            batch.pid,
            &batch.lane,
            TargetBatchCounters {
                received: spans_received,
                recorded: recorded_spans,
                dropped_bad_duration,
                spans_with_origin,
                spans_linked_origin: linked_origins,
                origin_links,
                total_duration_ns,
            },
        );
        if !events.is_empty() {
            self.server.bump_revision();
        }
        tracing::debug!(
            pid = batch.pid,
            lane = %batch.lane,
            spans = batch.spans.len(),
            recorded_spans = events.len(),
            spans_with_origin,
            linked_origins,
            total_duration_ns,
            "target spans ingested"
        );
    }

    async fn reporter_stats(&self, stats: TargetReporterStats) {
        if self.server.active_target_pid() != Some(stats.pid) {
            return;
        }
        self.server
            .target_lanes()
            .lock()
            .record_reporter_stats(stats);
    }

    async fn should_report(&self, pid: u32) -> bool {
        self.server.active_target_pid() == Some(pid)
    }
}

#[cfg(test)]
mod tests {
    use stax_live_proto::{
        FlameNode, LiveFilter, Profiler as _, RunConfig, StopReason, TargetIngest as _, TargetSpan,
        TargetSpanBatch, TopSort, ViewParams,
    };

    use super::*;

    /// The whole latch path server-side: pid gate, synthetic thread
    /// (named lane), synthetic symbol resolution, and duration-weighted
    /// synthetic intervals in the EXISTING aggregator — no separate
    /// span-specific view.
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
                    span("kernel_a", 1_000_000, 4_000_000), // 3ms -> 1 span
                    span("kernel_b", 4_000_000, 4_500_000), // 0.5ms -> 1 span
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
        assert_eq!(aggregator.last_event_ns(), Some(4_500_000));
        drop(aggregator);

        // Lane and span names resolve as symbols through the ordinary registry.
        let binaries = server.binaries().read();
        let lane = binaries
            .lookup_symbol(SYNTH_BINARY_BASE_AVMA)
            .expect("lane resolves");
        assert_eq!(lane.function_name, "GPU test");
        let a = binaries
            .lookup_symbol(SYNTH_BINARY_BASE_AVMA + SYNTH_SYMBOL_STRIDE)
            .expect("kernel_a resolves");
        assert_eq!(a.function_name, "kernel_a");
        let b = binaries
            .lookup_symbol(SYNTH_BINARY_BASE_AVMA + 2 * SYNTH_SYMBOL_STRIDE)
            .expect("kernel_b resolves");
        assert_eq!(b.function_name, "kernel_b");
        assert!(
            binaries
                .lookup_symbol(SYNTH_BINARY_BASE_AVMA + 3 * SYNTH_SYMBOL_STRIDE)
                .is_none(),
            "dropped wrong-pid span must not register symbols"
        );
        drop(binaries);

        let profiler = server.profiler();
        let top = profiler
            .top(10, TopSort::BySelf, view_params(Some(tid)))
            .await;
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].function_name.as_deref(), Some("kernel_a"));
        assert_eq!(top[0].self_on_cpu_ns, 3_000_000);
        assert_eq!(top[0].self_target_ns, 3_000_000);
        assert_eq!(top[0].self_pet_samples, 1);
        assert_eq!(top[0].self_target_spans, 1);
        assert_eq!(top[1].function_name.as_deref(), Some("kernel_b"));
        assert_eq!(top[1].self_on_cpu_ns, 500_000);
        assert_eq!(top[1].self_target_ns, 500_000);
        assert_eq!(top[1].self_pet_samples, 1);
        assert_eq!(top[1].self_target_spans, 1);

        let flame = profiler.flamegraph(view_params(Some(tid))).await;
        assert_eq!(flame.total_on_cpu_ns, 3_500_000);
        assert_eq!(flame.total_target_ns, 3_500_000);
        assert_eq!(flame.total_target_spans, 2);
        assert_eq!(flame.root.pet_samples, 2);
        assert_eq!(flame.root.target_spans, 2);
        assert_eq!(flame.root.children.len(), 1);
        let lane = &flame.root.children[0];
        assert_eq!(flame_node_name(lane, &flame.strings), Some("GPU test"));
        assert_eq!(lane.on_cpu_ns, 3_500_000);
        assert_eq!(lane.target_ns, 3_500_000);
        assert_eq!(lane.pet_samples, 2);
        assert_eq!(lane.target_spans, 2);
        assert_eq!(lane.children.len(), 2);
        assert_eq!(
            flame_node_name(&lane.children[0], &flame.strings),
            Some("kernel_a")
        );
        assert_eq!(lane.children[0].on_cpu_ns, 3_000_000);
        assert_eq!(lane.children[0].target_ns, 3_000_000);
        assert_eq!(lane.children[0].target_spans, 1);
        assert_eq!(
            flame_node_name(&lane.children[1], &flame.strings),
            Some("kernel_b")
        );
        assert_eq!(lane.children[1].on_cpu_ns, 500_000);
        assert_eq!(lane.children[1].target_ns, 500_000);
        assert_eq!(lane.children[1].target_spans, 1);

        let threads = profiler.threads().await;
        let thread = threads
            .threads
            .iter()
            .find(|thread| thread.tid == tid)
            .expect("synthetic thread row");
        assert_eq!(thread.name.as_deref(), Some("GPU test"));
        assert_eq!(thread.on_cpu_ns, 3_500_000);
        assert_eq!(thread.target_ns, 3_500_000);
        assert_eq!(thread.pet_samples, 2);
        assert_eq!(thread.target_spans, 2);

        let target_spans = profiler
            .target_spans("r".to_owned(), view_params(Some(tid)))
            .await;
        assert_eq!(target_spans.total_spans, 2);
        assert_eq!(target_spans.total_duration_ns, 3_500_000);
        assert_eq!(target_spans.groups.len(), 2);
        assert_eq!(
            target_spans.groups[0]
                .span_name
                .and_then(|index| target_spans.strings.get(index as usize).map(String::as_str)),
            Some("kernel_a")
        );
        assert_eq!(target_spans.groups[0].count, 1);
        assert_eq!(target_spans.groups[0].total_duration_ns, 3_000_000);
        assert_eq!(target_spans.groups[0].max_duration_ns, 3_000_000);
        assert_eq!(
            target_spans.groups[1]
                .span_name
                .and_then(|index| target_spans.strings.get(index as usize).map(String::as_str)),
            Some("kernel_b")
        );
        assert_eq!(target_spans.groups[1].count, 1);
        assert_eq!(target_spans.groups[1].total_duration_ns, 500_000);
        assert_eq!(target_spans.entries.len(), 2);
        assert_eq!(
            target_spans.entries[0]
                .span_name
                .and_then(|index| target_spans.strings.get(index as usize).map(String::as_str)),
            Some("kernel_b")
        );
        assert_eq!(target_spans.entries[0].duration_ns, 500_000);
        assert_eq!(target_spans.entries[0].origin_tid, None);
        assert_eq!(
            target_spans.entries[1]
                .lane_name
                .and_then(|index| target_spans.strings.get(index as usize).map(String::as_str)),
            Some("GPU test")
        );
        assert_eq!(target_spans.entries[1].duration_ns, 3_000_000);

        let diagnostics = server.target_lanes().lock().diagnostics();
        assert_eq!(diagnostics.batches, 1);
        assert_eq!(diagnostics.batches_dropped_no_active_run, 1);
        assert_eq!(diagnostics.spans_dropped_no_active_run, 1);
        assert_eq!(diagnostics.batches_dropped_wrong_pid, 1);
        assert_eq!(diagnostics.spans_dropped_wrong_pid, 1);
        assert_eq!(diagnostics.spans_received, 2);
        assert_eq!(diagnostics.spans_recorded, 2);
        assert_eq!(diagnostics.spans_dropped_bad_duration, 0);
        assert_eq!(diagnostics.total_duration_ns, 3_500_000);
        assert_eq!(diagnostics.lanes.len(), 1);
        assert_eq!(diagnostics.lanes[0].tid, tid);
        assert_eq!(diagnostics.lanes[0].name, "GPU test");
    }

    #[tokio::test]
    async fn reporter_stats_land_only_for_active_target() {
        let server = ServerState::new_for_tests();
        let service = TargetIngestService::new(server.clone());
        server.set_active_run_for_tests(42);

        service
            .reporter_stats(TargetReporterStats {
                pid: 41,
                batches_dropped_queue_full: 9,
                spans_dropped_queue_full: 90,
                batches_dropped_worker_disconnected: 8,
                spans_dropped_worker_disconnected: 80,
            })
            .await;
        service
            .reporter_stats(TargetReporterStats {
                pid: 42,
                batches_dropped_queue_full: 2,
                spans_dropped_queue_full: 7,
                batches_dropped_worker_disconnected: 1,
                spans_dropped_worker_disconnected: 3,
            })
            .await;

        let diagnostics = server.target_lanes().lock().diagnostics();
        assert_eq!(diagnostics.batches_dropped_target_queue_full, 2);
        assert_eq!(diagnostics.spans_dropped_target_queue_full, 7);
        assert_eq!(diagnostics.batches_dropped_target_worker_disconnected, 1);
        assert_eq!(diagnostics.spans_dropped_target_worker_disconnected, 3);
    }

    #[tokio::test]
    async fn ingest_links_spans_to_origin_cpu_stack() {
        let server = ServerState::new_for_tests();
        let service = TargetIngestService::new(server.clone());
        server.set_active_run_for_tests(42);

        const CPU_TID: u32 = 1234;
        const CPU_BASE: u64 = 0x1000_0000;
        const CPU_PARENT: u64 = CPU_BASE + 0x10;
        const CPU_LEAF: u64 = CPU_BASE + 0x20;

        server.binaries().write().insert(LoadedBinary {
            path: "/tmp/stax-origin-test".to_owned(),
            base_avma: CPU_BASE,
            avma_end: CPU_BASE + 0x40,
            text_svma: 0,
            arch: None,
            is_executable: true,
            symbols: vec![
                LiveSymbolOwned {
                    start_svma: CPU_PARENT - CPU_BASE,
                    end_svma: CPU_PARENT - CPU_BASE + 0x10,
                    name: b"cpu_parent".to_vec(),
                },
                LiveSymbolOwned {
                    start_svma: CPU_LEAF - CPU_BASE,
                    end_svma: CPU_LEAF - CPU_BASE + 0x10,
                    name: b"cpu_leaf".to_vec(),
                },
            ],
            text_bytes: None,
        });
        server.aggregator().write().record_pet_sample(
            CPU_TID,
            950_000,
            &[CPU_LEAF, CPU_PARENT],
            &[],
            PmuSample::default(),
        );

        service
            .ingest(TargetSpanBatch {
                pid: 42,
                lane: "GPU test".to_owned(),
                spans: vec![span_with_origin(
                    "kernel_a",
                    1_000_000,
                    4_000_000,
                    TargetSpanOrigin {
                        tid: CPU_TID,
                        timestamp_ns: 951_000,
                    },
                )],
            })
            .await;

        let profiler = server.profiler();
        let cpu_top = profiler
            .top(10, TopSort::BySelf, view_params(Some(CPU_TID)))
            .await;
        assert_eq!(cpu_top[0].function_name.as_deref(), Some("kernel_a"));
        assert_eq!(cpu_top[0].self_on_cpu_ns, 3_000_000);
        assert_eq!(cpu_top[0].self_target_ns, 3_000_000);
        assert_eq!(cpu_top[0].self_pet_samples, 1);
        assert_eq!(cpu_top[0].self_target_spans, 1);

        let flame = profiler.flamegraph(view_params(Some(CPU_TID))).await;
        assert_eq!(flame.total_on_cpu_ns, 3_000_000);
        assert_eq!(flame.total_target_ns, 3_000_000);
        assert_eq!(flame.total_target_spans, 1);
        assert_eq!(flame.root.children.len(), 1);
        let cpu_parent = &flame.root.children[0];
        assert_eq!(
            flame_node_name(cpu_parent, &flame.strings),
            Some("cpu_parent")
        );
        assert_eq!(cpu_parent.target_ns, 3_000_000);
        assert_eq!(cpu_parent.target_spans, 1);
        let cpu_leaf = &cpu_parent.children[0];
        assert_eq!(flame_node_name(cpu_leaf, &flame.strings), Some("cpu_leaf"));
        assert_eq!(cpu_leaf.target_ns, 3_000_000);
        let lane = &cpu_leaf.children[0];
        assert_eq!(flame_node_name(lane, &flame.strings), Some("GPU test"));
        assert_eq!(lane.target_spans, 1);
        let span = &lane.children[0];
        assert_eq!(flame_node_name(span, &flame.strings), Some("kernel_a"));
        assert_eq!(span.on_cpu_ns, 3_000_000);
        assert_eq!(span.target_ns, 3_000_000);

        let target_spans = profiler
            .target_spans("r".to_owned(), view_params(Some(CPU_TID)))
            .await;
        assert_eq!(target_spans.groups.len(), 1);
        assert_eq!(target_spans.groups[0].origin_tid, Some(CPU_TID));
        assert_eq!(target_spans.groups[0].origin_address, Some(CPU_LEAF));
        assert_eq!(target_spans.entries.len(), 1);
        assert_eq!(target_spans.entries[0].origin_tid, Some(CPU_TID));
        assert_eq!(target_spans.entries[0].origin_address, Some(CPU_LEAF));

        let threads = profiler.threads().await;
        let cpu_thread = threads
            .threads
            .iter()
            .find(|thread| thread.tid == CPU_TID)
            .expect("origin CPU thread row");
        assert_eq!(cpu_thread.on_cpu_ns, 3_000_000);
        assert_eq!(cpu_thread.target_ns, 3_000_000);
        assert_eq!(cpu_thread.pet_samples, 1);
        assert_eq!(cpu_thread.target_spans, 1);
        let lane_thread = threads
            .threads
            .iter()
            .find(|thread| thread.tid == SYNTH_TID_BASE)
            .expect("synthetic lane row");
        assert_eq!(lane_thread.on_cpu_ns, 3_000_000);
        assert_eq!(lane_thread.target_ns, 3_000_000);
        assert_eq!(lane_thread.pet_samples, 1);
        assert_eq!(lane_thread.target_spans, 1);

        let diagnostics = server.target_lanes().lock().diagnostics();
        assert_eq!(diagnostics.spans_with_origin, 1);
        assert_eq!(diagnostics.spans_linked_origin, 1);
        assert_eq!(diagnostics.spans_unlinked_origin, 0);
        assert_eq!(diagnostics.origin_linked_distance_min_ns, 1_000);
        assert_eq!(diagnostics.origin_linked_distance_avg_ns, 1_000);
        assert_eq!(diagnostics.origin_linked_distance_max_ns, 1_000);
    }

    #[tokio::test]
    async fn ingest_diagnoses_unlinked_origin_reasons() {
        let server = ServerState::new_for_tests();
        let service = TargetIngestService::new(server.clone());
        server.set_active_run_for_tests(42);

        const NO_STACK_TID: u32 = 2200;
        const TOO_FAR_TID: u32 = 2201;
        const TOO_FAR_SAMPLE_NS: u64 = 1_000_000;
        const TOO_FAR_ORIGIN_NS: u64 = TOO_FAR_SAMPLE_NS + ORIGIN_STACK_MAX_DISTANCE_NS + 7;

        {
            let mut aggregator = server.aggregator().write();
            aggregator.record_pet_sample(NO_STACK_TID, 1_000_000, &[], &[], PmuSample::default());
            aggregator.record_pet_sample(
                TOO_FAR_TID,
                TOO_FAR_SAMPLE_NS,
                &[0x2000_0000],
                &[],
                PmuSample::default(),
            );
        }

        service
            .ingest(TargetSpanBatch {
                pid: 42,
                lane: "GPU bad origins".to_owned(),
                spans: vec![
                    span_with_origin(
                        "synthetic_origin_tid",
                        1_000_000,
                        2_000_000,
                        TargetSpanOrigin {
                            tid: SYNTH_TID_BASE,
                            timestamp_ns: 1_000_000,
                        },
                    ),
                    span_with_origin(
                        "missing_origin_thread",
                        2_000_000,
                        3_000_000,
                        TargetSpanOrigin {
                            tid: 990_000,
                            timestamp_ns: 2_000_000,
                        },
                    ),
                    span_with_origin(
                        "empty_origin_stack",
                        3_000_000,
                        4_000_000,
                        TargetSpanOrigin {
                            tid: NO_STACK_TID,
                            timestamp_ns: 1_000_000,
                        },
                    ),
                    span_with_origin(
                        "stale_origin_stack",
                        4_000_000,
                        5_000_000,
                        TargetSpanOrigin {
                            tid: TOO_FAR_TID,
                            timestamp_ns: TOO_FAR_ORIGIN_NS,
                        },
                    ),
                ],
            })
            .await;

        let diagnostics = server.target_lanes().lock().diagnostics();
        assert_eq!(diagnostics.spans_with_origin, 4);
        assert_eq!(diagnostics.spans_linked_origin, 0);
        assert_eq!(diagnostics.spans_unlinked_origin, 4);
        assert_eq!(diagnostics.spans_origin_invalid_tid, 1);
        assert_eq!(diagnostics.spans_origin_no_thread, 1);
        assert_eq!(diagnostics.spans_origin_no_stack, 1);
        assert_eq!(diagnostics.spans_origin_too_far, 1);
        assert_eq!(diagnostics.origin_stack_max_distance_ns, 50_000_000);
        assert_eq!(diagnostics.origin_too_far_distance_min_ns, 50_000_007);
        assert_eq!(diagnostics.origin_too_far_distance_avg_ns, 50_000_007);
        assert_eq!(diagnostics.origin_too_far_distance_max_ns, 50_000_007);
        assert_eq!(diagnostics.lanes.len(), 1);
        let lane = &diagnostics.lanes[0];
        assert_eq!(lane.spans_origin_invalid_tid, 1);
        assert_eq!(lane.spans_origin_no_thread, 1);
        assert_eq!(lane.spans_origin_no_stack, 1);
        assert_eq!(lane.spans_origin_too_far, 1);
        assert_eq!(lane.origin_too_far_distance_avg_ns, 50_000_007);
    }

    #[tokio::test]
    async fn new_run_resets_target_lane_registry_and_republishes_symbols() {
        let server = ServerState::new_for_tests();
        let service = TargetIngestService::new(server.clone());

        let run1 = server
            .begin_run(test_run_config("first"))
            .expect("begin first run");
        server.apply_target_attached_in_process(run1, 42, 0);
        service
            .ingest(TargetSpanBatch {
                pid: 42,
                lane: "GPU test".to_owned(),
                spans: vec![span("kernel_a", 1_000_000, 2_000_000)],
            })
            .await;
        assert!(
            server
                .binaries()
                .read()
                .lookup_symbol(SYNTH_BINARY_BASE_AVMA + SYNTH_SYMBOL_STRIDE)
                .is_some(),
            "first run publishes synthetic span symbols"
        );
        server.finalize_run(run1, StopReason::UserStop);

        let run2 = server
            .begin_run(test_run_config("second"))
            .expect("begin second run");
        server.apply_target_attached_in_process(run2, 42, 0);
        service
            .ingest(TargetSpanBatch {
                pid: 42,
                lane: "GPU test".to_owned(),
                spans: vec![span("kernel_a", 3_000_000, 5_000_000)],
            })
            .await;

        let top = server
            .profiler()
            .top(10, TopSort::BySelf, view_params(Some(SYNTH_TID_BASE)))
            .await;
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].function_name.as_deref(), Some("kernel_a"));
        assert_eq!(top[0].self_target_ns, 2_000_000);
        let diagnostics = server.target_lanes().lock().diagnostics();
        assert_eq!(diagnostics.batches, 1);
        assert_eq!(diagnostics.spans_recorded, 1);
        assert_eq!(diagnostics.total_duration_ns, 2_000_000);
    }

    fn span(name: &str, start_ns: u64, end_ns: u64) -> TargetSpan {
        TargetSpan::new(name, start_ns, end_ns)
    }

    fn span_with_origin(
        name: &str,
        start_ns: u64,
        end_ns: u64,
        origin: TargetSpanOrigin,
    ) -> TargetSpan {
        TargetSpan::new(name, start_ns, end_ns).with_origin(origin)
    }

    fn test_run_config(label: &str) -> RunConfig {
        RunConfig {
            label: label.to_owned(),
            frequency_hz: 900,
            dwarf_unwind: false,
        }
    }

    fn view_params(tid: Option<u32>) -> ViewParams {
        ViewParams {
            tid,
            filter: LiveFilter {
                time_range: None,
                exclude_symbols: Vec::new(),
            },
        }
    }

    fn flame_node_name<'a>(node: &FlameNode, strings: &'a [String]) -> Option<&'a str> {
        node.function_name
            .and_then(|index| strings.get(index as usize))
            .map(String::as_str)
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
        let acceptor =
            vox::transport::local::LocalLinkAcceptor::bind(socket.to_string_lossy().into_owned())
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
        drop(aggregator);
        let profiler = server.profiler();
        let top = profiler
            .top(10, TopSort::BySelf, view_params(Some(SYNTH_TID_BASE)))
            .await;
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].function_name.as_deref(), Some("wire_kernel"));
        assert_eq!(top[0].self_on_cpu_ns, 6_000_000);
        assert_eq!(top[0].self_target_ns, 6_000_000);
        assert_eq!(top[0].self_pet_samples, 1);
        assert_eq!(top[0].self_target_spans, 1);
        let _ = std::fs::remove_file(&socket);
    }
}
