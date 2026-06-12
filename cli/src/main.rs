use std::collections::{BTreeSet, HashMap};
use std::env;
use std::error::Error;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::exit;

use figue as args;
use stax_core::args::{
    AnnotateArgs, ArchiveArgs, Cli, Command, CompareArgs, DiagnoseArgs, FlameArgs, RecordArgs,
    ThreadsArgs, TopArgs, WaitArgs,
};
#[cfg(target_os = "linux")]
use stax_core::cmd_setup_linux;
#[cfg(target_os = "macos")]
use stax_core::cmd_setup_mac;
use stax_live_proto::{
    DiagnosticsSnapshot, FlameNode, FlamegraphUpdate, LiveFilter, OffCpuBreakdown, ProfilerClient,
    RunControlClient, RunId, RunSummary, SavedIntervalKind, SavedRunArchive,
    SavedRunArchiveManifest, ServerStatus, StopReason, TargetIngestDiagnostics, ThreadsUpdate,
    TopEntry, TopSort, TopUpdate, ViewParams, WaitCondition, WaitOutcome,
};

#[cfg(target_os = "macos")]
mod launch;
#[cfg(target_os = "macos")]
use launch::TerminalSize;

#[cfg(target_os = "linux")]
mod launch_linux;

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
        #[cfg(target_os = "macos")]
        Command::Setup(args) => cmd_setup_mac::main(args)?,
        #[cfg(target_os = "linux")]
        Command::Setup(args) => cmd_setup_linux::main(args)?,
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Command::Setup(_args) => {
            return Err("stax setup is supported on macOS and Linux only".into());
        }
        Command::Status => block_on_async(async { run_status().await })?,
        Command::List => block_on_async(async { run_list().await })?,
        Command::Diagnose(args) => block_on_async(async { run_diagnose(args).await })?,
        Command::Dump => run_dump()?,
        Command::Wait(args) => block_on_async(async { run_wait(args).await })?,
        Command::Stop => block_on_async(async { run_stop().await })?,
        Command::Save(args) => block_on_async(async { run_save(args).await })?,
        Command::Open(args) => block_on_async(async { run_open(args).await })?,
        Command::SelectRun(args) => block_on_async(async { run_select_run(args).await })?,
        Command::Compare(args) => run_compare(args)?,
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
    #[cfg(target_os = "macos")]
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_oslog::OsLogger::new("eu.bearcove.stax", "default"))
        .try_init();
    #[cfg(not(target_os = "macos"))]
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
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
    for name in ["staxd", "stax-server", "stax"] {
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

/// Resolve whether this run uses `.eh_frame` DWARF unwinding.
///
/// On x86_64 Linux it is **on by default**: the system libc is built
/// `-fomit-frame-pointer`, so the kernel's frame-pointer `CALLCHAIN`
/// truncates for any sample landing in libc. It is off everywhere
/// else — macOS kperf already walks full user stacks, and aarch64
/// keeps a frame pointer by ABI. `--no-dwarf-unwind` forces it off;
/// `STAX_DWARF_UNWIND` (`0`/`off` or `1`/`on`) overrides either way.
fn resolve_dwarf_unwind(args: &RecordArgs) -> bool {
    if args.no_dwarf_unwind {
        return false;
    }
    if let Some(v) = env::var_os("STAX_DWARF_UNWIND") {
        let v = v.to_string_lossy();
        let v = v.trim();
        if !v.is_empty() {
            return !matches!(v, "0" | "false" | "off" | "no");
        }
    }
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
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
        dwarf_unwind: resolve_dwarf_unwind(&args),
    };

    match target {
        stax_core::args::TargetProcess::ByPid(pid) => {
            let run_id = client
                .start_attach(pid, config, args.daemon_socket.clone(), args.time_limit)
                .await
                .map_err(|e| format!("{e:?}"))?;
            eprintln!("stax: started run {}", run_id.0);
            wait_on_run(&client, run_id, None).await
        }
        stax_core::args::TargetProcess::Launch {
            program,
            args: rest,
        } => {
            #[cfg(target_os = "macos")]
            {
                run_record_launch(client, args, program, rest, config).await
            }
            #[cfg(target_os = "linux")]
            {
                run_record_launch_linux(client, args, program, rest, config).await
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                let _ = (program, rest, config);
                return Err("stax record -- <argv> is unsupported on this OS".into());
            }
        }
    }
}

#[cfg(target_os = "macos")]
async fn run_record_launch(
    client: RunControlClient,
    args: RecordArgs,
    program: String,
    rest: Vec<String>,
    config: stax_live_proto::RunConfig,
) -> Result<(), Box<dyn Error>> {
    let mut argv = Vec::with_capacity(1 + rest.len());
    argv.push(program);
    argv.extend(rest);
    let terminal_size = current_terminal_size();
    let cwd = env::current_dir()?.to_string_lossy().into_owned();
    let mut launched = launch::posix_spawn_suspended(&argv, Some(&cwd), terminal_size)
        .map_err(|e| format!("posix_spawn: {e}"))?;
    let target_pid = launched.pid;
    eprintln!("stax: spawned {} (pid {}, suspended)", argv[0], target_pid);

    let _raw_mode = RawMode::enable().ok().flatten();
    launched.start_pty_pump();
    let launched = std::sync::Arc::new(launched);

    let run_id = match client
        .start_attach(
            target_pid,
            config,
            args.daemon_socket.clone(),
            args.time_limit,
        )
        .await
    {
        Ok(id) => id,
        Err(e) => {
            launched.terminate();
            return Err(format!("{e:?}").into());
        }
    };
    eprintln!("stax: started run {}", run_id.0);

    launched
        .resume()
        .map_err(|e| format!("resume launched target: {e}"))?;

    // Background poller for SIGWINCH so the target sees terminal
    // resizes. Best-effort.
    spawn_sigwinch_pump(launched.clone());

    let child_exit_watcher = {
        let pid = target_pid;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                tick.tick().await;
                let mut status = 0;
                let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if r == pid as libc::pid_t {
                    return Some(status);
                }
                if r == -1 {
                    return None;
                }
            }
        })
    };

    let result = wait_on_run_with_child(&client, run_id, child_exit_watcher).await;
    drop(launched);
    drop(_raw_mode);
    result
}

/// Linux `stax record -- <argv>`: fork the child into a paused state,
/// attach the perf session, then resume so the very first target
/// instruction is sampled. Stdio is inherited (the child shares our
/// terminal), so there is no PTY relay / raw-mode / SIGWINCH plumbing.
///
/// Unlike macOS, Linux has no `posix_spawn(START_SUSPENDED)` and a
/// `pre_exec(SIGSTOP)` against `std::Command` deadlocks because
/// `spawn()` itself blocks on the child's exec sync pipe. So the
/// pause is built out of a parent→child "go" pipe — see
/// [`launch_linux::fork_suspended`].
#[cfg(target_os = "linux")]
async fn run_record_launch_linux(
    client: RunControlClient,
    args: RecordArgs,
    program: String,
    rest: Vec<String>,
    config: stax_live_proto::RunConfig,
) -> Result<(), Box<dyn Error>> {
    let mut argv = Vec::with_capacity(1 + rest.len());
    argv.push(program.clone());
    argv.extend(rest);
    let mut launched =
        launch_linux::fork_suspended(&argv).map_err(|e| format!("fork {program}: {e}"))?;
    let target_pid = launched.pid;
    eprintln!("stax: forked {program} (pid {target_pid}, paused)");

    let run_id = match client
        .start_attach(
            target_pid,
            config,
            args.daemon_socket.clone(),
            args.time_limit,
        )
        .await
    {
        Ok(id) => id,
        Err(e) => {
            launched.terminate();
            return Err(format!("{e:?}").into());
        }
    };
    eprintln!("stax: started run {}", run_id.0);

    // Now that perf is attached, unblock the child so it `execvp`s
    // the target — the kernel's `PERF_RECORD_MMAP*` for the new
    // program text fires through the events we just opened, so we
    // sample the target from its very first instruction.
    if let Err(e) = launched.resume() {
        launched.terminate();
        return Err(format!("resume launched target: {e}").into());
    }

    let child_exit_watcher = {
        let pid = target_pid;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                tick.tick().await;
                let mut status = 0;
                let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if r == pid as libc::pid_t {
                    return Some(status);
                }
                if r == -1 {
                    return None;
                }
            }
        })
    };

    let result = wait_on_run_with_child(&client, run_id, child_exit_watcher).await;
    // If the run stopped first (Ctrl-C / time limit) the target may
    // still be alive; tear it down. If it already exited, the watcher
    // reaped it and these are harmless no-ops (ESRCH/ECHILD).
    launched.terminate();
    result
}

