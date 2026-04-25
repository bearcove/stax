//! Smoke-test driver for `nerf-mac-kperf::record`.
//!
//! Usage (from repo root):
//!
//!     cargo build --example sample_pid -p nerf-mac-kperf
//!     sudo RUST_LOG=nerf_mac_kperf=trace \
//!         target/debug/examples/sample_pid <PID> [duration_secs]
//!
//! Until the PERF_CS_* parser lands, no `SampleEvent`s are emitted;
//! you should however see "drained N kdebug records" trace lines
//! ticking by while the target process is on-CPU.

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::env;
    use std::time::Duration;

    use nerf_mac_capture::{
        BinaryLoadedEvent, BinaryUnloadedEvent, JitdumpEvent, SampleEvent,
        SampleSink, ThreadNameEvent,
    };
    use nerf_mac_kperf::{record, RecordOptions};

    if env::var_os("RUST_LOG").is_none() {
        env::set_var("RUST_LOG", "nerf_mac_kperf=trace");
    }
    env_logger::init();

    let pid: u32 = env::args()
        .nth(1)
        .ok_or("usage: sample_pid <PID> [seconds]")?
        .parse()?;
    let secs: u64 = env::args()
        .nth(2)
        .map(|s| s.parse().unwrap_or(5))
        .unwrap_or(5);

    struct CountingSink {
        samples: u64,
    }
    impl SampleSink for CountingSink {
        fn on_sample(&mut self, _: SampleEvent<'_>) {
            self.samples += 1;
        }
        fn on_binary_loaded(&mut self, _: BinaryLoadedEvent<'_>) {}
        fn on_binary_unloaded(&mut self, _: BinaryUnloadedEvent<'_>) {}
        fn on_thread_name(&mut self, _: ThreadNameEvent<'_>) {}
        fn on_jitdump(&mut self, _: JitdumpEvent<'_>) {}
    }

    let opts = RecordOptions {
        pid,
        frequency_hz: 1000,
        duration: Some(Duration::from_secs(secs)),
        ..Default::default()
    };

    let mut sink = CountingSink { samples: 0 };
    record(opts, &mut sink, || false)?;

    println!(
        "duration={}s, samples emitted to sink: {} \
         (parser not implemented yet -- expect 0)",
        secs, sink.samples
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("nerf-mac-kperf is macOS-only");
}
