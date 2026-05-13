//! In-process recording driver. Replaces the per-attachment
//! `stax-shade` companion process. Same responsibilities — drive
//! `staxd-client` against the target, pump samples into the live
//! aggregator, own the PTY for `--launch` targets, posix_spawn the
//! suspended child and resume on first batch — minus the IPC.
//!
//! `task_for_pid` is no longer acquired here. kperf samples by PID
//! from the kernel side via staxd, with no per-target task port
//! required. The trade-off: peek/poke/breakpoint primitives that
//! needed the task port are gone with the shade; if they come back,
//! they'll come back behind a separate entitled helper, not
//! reintroduce the task-port-in-recorder pattern.

#![cfg(target_os = "macos")]

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eyre::WrapErr;
use stax_core::cmd_record_mac::LiveOnlySink;
use stax_core::live_sink::{
    BinaryLoadedEvent as LiveBinaryLoaded, BinaryUnloadedEvent as LiveBinaryUnloaded,
    CpuIntervalEvent as LiveCpuInterval, CpuIntervalKind as LiveCpuIntervalKind,
    LiveSink, MachOByteSource, SampleEvent as LiveSampleEvent, TargetAttached, ThreadName,
    WakeupEvent as LiveWakeup,
};
use stax_live::{IntervalKind, LiveSymbolOwned, LoadedBinary, PmuSample};
use stax_live_proto::{
    LaunchEnvVar, LaunchRequest, RunId, StopReason, TerminalInput, TerminalOutput, TerminalSize,
};

use crate::ServerState;

/// Spawn the recording task for an `--attach <pid>` run. Returns
/// immediately; the recording itself runs on a tokio task that
/// finalises the run when it ends.
pub fn spawn_attach(
    server: ServerState,
    run_id: RunId,
    pid: u32,
    frequency_hz: u32,
    daemon_socket: String,
    time_limit: Option<Duration>,
) {
    let stop_flag = Arc::new(AtomicBool::new(false));
    server.set_recording_stop_flag(run_id, stop_flag.clone());

    spawn_on_dedicated_runtime(move || async move {
        let result = run_attach(
            server.clone(),
            run_id,
            pid,
            frequency_hz,
            daemon_socket,
            time_limit,
            stop_flag,
        )
        .await;
        finalize(&server, run_id, result);
    });
}

/// Spawn the recording task for a `stax record -- <argv...>` run.
/// posix_spawns the target suspended, owns its PTY, drives the
/// recording, and resumes the target on first staxd batch.
pub fn spawn_launch(
    server: ServerState,
    run_id: RunId,
    request: LaunchRequest,
    terminal_input: vox::Rx<TerminalInput>,
    terminal_output: vox::Tx<TerminalOutput>,
) {
    let stop_flag = Arc::new(AtomicBool::new(false));
    server.set_recording_stop_flag(run_id, stop_flag.clone());

    spawn_on_dedicated_runtime(move || async move {
        let result = run_launch(
            server.clone(),
            run_id,
            request,
            terminal_input,
            terminal_output,
            stop_flag,
        )
        .await;
        finalize(&server, run_id, result);
    });
}

/// Run the recording loop on a dedicated OS thread with its own
/// current-thread tokio runtime. The staxd-client driver internally
/// builds futures (via vox::connect, etc.) that are not `Send`, so
/// we can't `tokio::spawn` it onto the multi-thread runtime that's
/// hosting the vox handlers; a current-thread runtime sidesteps the
/// `Send` requirement entirely.
fn spawn_on_dedicated_runtime<F, Fut>(make_future: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("recorder: build runtime failed: {e}");
                return;
            }
        };
        rt.block_on(make_future());
    });
}

fn finalize(server: &ServerState, run_id: RunId, result: eyre::Result<StopReason>) {
    let reason = match result {
        Ok(reason) => reason,
        Err(err) => {
            tracing::warn!(run_id = run_id.0, error = ?err, "recording failed");
            StopReason::RecorderError {
                message: format!("{err:?}"),
            }
        }
    };
    server.finalize_run(run_id, reason);
}

