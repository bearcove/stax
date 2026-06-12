//! Codec-style target-span integration with exact host timestamps.
//!
//! Run it under stax:
//!
//! ```text
//! stax record -- cargo run -p stax-target --example codec
//! stax threads -n 0
//! stax top --tid <codec decode synthetic tid> --sort self
//! stax flame --tid <origin cpu tid>
//! stax diagnose
//! ```
//!
//! This demonstrates APIs where the integrator controls or receives
//! exact start/end timestamps and reports spans directly.

use std::time::{Duration, Instant};

fn main() {
    let decode = stax_target::Lane::new("codec decode");
    let encode = stax_target::Lane::new("codec encode");

    println!("pid {}", std::process::id());
    println!("record this process with stax, then inspect the codec lanes");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut frames = 0usize;
    let mut was_active = false;
    while Instant::now() < deadline {
        let active = decode.reporting_active();
        if active && !was_active {
            println!("stax capture active; reporting codec spans");
        }
        was_active = active;

        run_stage(&decode, "decode packet", Duration::from_millis(2));
        std::thread::sleep(Duration::from_millis(1));
        run_stage(&decode, "reconstruct frame", Duration::from_millis(3));
        run_stage(&encode, "encode frame", Duration::from_millis(2));
        std::thread::sleep(Duration::from_millis(4));
        frames += 1;
    }

    println!("processed {frames} codec frames");
}

fn run_stage(lane: &stax_target::Lane, name: &'static str, duration: Duration) {
    let origin = lane.capture_origin();
    let Some(start_ns) = stax_target::now_ns() else {
        return;
    };
    busy_wait(duration);
    let Some(end_ns) = stax_target::now_ns() else {
        return;
    };
    if let Some(span) = lane.span_with_captured_origin(name, start_ns, end_ns, origin) {
        lane.report_one(span);
    }
}

fn busy_wait(duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut x = 1u64;
    while Instant::now() < deadline {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        std::hint::black_box(x);
    }
}
