//! End-to-end smoke for the sched_waking attribution chain.
//!
//! Run a privileged staxd (e.g. `sudo .../staxd --foreground --socket
//! /tmp/staxd-test.sock`) in another terminal. Then:
//!
//!   cargo run -p stax-linux-capture --example waking_smoke -- \
//!       /tmp/staxd-test.sock 4
//!
//! The example spawns a small child that does many short sleeps
//! (timer wakeups), drives `record_via_daemon` against the broker for
//! N seconds, and prints sample / wakeup / off-CPU counts plus how
//! many off-CPU intervals got a non-None `waker_tid`. With the broker
//! brokering sched_waking, the attributed count should be > 0.

#![cfg(target_os = "linux")]

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use stax_linux_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, RecordOptions, SampleEvent, SampleSink, ThreadNameEvent,
    WakeupEvent,
};
use stax_mac_capture::sample_sink::{CpuIntervalEvent, CpuIntervalKind};

#[derive(Default)]
struct WakingSums {
    samples: u64,
    wakeups: u64,
    on_cpu_intervals: u64,
    off_cpu_intervals: u64,
    off_cpu_with_waker: u64,
}

impl SampleSink for WakingSums {
    fn on_sample(&mut self, _ev: SampleEvent<'_>) {
        self.samples += 1;
    }
    fn on_binary_loaded(&mut self, _ev: BinaryLoadedEvent<'_>) {}
    fn on_binary_unloaded(&mut self, _ev: BinaryUnloadedEvent<'_>) {}
    fn on_thread_name(&mut self, _ev: ThreadNameEvent<'_>) {}
    fn on_wakeup(&mut self, _ev: WakeupEvent<'_>) {
        self.wakeups += 1;
    }
    fn on_cpu_interval(&mut self, ev: CpuIntervalEvent<'_>) {
        match &ev.kind {
            CpuIntervalKind::OnCpu => self.on_cpu_intervals += 1,
            CpuIntervalKind::OffCpu { waker_tid, .. } => {
                self.off_cpu_intervals += 1;
                if waker_tid.is_some() {
                    self.off_cpu_with_waker += 1;
                }
            }
        }
    }
}

fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut argv = std::env::args().skip(1);
    let socket = argv
        .next()
        .unwrap_or_else(|| "/tmp/staxd-test.sock".to_string());
    let secs: u64 = argv.next().and_then(|s| s.parse().ok()).unwrap_or(4);

    // A child that's mostly off-CPU on a short timer: lots of wakeups
    // for our session to attribute.
    let mut child = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!(
            "i=0; while [ $i -lt {n} ]; do sleep 0.01; i=$((i+1)); done",
            n = secs * 90 // ~90 sleeps/sec -> ~360 wakeups over 4s
        ))
        .spawn()?;
    let pid = child.id();
    println!("spawned bash pid {pid} for ~{secs}s of timer wakeups");

    let opts = RecordOptions {
        pid,
        frequency_hz: 999,
        duration: Some(Duration::from_secs(secs)),
        kernel_stacks: true,
    };
    let stop = AtomicBool::new(false);
    let mut sink = WakingSums::default();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let summary = rt.block_on(stax_linux_capture::record_via_daemon(
        &socket, &opts, &mut sink, &stop,
    ))?;

    let _ = child.kill();
    let _ = child.wait();

    println!(
        "samples={} (lost {}) elapsed={}ms",
        summary.samples,
        summary.lost_records,
        summary.session_ns / 1_000_000
    );
    println!(
        "wakeups={} on_cpu_intervals={} off_cpu_intervals={} off_cpu_with_waker={}",
        sink.wakeups, sink.on_cpu_intervals, sink.off_cpu_intervals, sink.off_cpu_with_waker
    );

    if sink.off_cpu_intervals > 0 {
        let pct = (sink.off_cpu_with_waker * 100) / sink.off_cpu_intervals;
        println!("waker attribution rate: {pct}%");
    }
    Ok(())
}