async fn run_attach(
    server: ServerState,
    run_id: RunId,
    pid: u32,
    frequency_hz: u32,
    daemon_socket: String,
    time_limit: Option<Duration>,
    stop_flag: Arc<AtomicBool>,
) -> eyre::Result<StopReason> {
    let opts = staxd_client::RemoteOptions {
        daemon_socket,
        pid,
        frequency_hz,
        duration: time_limit,
        ..Default::default()
    };
    drive_recording(server, run_id, opts, None, stop_flag, || false, || {}).await
}

async fn run_launch(
    server: ServerState,
    run_id: RunId,
    request: LaunchRequest,
    terminal_input: vox::Rx<TerminalInput>,
    terminal_output: vox::Tx<TerminalOutput>,
    stop_flag: Arc<AtomicBool>,
) -> eyre::Result<StopReason> {
    if request.command.is_empty() {
        eyre::bail!("--launch command is empty");
    }
    let frequency_hz = request.config.frequency_hz;
    let daemon_socket = request.daemon_socket.clone();
    let time_limit = request.time_limit_secs.map(Duration::from_secs);
    let cwd = request.cwd.clone();
    let env_pairs: Vec<(String, String)> = request
        .env
        .iter()
        .map(|LaunchEnvVar { key, value }| (key.clone(), value.clone()))
        .collect();
    let argv = request.command.clone();
    let terminal_size = request.terminal_size;

    let Spawned {
        pid,
        pre_resume,
        terminal,
    } = spawn_suspended(&argv, &cwd, &env_pairs, terminal_size)?;

    let terminal_pump = match terminal {
        Some(pty) => Some(start_terminal_pump(pty, terminal_input, terminal_output).await),
        None => None,
    };

    let opts = staxd_client::RemoteOptions {
        daemon_socket,
        pid,
        frequency_hz,
        duration: time_limit,
        ..Default::default()
    };

    let pre_resume = Arc::new(Mutex::new(Some(pre_resume)));
    let pre_resume_for_first_batch = pre_resume.clone();
    let on_first_batch = move || {
        if let Some(pre_resume) = pre_resume_for_first_batch
            .lock()
            .expect("pre_resume poisoned")
            .take()
        {
            if let Err(err) = pre_resume.resume() {
                tracing::warn!("failed to resume launched target: {err}");
            }
        }
    };

    let launched_pid = pid;
    let child_exit = Arc::new(Mutex::new(None::<ChildExit>));
    let child_exit_for_stop = child_exit.clone();
    let extra_stop = move || {
        if child_exit_for_stop
            .lock()
            .expect("child_exit poisoned")
            .is_none()
            && let Some(exit) = launched_child_exited(launched_pid)
        {
            tracing::info!(
                pid = launched_pid,
                code = exit.code,
                signal = exit.signal,
                "launched target exited; stopping recording"
            );
            *child_exit_for_stop.lock().expect("child_exit poisoned") = Some(exit);
            return true;
        }
        false
    };

    let result = drive_recording(
        server,
        run_id,
        opts,
        Some(launched_pid),
        stop_flag,
        extra_stop,
        on_first_batch,
    )
    .await;

    // Ensure target is resumed even if drive_session returned before the first-batch hook.
    if let Some(pre_resume) = pre_resume.lock().expect("pre_resume poisoned").take() {
        let _ = pre_resume.resume();
    }

    // Terminate the launched child if it's still alive after recording stops.
    if child_exit.lock().expect("child_exit poisoned").is_none()
        && let Some(exit) = terminate_launched_child(launched_pid)
    {
        *child_exit.lock().expect("child_exit poisoned") = Some(exit);
    }

    if let Some(pump) = terminal_pump {
        if let Some(exit) = *child_exit.lock().expect("child_exit poisoned") {
            pump.report_exit(exit);
        }
        pump.finish().await;
    }

    result
}

