//! Thread-pool style target-span integration.
//!
//! Run it under stax:
//!
//! ```text
//! stax record -- cargo run -p stax-target --example thread_pool
//! stax threads -n 0
//! stax top --tid <thread pool demo synthetic tid> --sort self
//! stax flame --tid <origin cpu tid>
//! stax diagnose
//! ```
//!
//! The queue side captures `Lane::capture_origin()` and carries the
//! token inside the work item. Worker threads then time the actual
//! work with `begin_span_with_captured_origin`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
struct Work {
    kind: usize,
    origin: stax_target::CapturedOrigin,
    busy_for: Duration,
}

fn main() {
    let lane = stax_target::Lane::new("thread pool demo");
    let mut senders = Vec::new();
    let mut workers = Vec::new();

    for worker_id in 0..3 {
        let (tx, rx) = mpsc::channel::<Work>();
        let worker_lane = lane.clone();
        let worker = std::thread::Builder::new()
            .name(format!("stax-target-pool-{worker_id}"))
            .spawn(move || worker_loop(worker_id, worker_lane, rx))
            .expect("spawn pool worker");
        senders.push(tx);
        workers.push(worker);
    }

    println!("pid {}", std::process::id());
    println!("record this process with stax, then inspect the `thread pool demo` lane");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut seq = 0usize;
    let mut was_active = false;
    while Instant::now() < deadline {
        let active = lane.reporting_active();
        if active && !was_active {
            println!("stax capture active; reporting thread-pool spans");
        }
        was_active = active;

        let work = Work {
            kind: seq % 4,
            origin: lane.capture_origin(),
            busy_for: Duration::from_millis(1 + (seq % 7) as u64),
        };
        let sender = &senders[seq % senders.len()];
        if sender.send(work).is_err() {
            break;
        }
        seq += 1;
        std::thread::sleep(Duration::from_millis(4));
    }

    drop(senders);
    for worker in workers {
        let _ = worker.join();
    }
    println!("submitted {seq} thread-pool jobs");
}

fn worker_loop(worker_id: usize, lane: stax_target::Lane, rx: mpsc::Receiver<Work>) {
    while let Ok(work) = rx.recv() {
        let open = lane.begin_span_with_captured_origin(job_name(work.kind), work.origin);
        busy_wait(work.busy_for + Duration::from_micros(worker_id as u64 * 250));
        if let Some(open) = open {
            open.finish_and_report(&lane);
        }
    }
}

fn job_name(kind: usize) -> &'static str {
    match kind {
        0 => "parse request",
        1 => "run transform",
        2 => "write result",
        _ => "flush metrics",
    }
}

fn busy_wait(duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut x = 0u64;
    while Instant::now() < deadline {
        x = x.rotate_left(7).wrapping_add(0x9e37_79b9);
        std::hint::black_box(x);
    }
}
