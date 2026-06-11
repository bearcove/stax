//! Probe the live stax-server's TargetIngest surface: connect, call
//! should_report for a pid (arg 1, default self), print the verdict.
use stax_live_proto::TargetIngestClient;

fn main() {
    let pid: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(std::process::id());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let uid = unsafe { libc::getuid() };
        let url = format!("local:///tmp/stax-server-{uid}.sock");
        match vox::connect::<TargetIngestClient>(&url).await {
            Ok(client) => match client.should_report(pid).await {
                Ok(active) => println!("connected; should_report({pid}) = {active}"),
                Err(e) => println!("connected but should_report failed: {e}"),
            },
            Err(e) => println!("connect failed (old server without TargetIngest?): {e}"),
        }
    });
}
