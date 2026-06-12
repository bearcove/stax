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

use stax_live::{IntervalKind, LiveSymbolOwned, LoadedBinary, PmuSample};
use stax_live_proto::{TargetIngest, TargetSpanBatch, TargetSpanOrigin};

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
        let mut total_duration_ns = 0u64;
        if !events.is_empty() {
            let mut aggregator = self.server.aggregator().write();
            let mut linked_origins = 0u64;
            for event in &events {
                total_duration_ns = total_duration_ns.saturating_add(event.end_ns - event.start_ns);
                let origin_tid = event
                    .origin
                    .map(|origin| origin.tid)
                    .filter(|&tid| tid < SYNTH_TID_BASE);
                let origin_stack = event.origin.and_then(|origin| {
                    if origin.tid >= SYNTH_TID_BASE {
                        return None;
                    }
                    aggregator.nearest_pet_stack(
                        origin.tid,
                        origin.timestamp_ns,
                        ORIGIN_STACK_MAX_DISTANCE_NS,
                    )
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
            tracing::debug!(
                pid = batch.pid,
                lane = %batch.lane,
                linked_origins,
                "target span CPU origins linked"
            );
        }
        if !events.is_empty() {
            self.server.bump_revision();
        }
        tracing::debug!(
            pid = batch.pid,
            lane = %batch.lane,
            spans = batch.spans.len(),
            recorded_spans = events.len(),
            total_duration_ns,
            "target spans ingested"
        );
    }

    async fn should_report(&self, pid: u32) -> bool {
        self.server.active_target_pid() == Some(pid)
    }
}

#[cfg(test)]
mod tests {
    use stax_live_proto::{
        FlameNode, LiveFilter, Profiler as _, TargetIngest as _, TargetSpan, TargetSpanBatch,
        TopSort, ViewParams,
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
        assert_eq!(top[0].self_pet_samples, 1);
        assert_eq!(top[1].function_name.as_deref(), Some("kernel_b"));
        assert_eq!(top[1].self_on_cpu_ns, 500_000);
        assert_eq!(top[1].self_pet_samples, 1);

        let flame = profiler.flamegraph(view_params(Some(tid))).await;
        assert_eq!(flame.total_on_cpu_ns, 3_500_000);
        assert_eq!(flame.root.pet_samples, 2);
        assert_eq!(flame.root.children.len(), 1);
        let lane = &flame.root.children[0];
        assert_eq!(flame_node_name(lane, &flame.strings), Some("GPU test"));
        assert_eq!(lane.on_cpu_ns, 3_500_000);
        assert_eq!(lane.pet_samples, 2);
        assert_eq!(lane.children.len(), 2);
        assert_eq!(
            flame_node_name(&lane.children[0], &flame.strings),
            Some("kernel_a")
        );
        assert_eq!(lane.children[0].on_cpu_ns, 3_000_000);
        assert_eq!(
            flame_node_name(&lane.children[1], &flame.strings),
            Some("kernel_b")
        );
        assert_eq!(lane.children[1].on_cpu_ns, 500_000);

        let threads = profiler.threads().await;
        let thread = threads
            .threads
            .iter()
            .find(|thread| thread.tid == tid)
            .expect("synthetic thread row");
        assert_eq!(thread.name.as_deref(), Some("GPU test"));
        assert_eq!(thread.on_cpu_ns, 3_500_000);
        assert_eq!(thread.pet_samples, 2);
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
        assert_eq!(cpu_top[0].self_pet_samples, 1);

        let flame = profiler.flamegraph(view_params(Some(CPU_TID))).await;
        assert_eq!(flame.total_on_cpu_ns, 3_000_000);
        assert_eq!(flame.root.children.len(), 1);
        let cpu_parent = &flame.root.children[0];
        assert_eq!(
            flame_node_name(cpu_parent, &flame.strings),
            Some("cpu_parent")
        );
        let cpu_leaf = &cpu_parent.children[0];
        assert_eq!(flame_node_name(cpu_leaf, &flame.strings), Some("cpu_leaf"));
        let lane = &cpu_leaf.children[0];
        assert_eq!(flame_node_name(lane, &flame.strings), Some("GPU test"));
        let span = &lane.children[0];
        assert_eq!(flame_node_name(span, &flame.strings), Some("kernel_a"));
        assert_eq!(span.on_cpu_ns, 3_000_000);

        let threads = profiler.threads().await;
        let cpu_thread = threads
            .threads
            .iter()
            .find(|thread| thread.tid == CPU_TID)
            .expect("origin CPU thread row");
        assert_eq!(cpu_thread.on_cpu_ns, 3_000_000);
        assert_eq!(cpu_thread.pet_samples, 1);
        let lane_thread = threads
            .threads
            .iter()
            .find(|thread| thread.tid == SYNTH_TID_BASE)
            .expect("synthetic lane row");
        assert_eq!(lane_thread.on_cpu_ns, 3_000_000);
        assert_eq!(lane_thread.pet_samples, 1);
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
        assert_eq!(top[0].self_pet_samples, 1);
        let _ = std::fs::remove_file(&socket);
    }
}
