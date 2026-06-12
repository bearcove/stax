//! Intentional bad-origin demo for `stax diagnose`.
//!
//! Run it under stax:
//!
//! ```text
//! stax record -- cargo run -p stax-target --example bad_origins
//! stax threads -n 0
//! stax top --tid <bad origin demo synthetic tid> --sort self
//! stax diagnose
//! ```
//!
//! This reports a mix of useful and intentionally flawed spans:
//! missing origins, stale origins, wrong-thread origins, and one bad
//! duration span that the server should drop.

use std::time::{Duration, Instant};

fn main() {
    let lane = stax_target::Lane::new("bad origin demo");

    println!("pid {}", std::process::id());
    println!("record this process with stax, then run `stax diagnose`");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut reported = false;
    while Instant::now() < deadline {
        if lane.reporting_active() && !reported {
            println!("stax capture active; reporting intentionally flawed spans");
            report_bad_spans(&lane);
            reported = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    println!(
        "{}",
        if reported {
            "reported bad-origin spans"
        } else {
            "stax capture never became active"
        }
    );
}

fn report_bad_spans(lane: &stax_target::Lane) {
    report_timed(
        lane,
        "missing origin span",
        stax_target::CapturedOrigin::active_without_origin(),
    );

    if let Some(now) = stax_target::now_ns() {
        lane.report_one(stax_target::TargetSpan::new(
            "bad duration span",
            now + 1_000_000,
            now,
        ));
    }

    let stale = lane.capture_origin();
    std::thread::sleep(Duration::from_millis(120));
    report_timed(lane, "stale origin span", stale);

    let wrong_thread = std::thread::spawn(|| {
        stax_target::current_span_origin()
            .map(stax_target::CapturedOrigin::from_origin)
            .unwrap_or_else(stax_target::CapturedOrigin::active_without_origin)
    })
    .join()
    .unwrap_or_else(|_| stax_target::CapturedOrigin::active_without_origin());
    report_timed(lane, "wrong thread origin span", wrong_thread);
}

fn report_timed(lane: &stax_target::Lane, name: &'static str, origin: stax_target::CapturedOrigin) {
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

fn busy_wait(duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut x = 0u64;
    while Instant::now() < deadline {
        x = x.wrapping_add(1).rotate_left(3);
        std::hint::black_box(x);
    }
}
