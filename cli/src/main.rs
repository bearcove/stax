use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::process::exit;

use figue as args;
use stax_core::{
    args::{AnnotateArgs, Cli, Command, RecordArgs, TopArgs, WaitArgs},
    cmd_record_mac, cmd_setup_mac,
};
use stax_live_proto::{
    LiveFilter, ProfilerClient, RunControlClient, RunSummary, ServerStatus, TopSort, ViewParams,
    WaitCondition, WaitOutcome,
};

fn main_impl() -> Result<(), Box<dyn Error>> {
    if env::var("RUST_LOG").is_err() {
        // cranelift_jit/cranelift_codegen log every JIT'd function at info,
        // which floods the terminal once we start the live RPC server.
        unsafe {
            env::set_var("RUST_LOG", "info,cranelift_jit=warn,cranelift_codegen=warn");
        }
    }

    env_logger::init();

    let cli: Cli = args::Driver::new(
        args::builder::<Cli>()
            .expect("failed to build CLI")
            .cli(|c| c.args(env::args().skip(1)))
            .help(|h| {
                h.program_name(env!("CARGO_PKG_NAME"))
                    .version(env!("CARGO_PKG_VERSION"))
            })
            .build(),
    )
    .run()
    .unwrap();

    match cli.command {
        Command::Record(args) => run_record(args)?,
        Command::Setup(args) => cmd_setup_mac::main(args)?,
        Command::Status => block_on_async(async { run_status().await })?,
        Command::List => block_on_async(async { run_list().await })?,
        Command::Wait(args) => block_on_async(async { run_wait(args).await })?,
        Command::Stop => block_on_async(async { run_stop().await })?,
        Command::Top(args) => block_on_async(async { run_top(args).await })?,
        Command::Annotate(args) => block_on_async(async { run_annotate(args).await })?,
    }
    Ok(())
}

fn main() {
    if let Err(error) = main_impl() {
        eprintln!("error: {error}");
        exit(1);
    }
}

fn block_on_async<F: std::future::Future<Output = Result<(), Box<dyn Error>>>>(
    fut: F,
) -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(fut)
}

fn run_record(args: RecordArgs) -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let (live_sink, _forwarder): (Option<Box<dyn stax_core::live_sink::LiveSink>>, _) =
        if let Some(ref addr) = args.serve {
            let (sink, _server_handle) = runtime.block_on(stax_live::start(addr))?;
            (Some(Box::new(sink)), None)
        } else if let Some(socket) = stax_server_socket() {
            match runtime.block_on(connect_to_server(&socket, &args)) {
                Ok((id, sink, fwd)) => {
                    eprintln!(
                        "stax: registered run {} with stax-server at {}",
                        id.0,
                        socket.display()
                    );
                    (Some(Box::new(sink)), Some(fwd))
                }
                Err(e) => {
                    eprintln!("stax: stax-server unreachable ({e}); recording without a sink");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

    let result = cmd_record_mac::main_with_live_sink(args, live_sink);
    drop(runtime);
    result
}

fn stax_server_socket() -> Option<PathBuf> {
    if let Ok(p) = env::var("STAX_SERVER_SOCKET") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    if let Ok(rt) = env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt).join("stax-server.sock");
        if p.exists() {
            return Some(p);
        }
    }
    let uid = unsafe { libc::getuid() };
    let p = PathBuf::from(format!("/tmp/stax-server-{uid}.sock"));
    p.exists().then_some(p)
}

async fn connect_to_server(
    socket: &std::path::Path,
    args: &RecordArgs,
) -> eyre::Result<(
    stax_live_proto::RunId,
    stax_core::ingest_sink::IngestSink,
    tokio::task::JoinHandle<()>,
)> {
    let label = args
        .command
        .first()
        .cloned()
        .unwrap_or_else(|| match args.pid {
            Some(p) => format!("pid {p}"),
            None => "(unnamed)".to_owned(),
        });
    let config = stax_live_proto::RunConfig {
        label,
        frequency_hz: args.frequency,
    };
    stax_core::ingest_sink::connect_and_register(&socket.to_string_lossy(), config).await
}

// --- agent-facing subcommands ------------------------------------------

fn require_server_socket() -> Result<String, Box<dyn Error>> {
    let socket = stax_server_socket().ok_or_else(|| {
        format!(
            "stax-server isn't running. \
             Start it with `stax-server` (or set STAX_SERVER_SOCKET if you've moved the socket)."
        )
    })?;
    Ok(format!("local://{}", socket.display()))
}

async fn run_status() -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let status = client.status().await.map_err(|e| format!("{e:?}"))?;
    print_server_status(&status);
    Ok(())
}

async fn run_list() -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let runs = client.list_runs().await.map_err(|e| format!("{e:?}"))?;
    if runs.is_empty() {
        println!("(no runs)");
    } else {
        for run in runs {
            print_run_one_line(&run);
        }
    }
    Ok(())
}