async fn drive_recording<ExtraStop, FirstBatch>(
    server: ServerState,
    run_id: RunId,
    opts: staxd_client::RemoteOptions,
    launched_pid: Option<u32>,
    stop_flag: Arc<AtomicBool>,
    mut extra_stop: ExtraStop,
    on_first_batch_action: FirstBatch,
) -> eyre::Result<StopReason>
where
    ExtraStop: FnMut() -> bool + Send + 'static,
    FirstBatch: FnOnce() + Send + 'static,
{
    let pid = opts.pid;
    let recording_start = Instant::now();
    tracing::info!(
        run_id = run_id.0,
        pid,
        launched_pid,
        frequency_hz = opts.frequency_hz,
        daemon_socket = %opts.daemon_socket,
        "recording lifecycle starting"
    );

    let sink = LiveOnlySink::new(Some(Box::new(ServerLiveSink {
        server: server.clone(),
        run_id,
    })));
    sink.notify_target_attached(pid);

    let stop_flag_for_should_stop = stop_flag.clone();
    let mut stop_reason_logged = false;
    let should_stop = move || {
        if stop_flag_for_should_stop.load(Ordering::Relaxed) {
            if !stop_reason_logged {
                stop_reason_logged = true;
                tracing::info!("recording stop requested");
            }
            return true;
        }
        extra_stop()
    };

    let mut first_batch_action = Some(on_first_batch_action);
    let on_first_batch = move || {
        tracing::info!(
            run_id = run_id.0,
            "staxd-client first batch / ready observed"
        );
        if let Some(action) = first_batch_action.take() {
            action();
        }
    };

    let result = staxd_client::drive_session_with_hooks(
        opts,
        sink,
        should_stop,
        on_first_batch,
        |_, _| {},
    )
    .await;

    match &result {
        Ok(()) => tracing::info!(
            run_id = run_id.0,
            elapsed = ?recording_start.elapsed(),
            "drive_session completed"
        ),
        Err(e) => tracing::warn!(
            run_id = run_id.0,
            elapsed = ?recording_start.elapsed(),
            error = %e,
            "drive_session failed"
        ),
    }

    let stop_reason = match result {
        Ok(()) => {
            if launched_pid.is_some() {
                StopReason::TargetExited
            } else {
                StopReason::UserStop
            }
        }
        Err(e) => StopReason::RecorderError {
            message: format!("staxd-client failed: {e}"),
        },
    };
    Ok(stop_reason)
}

/// `LiveSink` impl that records events straight into the
/// in-process aggregator + binary registry on `ServerState`. No
/// IngestBatch encoding, no IPC, no drainer task.
struct ServerLiveSink {
    server: ServerState,
    run_id: RunId,
}

#[async_trait::async_trait]
impl LiveSink for ServerLiveSink {
    async fn on_sample(&self, event: &LiveSampleEvent) {
        self.server.note_sample(self.run_id);
        self.server.aggregator().write().record_pet_sample(
            event.tid,
            event.timestamp,
            event.user_backtrace,
            event.kernel_backtrace,
            PmuSample {
                cycles: event.cycles,
                instructions: event.instructions,
                l1d_misses: event.l1d_misses,
                branch_mispreds: event.branch_mispreds,
            },
        );
        self.server.bump_revision();
    }

    async fn on_target_attached(&self, event: &TargetAttached) {
        self.server
            .apply_target_attached_in_process(self.run_id, event.pid, event.task_port);
    }

    async fn on_binary_loaded(&self, event: &LiveBinaryLoaded) {
        let symbols = event
            .symbols
            .iter()
            .map(|s| LiveSymbolOwned {
                start_svma: s.start_svma,
                end_svma: s.end_svma,
                name: s.name.to_vec(),
            })
            .collect();
        let binary = LoadedBinary {
            path: event.path.to_owned(),
            base_avma: event.base_avma,
            avma_end: event.base_avma.saturating_add(event.vmsize),
            text_svma: event.text_svma,
            arch: event.arch.map(|s| s.to_owned()),
            is_executable: event.is_executable,
            symbols,
            text_bytes: event.text_bytes.map(|b| b.to_vec()),
        };
        self.server.binaries().write().insert(binary);
        self.server.bump_revision();
    }

    async fn on_binary_unloaded(&self, _event: &LiveBinaryUnloaded) {
        self.server.bump_revision();
    }

