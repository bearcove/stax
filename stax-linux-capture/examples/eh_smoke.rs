//! End-to-end smoke for `.eh_frame` userspace unwinding.
//!
//! Builds nothing on its own — give it the path of a
//! `-fomit-frame-pointer` binary as the only argument:
//!
//!   cargo run -p stax-linux-capture --example eh_smoke -- /tmp/eh_demo
//!
//! It launches the child, captures it twice (first plain, then with
//! `RecordOptions::dwarf_unwind = true`), and prints the observed
//! max / mean user-stack depth side-by-side. With the unwinder
//! engaged, depths should jump for any image that lacks frame
//! pointers (libc, OpenSSL, Rust release builds, …).

#![cfg(target_os = "linux")]

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use stax_linux_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, RecordOptions, SampleEvent, SampleSink, ThreadNameEvent,
    WakeupEvent,
};
use stax_mac_capture::sample_sink::CpuIntervalEvent;

#[derive(Default)]
struct DepthSums {
    samples: u64,
    sum_depth: u64,
    max_depth: u64,
}

impl SampleSink for DepthSums {
    fn on_sample(&mut self, ev: SampleEvent<'_>) {
        let d = ev.backtrace.len() as u64;
        self.samples += 1;
        self.sum_depth += d;
        if d > self.max_depth {
            self.max_depth = d;
        }
    }
    fn on_binary_loaded(&mut self, _ev: BinaryLoadedEvent<'_>) {}
    fn on_binary_unloaded(&mut self, _ev: BinaryUnloadedEvent<'_>) {}
    fn on_thread_name(&mut self, _ev: ThreadNameEvent<'_>) {}
    fn on_wakeup(&mut self, _ev: WakeupEvent<'_>) {}
    fn on_cpu_interval(&mut self, _ev: CpuIntervalEvent<'_>) {}
}

fn run_once(prog: &str, dwarf_unwind: bool, secs: u64) -> eyre::Result<DepthSums> {
    let mut child = std::process::Command::new(prog).spawn()?;
    let pid = child.id();

    let opts = RecordOptions {
        pid,
        frequency_hz: 999,
        duration: Some(Duration::from_secs(secs)),
        kernel_stacks: false,
        dwarf_unwind,
    };
    let stop = AtomicBool::new(false);
    let mut sink = DepthSums::default();
    let _ = stax_linux_capture::record(&opts, &mut sink, &stop)?;

    let _ = child.kill();
    let _ = child.wait();
    Ok(sink)
}

fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let prog = std::env::args().nth(1).ok_or_else(|| {
        eyre::eyre!("usage: eh_smoke <path-to-test-binary>")
    })?;
    let secs: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    println!("=== run #1: dwarf_unwind=false (kernel CALLCHAIN only) ===");
    let off = run_once(&prog, false, secs)?;
    let off_mean = if off.samples == 0 { 0 } else { off.sum_depth / off.samples };
    println!(
        "  samples={} max_depth={} mean_depth={}",
        off.samples, off.max_depth, off_mean
    );

    println!("=== run #2: dwarf_unwind=true  (.eh_frame replay) ===");
    let on = run_once(&prog, true, secs)?;
    let on_mean = if on.samples == 0 { 0 } else { on.sum_depth / on.samples };
    println!(
        "  samples={} max_depth={} mean_depth={}",
        on.samples, on.max_depth, on_mean
    );

    if on.max_depth > off.max_depth {
        println!(
            "  ✓ DWARF unwinder caught deeper stacks (+{} frames at peak)",
            on.max_depth - off.max_depth
        );
    } else if on.max_depth == off.max_depth {
        println!("  = same max depth (binary likely has frame pointers)");
    } else {
        println!(
            "  ✗ DWARF unwinder LOST frames at peak ({} -> {})",
            off.max_depth, on.max_depth
        );
    }
    Ok(())
}