async fn wait_on_run(
    client: &RunControlClient,
    run_id: stax_live_proto::RunId,
    _terminal: Option<()>,
) -> Result<(), Box<dyn Error>> {
    let wait_client = client.clone();
    tokio::select! {
        outcome = wait_client.wait_active(WaitCondition::UntilStopped, None) => {
            match outcome.map_err(|e| format!("{e:?}"))? {
                WaitOutcome::Stopped { summary } => {
                    println!("stopped:");
                    print_run_one_line(&summary);
                    fail_on_recorder_error(&summary)?;
                }
                WaitOutcome::NoActiveRun => {
                    print_finished_run_or_message(client, run_id).await?;
                }
                other => {
                    eprintln!("stax: unexpected wait outcome: {other:?}");
                }
            }
        }
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|e| format!("waiting for Ctrl-C: {e}"))?;
            let summary = client.stop_active().await.map_err(|e| format!("{e:?}"))?;
            println!("stopped:");
            print_run_one_line(&summary);
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn wait_on_run_with_child(
    client: &RunControlClient,
    run_id: stax_live_proto::RunId,
    child_watcher: tokio::task::JoinHandle<Option<libc::c_int>>,
) -> Result<(), Box<dyn Error>> {
    let wait_client = client.clone();
    tokio::select! {
        outcome = wait_client.wait_active(WaitCondition::UntilStopped, None) => {
            match outcome.map_err(|e| format!("{e:?}"))? {
                WaitOutcome::Stopped { summary } => {
                    println!("stopped:");
                    print_run_one_line(&summary);
                    fail_on_recorder_error(&summary)?;
                }
                WaitOutcome::NoActiveRun => {
                    print_finished_run_or_message(client, run_id).await?;
                }
                other => {
                    eprintln!("stax: unexpected wait outcome: {other:?}");
                }
            }
        }
        _ = child_watcher => {
            // Target reaped; stop the recording.
            let summary = client.stop_active().await.map_err(|e| format!("{e:?}"))?;
            println!("target exited; stopped:");
            print_run_one_line(&summary);
            fail_on_recorder_error(&summary)?;
        }
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|e| format!("waiting for Ctrl-C: {e}"))?;
            let summary = client.stop_active().await.map_err(|e| format!("{e:?}"))?;
            println!("stopped:");
            print_run_one_line(&summary);
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn_sigwinch_pump(launched: std::sync::Arc<launch::Launched>) {
    tokio::spawn(async move {
        if let Ok(mut sigwinch) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        {
            while sigwinch.recv().await.is_some() {
                if let Some(size) = current_terminal_size() {
                    launched.resize(size);
                }
            }
        }
    });
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

#[cfg(target_os = "macos")]
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

async fn run_diagnose(args: DiagnoseArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    select_query_run_if_requested(&url, "diagnose --run", args.run).await?;
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

async fn run_save(args: ArchiveArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("save", &client);
    client
        .save_current(args.path.clone())
        .await
        .map_err(|e| format!("{e:?}"))?;
    println!("saved: {}", args.path);
    Ok(())
}

async fn run_open(args: ArchiveArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("open", &client);
    client
        .open_saved(args.path.clone())
        .await
        .map_err(|e| format!("{e:?}"))?;
    println!("opened: {}", args.path);
    Ok(())
}

async fn run_select_run(args: stax_core::args::SelectRunArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("select-run", &client);
    let summary = client
        .select_run(stax_live_proto::RunId(args.run_id))
        .await
        .map_err(|e| format!("{e:?}"))?;
    println!("selected:");
    print_run_one_line(&summary);
    Ok(())
}

async fn select_query_run_if_requested(
    url: &str,
    surface: &'static str,
    run_id: Option<u64>,
) -> Result<(), Box<dyn Error>> {
    let Some(run_id) = run_id else {
        return Ok(());
    };
    let client: RunControlClient = vox::connect(url).await?;
    let _debug_registration = register_run_control_client(surface, &client);
    client
        .select_run(RunId(run_id))
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

fn run_compare(args: CompareArgs) -> Result<(), Box<dyn Error>> {
    let baseline = read_saved_archive(Path::new(&args.baseline))?;
    let candidate = read_saved_archive(Path::new(&args.candidate))?;
    let baseline_stats = summarize_archive(&baseline);
    let candidate_stats = summarize_archive(&candidate);

    println!("stax compare");
    println!("baseline:  {}  ({})", baseline_stats.label, args.baseline);
    println!("candidate: {}  ({})", candidate_stats.label, args.candidate);
    println!();
    println!(
        "{:<28} {:>14} {:>14} {:>14}",
        "metric", "baseline", "candidate", "delta"
    );
    print_count_metric(
        "PET samples",
        baseline_stats.pet_samples,
        candidate_stats.pet_samples,
    );
    print_duration_metric(
        "on-CPU intervals",
        baseline_stats.on_cpu_ns,
        candidate_stats.on_cpu_ns,
    );
    print_duration_metric(
        "off-CPU intervals",
        baseline_stats.off_cpu_ns,
        candidate_stats.off_cpu_ns,
    );
    print_duration_metric(
        "target time",
        baseline_stats.target_ns,
        candidate_stats.target_ns,
    );
    print_count_metric(
        "target spans",
        baseline_stats.target_spans,
        candidate_stats.target_spans,
    );
    print_count_metric(
        "target lanes",
        baseline_stats.target_lanes,
        candidate_stats.target_lanes,
    );
    print_count_metric(
        "spans with origin",
        baseline_stats.spans_with_origin,
        candidate_stats.spans_with_origin,
    );
    print_count_metric(
        "linked origins",
        baseline_stats.spans_linked_origin,
        candidate_stats.spans_linked_origin,
    );
    print_count_metric(
        "unlinked origins",
        baseline_stats.spans_unlinked_origin,
        candidate_stats.spans_unlinked_origin,
    );
    print_count_metric(
        "missing origins",
        baseline_stats.spans_missing_origin,
        candidate_stats.spans_missing_origin,
    );
    print_count_metric(
        "bad-duration drops",
        baseline_stats.spans_dropped_bad_duration,
        candidate_stats.spans_dropped_bad_duration,
    );
    print_count_metric(
        "target queue drops",
        baseline_stats.spans_dropped_target_queue_full,
        candidate_stats.spans_dropped_target_queue_full,
    );
    print_count_metric(
        "worker disconnect drops",
        baseline_stats.spans_dropped_target_worker_disconnected,
        candidate_stats.spans_dropped_target_worker_disconnected,
    );

    let lanes = compare_lanes(&baseline_stats.lanes, &candidate_stats.lanes);
    if !lanes.is_empty() {
        println!();
        println!("top target lanes by max duration:");
        println!(
            "{:<32} {:>12} {:>8} {:>12} {:>8} {:>12}",
            "lane", "base ms", "spans", "cand ms", "spans", "delta ms"
        );
        for lane in lanes.into_iter().take(10) {
            let baseline_lane = baseline_stats.lanes.get(&lane).copied().unwrap_or_default();
            let candidate_lane = candidate_stats
                .lanes
                .get(&lane)
                .copied()
                .unwrap_or_default();
            println!(
                "{:<32} {:>12.3} {:>8} {:>12.3} {:>8} {:>12}",
                truncate_label(&lane, 32),
                baseline_lane.target_ns as f64 / 1e6,
                baseline_lane.target_spans,
                candidate_lane.target_ns as f64 / 1e6,
                candidate_lane.target_spans,
                format_delta_ms(candidate_lane.target_ns as i128 - baseline_lane.target_ns as i128),
            );
        }
    }
    Ok(())
}

const ARCHIVE_FORMAT_VERSION: u32 = 2;
const ARCHIVE_V1_FORMAT_VERSION: u32 = 1;
const ARCHIVE_V1_FILE_NAME: &str = "archive.json";
const ARCHIVE_MANIFEST_FILE_NAME: &str = "manifest.json";
#[cfg(test)]
const ARCHIVE_AGGREGATOR_FILE_NAME: &str = "aggregator.json";
#[cfg(test)]
const ARCHIVE_BINARIES_FILE_NAME: &str = "binaries.json";
#[cfg(test)]
const ARCHIVE_TARGET_INGEST_FILE_NAME: &str = "target-ingest.json";

fn read_saved_archive(path: &Path) -> Result<SavedRunArchive, Box<dyn Error>> {
    let archive = if path.is_dir() {
        let manifest_path = path.join(ARCHIVE_MANIFEST_FILE_NAME);
        if manifest_path.exists() {
            read_saved_archive_manifest(&manifest_path)?
        } else {
            read_saved_archive_v1(&path.join(ARCHIVE_V1_FILE_NAME))?
        }
    } else if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ARCHIVE_MANIFEST_FILE_NAME)
    {
        read_saved_archive_manifest(path)?
    } else {
        read_saved_archive_v1(path)?
    };
    if !is_supported_archive_version(archive.format_version) {
        return Err(format!(
            "unsupported stax archive version {} in {} (supported: {}, {})",
            archive.format_version,
            path.display(),
            ARCHIVE_V1_FORMAT_VERSION,
            ARCHIVE_FORMAT_VERSION
        )
        .into());
    }
    Ok(archive)
}

fn read_saved_archive_v1(archive_path: &Path) -> Result<SavedRunArchive, Box<dyn Error>> {
    let bytes = std::fs::read(&archive_path)
        .map_err(|e| format!("read {}: {e}", archive_path.display()))?;
    facet_json::from_slice(&bytes)
        .map_err(|e| format!("parse {}: {e}", archive_path.display()).into())
}

fn read_saved_archive_manifest(manifest_path: &Path) -> Result<SavedRunArchive, Box<dyn Error>> {
    let bytes = std::fs::read(manifest_path)
        .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
    let manifest: SavedRunArchiveManifest = facet_json::from_slice(&bytes)
        .map_err(|e| format!("parse {}: {e}", manifest_path.display()))?;
    if !is_supported_archive_version(manifest.format_version) {
        return Err(format!(
            "unsupported stax archive version {} in {} (supported: {}, {})",
            manifest.format_version,
            manifest_path.display(),
            ARCHIVE_V1_FORMAT_VERSION,
            ARCHIVE_FORMAT_VERSION
        )
        .into());
    }
    let base = manifest_path.parent().ok_or_else(|| {
        format!(
            "manifest {} has no parent directory",
            manifest_path.display()
        )
    })?;
    Ok(SavedRunArchive {
        format_version: manifest.format_version,
        saved_at_unix_ns: manifest.saved_at_unix_ns,
        runs: manifest.runs,
        aggregator: read_saved_json(&archive_member_path(base, &manifest.files.aggregator)?)?,
        binaries: read_saved_json(&archive_member_path(base, &manifest.files.binaries)?)?,
        target_ingest: read_saved_json(&archive_member_path(base, &manifest.files.target_ingest)?)?,
    })
}

fn read_saved_json<T: facet::Facet<'static>>(path: &Path) -> Result<T, Box<dyn Error>> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    facet_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()).into())
}

fn archive_member_path(base: &Path, member: &str) -> Result<PathBuf, Box<dyn Error>> {
    let member_path = Path::new(member);
    let mut has_component = false;
    for component in member_path.components() {
        match component {
            std::path::Component::Normal(_) => has_component = true,
            _ => {
                return Err(format!(
                    "archive member path {member:?} must stay inside {}",
                    base.display()
                )
                .into());
            }
        }
    }
    if !has_component {
        return Err(format!("archive member path {member:?} must not be empty").into());
    }
    Ok(base.join(member_path))
}

fn is_supported_archive_version(version: u32) -> bool {
    matches!(version, ARCHIVE_V1_FORMAT_VERSION | ARCHIVE_FORMAT_VERSION)
}

#[derive(Clone, Copy, Debug, Default)]
struct LaneCompareStats {
    target_ns: u64,
    target_spans: u64,
}

#[derive(Debug, Default)]
struct ArchiveCompareStats {
    label: String,
    pet_samples: u64,
    on_cpu_ns: u64,
    off_cpu_ns: u64,
    target_ns: u64,
    target_spans: u64,
    target_lanes: u64,
    spans_with_origin: u64,
    spans_linked_origin: u64,
    spans_unlinked_origin: u64,
    spans_missing_origin: u64,
    spans_dropped_bad_duration: u64,
    spans_dropped_target_queue_full: u64,
    spans_dropped_target_worker_disconnected: u64,
    lanes: HashMap<String, LaneCompareStats>,
}

fn summarize_archive(archive: &SavedRunArchive) -> ArchiveCompareStats {
    let mut stats = ArchiveCompareStats {
        label: archive
            .runs
            .last()
            .map(|run| run.label.clone())
            .unwrap_or_else(|| "(no run summary)".to_owned()),
        spans_dropped_bad_duration: archive.target_ingest.spans_dropped_bad_duration,
        spans_dropped_target_queue_full: archive.target_ingest.spans_dropped_target_queue_full,
        spans_dropped_target_worker_disconnected: archive
            .target_ingest
            .spans_dropped_target_worker_disconnected,
        ..ArchiveCompareStats::default()
    };
    let thread_names: HashMap<u32, String> = archive
        .aggregator
        .thread_names
        .iter()
        .map(|thread| (thread.tid, thread.name.clone()))
        .collect();

    for thread in &archive.aggregator.threads {
        stats.pet_samples = stats
            .pet_samples
            .saturating_add(thread.pet_samples.len() as u64);
        for interval in &thread.intervals {
            let duration_ns = interval.end_ns.saturating_sub(interval.start_ns);
            match &interval.kind {
                SavedIntervalKind::OnCpu => {
                    stats.on_cpu_ns = stats.on_cpu_ns.saturating_add(duration_ns);
                }
                SavedIntervalKind::OffCpu { .. } => {
                    stats.off_cpu_ns = stats.off_cpu_ns.saturating_add(duration_ns);
                }
                SavedIntervalKind::SyntheticSpan { stack, origin_tid } => {
                    stats.target_ns = stats.target_ns.saturating_add(duration_ns);
                    stats.target_spans = stats.target_spans.saturating_add(1);
                    match origin_tid {
                        Some(_) => {
                            stats.spans_with_origin = stats.spans_with_origin.saturating_add(1);
                            if stack.len() > 2 {
                                stats.spans_linked_origin =
                                    stats.spans_linked_origin.saturating_add(1);
                            } else {
                                stats.spans_unlinked_origin =
                                    stats.spans_unlinked_origin.saturating_add(1);
                            }
                        }
                        None => {
                            stats.spans_missing_origin =
                                stats.spans_missing_origin.saturating_add(1);
                        }
                    }
                    let lane_name = thread_names
                        .get(&thread.tid)
                        .cloned()
                        .unwrap_or_else(|| format!("tid {}", thread.tid));
                    let lane = stats.lanes.entry(lane_name).or_default();
                    lane.target_ns = lane.target_ns.saturating_add(duration_ns);
                    lane.target_spans = lane.target_spans.saturating_add(1);
                }
            }
        }
    }
    stats.target_lanes = stats.lanes.len() as u64;
    stats
}

fn compare_lanes(
    baseline: &HashMap<String, LaneCompareStats>,
    candidate: &HashMap<String, LaneCompareStats>,
) -> Vec<String> {
    let mut names: BTreeSet<String> = baseline.keys().cloned().collect();
    names.extend(candidate.keys().cloned());
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort_by(|a, b| {
        let a_max = baseline
            .get(a)
            .map(|lane| lane.target_ns)
            .unwrap_or_default()
            .max(
                candidate
                    .get(a)
                    .map(|lane| lane.target_ns)
                    .unwrap_or_default(),
            );
        let b_max = baseline
            .get(b)
            .map(|lane| lane.target_ns)
            .unwrap_or_default()
            .max(
                candidate
                    .get(b)
                    .map(|lane| lane.target_ns)
                    .unwrap_or_default(),
            );
        b_max.cmp(&a_max).then_with(|| a.cmp(b))
    });
    names
}

fn print_count_metric(label: &str, baseline: u64, candidate: u64) {
    println!(
        "{:<28} {:>14} {:>14} {:>14}",
        label,
        baseline,
        candidate,
        format_delta_count(candidate as i128 - baseline as i128),
    );
}

fn print_duration_metric(label: &str, baseline_ns: u64, candidate_ns: u64) {
    println!(
        "{:<28} {:>14.3} {:>14.3} {:>14}",
        label,
        baseline_ns as f64 / 1e6,
        candidate_ns as f64 / 1e6,
        format_delta_ms(candidate_ns as i128 - baseline_ns as i128),
    );
}

fn format_delta_count(delta: i128) -> String {
    if delta >= 0 {
        format!("+{delta}")
    } else {
        delta.to_string()
    }
}

fn format_delta_ms(delta_ns: i128) -> String {
    let sign = if delta_ns >= 0 { "+" } else { "-" };
    let abs_ms = delta_ns.unsigned_abs() as f64 / 1e6;
    format!("{sign}{abs_ms:.3}")
}

fn truncate_label(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        return label.to_owned();
    }
    let mut out: String = label.chars().take(max_chars.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

async fn run_top(args: TopArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    select_query_run_if_requested(&url, "top --run", args.run).await?;
    let sort = match args.sort.as_str() {
        "self" => TopSort::BySelf,
        "total" => TopSort::ByTotal,
        other => {
            return Err(format!("unknown --sort value {other:?} (use `self` or `total`)").into());
        }
    };
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("top", &client);
    let update = client
        .top_update(
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
    let threads = client.threads().await.ok();
    if update.entries.is_empty() {
        print_empty_top(&update, threads.as_ref(), args.tid, args.limit);
        return Ok(());
    }
    let entries = update.entries;
    println!(
        "{:>10} {:>10} {:>8} {:>8}  function",
        "active ms", "target ms", "samples", "spans",
    );
    for e in &entries {
        let name = e.function_name.as_deref().unwrap_or("<unresolved>");
        let bin = e.binary.as_deref().unwrap_or("?");
        let (active_ns, target_ns, samples, spans) = match sort {
            TopSort::BySelf => (
                e.self_on_cpu_ns,
                e.self_target_ns,
                e.self_pet_samples,
                e.self_target_spans,
            ),
            TopSort::ByTotal => (
                e.total_on_cpu_ns,
                e.total_target_ns,
                e.total_pet_samples,
                e.total_target_spans,
            ),
        };
        println!(
            "{:>10.3} {:>10.3} {:>8} {:>8}  {} ({})",
            active_ns as f64 / 1e6,
            target_ns as f64 / 1e6,
            samples,
            spans,
            name,
            bin,
        );
    }
    maybe_print_metal_target_hint(
        threads.as_ref(),
        top_entries_mention_metal_cooperation(&entries),
    );
    Ok(())
}

async fn run_annotate(args: AnnotateArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    select_query_run_if_requested(&url, "annotate --run", args.run).await?;
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
    select_query_run_if_requested(&url, "threads --run", args.run).await?;
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("threads", &client);
    let update = client.threads().await.map_err(|e| format!("{e:?}"))?;
    print_threads(&update, args.limit);
    Ok(())
}

fn print_threads(update: &ThreadsUpdate, limit: u32) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    write_threads(&mut out, update, limit).expect("write threads output");
}

fn write_threads<W: Write>(out: &mut W, update: &ThreadsUpdate, limit: u32) -> io::Result<()> {
    let mut threads: Vec<&stax_live_proto::ThreadInfo> = update.threads.iter().collect();
    threads.sort_by(|a, b| {
        let a_total = a.on_cpu_ns.saturating_add(off_cpu_total_ns(&a.off_cpu));
        let b_total = b.on_cpu_ns.saturating_add(off_cpu_total_ns(&b.off_cpu));
        b_total
            .cmp(&a_total)
            .then_with(|| b.target_spans.cmp(&a.target_spans))
            .then_with(|| b.pet_samples.cmp(&a.pet_samples))
            .then_with(|| a.tid.cmp(&b.tid))
    });
    if threads.is_empty() {
        writeln!(out, "(no thread samples yet — is a recording in progress?)")?;
        return Ok(());
    }
    writeln!(
        out,
        "{:>10} {:>10} {:>10} {:>8} {:>8} {:>7} {:>9}  tid    name",
        "cpu ms", "target ms", "off-CPU ms", "samples", "spans", "kind", "blocked",
    )?;
    let take = if limit == 0 {
        threads.len()
    } else {
        limit as usize
    };
    let mut visible: Vec<&stax_live_proto::ThreadInfo> =
        threads.iter().take(take).copied().collect();
    if limit != 0 {
        visible.extend(
            threads
                .iter()
                .skip(take)
                .copied()
                .filter(|t| t.tid >= SYNTH_TID_BASE && t.target_spans > 0),
        );
    }
    for t in &visible {
        let off_total = off_cpu_total_ns(&t.off_cpu);
        let dominant = dominant_off_cpu_reason(&t.off_cpu);
        let cpu_ns = t.on_cpu_ns.saturating_sub(t.target_ns);
        writeln!(
            out,
            "{:>10.2} {:>10.2} {:>10.2} {:>8} {:>8} {:>7} {:>9}  {:<6} {}",
            cpu_ns as f64 / 1e6,
            t.target_ns as f64 / 1e6,
            off_total as f64 / 1e6,
            t.pet_samples,
            t.target_spans,
            thread_kind(t),
            dominant,
            t.tid,
            t.name.as_deref().unwrap_or("(unnamed)"),
        )?;
    }
    if threads.len() > visible.len() {
        let hidden = threads.len() - visible.len();
        writeln!(
            out,
            "…{hidden} more non-target thread{}",
            if hidden == 1 { "" } else { "s" }
        )?;
    }
    Ok(())
}

fn thread_kind(thread: &stax_live_proto::ThreadInfo) -> &'static str {
    if thread.tid >= SYNTH_TID_BASE {
        "target"
    } else {
        "thread"
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
    select_query_run_if_requested(&url, "flame --run", args.run).await?;
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
    let threads = client.threads().await.ok();
    print_flame(&update, args.max_depth, args.threshold_pct);
    if update.root.children.is_empty() {
        maybe_print_empty_view_hint("flame", &update.total_off_cpu, threads.as_ref(), args.tid);
    }
    maybe_print_metal_target_hint(
        threads.as_ref(),
        flame_mentions_metal_cooperation(&update.root, &update.strings),
    );
    Ok(())
}

fn print_flame(update: &FlamegraphUpdate, max_depth: usize, threshold_pct: f64) {
    let total = update.total_on_cpu_ns.max(1) as f64;
    println!(
        "# stax flame · total active {:.3}s · target {:.3}s · off-CPU {:.3}s",
        update.total_on_cpu_ns as f64 / 1e9,
        update.total_target_ns as f64 / 1e9,
        off_cpu_total_ns(&update.total_off_cpu) as f64 / 1e9,
    );
    if let Some(tid) = update.root.children.first().and(None::<u32>) {
        // placeholder — root has no tid annotation; left as a hook
        // for future per-thread renders.
        let _ = tid;
    }
    println!();
    println!("```");
    println!(
        "{:>8} {:>8} {:>7} {:>5}  frame",
        "active", "target", "spans", "%",
    );
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

fn print_empty_top(
    update: &TopUpdate,
    threads: Option<&ThreadsUpdate>,
    tid: Option<u32>,
    limit: u32,
) {
    if limit == 0 {
        println!("(no entries requested — --limit is 0)");
        return;
    }
    println!("(no CPU samples or target spans in this view yet — is a recording in progress?)");
    maybe_print_empty_view_hint("top", &update.total_off_cpu, threads, tid);
}

fn maybe_print_empty_view_hint(
    command: &str,
    off_cpu: &OffCpuBreakdown,
    threads: Option<&ThreadsUpdate>,
    tid: Option<u32>,
) {
    if let Some(hint) = empty_view_hint(command, off_cpu, threads, tid) {
        eprintln!("{hint}");
    }
}

fn empty_view_hint(
    command: &str,
    off_cpu: &OffCpuBreakdown,
    threads: Option<&ThreadsUpdate>,
    tid: Option<u32>,
) -> Option<String> {
    let target_lanes = threads.map(target_lane_summaries).unwrap_or_default();
    if !target_lanes.is_empty() {
        let first_tid = target_lanes[0].0;
        let lanes = format_target_lane_summaries(&target_lanes);
        let filter = tid
            .map(|tid| format!(" outside `--tid {tid}`"))
            .unwrap_or_default();
        return Some(format!(
            "hint: target lanes exist{filter}: {lanes}. Try `stax {command} --tid {first_tid}` or `stax threads -n 0`."
        ));
    }

    let off_cpu_ns = off_cpu_total_ns(off_cpu);
    if off_cpu_ns > 0 {
        return Some(format!(
            "hint: this {command} view has no CPU or target-span frames, but the run has {:.3}s off-CPU time. Run `stax threads -n 0`; if the interesting work runs on a GPU, accelerator, executor, or runtime lane, link `stax-target` and report spans.",
            off_cpu_ns as f64 / 1e9
        ));
    }

    if threads.map(threads_have_activity).unwrap_or(false) {
        return Some(format!(
            "hint: thread activity exists but no CPU or target-span frames landed in this {command} view. Try `stax wait --for-samples 100`, then `stax threads -n 0`; for executor/GPU/accelerator work, add `stax-target` spans."
        ));
    }

    None
}

fn threads_have_activity(threads: &ThreadsUpdate) -> bool {
    threads.threads.iter().any(|thread| {
        thread.on_cpu_ns > 0
            || off_cpu_total_ns(&thread.off_cpu) > 0
            || thread.pet_samples > 0
            || thread.target_spans > 0
    })
}

fn target_lane_summaries(threads: &ThreadsUpdate) -> Vec<(u32, String)> {
    threads
        .threads
        .iter()
        .filter(|thread| thread.tid >= SYNTH_TID_BASE && thread.target_spans > 0)
        .map(|thread| {
            (
                thread.tid,
                thread
                    .name
                    .as_deref()
                    .unwrap_or("(unnamed target lane)")
                    .to_owned(),
            )
        })
        .take(3)
        .collect()
}

fn format_target_lane_summaries(lanes: &[(u32, String)]) -> String {
    lanes
        .iter()
        .map(|(tid, name)| format!("{tid} {name}"))
        .collect::<Vec<_>>()
        .join(", ")
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
        "{:>8.2} {:>8.2} {:>7} {:>5.1}  {indent}{prefix}{label}",
        node.on_cpu_ns as f64 / 1e6,
        node.target_ns as f64 / 1e6,
        node.target_spans,
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

const SYNTH_TID_BASE: u32 = 0xFFF0_0000;

fn maybe_print_metal_target_hint(threads: Option<&ThreadsUpdate>, saw_metal_dispatch: bool) {
    if !saw_metal_dispatch {
        return;
    }
    if threads.map(has_synthetic_target_lane).unwrap_or(false) {
        return;
    }
    eprintln!(
        "hint: Metal command/dispatch frames are visible, but no cooperating target lane is present. \
         Link stax-target in the profiled process and report Metal 4 timestamp-counter spans from the dispatch/reporting thread; \
         then `stax threads` will show the synthetic lane, `stax top --tid <lane tid>` will show per-kernel durations, \
         and span origins let `stax flame --tid <cpu tid>` show CPU stack -> GPU lane -> kernel."
    );
}

fn has_synthetic_target_lane(threads: &ThreadsUpdate) -> bool {
    threads
        .threads
        .iter()
        .any(|thread| thread.tid >= SYNTH_TID_BASE && thread.target_spans > 0)
}

fn top_entries_mention_metal_cooperation(entries: &[TopEntry]) -> bool {
    entries.iter().any(|entry| {
        mentions_metal_cooperation(entry.function_name.as_deref(), entry.binary.as_deref())
    })
}

fn flame_mentions_metal_cooperation(node: &FlameNode, strings: &[String]) -> bool {
    let name = node
        .function_name
        .and_then(|index| strings.get(index as usize).map(String::as_str));
    let binary = node
        .binary
        .and_then(|index| strings.get(index as usize).map(String::as_str));
    mentions_metal_cooperation(name, binary)
        || node
            .children
            .iter()
            .any(|child| flame_mentions_metal_cooperation(child, strings))
}

fn mentions_metal_cooperation(function_name: Option<&str>, binary: Option<&str>) -> bool {
    let name = function_name.unwrap_or_default().to_ascii_lowercase();
    let binary = binary.unwrap_or_default().to_ascii_lowercase();
    let metal_binary =
        binary.contains("metal") || binary.contains("agx") || binary.contains("metalkit");
    let metal_name = name.contains("metal4")
        || name.contains("mtlcommandbuffer")
        || name.contains("mtlcommandqueue")
        || name.contains("mtlcomputecommandencoder")
        || name.contains("mtlblitcommandencoder")
        || name.contains("mtlrendercommandencoder")
        || name.contains("dispatchthreadgroups")
        || name.contains("dispatchthreads")
        || name.contains("commandbuffer")
        || name.contains("command_buffer");
    metal_name
        || (metal_binary
            && (name.contains("dispatch")
                || name.contains("commit")
                || name.contains("command")
                || name.contains("encode")))
}

#[cfg(test)]
mod tests {
    use super::{
        ARCHIVE_AGGREGATOR_FILE_NAME, ARCHIVE_BINARIES_FILE_NAME, ARCHIVE_FORMAT_VERSION,
        ARCHIVE_MANIFEST_FILE_NAME, ARCHIVE_TARGET_INGEST_FILE_NAME, ARCHIVE_V1_FILE_NAME,
        ARCHIVE_V1_FORMAT_VERSION, SYNTH_TID_BASE, empty_view_hint, mentions_metal_cooperation,
        read_saved_archive, summarize_archive, target_ingest_hints, thread_kind, write_threads,
    };
    use stax_live_proto::{
        OffCpuBreakdown, SavedAggregator, SavedBinaryRegistry, SavedInterval, SavedIntervalKind,
        SavedPetSample, SavedPmuSample, SavedRunArchive, SavedRunArchiveFiles,
        SavedRunArchiveManifest, SavedRunArchiveProvenance, SavedThread, SavedThreadName,
        TargetIngestDiagnostics, ThreadInfo, ThreadsUpdate,
    };

    #[test]
    fn metal_hint_detects_dispatch_and_command_buffer_frames() {
        assert!(mentions_metal_cooperation(
            Some("-[MTLComputeCommandEncoder dispatchThreadgroups:threadsPerThreadgroup:]"),
            Some("Metal")
        ));
        assert!(mentions_metal_cooperation(
            Some("bee::helix_metal4::encoder::dispatch_kernel"),
            Some("hx")
        ));
        assert!(mentions_metal_cooperation(
            Some("-[AGXG17FamilyCommandBuffer commit]"),
            Some("AGXMetalG17X")
        ));
        assert!(!mentions_metal_cooperation(
            Some("std::thread::park"),
            Some("libstd")
        ));
    }

    #[test]
    fn empty_view_hint_points_to_existing_target_lanes() {
        let threads = ThreadsUpdate {
            threads: vec![thread(
                SYNTH_TID_BASE + 7,
                Some("executor demo"),
                5,
                0,
                0,
                1,
            )],
        };
        let hint = empty_view_hint("top", &OffCpuBreakdown::default(), Some(&threads), Some(42))
            .expect("target lane hint");

        assert!(hint.contains("outside `--tid 42`"));
        assert!(hint.contains("4293918727 executor demo"));
        assert!(hint.contains("stax top --tid 4293918727"));
    }

    #[test]
    fn empty_view_hint_points_off_cpu_runs_to_threads_and_target_spans() {
        let off_cpu = OffCpuBreakdown {
            sleep_ns: 2_000_000_000,
            ..OffCpuBreakdown::default()
        };
        let hint = empty_view_hint("flame", &off_cpu, None, None).expect("off-cpu discovery hint");

        assert!(hint.contains("2.000s off-CPU"));
        assert!(hint.contains("stax threads -n 0"));
        assert!(hint.contains("stax-target"));
    }

    #[test]
    fn empty_view_hint_suggests_waiting_when_only_thread_metadata_landed() {
        let threads = ThreadsUpdate {
            threads: vec![thread(123, Some("main"), 0, 0, 1, 0)],
        };
        let hint = empty_view_hint("top", &OffCpuBreakdown::default(), Some(&threads), None)
            .expect("thread activity hint");

        assert!(hint.contains("stax wait --for-samples 100"));
        assert!(hint.contains("stax threads -n 0"));
    }

    #[test]
    fn target_ingest_hints_explain_missing_batches() {
        let hints = target_ingest_hints(&TargetIngestDiagnostics::default());

        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("links `stax-target`"));
        assert!(hints[0].contains("reporting_active()"));
    }

    #[test]
    fn target_ingest_hints_explain_no_active_run_drops() {
        let hints = target_ingest_hints(&TargetIngestDiagnostics {
            batches_dropped_no_active_run: 1,
            spans_dropped_no_active_run: 3,
            ..TargetIngestDiagnostics::default()
        });

        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("no recording was active"));
        assert!(hints[0].contains("stax record --pid <pid>"));
    }

    #[test]
    fn target_ingest_hints_explain_wrong_pid_drops() {
        let hints = target_ingest_hints(&TargetIngestDiagnostics {
            batches_dropped_wrong_pid: 1,
            spans_dropped_wrong_pid: 3,
            ..TargetIngestDiagnostics::default()
        });

        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("not the active run target"));
        assert!(hints[0].contains("helper processes"));
    }

    #[test]
    fn target_ingest_hints_explain_target_queue_drops() {
        let hints = target_ingest_hints(&TargetIngestDiagnostics {
            batches_dropped_target_queue_full: 2,
            spans_dropped_target_queue_full: 12,
            ..TargetIngestDiagnostics::default()
        });

        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("local queue filled"));
        assert!(hints[0].contains("batch spans together"));
    }

    #[test]
    fn target_ingest_hints_explain_target_worker_drops() {
        let hints = target_ingest_hints(&TargetIngestDiagnostics {
            batches_dropped_target_worker_disconnected: 2,
            spans_dropped_target_worker_disconnected: 12,
            ..TargetIngestDiagnostics::default()
        });

        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("background worker disconnected"));
        assert!(hints[0].contains("target logs"));
    }

    #[test]
    fn target_ingest_hints_explain_bad_durations_and_missing_origins() {
        let hints = target_ingest_hints(&TargetIngestDiagnostics {
            batches: 1,
            spans_received: 4,
            spans_recorded: 3,
            spans_dropped_bad_duration: 1,
            ..TargetIngestDiagnostics::default()
        });

        assert_eq!(hints.len(), 2);
        assert!(hints[0].contains("end <= start"));
        assert!(hints[1].contains("have no origins"));
    }

    #[test]
    fn target_ingest_hints_explain_unlinked_origins() {
        let hints = target_ingest_hints(&TargetIngestDiagnostics {
            batches: 1,
            spans_received: 3,
            spans_recorded: 3,
            spans_with_origin: 3,
            spans_linked_origin: 1,
            spans_unlinked_origin: 2,
            ..TargetIngestDiagnostics::default()
        });

        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("2 of 3"));
        assert!(hints[0].contains("queue/submit time"));
        assert!(hints[0].contains("stax flame --tid <cpu tid>"));
    }

    #[test]
    fn target_ingest_hints_explain_unlinked_origin_reasons() {
        let hints = target_ingest_hints(&TargetIngestDiagnostics {
            batches: 1,
            spans_received: 4,
            spans_recorded: 4,
            spans_with_origin: 4,
            spans_unlinked_origin: 4,
            spans_origin_invalid_tid: 1,
            spans_origin_no_thread: 1,
            spans_origin_no_stack: 1,
            spans_origin_too_far: 1,
            origin_stack_max_distance_ns: 50_000_000,
            origin_too_far_distance_avg_ns: 50_000_007,
            origin_too_far_distance_max_ns: 50_000_007,
            ..TargetIngestDiagnostics::default()
        });

        assert_eq!(hints.len(), 4);
        assert!(hints[0].contains("synthetic target tid"));
        assert!(hints[1].contains("no PET samples"));
        assert!(hints[2].contains("no user stacks"));
        assert!(hints[3].contains("too far"));
        assert!(hints[3].contains("50.000ms"));
    }

    #[test]
    fn thread_kind_labels_synthetic_target_lanes() {
        let cpu = thread(123, Some("main"), 1, 0, 1, 0);
        let target = thread(SYNTH_TID_BASE, Some("gpu"), 1, 0, 0, 1);

        assert_eq!(thread_kind(&cpu), "thread");
        assert_eq!(thread_kind(&target), "target");
    }

    #[test]
    fn threads_output_keeps_target_lanes_past_limit() {
        let update = ThreadsUpdate {
            threads: vec![
                thread(10, Some("busy cpu"), 10_000_000, 0, 10, 0),
                thread(11, Some("less busy cpu"), 5_000_000, 0, 5, 0),
                thread(SYNTH_TID_BASE + 9, Some("GPU queue"), 1_000_000, 0, 0, 2),
            ],
        };
        let mut out = Vec::new();
        write_threads(&mut out, &update, 1).expect("write thread table");
        let out = String::from_utf8(out).expect("utf8 thread table");

        assert!(out.contains("busy cpu"));
        assert!(out.contains("GPU queue"));
        assert!(out.contains("target"));
        assert!(out.contains("…1 more non-target thread"));
        assert!(!out.contains("less busy cpu"));
    }

    #[test]
    fn summarize_archive_counts_target_and_origin_dimensions() {
        let archive = SavedRunArchive {
            format_version: 1,
            saved_at_unix_ns: 0,
            runs: Vec::new(),
            aggregator: SavedAggregator {
                session_start_ns: Some(0),
                last_event_ns: Some(10_000),
                thread_names: vec![SavedThreadName {
                    tid: SYNTH_TID_BASE,
                    name: "GPU lane".to_owned(),
                }],
                threads: vec![SavedThread {
                    tid: SYNTH_TID_BASE,
                    pet_samples: vec![SavedPetSample {
                        timestamp_ns: 1,
                        stack: vec![1],
                        kernel_stack: Vec::new(),
                        pmc: SavedPmuSample::default(),
                    }],
                    intervals: vec![
                        SavedInterval {
                            start_ns: 0,
                            end_ns: 1_000,
                            kind: SavedIntervalKind::OnCpu,
                        },
                        SavedInterval {
                            start_ns: 1_000,
                            end_ns: 3_000,
                            kind: SavedIntervalKind::OffCpu {
                                stack: vec![1],
                                waker_tid: None,
                                waker_user_stack: None,
                            },
                        },
                        SavedInterval {
                            start_ns: 3_000,
                            end_ns: 6_000,
                            kind: SavedIntervalKind::SyntheticSpan {
                                stack: vec![10, 20, 30],
                                origin_tid: Some(7),
                            },
                        },
                        SavedInterval {
                            start_ns: 6_000,
                            end_ns: 8_000,
                            kind: SavedIntervalKind::SyntheticSpan {
                                stack: vec![10, 20],
                                origin_tid: Some(7),
                            },
                        },
                        SavedInterval {
                            start_ns: 8_000,
                            end_ns: 9_000,
                            kind: SavedIntervalKind::SyntheticSpan {
                                stack: vec![10, 20],
                                origin_tid: None,
                            },
                        },
                    ],
                    wakeups: Vec::new(),
                }],
            },
            binaries: SavedBinaryRegistry::default(),
            target_ingest: TargetIngestDiagnostics {
                spans_dropped_bad_duration: 2,
                spans_dropped_target_queue_full: 3,
                spans_dropped_target_worker_disconnected: 4,
                ..TargetIngestDiagnostics::default()
            },
        };

        let stats = summarize_archive(&archive);

        assert_eq!(stats.pet_samples, 1);
        assert_eq!(stats.on_cpu_ns, 1_000);
        assert_eq!(stats.off_cpu_ns, 2_000);
        assert_eq!(stats.target_ns, 6_000);
        assert_eq!(stats.target_spans, 3);
        assert_eq!(stats.target_lanes, 1);
        assert_eq!(stats.spans_with_origin, 2);
        assert_eq!(stats.spans_linked_origin, 1);
        assert_eq!(stats.spans_unlinked_origin, 1);
        assert_eq!(stats.spans_missing_origin, 1);
        assert_eq!(stats.spans_dropped_bad_duration, 2);
        assert_eq!(stats.spans_dropped_target_queue_full, 3);
        assert_eq!(stats.spans_dropped_target_worker_disconnected, 4);
        assert_eq!(stats.lanes["GPU lane"].target_ns, 6_000);
        assert_eq!(stats.lanes["GPU lane"].target_spans, 3);
    }

    #[test]
    fn read_saved_archive_accepts_v2_manifest_layout() {
        let archive_dir = temp_archive_dir("cli-v2");
        let _ = std::fs::remove_dir_all(&archive_dir);
        std::fs::create_dir_all(&archive_dir).expect("create archive dir");

        let manifest = SavedRunArchiveManifest {
            format_version: ARCHIVE_FORMAT_VERSION,
            saved_at_unix_ns: 123,
            provenance: test_provenance(),
            runs: Vec::new(),
            files: SavedRunArchiveFiles {
                aggregator: ARCHIVE_AGGREGATOR_FILE_NAME.to_owned(),
                binaries: ARCHIVE_BINARIES_FILE_NAME.to_owned(),
                target_ingest: ARCHIVE_TARGET_INGEST_FILE_NAME.to_owned(),
            },
        };
        write_test_json(&archive_dir.join(ARCHIVE_MANIFEST_FILE_NAME), &manifest);
        write_test_json(
            &archive_dir.join(ARCHIVE_AGGREGATOR_FILE_NAME),
            &SavedAggregator::default(),
        );
        write_test_json(
            &archive_dir.join(ARCHIVE_BINARIES_FILE_NAME),
            &SavedBinaryRegistry::default(),
        );
        write_test_json(
            &archive_dir.join(ARCHIVE_TARGET_INGEST_FILE_NAME),
            &TargetIngestDiagnostics::default(),
        );

        let from_dir = read_saved_archive(&archive_dir).expect("read archive directory");
        assert_eq!(from_dir.format_version, ARCHIVE_FORMAT_VERSION);
        assert_eq!(from_dir.saved_at_unix_ns, 123);

        let from_manifest = read_saved_archive(&archive_dir.join(ARCHIVE_MANIFEST_FILE_NAME))
            .expect("read manifest path");
        assert_eq!(from_manifest.format_version, ARCHIVE_FORMAT_VERSION);
        assert_eq!(from_manifest.saved_at_unix_ns, 123);

        let _ = std::fs::remove_dir_all(&archive_dir);
    }

    #[test]
    fn read_saved_archive_accepts_legacy_v1_archive_json_layout() {
        let archive_dir = temp_archive_dir("cli-v1");
        let _ = std::fs::remove_dir_all(&archive_dir);
        std::fs::create_dir_all(&archive_dir).expect("create archive dir");

        let archive = SavedRunArchive {
            format_version: ARCHIVE_V1_FORMAT_VERSION,
            saved_at_unix_ns: 456,
            runs: Vec::new(),
            aggregator: SavedAggregator::default(),
            binaries: SavedBinaryRegistry::default(),
            target_ingest: TargetIngestDiagnostics::default(),
        };
        write_test_json(&archive_dir.join(ARCHIVE_V1_FILE_NAME), &archive);

        let from_dir = read_saved_archive(&archive_dir).expect("read legacy archive directory");
        assert_eq!(from_dir.format_version, ARCHIVE_V1_FORMAT_VERSION);
        assert_eq!(from_dir.saved_at_unix_ns, 456);

        let from_file =
            read_saved_archive(&archive_dir.join(ARCHIVE_V1_FILE_NAME)).expect("read legacy file");
        assert_eq!(from_file.format_version, ARCHIVE_V1_FORMAT_VERSION);
        assert_eq!(from_file.saved_at_unix_ns, 456);

        let _ = std::fs::remove_dir_all(&archive_dir);
    }

    #[test]
    fn read_saved_archive_rejects_manifest_paths_outside_archive() {
        let archive_dir = temp_archive_dir("cli-v2-bad-path");
        let _ = std::fs::remove_dir_all(&archive_dir);
        std::fs::create_dir_all(&archive_dir).expect("create archive dir");

        let manifest = SavedRunArchiveManifest {
            format_version: ARCHIVE_FORMAT_VERSION,
            saved_at_unix_ns: 123,
            provenance: test_provenance(),
            runs: Vec::new(),
            files: SavedRunArchiveFiles {
                aggregator: "../outside.json".to_owned(),
                binaries: ARCHIVE_BINARIES_FILE_NAME.to_owned(),
                target_ingest: ARCHIVE_TARGET_INGEST_FILE_NAME.to_owned(),
            },
        };
        write_test_json(&archive_dir.join(ARCHIVE_MANIFEST_FILE_NAME), &manifest);

        let error = read_saved_archive(&archive_dir)
            .expect_err("manifest path outside archive should be rejected")
            .to_string();
        assert!(error.contains("must stay inside"));

        let _ = std::fs::remove_dir_all(&archive_dir);
    }

    fn thread(
        tid: u32,
        name: Option<&str>,
        on_cpu_ns: u64,
        off_cpu_ns: u64,
        pet_samples: u64,
        target_spans: u64,
    ) -> ThreadInfo {
        ThreadInfo {
            tid,
            name: name.map(str::to_owned),
            on_cpu_ns,
            target_ns: if target_spans > 0 { on_cpu_ns } else { 0 },
            off_cpu: OffCpuBreakdown {
                sleep_ns: off_cpu_ns,
                ..OffCpuBreakdown::default()
            },
            pet_samples,
            target_spans,
        }
    }

    fn write_test_json<T: facet::Facet<'static>>(path: &std::path::Path, value: &T) {
        let bytes = facet_json::to_vec_pretty(value).expect("serialize test json");
        std::fs::write(path, bytes).expect("write test json");
    }

    fn test_provenance() -> SavedRunArchiveProvenance {
        SavedRunArchiveProvenance {
            producer: "stax-test".to_owned(),
            producer_version: "0.0.0-test".to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        }
    }

    fn temp_archive_dir(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "stax-cli-archive-{name}-{}-{nonce}",
            std::process::id()
        ))
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
    let target = &snapshot.target_ingest;
    println!("target ingest:");
    print_target_ingest_drop_counts(target);
    if target.batches == 0 {
        println!("  no target span batches ingested");
        print_target_ingest_hints(target);
        return;
    }
    println!(
        "  batches {}  spans {}/{} recorded  dropped {}  duration {:.3}ms",
        target.batches,
        target.spans_recorded,
        target.spans_received,
        target.spans_dropped_bad_duration,
        target.total_duration_ns as f64 / 1e6,
    );
    if target.spans_with_origin > 0 {
        println!(
            "  origins {} linked / {} unlinked",
            target.spans_linked_origin, target.spans_unlinked_origin,
        );
        print_target_origin_diagnostics(target);
    }
    if !target.lanes.is_empty() {
        println!(
            "  {:>10} {:>8} {:>8} {:>8} {:>8} {:>10}  lane",
            "duration", "spans", "origin", "linked", "unlinked", "tid",
        );
        for lane in &target.lanes {
            println!(
                "  {:>10.3} {:>8} {:>8} {:>8} {:>8} {:>10}  {}",
                lane.total_duration_ns as f64 / 1e6,
                lane.spans_recorded,
                lane.spans_with_origin,
                lane.spans_linked_origin,
                lane.spans_unlinked_origin,
                lane.tid,
                lane.name,
            );
            if lane.spans_unlinked_origin > 0 {
                println!(
                    "    origin failures for {}: bad_tid {}  no_thread {}  no_stack {}  too_far {}",
                    lane.name,
                    lane.spans_origin_invalid_tid,
                    lane.spans_origin_no_thread,
                    lane.spans_origin_no_stack,
                    lane.spans_origin_too_far,
                );
            }
        }
    }
    print_target_ingest_hints(target);
}