    async fn on_thread_name(&self, event: &ThreadName) {
        self.server
            .aggregator()
            .write()
            .set_thread_name(event.tid, event.name.to_owned());
        self.server.bump_revision();
    }

    async fn on_wakeup(&self, event: &LiveWakeup) {
        self.server.aggregator().write().record_wakeup(
            event.timestamp,
            event.waker_tid,
            event.wakee_tid,
            event.waker_user_stack.to_vec(),
            event.waker_kernel_stack.to_vec(),
        );
        self.server.bump_revision();
    }

    async fn on_cpu_interval(&self, event: &LiveCpuInterval) {
        let kind = match &event.kind {
            LiveCpuIntervalKind::OnCpu => IntervalKind::OnCpu,
            LiveCpuIntervalKind::OffCpu {
                stack,
                waker_tid,
                waker_user_stack,
            } => {
                self.server.note_off_cpu(self.run_id);
                IntervalKind::OffCpu {
                    stack: stack.to_vec().into_boxed_slice(),
                    waker_tid: *waker_tid,
                    waker_user_stack: waker_user_stack
                        .map(|s| s.to_vec().into_boxed_slice()),
                }
            }
        };
        self.server
            .aggregator()
            .write()
            .record_interval(event.tid, event.start_ns, event.end_ns, kind);
        self.server.bump_revision();
    }

    async fn on_macho_byte_source(&self, source: Arc<dyn MachOByteSource>) {
        self.server.binaries().write().set_macho_byte_source(source);
    }
}

// ----- posix_spawn(SUSPENDED) + PTY + child lifecycle, ported from
// the old stax-shade main.rs. -----

struct Spawned {
    pid: u32,
    pre_resume: PreResume,
    terminal: Option<Pty>,
}

struct PreResume {
    pid: u32,
}

impl PreResume {
    fn resume(self) -> eyre::Result<()> {
        // The child was started with POSIX_SPAWN_START_SUSPENDED:
        // the kernel suspended it via SIGSTOP before its first
        // instruction. SIGCONT resumes it without needing a Mach
        // task port. (The old shade went through task_resume because
        // it already had the port in hand; here we don't.)
        let r = unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGCONT) };
        if r != 0 {
            return Err(std::io::Error::last_os_error())
                .wrap_err_with(|| format!("SIGCONT pid {}", self.pid));
        }
        tracing::info!(pid = self.pid, "target resumed");
        Ok(())
    }
}

struct Pty {
    master: RawFd,
    slave: RawFd,
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            if self.master >= 0 {
                libc::close(self.master);
            }
            if self.slave >= 0 {
                libc::close(self.slave);
            }
        }
    }
}

fn open_pty(size: TerminalSize) -> eyre::Result<Pty> {
    use std::ptr;

    let mut master = -1;
    let mut slave = -1;
    let mut winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut winsize,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).wrap_err("openpty");
    }
    Ok(Pty { master, slave })
}

fn set_pty_size(fd: RawFd, size: TerminalSize) {
    let winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &winsize);
    }
}

