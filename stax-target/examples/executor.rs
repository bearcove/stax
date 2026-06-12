//! Cooperating target-span demo for executor-style work.
//!
//! Run it under stax:
//!
//! ```text
//! stax record -- cargo run -p stax-target --example executor
//! stax threads
//! stax flame --tid <executor demo synthetic tid>
//! ```
//!
//! The important pattern is the split:
//!
//! - enqueue side captures `Lane::capture_origin()`;
//! - worker side times the actual work with `begin_span_with_captured_origin`;
//! - stax can then render `CPU enqueue stack -> executor lane -> job`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
struct Work {
    kind: usize,
    origin: stax_target::CapturedOrigin,
    busy_for: Duration,
}

fn main() {
    let lane = stax_target::Lane::new("executor demo");
    let (tx, rx) = mpsc::channel::<Work>();
    let worker_lane = lane.clone();
    let worker = std::thread::Builder::new()
        .name("stax-target-demo-worker".to_owned())
        .spawn(move || worker_loop(worker_lane, rx))
        .expect("spawn demo worker");

    println!("pid {}", std::process::id());
    println!("record this process with stax, then inspect the `executor demo` lane");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut seq = 0usize;
    let mut was_active = false;
    while Instant::now() < deadline {
        let active = lane.reporting_active();
        if active && !was_active {
            println!("stax capture active; reporting executor spans");
        }
        was_active = active;

        let work = Work {
            kind: seq % 4,
            origin: lane.capture_origin(),
            busy_for: Duration::from_millis(2 + (seq % 5) as u64),
        };
        if tx.send(work).is_err() {
            break;
        }
        seq += 1;
        std::thread::sleep(Duration::from_millis(8));
    }

    drop(tx);
    let _ = worker.join();
    println!("submitted {seq} demo jobs");
}

fn worker_loop(lane: stax_target::Lane, rx: mpsc::Receiver<Work>) {
    while let Ok(work) = rx.recv() {
        let open = lane.begin_span_with_captured_origin(job_name(work.kind), work.origin);
        busy_wait(work.busy_for);
        if let Some(open) = open {
            open.finish_and_report(&lane);
        }
    }
}

fn job_name(kind: usize) -> &'static str {
    match kind {
        0 => "parse batch",
        1 => "prepare command buffer",
        2 => "execute target job",
        _ => "collect completion",
    }
}

fn busy_wait(duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut x = 0u64;
    while Instant::now() < deadline {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        std::hint::black_box(x);
    }
}
