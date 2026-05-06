use std::env;
use std::error::Error;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::exit;

use figue as args;
use stax_core::{
    args::{AnnotateArgs, Cli, Command, FlameArgs, RecordArgs, ThreadsArgs, TopArgs, WaitArgs},
    cmd_setup_mac,
};
use stax_live_proto::{
    DiagnosticsSnapshot, FlameNode, FlamegraphUpdate, LaunchEnvVar, LaunchRequest, LiveFilter,
    OffCpuBreakdown, ProfilerClient, RunControlClient, RunSummary, ServerStatus, StopReason,
    TerminalInput, TerminalOutput, TerminalSize, ThreadsUpdate, TopSort, ViewParams,
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
    init_tracing();
    let _vox_sigusr1_dump = stax_vox_observe::install_global_sigusr1_dump("stax");

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
        Command::Diagnose => block_on_async(async { run_diagnose().await })?,
        Command::Dump => run_dump()?,
        Command::Wait(args) => block_on_async(async { run_wait(args).await })?,
        Command::Stop => block_on_async(async { run_stop().await })?,
        Command::Top(args) => block_on_async(async { run_top(args).await })?,
        Command::Annotate(args) => block_on_async(async { run_annotate(args).await })?,
        Command::Flame(args) => block_on_async(async { run_flame(args).await })?,
        Command::Threads(args) => block_on_async(async { run_threads(args).await })?,
    }
    Ok(())
}

fn main() {
    if let Err(error) = main_impl() {
        eprintln!("error: {error}");
        exit(1);
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,stax=info,stax_vox_observe=info"));
    let oslog = tracing_oslog::OsLogger::new("eu.bearcove.stax", "default");
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(oslog)
        .try_init();
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
    block_on_async(async { run_record_async(args).await })
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

// --- agent-facing subcommands ------------------------------------------

fn run_dump() -> Result<(), Box<dyn Error>> {
    let self_pid = std::process::id();
    let mut targets = Vec::new();
    for name in ["staxd", "stax-server", "stax-shade", "stax"] {
        for pid in pids_by_exact_process_name(name)? {
            if pid != self_pid {
                targets.push(DumpTarget {
                    name: name.to_owned(),
                    pid,
                });
            }
        }
    }
    targets.sort_by(|a, b| (a.pid, &a.name).cmp(&(b.pid, &b.name)));
    targets.dedup_by_key(|target| target.pid);

    if targets.is_empty() {
        println!("no stax processes found");
        return Ok(());
    }

    let mut failed = false;
    for target in targets {
        let rc = unsafe { libc::kill(target.pid as libc::pid_t, libc::SIGUSR1) };
        if rc == 0 {
            println!("signaled {} pid {}", target.name, target.pid);
        } else {
            failed = true;
            eprintln!(
                "failed to signal {} pid {}: {}",
                target.name,
                target.pid,
                std::io::Error::last_os_error()
            );
        }
    }

    if failed {
        Err("one or more dump signals failed".into())
    } else {
        Ok(())
    }
}

struct DumpTarget {
    name: String,
    pid: u32,
}

fn pids_by_exact_process_name(name: &str) -> Result<Vec<u32>, Box<dyn Error>> {
    let output = std::process::Command::new("pgrep")
        .args(["-x", name])
        .output()?;
    if output.status.success() {
        return String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                line.parse::<u32>()
                    .map_err(|e| format!("pgrep returned invalid pid {line:?}: {e}").into())
            })
            .collect();
    }
    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    Err(format!(
        "pgrep -x {name} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
    .into())
}

async fn run_record_async(args: RecordArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("record", &client);
    let target = args.target()?;
    let label = args
        .command
        .first()
        .cloned()
        .unwrap_or_else(|| "(attached)".to_owned());
    let config = stax_live_proto::RunConfig {
        label,
        frequency_hz: args.frequency,
    };

    let mut terminal_relay = None;
    let run_id = match target {
        stax_core::args::TargetProcess::ByPid(pid) => client
            .start_attach(pid, config, args.daemon_socket.clone(), args.time_limit)
            .await
            .map_err(|e| format!("{e:?}"))?,
        stax_core::args::TargetProcess::Launch {
            program,
            args: rest,
        } => {
            let mut command = Vec::with_capacity(1 + rest.len());
            command.push(program);
            command.extend(rest);
            let (terminal_input_tx, terminal_input_rx) = vox::channel::<TerminalInput>();
            let (terminal_output_tx, terminal_output_rx) = vox::channel::<TerminalOutput>();
            let terminal_size = current_terminal_size_or_default();
            let request = LaunchRequest {
                command,
                cwd: env::current_dir()?.to_string_lossy().into_owned(),
                env: env::vars_os()
                    .filter_map(|(key, value)| {
                        Some(LaunchEnvVar {
                            key: key.into_string().ok()?,
                            value: value.into_string().ok()?,
                        })
                    })
                    .collect(),
                config,
                daemon_socket: args.daemon_socket.clone(),
                time_limit_secs: args.time_limit,
                terminal_size: Some(terminal_size),
            };
            let run_id = client
                .start_launch(request, terminal_input_rx, terminal_output_tx)
                .await
                .map_err(|e| format!("{e:?}"))?;
            terminal_relay = Some(TerminalRelay::start(
                terminal_input_tx,
                terminal_output_rx,
                Some(terminal_size),
            ));
            run_id
        }
    };
    eprintln!("stax: started run {}", run_id.0);

    let wait_client = client.clone();
    tokio::select! {
        outcome = wait_client.wait_active(WaitCondition::UntilStopped, None) => {
            match outcome.map_err(|e| format!("{e:?}"))? {
                WaitOutcome::Stopped { summary } => {
                    drop(terminal_relay.take());
                    println!("stopped:");
                    print_run_one_line(&summary);
                    fail_on_recorder_error(&summary)?;
                }
                WaitOutcome::NoActiveRun => {
                    drop(terminal_relay.take());
                    print_finished_run_or_message(&client, run_id).await?;
                }
                other => {
                    eprintln!("stax: unexpected wait outcome: {other:?}");
                }
            }
        }
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|e| format!("waiting for Ctrl-C: {e}"))?;
            let summary = client.stop_active().await.map_err(|e| format!("{e:?}"))?;
            drop(terminal_relay.take());
            println!("stopped:");
            print_run_one_line(&summary);
        }
    }
    drop(terminal_relay);
    Ok(())
}

