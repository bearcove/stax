//! Probe the live stax-server's TargetIngest surface: should_report for
//! a pid, plus optional fake-batch injection ("ingest" as 2nd arg) to
//! discriminate server-side vs target-side failures.
use stax_live_proto::{TargetIngestClient, TargetSpan, TargetSpanBatch};

fn main() {
    let pid: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(std::process::id());
    let inject = std::env::args().nth(2).as_deref() == Some("ingest");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let socket = std::env::var("STAX_SERVER_SOCKET").unwrap_or_else(|_| {
            let uid = unsafe { libc::getuid() };
            format!("/tmp/stax-server-{uid}.sock")
        });
        let url = format!("local://{socket}");
        let client: TargetIngestClient = match vox::connect(&url).await {
            Ok(client) => client,
            Err(e) => {
                println!("connect failed: {e}");
                return;
            }
        };
        match client.should_report(pid).await {
            Ok(active) => println!("should_report({pid}) = {active}"),
            Err(e) => println!("should_report failed: {e}"),
        }
        if inject {
            // mach_absolute_time is 24MHz ticks on Apple Silicon; the
            // aggregator wants ns. 125/3 is the timebase ratio.
            unsafe extern "C" {
                fn mach_absolute_time() -> u64;
            }
            let now_ns = unsafe { mach_absolute_time() } * 125 / 3;
            let batch = TargetSpanBatch {
                pid,
                lane: "GPU probe".to_owned(),
                spans: vec![TargetSpan::new(
                    "probe_fake_kernel",
                    now_ns - 50_000_000,
                    now_ns,
                )],
            };
            match client.ingest(batch).await {
                Ok(()) => println!("ingest ok (50ms fake span sent)"),
                Err(e) => println!("ingest failed: {e}"),
            }
        }
    });
}
