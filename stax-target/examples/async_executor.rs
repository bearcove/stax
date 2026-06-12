//! Async-executor style target-span integration.
//!
//! Run it under stax:
//!
//! ```text
//! stax record -- cargo run -p stax-target --example async_executor
//! stax threads -n 0
//! stax top --tid <async executor demo synthetic tid> --sort self
//! stax flame --tid <origin cpu tid>
//! stax diagnose
//! ```
//!
//! The scheduling side captures `Lane::capture_origin()` before
//! sending work into an async channel. The async worker opens the span
//! when it starts executing that work.

use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
struct Work {
    kind: usize,
    origin: stax_target::CapturedOrigin,
    delay_for: Duration,
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build tokio runtime");
    runtime.block_on(async_main());
}

async fn async_main() {
    let lane = stax_target::Lane::new("async executor demo");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Work>();
    let worker_lane = lane.clone();
    let worker = tokio::spawn(async move {
        while let Some(work) = rx.recv().await {
            let open =
                worker_lane.begin_span_with_captured_origin(job_name(work.kind), work.origin);
            tokio::time::sleep(work.delay_for).await;
            if let Some(open) = open {
                open.finish_and_report(&worker_lane);
            }
        }
    });

    println!("pid {}", std::process::id());
    println!("record this process with stax, then inspect the `async executor demo` lane");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut seq = 0usize;
    let mut was_active = false;
    while Instant::now() < deadline {
        let active = lane.reporting_active();
        if active && !was_active {
            println!("stax capture active; reporting async executor spans");
        }
        was_active = active;

        if tx
            .send(Work {
                kind: seq % 4,
                origin: lane.capture_origin(),
                delay_for: Duration::from_millis(2 + (seq % 5) as u64),
            })
            .is_err()
        {
            break;
        }
        seq += 1;
        tokio::time::sleep(Duration::from_millis(6)).await;
    }

    drop(tx);
    let _ = worker.await;
    println!("submitted {seq} async jobs");
}

fn job_name(kind: usize) -> &'static str {
    match kind {
        0 => "poll decode future",
        1 => "await device fence",
        2 => "resume continuation",
        _ => "publish result",
    }
}