async fn print_finished_run_or_message(
    client: &RunControlClient,
    run_id: stax_live_proto::RunId,
) -> Result<(), Box<dyn Error>> {
    let runs = client.list_runs().await.map_err(|e| format!("{e:?}"))?;
    let Some(summary) = runs.into_iter().find(|run| run.id == run_id) else {
        eprintln!("stax: run ended before wait attached");
        return Ok(());
    };
    println!("stopped:");
    print_run_one_line(&summary);
    fail_on_recorder_error(&summary)?;
    Ok(())
}

fn fail_on_recorder_error(summary: &RunSummary) -> Result<(), Box<dyn Error>> {
    if let Some(StopReason::RecorderError { message }) = &summary.stop_reason {
        return Err(format!("recorder failed: {message}").into());
    }
    Ok(())
}

struct TerminalRelay {
    _raw_mode: Option<RawMode>,
}

impl TerminalRelay {
    fn start(
        terminal_input: vox::Tx<TerminalInput>,
        mut terminal_output: vox::Rx<TerminalOutput>,
        initial_size: Option<TerminalSize>,
    ) -> Self {
        let raw_mode = RawMode::enable().ok().flatten();
        let (input_events_tx, mut input_events_rx) =
            tokio::sync::mpsc::unbounded_channel::<TerminalInput>();

        if let Some(size) = initial_size {
            let _ = input_events_tx.send(TerminalInput::Resize { size });
        }

        let stdin_events = input_events_tx.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 8192];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => {
                        let _ = stdin_events.send(TerminalInput::Close);
                        break;
                    }
                    Ok(n) => {
                        if stdin_events
                            .send(TerminalInput::Bytes {
                                data: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        let _ = stdin_events.send(TerminalInput::Close);
                        break;
                    }
                }
            }
        });

        #[cfg(unix)]
        {
            let resize_events = input_events_tx.clone();
            tokio::spawn(async move {
                if let Ok(mut sigwinch) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                {
                    while sigwinch.recv().await.is_some() {
                        if let Some(size) = current_terminal_size() {
                            let _ = resize_events.send(TerminalInput::Resize { size });
                        }
                    }
                }
            });
        }

        tokio::spawn(async move {
            while let Some(event) = input_events_rx.recv().await {
                if terminal_input.send(event).await.is_err() {
                    break;
                }
            }
            let _ = terminal_input.close(Default::default()).await;
        });

        tokio::spawn(async move {
            let mut stdout = std::io::stdout();
            loop {
                match terminal_output.recv().await {
                    Ok(Some(output_sref)) => {
                        let mut output = None;
                        let _ = output_sref.map(|value| {
                            output = Some(value);
                        });
                        match output.expect("output set") {
                            TerminalOutput::Bytes { data } => {
                                let _ = stdout.write_all(&data);
                                let _ = stdout.flush();
                            }
                            TerminalOutput::ExitStatus { .. } => {}
                            TerminalOutput::Error { message } => {
                                eprintln!("stax terminal: {message}");
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("stax terminal recv failed: {e:?}");
                        break;
                    }
                }
            }
        });

        Self {
            _raw_mode: raw_mode,
        }
    }
}

