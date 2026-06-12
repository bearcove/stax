//! Model-runtime style target-span integration.
//!
//! Run it under stax:
//!
//! ```text
//! stax record -- cargo run -p stax-target --example model_runtime
//! stax threads -n 0
//! stax top --tid <model attention synthetic tid> --sort self
//! stax flame --tid <origin cpu tid>
//! stax diagnose
//! ```
//!
//! This mirrors a small inference runtime: semantic lanes, stable span
//! names, and a `SpanBuilder` for timestamped stage reporting.

use std::time::{Duration, Instant};

fn main() {
    let scheduler = stax_target::Lane::new("model scheduler");
    let attention = stax_target::Lane::new("model attention");
    let cache = stax_target::Lane::new("model kv cache");

    println!("pid {}", std::process::id());
    println!("record this process with stax, then inspect the model lanes");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut pulses = 0usize;
    let mut was_active = false;
    while Instant::now() < deadline {
        let active = scheduler.reporting_active();
        if active && !was_active {
            println!("stax capture active; reporting model-runtime spans");
        }
        was_active = active;

        dispatch_stage(&scheduler, "schedule batch", Duration::from_millis(1));
        dispatch_stage(&attention, "attention prefill", Duration::from_millis(3));
        dispatch_stage(&cache, "kv cache update", Duration::from_millis(1));
        dispatch_stage(&attention, "attention decode", Duration::from_millis(2));
        std::thread::sleep(Duration::from_millis(5));
        pulses += 1;
    }

    println!("processed {pulses} model pulses");
}

fn dispatch_stage(lane: &stax_target::Lane, name: &'static str, duration: Duration) {
    let origin = lane.capture_origin();
    let Some(start_ns) = stax_target::now_ns() else {
        return;
    };
    busy_wait(duration);
    let Some(end_ns) = stax_target::now_ns() else {
        return;
    };
    if let Some(span) = lane
        .span_builder(name, start_ns, end_ns)
        .with_captured_origin(origin)
        .build()
    {
        lane.report_one(span);
    }
}

fn busy_wait(duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut x = 0x1234_5678_u64;
    while Instant::now() < deadline {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        std::hint::black_box(x);
    }
}
