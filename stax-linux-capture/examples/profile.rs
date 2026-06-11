//! End-to-end smoke test for the Linux capture backend.
//!
//! Spins a CPU workload in a thread, profiles this very process for a
//! couple of seconds, resolves each sample's leaf address against the
//! images we were told about, and prints a top-symbols table. Proves
//! the whole spine: perf_event_open → ring drain → record parse →
//! `SampleSink` → ELF symbolization.
//!
//!   cargo run -p stax-linux-capture --example profile

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use stax_linux_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, RecordOptions, SampleEvent, SampleSink, ThreadNameEvent,
};

struct Image {
    base_avma: u64,
    vmsize: u64,
    text_svma: u64,
    path: String,
    // (start_svma, end_svma, name)
    symbols: Vec<(u64, u64, String)>,
}

#[derive(Default)]
struct Collector {
    images: Vec<Image>,
    samples: u64,
    user_frames: u64,
    kernel_frames: u64,
    leaf_hist: HashMap<String, u64>,
    threads: Vec<(u32, String)>,
}

impl Collector {
    fn resolve(&self, pc: u64) -> Option<String> {
        for img in &self.images {
            if pc >= img.base_avma && pc < img.base_avma + img.vmsize.max(1) {
                let svma = pc.wrapping_sub(img.base_avma).wrapping_add(img.text_svma);
                // binary search the sorted symbol table
                let idx = img.symbols.partition_point(|s| s.0 <= svma).wrapping_sub(1);
                if let Some((s, e, name)) = img.symbols.get(idx) {
                    if svma >= *s && svma < *e {
                        let short = img.path.rsplit('/').next().unwrap_or(&img.path);
                        return Some(format!("{name}  ({short})"));
                    }
                }
                return Some(format!(
                    "{:#x}  ({})",
                    svma,
                    img.path.rsplit('/').next().unwrap_or(&img.path)
                ));
            }
        }
        None
    }
}

impl SampleSink for Collector {
    fn on_sample(&mut self, s: SampleEvent<'_>) {
        self.samples += 1;
        self.user_frames += s.backtrace.len() as u64;
        self.kernel_frames += s.kernel_backtrace.len() as u64;
        if let Some(&leaf) = s.backtrace.first() {
            let name = self
                .resolve(leaf)
                .unwrap_or_else(|| format!("{leaf:#x}  (?)"));
            *self.leaf_hist.entry(name).or_insert(0) += 1;
        } else if let Some(&kleaf) = s.kernel_backtrace.first() {
            *self
                .leaf_hist
                .entry(format!("{kleaf:#x}  (kernel)"))
                .or_insert(0) += 1;
        }
    }
    fn on_binary_loaded(&mut self, e: BinaryLoadedEvent<'_>) {
        self.images.push(Image {
            base_avma: e.base_avma,
            vmsize: e.vmsize,
            text_svma: e.text_svma,
            path: e.path.to_string(),
            symbols: e
                .symbols
                .iter()
                .map(|s| {
                    (
                        s.start_svma,
                        s.end_svma,
                        String::from_utf8_lossy(&s.name).into_owned(),
                    )
                })
                .collect(),
        });
    }
    fn on_binary_unloaded(&mut self, _e: BinaryUnloadedEvent<'_>) {}
    fn on_thread_name(&mut self, e: ThreadNameEvent<'_>) {
        self.threads.push((e.tid, e.name.to_string()));
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("stax_linux_capture=info")
        .without_time()
        .init();

    // A CPU workload to actually catch in the act.
    let stop = Arc::new(AtomicBool::new(false));
    let acc = Arc::new(AtomicU64::new(0));
    let w_stop = stop.clone();
    let w_acc = acc.clone();
    let worker = std::thread::Builder::new()
        .name("burner".into())
        .spawn(move || {
            let mut x: u64 = 1;
            while !w_stop.load(Ordering::Relaxed) {
                for _ in 0..200_000 {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                }
                w_acc.fetch_add(x & 1, Ordering::Relaxed);
            }
        })
        .unwrap();

    let opts = RecordOptions {
        pid: std::process::id(),
        frequency_hz: 997,
        duration: Some(Duration::from_secs(2)),
        kernel_stacks: true,
    };
    println!(
        "profiling self (pid {}) for 2s @ {}Hz…",
        opts.pid, opts.frequency_hz
    );

    let mut col = Collector::default();
    let never = AtomicBool::new(false);
    let t0 = Instant::now();
    let summary = stax_linux_capture::record(&opts, &mut col, &never).expect("record");

    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    let _ = acc.load(Ordering::Relaxed);

    println!(
        "\n=== summary ===\nwall={:?} samples={} lost={} binaries={} \
         user_frames={} kernel_frames={} threads={}",
        t0.elapsed(),
        summary.samples,
        summary.lost_records,
        summary.binaries,
        col.user_frames,
        col.kernel_frames,
        col.threads.len(),
    );

    let mut top: Vec<(String, u64)> = col.leaf_hist.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\n=== top leaf symbols ===");
    for (name, n) in top.into_iter().take(15) {
        println!("{n:6}  {name}");
    }

    assert!(
        summary.samples > 0,
        "captured zero samples — capture broken"
    );
    assert!(
        col.user_frames > 0,
        "no user stack frames — callchain broken"
    );
    assert!(summary.binaries > 0, "no images — MMAP2/ELF path broken");
    println!("\nOK: capture + symbolization spine works on Linux.");
}