struct RawMode {
    fd: libc::c_int,
    original: libc::termios,
}

impl RawMode {
    fn enable() -> std::io::Result<Option<Self>> {
        let fd = libc::STDIN_FILENO;
        if unsafe { libc::isatty(fd) } == 0 {
            return Ok(None);
        }
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
            // Keep Ctrl-C/Ctrl-\ signal generation enabled so the
            // CLI can still be interrupted while in terminal relay mode.
            raw.c_lflag |= libc::ISIG;
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Some(Self { fd, original }))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

fn current_terminal_size() -> Option<TerminalSize> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
    let r = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) };
    if r != 0 {
        return None;
    }
    let size = unsafe { size.assume_init() };
    if size.ws_row == 0 || size.ws_col == 0 {
        return None;
    }
    Some(TerminalSize {
        rows: size.ws_row,
        cols: size.ws_col,
    })
}

fn current_terminal_size_or_default() -> TerminalSize {
    current_terminal_size().unwrap_or(TerminalSize { rows: 24, cols: 80 })
}

fn require_server_socket() -> Result<String, Box<dyn Error>> {
    let socket = stax_server_socket().ok_or_else(|| {
        "stax-server isn't running. \
             Start it with `stax-server` (or set STAX_SERVER_SOCKET if you've moved the socket)."
            .to_string()
    })?;
    Ok(format!("local://{}", socket.display()))
}

fn register_run_control_client(
    surface: &'static str,
    client: &RunControlClient,
) -> stax_vox_observe::VoxDebugRegistration {
    stax_vox_observe::register_global_caller("stax", surface, "RunControl", &client.caller)
}

fn register_profiler_client(
    surface: &'static str,
    client: &ProfilerClient,
) -> stax_vox_observe::VoxDebugRegistration {
    stax_vox_observe::register_global_caller("stax", surface, "Profiler", &client.caller)
}

async fn run_status() -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("status", &client);
    let status = client.status().await.map_err(|e| format!("{e:?}"))?;
    print_server_status(&status);
    Ok(())
}

async fn run_list() -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("list", &client);
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

async fn run_diagnose() -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("diagnose", &client);
    let snapshot = client.diagnostics().await.map_err(|e| format!("{e:?}"))?;
    print_diagnostics(&snapshot);
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
    let _debug_registration = register_run_control_client("wait", &client);
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
    let _debug_registration = register_run_control_client("stop", &client);
    let result = client.stop_active().await;
    match result {
        Ok(summary) => {
            println!("stopped:");
            print_run_one_line(&summary);
        }
        Err(vox::VoxError::User(err)) => return Err(format!("{err:?}").into()),
        Err(e) => return Err(format!("{e:?}").into()),
    }
    Ok(())
}

async fn run_top(args: TopArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let sort = match args.sort.as_str() {
        "self" => TopSort::BySelf,
        "total" => TopSort::ByTotal,
        other => {
            return Err(format!("unknown --sort value {other:?} (use `self` or `total`)").into());
        }
    };
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("top", &client);
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
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("annotate", &client);
    let view_params = ViewParams {
        tid: args.tid,
        filter: LiveFilter {
            time_range: None,
            exclude_symbols: Vec::new(),
        },
    };
    let address = resolve_target(&client, &args.target, view_params.clone()).await?;
    let view = client
        .annotated(address, view_params)
        .await
        .map_err(|e| format!("{e:?}"))?;
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
        // Token classes don't carry colour info on the terminal path;
        // just concatenate the text runs for a plain-text view.
        let plain: String = line.tokens.iter().map(|t| t.text.as_str()).collect();
        println!(
            "  {:#x}  {:>5} samples  {}",
            line.address, line.self_pet_samples, plain
        );
    }
    Ok(())
}