async fn run_wait(args: WaitArgs) -> Result<(), Box<dyn Error>> {
    let condition = match (args.for_samples, args.for_seconds, args.until_symbol) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
            return Err(
                "--for-samples, --for-seconds, --until-symbol are mutually exclusive".into(),
            );
        }
        (Some(count), _, _) => WaitCondition::ForSamples { count },
        (_, Some(seconds), _) => WaitCondition::ForSeconds { seconds },
        (_, _, Some(needle)) => WaitCondition::UntilSymbolSeen { needle },
        _ => WaitCondition::UntilStopped,
    };

    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let outcome = client
        .wait_active(condition, args.timeout_ms)
        .await
        .map_err(|e| format!("{e:?}"))?;
    match outcome {
        WaitOutcome::ConditionMet { summary } => {
            println!("condition met:");
            print_run_one_line(&summary);
        }
        WaitOutcome::Stopped { summary } => {
            println!("run stopped:");
            print_run_one_line(&summary);
        }
        WaitOutcome::TimedOut { summary } => {
            println!("timed out:");
            print_run_one_line(&summary);
            return Err("timed out waiting".into());
        }
        WaitOutcome::NoActiveRun => {
            return Err("no active run".into());
        }
    }
    Ok(())
}

async fn run_stop() -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let result = client.stop_active().await;
    match result {
        Ok(summary) => {
            println!("stopped:");
            print_run_one_line(&summary);
        }
        Err(vox::VoxError::User(msg)) => return Err(format!("{msg}").into()),
        Err(e) => return Err(format!("{e:?}").into()),
    }
    Ok(())
}

async fn run_top(args: TopArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let sort = match args.sort.as_str() {
        "self" => TopSort::BySelf,
        "total" => TopSort::ByTotal,
        other => return Err(format!("unknown --sort value {other:?} (use `self` or `total`)").into()),
    };
    let client: ProfilerClient = vox::connect(&url).await?;
    let entries = client
        .top(
            args.limit,
            sort,
            ViewParams {
                tid: args.tid,
                filter: LiveFilter {
                    time_range: None,
                    exclude_symbols: Vec::new(),
                },
            },
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    if entries.is_empty() {
        println!("(no samples yet — is a recording in progress?)");
        return Ok(());
    }
    for e in entries {
        let name = e.function_name.as_deref().unwrap_or("<unresolved>");
        let bin = e.binary.as_deref().unwrap_or("?");
        println!(
            "{:>10.3}ms  {:>8} samples  {} ({})",
            e.self_on_cpu_ns as f64 / 1e6,
            e.self_pet_samples,
            name,
            bin,
        );
    }
    Ok(())
}

async fn run_annotate(args: AnnotateArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let address = parse_address(&args.address)?;
    let client: ProfilerClient = vox::connect(&url).await?;
    // subscribe_annotated streams updates every ~250ms; we want a one-shot
    // snapshot, so take the first item and drop the channel.
    let (tx, mut rx) = vox::channel();
    client
        .subscribe_annotated(
            address,
            ViewParams {
                tid: args.tid,
                filter: LiveFilter {
                    time_range: None,
                    exclude_symbols: Vec::new(),
                },
            },
            tx,
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    let view_sref = rx
        .recv()
        .await
        .map_err(|e| format!("{e:?}"))?
        .ok_or("annotate stream closed before sending an update")?;
    view_sref.map(|view| {
        println!(
            "; {} ({}) @ {:#x}",
            view.function_name, view.language, view.base_address
        );
        for line in view.lines {
            if let Some(hdr) = &line.source_header
                && !hdr.file.is_empty()
            {
                println!("; {}:{}", hdr.file, hdr.line);
            }
            // .html carries arborium-tagged HTML; strip the tags for a
            // plain-text terminal view.
            let plain = strip_html_tags(&line.html);
            println!(
                "  {:#x}  {:>5} samples  {}",
                line.address, line.self_pet_samples, plain
            );
        }
    });
    Ok(())
}

fn parse_address(raw: &str) -> Result<u64, Box<dyn Error>> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        return u64::from_str_radix(rest, 16)
            .map_err(|e| format!("invalid hex address {raw:?}: {e}").into());
    }
    trimmed
        .parse::<u64>()
        .map_err(|e| format!("invalid address {raw:?}: {e}").into())
}

/// Naive HTML-tag stripper for arborium output. Arborium emits things
/// like `<a-k>mov</a-k>` (custom elements, no attributes); this drops
/// every `<…>` run without trying to be clever.
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn print_server_status(status: &ServerStatus) {
    if let Some(active) = status.active.first() {
        println!("active run:");
        print_run_one_line(active);
    } else {
        println!("no active run");
    }
}

fn print_run_one_line(run: &RunSummary) {
    let pid = run
        .target_pid
        .map(|p| format!("pid {p}"))
        .unwrap_or_else(|| "no pid".to_owned());
    let state = match run.state {
        stax_live_proto::RunState::Recording => "recording",
        stax_live_proto::RunState::Stopped => "stopped",
    };
    println!(
        "  run {}  [{state}]  {}  {} samples / {} intervals  ({})",
        run.id.0, pid, run.pet_samples, run.off_cpu_intervals, run.label
    );
}