fn print_target_origin_diagnostics(target: &TargetIngestDiagnostics) {
    if target.spans_linked_origin > 0 {
        println!(
            "  linked origin PET distance min/avg/max {:.3}/{:.3}/{:.3}ms",
            target.origin_linked_distance_min_ns as f64 / 1e6,
            target.origin_linked_distance_avg_ns as f64 / 1e6,
            target.origin_linked_distance_max_ns as f64 / 1e6,
        );
    }
    let explained_unlinked = target
        .spans_origin_invalid_tid
        .saturating_add(target.spans_origin_no_thread)
        .saturating_add(target.spans_origin_no_stack)
        .saturating_add(target.spans_origin_too_far);
    if explained_unlinked > 0 {
        println!(
            "  unlinked origins by reason: bad_tid {}  no_thread {}  no_stack {}  too_far {}",
            target.spans_origin_invalid_tid,
            target.spans_origin_no_thread,
            target.spans_origin_no_stack,
            target.spans_origin_too_far,
        );
    }
    if target.spans_origin_too_far > 0 {
        println!(
            "  too-far origin PET distance min/avg/max {:.3}/{:.3}/{:.3}ms (limit {:.3}ms)",
            target.origin_too_far_distance_min_ns as f64 / 1e6,
            target.origin_too_far_distance_avg_ns as f64 / 1e6,
            target.origin_too_far_distance_max_ns as f64 / 1e6,
            target.origin_stack_max_distance_ns as f64 / 1e6,
        );
    }
}

