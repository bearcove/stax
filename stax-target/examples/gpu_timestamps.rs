//! GPU timestamp-counter integration skeleton.
//!
//! Run it under stax:
//!
//! ```text
//! stax record -- cargo run -p stax-target --example gpu_timestamps
//! stax threads -n 0
//! stax top --tid <gpu timestamp skeleton synthetic tid> --sort self
//! stax flame --tid <origin cpu tid>
//! stax diagnose
//! ```
//!
//! This example uses a fake GPU clock so it compiles on every platform.
//! In a real Metal 4 integration, `FakeGpuClock::dispatch` is where the
//! command encoder/command buffer would collect timestamp-counter values,
//! convert them to stax nanoseconds, and report them after completion.

use std::time::{Duration, Instant};

struct FakeGpuClock {
    epoch_ns: u64,
    next_tick: u64,
    ns_per_tick: u64,
}

struct GpuCompletion {
    kernel: &'static str,
    start_ns: u64,
    end_ns: u64,
    origin: stax_target::CapturedOrigin,
}

fn main() {
    let lane = stax_target::Lane::new("gpu timestamp skeleton");
    let mut clock = FakeGpuClock::new();

    println!("pid {}", std::process::id());
    println!("record this process with stax, then inspect the GPU timestamp lane");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut dispatches = 0usize;
    let mut was_active = false;
    while Instant::now() < deadline {
        let active = lane.reporting_active();
        if active && !was_active {
            println!("stax capture active; reporting fake GPU timestamp spans");
        }
        was_active = active;

        let origin = lane.capture_origin();
        let completion = clock.dispatch(kernel_name(dispatches), origin);
        std::thread::sleep(Duration::from_millis(2));
        if let Some(span) = lane.span_with_captured_origin(
            completion.kernel,
            completion.start_ns,
            completion.end_ns,
            completion.origin,
        ) {
            lane.report_one(span);
        }

        dispatches += 1;
        std::thread::sleep(Duration::from_millis(5));
    }

    println!("submitted {dispatches} fake GPU dispatches");
}

impl FakeGpuClock {
    fn new() -> Self {
        Self {
            epoch_ns: stax_target::now_ns().unwrap_or(0),
            next_tick: 0,
            ns_per_tick: 41,
        }
    }

    fn dispatch(
        &mut self,
        kernel: &'static str,
        origin: stax_target::CapturedOrigin,
    ) -> GpuCompletion {
        let start_tick = self.next_tick;
        let duration_ticks = 30_000 + (start_tick % 17_000);
        let end_tick = start_tick + duration_ticks;
        self.next_tick = end_tick + 50_000;

        GpuCompletion {
            kernel,
            start_ns: self.to_stax_ns(start_tick),
            end_ns: self.to_stax_ns(end_tick),
            origin,
        }
    }

    fn to_stax_ns(&self, tick: u64) -> u64 {
        self.epoch_ns
            .saturating_add(tick.saturating_mul(self.ns_per_tick))
    }
}

fn kernel_name(index: usize) -> &'static str {
    match index % 4 {
        0 => "matmul_tq1s",
        1 => "rmsnorm",
        2 => "attention_score",
        _ => "sample_logits",
    }
}