async fn run_threads(args: ThreadsArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("threads", &client);
    let update = client.threads().await.map_err(|e| format!("{e:?}"))?;
    print_threads(&update, args.limit);
    Ok(())
}

fn print_threads(update: &ThreadsUpdate, limit: u32) {
    let mut threads: Vec<&stax_live_proto::ThreadInfo> = update.threads.iter().collect();
    threads.sort_by(|a, b| b.on_cpu_ns.cmp(&a.on_cpu_ns));
    if threads.is_empty() {
        println!("(no thread samples yet — is a recording in progress?)");
        return;
    }
    println!(
        "{:>10} {:>10} {:>10} {:>9}  tid    name",
        "on-CPU ms", "off-CPU ms", "samples", "blocked",
    );
    let take = if limit == 0 {
        threads.len()
    } else {
        limit as usize
    };
    for t in threads.iter().take(take) {
        let off_total = off_cpu_total_ns(&t.off_cpu);
        let dominant = dominant_off_cpu_reason(&t.off_cpu);
        println!(
            "{:>10.2} {:>10.2} {:>10} {:>9}  {:<6} {}",
            t.on_cpu_ns as f64 / 1e6,
            off_total as f64 / 1e6,
            t.pet_samples,
            dominant,
            t.tid,
            t.name.as_deref().unwrap_or("(unnamed)"),
        );
    }
    if threads.len() > take {
        println!("…{} more threads", threads.len() - take);
    }
}


/// Pick the largest field of the off-CPU breakdown so the user can
/// see at a glance whether a thread was idle vs. blocked vs. doing
/// IO. Returns the bucket name padded to a stable width.
fn dominant_off_cpu_reason(b: &OffCpuBreakdown) -> &'static str {
    let buckets: [(u64, &str); 10] = [
        (b.idle_ns, "idle"),
        (b.lock_ns, "lock"),
        (b.semaphore_ns, "sem"),
        (b.ipc_ns, "ipc"),
        (b.io_read_ns, "ioR"),
        (b.io_write_ns, "ioW"),
        (b.readiness_ns, "ready"),
        (b.sleep_ns, "sleep"),
        (b.connect_ns, "conn"),
        (b.other_ns, "other"),
    ];
    let mut best = ("-", 0u64);
    for (ns, name) in buckets {
        if ns > best.1 {
            best = (name, ns);
        }
    }
    best.0
}

async fn run_flame(args: FlameArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("flame", &client);
    let update = client
        .flamegraph(ViewParams {
            tid: args.tid,
            filter: LiveFilter {
                time_range: None,
                exclude_symbols: Vec::new(),
            },
        })
        .await
        .map_err(|e| format!("{e:?}"))?;
    print_flame(&update, args.max_depth, args.threshold_pct);
    Ok(())
}

fn print_flame(update: &FlamegraphUpdate, max_depth: usize, threshold_pct: f64) {
    let total = update.total_on_cpu_ns.max(1) as f64;
    println!(
        "# stax flame · total on-CPU {:.3}s · off-CPU {:.3}s",
        update.total_on_cpu_ns as f64 / 1e9,
        off_cpu_total_ns(&update.total_off_cpu) as f64 / 1e9,
    );
    if let Some(tid) = update.root.children.first().and(None::<u32>) {
        // placeholder — root has no tid annotation; left as a hook
        // for future per-thread renders.
        let _ = tid;
    }
    println!();
    println!("```");
    print_flame_node(
        &update.root,
        &update.strings,
        total,
        threshold_pct,
        0,
        max_depth,
    );
    println!("```");
}

fn off_cpu_total_ns(b: &OffCpuBreakdown) -> u64 {
    b.idle_ns
        + b.lock_ns
        + b.semaphore_ns
        + b.ipc_ns
        + b.io_read_ns
        + b.io_write_ns
        + b.readiness_ns
        + b.sleep_ns
        + b.connect_ns
        + b.other_ns
}