fn print_target_ingest_drop_counts(target: &TargetIngestDiagnostics) {
    if target.batches_dropped_no_active_run > 0 {
        println!(
            "  dropped while no run active: {} batch{} / {} span{}",
            target.batches_dropped_no_active_run,
            plural(target.batches_dropped_no_active_run),
            target.spans_dropped_no_active_run,
            plural(target.spans_dropped_no_active_run),
        );
    }
    if target.batches_dropped_wrong_pid > 0 {
        println!(
            "  dropped wrong pid: {} batch{} / {} span{}",
            target.batches_dropped_wrong_pid,
            plural(target.batches_dropped_wrong_pid),
            target.spans_dropped_wrong_pid,
            plural(target.spans_dropped_wrong_pid),
        );
    }
    if target.batches_dropped_target_queue_full > 0 {
        println!(
            "  dropped in stax-target queue: {} batch{} / {} span{}",
            target.batches_dropped_target_queue_full,
            plural(target.batches_dropped_target_queue_full),
            target.spans_dropped_target_queue_full,
            plural(target.spans_dropped_target_queue_full),
        );
    }
    if target.batches_dropped_target_worker_disconnected > 0 {
        println!(
            "  dropped after stax-target worker stopped: {} batch{} / {} span{}",
            target.batches_dropped_target_worker_disconnected,
            plural(target.batches_dropped_target_worker_disconnected),
            target.spans_dropped_target_worker_disconnected,
            plural(target.spans_dropped_target_worker_disconnected),
        );
    }
}

