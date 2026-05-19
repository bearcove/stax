//! A real, usable Linux profiler output: profile a CPU workload and
//! write a flamegraph SVG — capture → ELF symbolize → demangle (the
//! OS-neutral `stax-demangle`) → folded stacks → `inferno`.
//!
//! This proves the Phase-4 value path (a recording produces a
//! flamegraph on Linux) without yet needing the full live-UI / vox
//! server stack ported.
//!
//!   cargo run -p stax-linux-capture --example flamegraph
//!   # -> /tmp/stax-linux-flame.svg  (+ .folded)

use std::collections::HashMap;
use std::fs::File;
use std::hint::black_box;
use std::io::BufWriter;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use stax_linux_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, RecordOptions, SampleEvent, SampleSink,
    ThreadNameEvent,
};

struct Image {
    base: u64,
    size: u64,
    text_svma: u64,
    short: String,
    syms: Vec<(u64, u64, Vec<u8>)>, // sorted by start_svma
}

#[derive(Default)]
struct Folder {
    images: Vec<Image>,
    tnames: HashMap<u32, String>,
    folded: HashMap<String, u64>,
    samples: u64,
}

impl Folder {
    fn frame(&self, pc: u64) -> String {
        for im in &self.images {
            if pc >= im.base && pc < im.base + im.size.max(1) {
                let svma = pc.wrapping_sub(im.base).wrapping_add(im.text_svma);
                let i = im.syms.partition_point(|s| s.0 <= svma).wrapping_sub(1);
                if let Some((s, e, name)) = im.syms.get(i) {
                    if svma >= *s && svma < *e {
                        return stax_demangle::demangle_bytes(name).name;
                    }
                }
                return format!("{}+{:#x}", im.short, svma);
            }
        }
        format!("[unknown {pc:#x}]")
    }
}

impl SampleSink for Folder {
    fn on_sample(&mut self, s: SampleEvent<'_>) {
        self.samples += 1;
        let thread = self
            .tnames
            .get(&s.tid)
            .cloned()
            .unwrap_or_else(|| format!("tid {}", s.tid));
        // Folded wants root → leaf; perf callchain is leaf → root.
        let mut stack = vec![thread];
        for &pc in s.backtrace.iter().rev() {
            stack.push(self.frame(pc));
        }
        // Kernel frames sit above the user leaf, tagged so they read
        // clearly in the graph.
        for &pc in s.kernel_backtrace.iter().rev() {
            stack.push(format!("[k] {pc:#x}"));
        }
        *self.folded.entry(stack.join(";")).or_insert(0) += 1;
    }
    fn on_binary_loaded(&mut self, e: BinaryLoadedEvent<'_>) {
        let mut syms: Vec<(u64, u64, Vec<u8>)> = e
            .symbols
            .iter()
            .map(|s| (s.start_svma, s.end_svma, s.name.clone()))
            .collect();
        syms.sort_by_key(|s| s.0);
        self.images.push(Image {
            base: e.base_avma,
            size: e.vmsize,
            text_svma: e.text_svma,
            short: e.path.rsplit('/').next().unwrap_or(e.path).to_string(),
            syms,
        });
    }
    fn on_binary_unloaded(&mut self, _e: BinaryUnloadedEvent<'_>) {}
    fn on_thread_name(&mut self, e: ThreadNameEvent<'_>) {
        self.tnames.insert(e.tid, e.name.to_string());
    }
}

// A recognizable, deep-ish call chain so the flamegraph has shape.
#[inline(never)]
fn hot_inner(seed: u64) -> u64 {
    let mut x = seed;
    for _ in 0..150_000 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
    }
    black_box(x)
}

#[inline(never)]
fn hot_outer(stop: &AtomicBool, acc: &AtomicU64) {
    let mut s = 1u64;
    while !stop.load(Ordering::Relaxed) {
        s = hot_inner(s);
        acc.fetch_add(s & 1, Ordering::Relaxed);
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("stax_linux_capture=warn")
        .without_time()
        .init();

    let stop = Arc::new(AtomicBool::new(false));
    let acc = Arc::new(AtomicU64::new(0));
    let (ws, wa) = (stop.clone(), acc.clone());
    let worker = std::thread::Builder::new()
        .name("burner".into())
        .spawn(move || hot_outer(&ws, &wa))
        .unwrap();

    let opts = RecordOptions {
        pid: std::process::id(),
        frequency_hz: 997,
        duration: Some(Duration::from_secs(3)),
        kernel_stacks: true,
    };
    println!("profiling pid {} for 3s @ {}Hz…", opts.pid, opts.frequency_hz);

    let mut f = Folder::default();
    let never = AtomicBool::new(false);
    let summary = stax_linux_capture::record(&opts, &mut f, &never).expect("record");

    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();

    // Folded stacks, descending by weight (stable, diffable output).
    let mut lines: Vec<(String, u64)> = f.folded.iter().map(|(k, v)| (k.clone(), *v)).collect();
    lines.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let folded_path = "/tmp/stax-linux-flame.folded";
    let svg_path = "/tmp/stax-linux-flame.svg";
    {
        use std::io::Write;
        let mut w = BufWriter::new(File::create(folded_path).unwrap());
        for (k, v) in &lines {
            writeln!(w, "{k} {v}").unwrap();
        }
    }

    let folded_lines: Vec<String> = lines.iter().map(|(k, v)| format!("{k} {v}")).collect();
    let mut opt = inferno::flamegraph::Options::default();
    opt.title = format!(
        "stax-linux-capture — pid {} — {} samples",
        opts.pid, summary.samples
    );
    let svg = BufWriter::new(File::create(svg_path).unwrap());
    inferno::flamegraph::from_lines(&mut opt, folded_lines.iter().map(|s| s.as_str()), svg)
        .expect("inferno render");

    println!(
        "samples={} lost={} binaries={} unique_stacks={}\nwrote {svg_path}\nwrote {folded_path}",
        summary.samples,
        summary.lost_records,
        summary.binaries,
        lines.len()
    );
    println!("\ntop folded stacks:");
    for (k, v) in lines.iter().take(6) {
        // Trim to the last 3 frames for a readable console preview.
        let tail: Vec<&str> = k.rsplit(';').take(3).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        println!("{v:6}  …;{}", tail.join(";"));
    }

    let burner_hot = lines
        .iter()
        .any(|(k, _)| k.contains("hot_inner") && k.contains("burner"));
    assert!(summary.samples > 100, "too few samples");
    assert!(
        burner_hot,
        "expected the burner thread's hot_inner to dominate — \
         symbolization or thread attribution is wrong"
    );
    let svg_len = std::fs::metadata(svg_path).map(|m| m.len()).unwrap_or(0);
    assert!(svg_len > 4096, "flamegraph SVG looks empty ({svg_len} bytes)");
    println!("\nOK: usable flamegraph profiler works on Linux ({svg_len} byte SVG).");
}