fn spawn_suspended(
    argv: &[String],
    cwd: &str,
    env_pairs: &[(String, String)],
    terminal_size: Option<TerminalSize>,
) -> eyre::Result<Spawned> {
    use std::ffi::CString;
    use std::os::raw::c_char;
    use std::ptr;

    let program = CString::new(argv[0].as_str())
        .map_err(|_| eyre::eyre!("program path contains an interior NUL"))?;
    let argv_c: Vec<CString> = argv
        .iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| eyre::eyre!("argv contains an interior NUL"))?;
    let mut argv_p: Vec<*mut c_char> = argv_c
        .iter()
        .map(|c| c.as_ptr() as *mut c_char)
        .collect();
    argv_p.push(ptr::null_mut());

    let env_cstrings: Vec<CString> = env_pairs
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| eyre::eyre!("env contains an interior NUL"))?;
    let mut env_p: Vec<*mut c_char> = env_cstrings
        .iter()
        .map(|c| c.as_ptr() as *mut c_char)
        .collect();
    env_p.push(ptr::null_mut());

    let mut pty = match terminal_size {
        Some(size) => Some(open_pty(size)?),
        None => None,
    };

    let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
    let r = unsafe { libc::posix_spawnattr_init(&mut attr) };
    if r != 0 {
        eyre::bail!("posix_spawnattr_init: {r}");
    }
    let flags = libc::POSIX_SPAWN_START_SUSPENDED | libc::POSIX_SPAWN_SETSIGDEF;
    let r = unsafe { libc::posix_spawnattr_setflags(&mut attr, flags as libc::c_short) };
    if r != 0 {
        unsafe {
            libc::posix_spawnattr_destroy(&mut attr);
        }
        eyre::bail!("posix_spawnattr_setflags: {r}");
    }

    let mut actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
    let actions_ptr = if let Some(pty) = &pty {
        let r = unsafe { libc::posix_spawn_file_actions_init(&mut actions) };
        if r != 0 {
            unsafe {
                libc::posix_spawnattr_destroy(&mut attr);
            }
            eyre::bail!("posix_spawn_file_actions_init: {r}");
        }
        for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            let r = unsafe { libc::posix_spawn_file_actions_adddup2(&mut actions, pty.slave, fd) };
            if r != 0 {
                unsafe {
                    libc::posix_spawn_file_actions_destroy(&mut actions);
                    libc::posix_spawnattr_destroy(&mut attr);
                }
                eyre::bail!("posix_spawn_file_actions_adddup2({fd}): {r}");
            }
        }
        if pty.slave > libc::STDERR_FILENO {
            let r = unsafe { libc::posix_spawn_file_actions_addclose(&mut actions, pty.slave) };
            if r != 0 {
                unsafe {
                    libc::posix_spawn_file_actions_destroy(&mut actions);
                    libc::posix_spawnattr_destroy(&mut attr);
                }
                eyre::bail!("posix_spawn_file_actions_addclose(slave): {r}");
            }
        }
        let r = unsafe { libc::posix_spawn_file_actions_addclose(&mut actions, pty.master) };
        if r != 0 {
            unsafe {
                libc::posix_spawn_file_actions_destroy(&mut actions);
                libc::posix_spawnattr_destroy(&mut attr);
            }
            eyre::bail!("posix_spawn_file_actions_addclose(master): {r}");
        }
        &actions as *const libc::posix_spawn_file_actions_t
    } else {
        ptr::null()
    };

    let cwd_c = CString::new(cwd)
        .map_err(|_| eyre::eyre!("cwd contains an interior NUL"))?;
    let mut pid: libc::pid_t = 0;
    // chdir in the parent before posix_spawnp so the child inherits.
    let prev = std::env::current_dir().ok();
    if !cwd.is_empty() {
        // SAFETY: chdir(2). Restored below.
        unsafe { libc::chdir(cwd_c.as_ptr()) };
    }
    let r = unsafe {
        libc::posix_spawnp(
            &mut pid,
            program.as_ptr(),
            actions_ptr,
            &attr,
            argv_p.as_ptr(),
            env_p.as_ptr(),
        )
    };
    if let Some(prev) = prev {
        let _ = std::env::set_current_dir(prev);
    }
    unsafe {
        if !actions.is_null() {
            libc::posix_spawn_file_actions_destroy(&mut actions);
        }
        libc::posix_spawnattr_destroy(&mut attr);
    }
    if r != 0 {
        eyre::bail!("posix_spawn({}): {r}", argv[0]);
    }
    let pid_u32 = pid as u32;
    tracing::info!(pid = pid_u32, program = %argv[0], "spawned target (suspended)");

    Ok(Spawned {
        pid: pid_u32,
        pre_resume: PreResume { pid: pid_u32 },
        terminal: pty.take(),
    })
}

#[derive(Clone, Copy, Debug)]
struct ChildExit {
    code: Option<i32>,
    signal: Option<i32>,
}

fn launched_child_exited(pid: u32) -> Option<ChildExit> {
    let mut status = 0;
    // SAFETY: waitpid on our direct child; WNOHANG is non-blocking.
    let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    if r == pid as libc::pid_t {
        Some(decode_wait_status(status))
    } else if r == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            tracing::warn!(pid, error = %err, "waitpid failed while polling launched target");
        }
        None
    } else {
        None
    }
}

