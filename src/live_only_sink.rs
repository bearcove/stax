//! OS-neutral bridge from the sync `SampleSink` (what a capture backend
//! drives — kperf on macOS, perf_event_open on Linux) to the async
//! `LiveSink` the server-ingest path consumes.
//!
//! This lived in `cmd_record_mac` but is pure glue over two
//! platform-neutral traits, so it moved here to be shared by the macOS
//! daemon path, `cmd_record_linux`, and `stax-shade` on both.

use crate::live_sink::{
    BinaryLoadedEvent as LiveBinaryLoadedEvent, BinaryUnloadedEvent as LiveBinaryUnloadedEvent,
    LiveSink, LiveSymbol, SampleEvent as LiveSampleEvent, TargetAttached,
    ThreadName as LiveThreadName, WakeupEvent as LiveWakeupEvent,
};
use stax_mac_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, JitdumpEvent, SampleEvent, SampleSink, ThreadNameEvent,
    WakeupEvent,
};

pub struct LiveOnlySink {
    live_sink: Option<Box<dyn LiveSink>>,
}

impl LiveOnlySink {
    pub fn new(live_sink: Option<Box<dyn LiveSink>>) -> Self {
        Self { live_sink }
    }

    pub fn notify_target_attached(&self, pid: u32) {
        if let Some(live) = self.live_sink.as_ref() {
            futures::executor::block_on(
                live.on_target_attached(&TargetAttached { pid, task_port: 0 }),
            );
        }
    }

    /// A `Fn() -> bool` reflecting the live sink's out-of-band stop
    /// signal (server closed the ingest channel, etc.). Always-false
    /// when the sink exposes no stop flag, so the `should_stop` pattern
    /// in the `record_*` paths stays symmetric.
    pub fn live_sink_stop_flag(&self) -> impl Fn() -> bool + Send + Sync + 'static {
        let flag = self.live_sink.as_ref().and_then(|s| s.stop_flag());
        move || {
            flag.as_ref()
                .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(false)
        }
    }
}

pub fn notify_target_attached(sink: &LiveOnlySink, pid: u32) {
    sink.notify_target_attached(pid);
}

/// Drive an async `LiveSink` callback to completion independently of
/// tokio (safe from inside an already-async context). Hot-path
/// callbacks are contractually non-yielding (push to an mpsc and
/// return); slow paths may yield and we just block until they settle.
fn block_sink<F: std::future::Future<Output = ()>>(fut: F) {
    futures::executor::block_on(fut);
}

impl SampleSink for LiveOnlySink {
    fn on_sample(&mut self, ev: SampleEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        block_sink(sink.on_sample(&LiveSampleEvent {
            timestamp: ev.timestamp_ns,
            pid: ev.pid,
            tid: ev.tid,
            cpu: u32::MAX,
            kernel_backtrace: ev.kernel_backtrace,
            user_backtrace: ev.backtrace,
            cycles: ev.cycles,
            instructions: ev.instructions,
            l1d_misses: ev.l1d_misses,
            branch_mispreds: ev.branch_mispreds,
        }));
    }

    fn on_cpu_interval(&mut self, ev: stax_mac_capture::sample_sink::CpuIntervalEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        match ev.kind {
            stax_mac_capture::sample_sink::CpuIntervalKind::OnCpu => {
                block_sink(sink.on_cpu_interval(&crate::live_sink::CpuIntervalEvent {
                    pid: ev.pid,
                    tid: ev.tid,
                    start_ns: ev.start_ns,
                    end_ns: ev.end_ns,
                    kind: crate::live_sink::CpuIntervalKind::OnCpu,
                }));
            }
            stax_mac_capture::sample_sink::CpuIntervalKind::OffCpu {
                stack,
                waker_tid,
                waker_user_stack,
            } => {
                block_sink(sink.on_cpu_interval(&crate::live_sink::CpuIntervalEvent {
                    pid: ev.pid,
                    tid: ev.tid,
                    start_ns: ev.start_ns,
                    end_ns: ev.end_ns,
                    kind: crate::live_sink::CpuIntervalKind::OffCpu {
                        stack,
                        waker_tid,
                        waker_user_stack,
                    },
                }));
            }
        }
    }

    fn on_binary_loaded(&mut self, ev: BinaryLoadedEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        let live_symbols: Vec<LiveSymbol<'_>> = ev
            .symbols
            .iter()
            .map(|s| LiveSymbol {
                start_svma: s.start_svma,
                end_svma: s.end_svma,
                name: &s.name,
            })
            .collect();
        block_sink(sink.on_binary_loaded(&LiveBinaryLoadedEvent {
            path: ev.path,
            base_avma: ev.base_avma,
            vmsize: ev.vmsize,
            text_svma: ev.text_svma,
            arch: ev.arch,
            is_executable: ev.is_executable,
            symbols: &live_symbols,
            text_bytes: ev.text_bytes,
        }));
    }

    fn on_binary_unloaded(&mut self, ev: BinaryUnloadedEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        block_sink(sink.on_binary_unloaded(&LiveBinaryUnloadedEvent {
            path: ev.path,
            base_avma: ev.base_avma,
        }));
    }

    fn on_thread_name(&mut self, ev: ThreadNameEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        block_sink(sink.on_thread_name(&LiveThreadName {
            pid: ev.pid,
            tid: ev.tid,
            name: ev.name,
        }));
    }

    fn on_jitdump(&mut self, _ev: JitdumpEvent<'_>) {}

    fn on_wakeup(&mut self, ev: WakeupEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        block_sink(sink.on_wakeup(&LiveWakeupEvent {
            timestamp: ev.timestamp_ns,
            pid: ev.pid,
            waker_tid: ev.waker_tid,
            wakee_tid: ev.wakee_tid,
            waker_user_stack: ev.waker_user_stack,
            waker_kernel_stack: ev.waker_kernel_stack,
        }));
    }

    // `LiveSink::on_macho_byte_source` only exists on macOS (dyld
    // shared cache). The Linux capture backend never emits this event,
    // so the trait's default no-op is used there.
    #[cfg(target_os = "macos")]
    fn on_macho_byte_source(
        &mut self,
        source: std::sync::Arc<dyn stax_mac_capture::MachOByteSource>,
    ) {
        if let Some(sink) = self.live_sink.as_ref() {
            block_sink(sink.on_macho_byte_source(source));
        }
    }
}