fn print_flame_node(
    node: &FlameNode,
    strings: &[String],
    total_ns: f64,
    threshold_pct: f64,
    depth: usize,
    max_depth: usize,
) {
    let pct = node.on_cpu_ns as f64 / total_ns * 100.0;
    if depth > 0 && pct < threshold_pct {
        return;
    }

    let label = if depth == 0 {
        "(root)".to_owned()
    } else {
        let name = node
            .function_name
            .and_then(|i| strings.get(i as usize).map(String::as_str))
            .unwrap_or("<unresolved>");
        let bin = node
            .binary
            .and_then(|i| strings.get(i as usize).map(String::as_str))
            .unwrap_or("?");
        format!("{name}  ({bin})")
    };
    let indent = "  ".repeat(depth);
    println!(
        "{:>8.2}ms {:>5.1}%  {indent}{prefix}{label}",
        node.on_cpu_ns as f64 / 1e6,
        pct,
        indent = indent,
        prefix = if depth == 0 { "" } else { "└─ " },
        label = label,
    );

    if depth + 1 > max_depth {
        if !node.children.is_empty() {
            let truncated = node.children.len();
            println!(
                "{indent}   …{truncated} more frame{plural}",
                indent = "  ".repeat(depth + 1),
                truncated = truncated,
                plural = if truncated == 1 { "" } else { "s" }
            );
        }
        return;
    }

    // Sort children by on_cpu_ns descending for a focused view.
    let mut children: Vec<&FlameNode> = node.children.iter().collect();
    children.sort_by(|a, b| b.on_cpu_ns.cmp(&a.on_cpu_ns));
    for child in children {
        print_flame_node(
            child,
            strings,
            total_ns,
            threshold_pct,
            depth + 1,
            max_depth,
        );
    }
}

fn parse_address(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))?;
    u64::from_str_radix(rest, 16).ok()
}

/// Look up the address to feed to `subscribe_annotated`. `target`
/// is either a hex address (returned as-is) or a substring of a
/// demangled function name; in the latter case we ask the server
/// for the top-N leaf-self functions and return the hottest one
/// whose name contains the substring (case-insensitive).
async fn resolve_target(
    client: &ProfilerClient,
    target: &str,
    params: ViewParams,
) -> Result<u64, Box<dyn Error>> {
    if let Some(addr) = parse_address(target) {
        return Ok(addr);
    }
    let needle = target.to_lowercase();
    // 256 entries is enough to catch any function the user is
    // realistically asking about; we sort by self_pet_samples on
    // the server side already.
    let entries = client
        .top(256, TopSort::BySelf, params)
        .await
        .map_err(|e| format!("{e:?}"))?;
    if entries.is_empty() {
        return Err("no samples on the server (run a recording first, then retry)".into());
    }
    let hit = entries.iter().find(|e| {
        e.function_name
            .as_deref()
            .map(|n| n.to_lowercase().contains(&needle))
            .unwrap_or(false)
    });
    match hit {
        Some(e) => {
            eprintln!(
                "stax: matched {:?} → {} ({} self samples)",
                target,
                e.function_name.as_deref().unwrap_or("<unresolved>"),
                e.self_pet_samples,
            );
            Ok(e.address)
        }
        None => {
            // Help the user out by showing what *did* land in top.
            let mut suggestions: Vec<&str> = entries
                .iter()
                .filter_map(|e| e.function_name.as_deref())
                .take(8)
                .collect();
            suggestions.dedup();
            let hint = if suggestions.is_empty() {
                String::new()
            } else {
                format!(
                    "\nhottest names in this run:\n  - {}",
                    suggestions.join("\n  - "),
                )
            };
            Err(format!("no symbol matching {target:?} in the current run{hint}").into())
        }
    }
}

fn print_server_status(status: &ServerStatus) {
    if let Some(active) = status.active.first() {
        println!("active run:");
        print_run_one_line(active);
    } else {
        println!("no active run");
    }
}

fn print_diagnostics(snapshot: &DiagnosticsSnapshot) {
    println!("stax diagnostics");
    if let Some(active) = snapshot.active.first() {
        println!("active run:");
        print_run_one_line(active);
    } else {
        println!("active run: none");
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
        "  run {}  [{state}]  {}  {} kperf / {} intervals  ({})",
        run.id.0, pid, run.pet_samples, run.off_cpu_intervals, run.label
    );
}