fn print_target_ingest_hints(target: &TargetIngestDiagnostics) {
    for hint in target_ingest_hints(target) {
        println!("  hint: {hint}");
    }
}

fn target_ingest_hints(target: &TargetIngestDiagnostics) -> Vec<String> {
    let mut hints = Vec::new();
    if target.batches_dropped_no_active_run > 0 {
        hints.push(
            concat!(
                "target span batches arrived while no recording was active; ",
                "start `stax record --pid <pid>` or launch under `stax record -- ...` ",
                "before expecting spans to land",
            )
            .to_owned(),
        );
    }
    if target.batches_dropped_wrong_pid > 0 {
        hints.push(
            concat!(
                "target span batches arrived from a pid that is not the active run target; ",
                "make sure the instrumented process is the one stax is recording, ",
                "especially across helper processes",
            )
            .to_owned(),
        );
    }
    if target.batches_dropped_target_queue_full > 0 {
        hints.push(format!(
            "stax-target dropped {} batch{} before they reached the server because its local queue filled; batch spans together, reduce per-item span cardinality, and keep reporting behind `reporting_active()`",
            target.batches_dropped_target_queue_full,
            plural(target.batches_dropped_target_queue_full),
        ));
    }
    if target.batches_dropped_target_worker_disconnected > 0 {
        hints.push(format!(
            "stax-target dropped {} batch{} after its background worker disconnected; check target logs for stax-target runtime/connect failures",
            target.batches_dropped_target_worker_disconnected,
            plural(target.batches_dropped_target_worker_disconnected),
        ));
    }
    if target.batches == 0 {
        if hints.is_empty() {
            hints.push(
                concat!(
                    "if you expected target lanes, confirm the process links `stax-target`, ",
                    "polls `reporting_active()` while this pid is recorded, ",
                    "and can reach the `stax-server` socket",
                )
                .to_owned(),
            );
        }
        return hints;
    }

    if target.spans_dropped_bad_duration > 0 {
        hints.push(format!(
            "{} span{} had end <= start; target timestamps must use one monotonic nanosecond clock, e.g. `stax_target::now_ns()` or Metal 4 timestamps converted to mach time",
            target.spans_dropped_bad_duration,
            plural(target.spans_dropped_bad_duration),
        ));
    }

    if target.spans_recorded > 0 && target.spans_with_origin == 0 {
        hints.push(
            "spans have no origins; synthetic lane views work, but CPU stack -> lane attribution needs `stax_target::current_span_origin()` captured at queue/dispatch time"
                .to_owned(),
        );
    } else if target.spans_unlinked_origin > 0 {
        if target.spans_origin_invalid_tid > 0 {
            hints.push(format!(
                "{} origin{} used a synthetic target tid; capture origins on a real CPU thread before reporting target work",
                target.spans_origin_invalid_tid,
                plural(target.spans_origin_invalid_tid),
            ));
        }
        if target.spans_origin_no_thread > 0 {
            hints.push(format!(
                "{} origin{} referenced a tid with no PET samples in this run; confirm the profiled pid/thread is the one dispatching the work",
                target.spans_origin_no_thread,
                plural(target.spans_origin_no_thread),
            ));
        }
        if target.spans_origin_no_stack > 0 {
            hints.push(format!(
                "{} origin thread{} had PET samples but no user stacks; check stack unwinding/frame pointers or whether the dispatch site was sampled only in kernel/runtime glue",
                target.spans_origin_no_stack,
                plural(target.spans_origin_no_stack),
            ));
        }
        if target.spans_origin_too_far > 0 {
            hints.push(format!(
                "{} origin{} were too far from the nearest sampled CPU stack (avg {:.3}ms, max {:.3}ms, limit {:.3}ms); capture origins immediately at queue/submit time, not at completion or long before dispatch",
                target.spans_origin_too_far,
                plural(target.spans_origin_too_far),
                target.origin_too_far_distance_avg_ns as f64 / 1e6,
                target.origin_too_far_distance_max_ns as f64 / 1e6,
                target.origin_stack_max_distance_ns as f64 / 1e6,
            ));
        }
        let explained_unlinked = target
            .spans_origin_invalid_tid
            .saturating_add(target.spans_origin_no_thread)
            .saturating_add(target.spans_origin_no_stack)
            .saturating_add(target.spans_origin_too_far);
        if explained_unlinked < target.spans_unlinked_origin {
            hints.push(format!(
                "{} of {} origin-carrying span{} did not link to a sampled CPU stack; capture origins on the dispatching OS thread immediately at queue/submit time, then inspect with `stax top --tid <cpu tid> --sort total` or `stax flame --tid <cpu tid>`",
                target.spans_unlinked_origin,
                target.spans_with_origin,
                plural(target.spans_with_origin),
            ));
        }
    }

    hints
}

fn plural(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
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
