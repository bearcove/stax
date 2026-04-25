//! Spawn a child process with our preload dylib injected and accept its
//! Mach task port via IPC. Adapted from
//! samply/src/mac/process_launcher.rs (commit
//! 1920bd32c569de5650d1129eb035f43bd28ace27). MIT OR Apache-2.0; see
//! LICENSE-MIT and LICENSE-APACHE at the crate root.
//!
//! Differences from samply:
//!   - The preload dylib is bundled by our own build.rs and dropped to a
//!     tempfile via `crate::preload::stage_preload_dylib`. samply ships a
//!     gzipped blob and decodes with flate2; we keep raw bytes for now.
//!   - `crate::shared::ctrl_c::CtrlC` is replaced with a small inline
//!     SIGINT-ignore guard.
//!   - The `MarkerFilePath` / `DotnetTracePath` IPC message kinds are
//!     dropped; we only handle `My task` and `Jitdump`.
//!   - The iteration_count / ignore_exit_code knobs are dropped: launch
//!     once, profile until exit.

use std::ffi::{OsStr, OsString};
use std::os::unix::prelude::OsStrExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use mach2::port::mach_port_t;
use mach2::task::task_resume;

use crate::mach_ipc::{BlockingMode, MachError, OsIpcMultiShotServer, OsIpcSender};
use crate::preload::{stage_preload_dylib, TempPreload};

/// Holder for the launched child + the env vars we set on it. The
/// `_temp_preload` field's Drop removes the staged dylib once recording
/// completes.
pub struct TaskLauncher {
    program: OsString,
    args: Vec<OsString>,
    extra_env: Vec<(OsString, OsString)>,
    _temp_preload: TempPreload,
}

impl TaskLauncher {
    pub fn new<I, S>(
        program: S,
        args: I,
        bootstrap_server_name: &str,
    ) -> Result<Self, MachError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let preload = stage_preload_dylib().map_err(|err| {
            log::error!("staging preload dylib failed: {err}");
            MachError::from(libc::EIO)
        })?;

        // Both the unprefixed name and `__XPC_<name>` -- the latter so the
        // env var crosses XPC service launches.
        let mut extra_env = Vec::new();
        let mut push_env = |k: &str, v: &OsStr| {
            extra_env.push((OsString::from(k), v.to_owned()));
            extra_env.push((OsString::from(format!("__XPC_{k}")), v.to_owned()));
        };
        push_env("DYLD_INSERT_LIBRARIES", &preload.dylib_path_os());
        push_env("NERF_BOOTSTRAP_SERVER_NAME", OsStr::new(bootstrap_server_name));

        Ok(Self {
            program: program.into(),
            args: args.into_iter().map(|a| a.into()).collect(),
            extra_env,
            _temp_preload: preload,
        })
    }

    pub fn launch_child(&self) -> Child {
        match Command::new(&self.program)
            .args(&self.args)
            .envs(self.extra_env.iter().map(|(k, v)| (k, v)))
            .spawn()
        {
            Ok(child) => child,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "error: could not find an executable named '{}'",
                    self.program.to_string_lossy()
                );
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("error: could not launch child process: {err}");
                std::process::exit(1);
            }
        }
    }
}

/// Receiver for the bootstrap messages our preload dylib sends back.
pub struct TaskAccepter {
    server: OsIpcMultiShotServer,
    server_name: String,
    queue: Vec<ReceivedStuff>,
}

impl TaskAccepter {
    pub fn new() -> Result<Self, MachError> {
        let (server, server_name) = OsIpcMultiShotServer::new()?;
        Ok(Self {
            server,
            server_name,
            queue: Vec::new(),
        })
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn queue_received_stuff(&mut self, rs: ReceivedStuff) {
        self.queue.push(rs);
    }

    pub fn next_message(&mut self, timeout: Duration) -> Result<ReceivedStuff, MachError> {
        if let Some(rs) = self.queue.pop() {
            return Ok(rs);
        }
        let (res, mut channels, _) = self
            .server
            .accept(BlockingMode::BlockingWithTimeout(timeout))?;
        let received_stuff = match res.split_at(7) {
            (b"My task", pid_bytes) => {
                assert!(pid_bytes.len() == 4);
                let pid =
                    u32::from_le_bytes([pid_bytes[0], pid_bytes[1], pid_bytes[2], pid_bytes[3]]);
                let task_channel = channels.pop().unwrap();
                let sender_channel = channels.pop().unwrap();
                let sender_channel = sender_channel.into_sender();
                let task = task_channel.into_port();
                ReceivedStuff::AcceptedTask(AcceptedTask {
                    task,
                    pid,
                    sender_channel: Some(sender_channel),
                })
            }
            (b"Jitdump", jitdump_info) => {
                let pid_bytes = &jitdump_info[0..4];
                let pid =
                    u32::from_le_bytes([pid_bytes[0], pid_bytes[1], pid_bytes[2], pid_bytes[3]]);
                let len = jitdump_info[4] as usize;
                let path = &jitdump_info[5..][..len];
                ReceivedStuff::JitdumpPath(pid, OsStr::from_bytes(path).into())
            }
            (other, _) => {
                // MarkerF / NetTrac etc. -- accept but ignore for now.
                log::debug!(
                    "TaskAccepter: ignoring unrecognised message kind {:?}",
                    other
                );
                ReceivedStuff::Ignored
            }
        };
        Ok(received_stuff)
    }
}

pub enum ReceivedStuff {
    AcceptedTask(AcceptedTask),
    JitdumpPath(u32, PathBuf),
    Ignored,
}

pub struct AcceptedTask {
    task: mach_port_t,
    pid: u32,
    sender_channel: Option<OsIpcSender>,
}

impl AcceptedTask {
    pub fn task(&self) -> mach_port_t {
        self.task
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Tell the child it can stop blocking and resume normal execution.
    /// The preload dylib's bootstrap code sits in a `recv` until we send
    /// it a `Proceed` byte; once acknowledged, the child runs as usual
    /// and we sample its task port.
    pub fn start_execution(&self) {
        if let Some(sender_channel) = &self.sender_channel {
            let _ = sender_channel.send(b"Proceed", vec![]);
        } else {
            unsafe { task_resume(self.task) };
        }
    }
}

