//! Blessed stax-target corpus: CPU work, off-CPU waits, linked target spans,
//! and intentionally flawed target spans in one recording.
//!
//! Run it under stax:
//!
//! ```text
//! just demo-corpus
//!
//! # or directly:
//! stax record -- cargo run -p stax-target --example corpus
//! stax threads -n 0
//! stax top --tid <corpus executor synthetic tid> --sort self
//! stax flame --tid <origin cpu tid> --threshold-pct 0
//! stax diagnose
//! ```

use std::sync::mpsc;
use std::time::{Duration, Instant};

const SYNTH_TID_BASE: u32 = 0xFFF0_0000;

#[derive(Clone, Copy)]
struct ExecutorWork {
    kind: usize,
    origin: stax_target::CapturedOrigin,
    busy_for: Duration,
}

fn main() {
    let executor_lane = stax_target::Lane::new("corpus executor");
    let gpu_lane = stax_target::Lane::metal("corpus gpu");
    let bad_lane = stax_target::Lane::new("corpus bad origins");

    let (tx, rx) = mpsc::channel::<ExecutorWork>();
    let worker_lane = executor_lane.clone();
    let worker = std::thread::Builder::new()
        .name("stax-corpus-worker".to_owned())
        .spawn(move || executor_worker(worker_lane, rx))
        .expect("spawn corpus worker");

    println!("pid {}", std::process::id());
    println!("record this process with stax, then inspect corpus lanes and `stax diagnose`");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut seq = 0usize;
    let mut reported_bad = false;
    let mut was_active = false;

    while Instant::now() < deadline {
        let active = executor_lane.reporting_active();
        if active && !was_active {
            println!("stax capture active; reporting corpus spans");
        }
        was_active = active;

        corpus_cpu_parse();
        submit_executor_work(&executor_lane, &tx, seq);
        dispatch_gpu_work(&gpu_lane, seq);
        corpus_offcpu_wait();

        if active && !reported_bad {
            report_bad_origin_spans(&bad_lane);
            reported_bad = true;
        }

        seq += 1;
    }

    drop(tx);
    let _ = worker.join();
    println!("submitted {seq} corpus iterations");
}

#[inline(never)]
fn submit_executor_work(lane: &stax_target::Lane, tx: &mpsc::Sender<ExecutorWork>, seq: usize) {
    let work = ExecutorWork {
        kind: seq % 4,
        origin: lane.capture_origin(),
        busy_for: Duration::from_millis(2 + (seq % 4) as u64),
    };
    let _ = tx.send(work);
}

fn executor_worker(lane: stax_target::Lane, rx: mpsc::Receiver<ExecutorWork>) {
    while let Ok(work) = rx.recv() {
        let open = lane.begin_span_with_captured_origin(executor_work_name(work.kind), work.origin);
        corpus_executor_run(work.busy_for);
        if let Some(open) = open {
            open.finish_and_report(&lane);
        }
    }
}

fn executor_work_name(kind: usize) -> &'static str {
    match kind {
        0 => "corpus parse batch",
        1 => "corpus schedule command",
        2 => "corpus run work item",
        _ => "corpus collect completion",
    }
}

#[inline(never)]
fn dispatch_gpu_work(lane: &stax_target::Lane, seq: usize) {
    let origin = lane.capture_origin();
    let Some(start_ns) = stax_target::now_ns() else {
        return;
    };
    corpus_gpu_kernel(Duration::from_millis(1 + (seq % 3) as u64));
    let Some(end_ns) = stax_target::now_ns() else {
        return;
    };
    let name = match seq % 3 {
        0 => "corpus matmul kernel",
        1 => "corpus reduce kernel",
        _ => "corpus copy kernel",
    };
    if let Some(span) = lane.span_with_captured_origin(name, start_ns, end_ns, origin) {
        lane.report_one(span);
    }
}

fn report_bad_origin_spans(lane: &stax_target::Lane) {
    report_timed_span(
        lane,
        "corpus missing origin",
        stax_target::CapturedOrigin::active_without_origin(),
    );

    if let Some(now) = stax_target::now_ns() {
        lane.report_one(stax_target::TargetSpan::new(
            "corpus bad duration",
            now + 1_000_000,
            now,
        ));
        report_timed_span(
            lane,
            "corpus synthetic origin tid",
            stax_target::CapturedOrigin::from_origin(stax_target::TargetSpanOrigin {
                tid: SYNTH_TID_BASE,
                timestamp_ns: now,
            }),
        );
        report_timed_span(
            lane,
            "corpus missing origin thread",
            stax_target::CapturedOrigin::from_origin(stax_target::TargetSpanOrigin {
                tid: 990_000_000,
                timestamp_ns: now,
            }),
        );
    }

    let stale = lane
        .current_origin()
        .map(|mut origin| {
            origin.timestamp_ns = origin.timestamp_ns.saturating_sub(1_000_000_000);
            stax_target::CapturedOrigin::from_origin(origin)
        })
        .unwrap_or_else(stax_target::CapturedOrigin::active_without_origin);
    corpus_cpu_parse();
    report_timed_span(lane, "corpus stale origin", stale);
}

fn report_timed_span(
    lane: &stax_target::Lane,
    name: &'static str,
    origin: stax_target::CapturedOrigin,
) {
    let Some(start_ns) = stax_target::now_ns() else {
        return;
    };
    busy_wait(Duration::from_millis(2));
    let Some(end_ns) = stax_target::now_ns() else {
        return;
    };
    if let Some(span) = lane.span_with_captured_origin(name, start_ns, end_ns, origin) {
        lane.report_one(span);
    }
}

#[inline(never)]
fn corpus_cpu_parse() {
    busy_wait(Duration::from_millis(3));
}

#[inline(never)]
fn corpus_executor_run(duration: Duration) {
    busy_wait(duration);
}

#[inline(never)]
fn corpus_gpu_kernel(duration: Duration) {
    busy_wait(duration);
}

#[inline(never)]
fn corpus_offcpu_wait() {
    std::thread::sleep(Duration::from_millis(4));
}

fn busy_wait(duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut x = 0x1234_5678_9abc_def0u64;
    while Instant::now() < deadline {
        x ^= x.rotate_left(7);
        x = x.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        std::hint::black_box(x);
    }
}
