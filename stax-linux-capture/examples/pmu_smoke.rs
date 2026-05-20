//! Tiny end-to-end smoke for the PMU counter group: record `argv[1]`
//! (or fall back to a small CPU-bound loop in this process) for a
//! couple of seconds and print accumulated cycles / instructions /
//! L1D misses / branch mispredicts.
//!
//! Run: `cargo run -p stax-linux-capture --example pmu_smoke -- /tmp/busy`.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use stax_linux_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, RecordOptions, SampleEvent, SampleSink, ThreadNameEvent,
};

#[derive(Default)]
struct PmuSums {
    samples: u64,
    cycles: u128,
    instructions: u128,
    l1d_misses: u128,
    branch_mispreds: u128,
}

impl SampleSink for PmuSums {
    fn on_sample(&mut self, ev: SampleEvent<'_>) {
        self.samples += 1;
        self.cycles += ev.cycles as u128;
        self.instructions += ev.instructions as u128;
        self.l1d_misses += ev.l1d_misses as u128;
        self.branch_mispreds += ev.branch_mispreds as u128;
    }
    fn on_binary_loaded(&mut self, _ev: BinaryLoadedEvent<'_>) {}
    fn on_binary_unloaded(&mut self, _ev: BinaryUnloadedEvent<'_>) {}
    fn on_thread_name(&mut self, _ev: ThreadNameEvent<'_>) {}
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Either profile a launched child (argv[1]) or this process itself
    // running a CPU-bound loop. Either way we just want a few seconds
    // of samples on a busy thread so the HW counter deltas are big
    // enough to be obviously non-zero.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let child = if let Some(prog) = argv.first() {
        Some(
            std::process::Command::new(prog)
                .args(&argv[1..])
                .spawn()
                .expect("spawn"),
        )
    } else {
        None
    };
    let pid = match child.as_ref() {
        Some(c) => c.id(),
        None => std::process::id(),
    };

    let opts = RecordOptions {
        pid,
        frequency_hz: 999,
        duration: Some(Duration::from_secs(2)),
        kernel_stacks: true,
    };
    let stop = AtomicBool::new(false);

    // If we're profiling ourselves, do CPU-bound work on a background
    // thread so the sampler has something to attribute counts to.
    let _busy = if child.is_none() {
        Some(std::thread::spawn(|| {
            let deadline = Instant::now() + Duration::from_millis(2_500);
            let mut s: u64 = 0;
            while Instant::now() < deadline {
                for i in 0..1_000_000u64 {
                    s = s.wrapping_add(i);
                }
            }
            std::hint::black_box(s);
        }))
    } else {
        None
    };

    let mut sink = PmuSums::default();
    let summary = stax_linux_capture::record(&opts, &mut sink, &stop).expect("record");

    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
    stop.store(true, Ordering::Relaxed);

    println!(
        "samples={} (lost {}), elapsed={}ms",
        summary.samples,
        summary.lost_records,
        summary.session_ns / 1_000_000
    );
    println!("--- PMU totals (Σ over samples) ---");
    println!("  cycles          = {}", sink.cycles);
    println!("  instructions    = {}", sink.instructions);
    println!("  l1d_read_misses = {}", sink.l1d_misses);
    println!("  branch_misses   = {}", sink.branch_mispreds);
    if sink.samples > 0 {
        let avg = |n: u128| -> u128 { n / sink.samples as u128 };
        println!("--- per-sample avg ---");
        println!("  cycles/sample        = {}", avg(sink.cycles));
        println!("  instructions/sample  = {}", avg(sink.instructions));
        println!("  l1d_misses/sample    = {}", avg(sink.l1d_misses));
        println!("  branch_misses/sample = {}", avg(sink.branch_mispreds));
        if sink.cycles > 0 {
            // Linux portable counters report retired vs. total cycles,
            // so IPC = instructions / cycles is meaningful.
            let ipc_x1000 = (sink.instructions * 1000) / sink.cycles;
            println!(
                "  IPC ≈ {}.{:03}",
                ipc_x1000 / 1000,
                ipc_x1000 % 1000
            );
        }
    }
}