fn terminate_launched_child(pid: u32) -> Option<ChildExit> {
    let mut status = 0;
    let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    if r == 0 {
        tracing::warn!(pid, "terminating launched target after recording ended");
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
            libc::waitpid(pid as libc::pid_t, &mut status, 0);
        }
        Some(decode_wait_status(status))
    } else if r == pid as libc::pid_t {
        Some(decode_wait_status(status))
    } else {
        None
    }
}

fn decode_wait_status(status: libc::c_int) -> ChildExit {
    if libc::WIFEXITED(status) {
        ChildExit {
            code: Some(libc::WEXITSTATUS(status)),
            signal: None,
        }
    } else if libc::WIFSIGNALED(status) {
        ChildExit {
            code: None,
            signal: Some(libc::WTERMSIG(status)),
        }
    } else {
        ChildExit {
            code: None,
            signal: None,
        }
    }
}

// ----- PTY pump -----

struct TerminalPump {
    events: tokio::sync::mpsc::UnboundedSender<TerminalOutput>,
    output_task: tokio::task::JoinHandle<()>,
}

impl TerminalPump {
    fn report_exit(&self, exit: ChildExit) {
        let _ = self.events.send(TerminalOutput::ExitStatus {
            code: exit.code,
            signal: exit.signal,
        });
    }

    async fn finish(self) {
        drop(self.events);
        let _ = self.output_task.await;
    }
}

async fn start_terminal_pump(
    mut pty: Pty,
    mut input_from_frontend: vox::Rx<TerminalInput>,
    output_to_frontend: vox::Tx<TerminalOutput>,
) -> TerminalPump {
    let read_fd = pty.master;
    let write_fd = unsafe { libc::dup(read_fd) };
    if write_fd < 0 {
        let err = std::io::Error::last_os_error();
        tracing::warn!("dup pty master failed: {err}");
    }
    unsafe {
        libc::close(pty.slave);
    }
    pty.master = -1;
    pty.slave = -1;

    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<TerminalOutput>();
    let events_from_reader = events_tx.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                let data = buf[..n as usize].to_vec();
                if events_from_reader
                    .send(TerminalOutput::Bytes { data })
                    .is_err()
                {
                    break;
                }
                continue;
            }
            if n == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EIO) => break,
                _ => {
                    let _ = events_from_reader.send(TerminalOutput::Error {
                        message: format!("pty read failed: {err}"),
                    });
                    break;
                }
            }
        }
        unsafe {
            libc::close(read_fd);
        }
    });

    let (input_tx, input_rx) = std::sync::mpsc::channel::<TerminalInput>();
    std::thread::spawn(move || {
        for input in input_rx {
            match input {
                TerminalInput::Bytes { data } => {
                    let mut offset = 0;
                    while offset < data.len() {
                        let n = unsafe {
                            libc::write(
                                write_fd,
                                data[offset..].as_ptr().cast(),
                                data.len() - offset,
                            )
                        };
                        if n > 0 {
                            offset += n as usize;
                        } else if std::io::Error::last_os_error().raw_os_error()
                            != Some(libc::EINTR)
                        {
                            break;
                        }
                    }
                }
                TerminalInput::Resize { size } => set_pty_size(write_fd, size),
                TerminalInput::Close => break,
            }
        }
        unsafe {
            libc::close(write_fd);
        }
    });

    tokio::spawn(async move {
        loop {
            match input_from_frontend.recv().await {
                Ok(Some(input_sref)) => {
                    let mut input = None;
                    let _ = input_sref.map(|value| {
                        input = Some(value);
                    });
                    if input_tx.send(input.expect("input set")).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = input_tx.send(TerminalInput::Close);
                    break;
                }
                Err(e) => {
                    tracing::warn!("terminal input recv failed: {e:?}");
                    let _ = input_tx.send(TerminalInput::Close);
                    break;
                }
            }
        }
    });

    let output_task = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            if output_to_frontend.send(event).await.is_err() {
                break;
            }
        }
        let _ = output_to_frontend.close(Default::default()).await;
    });

    TerminalPump {
        events: events_tx,
        output_task,
    }
}
