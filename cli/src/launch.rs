//! Launch + PTY ownership for `stax record -- <argv...>`.
//!
//! The CLI is the natural home for these responsibilities now that
//! the `stax-shade` companion process is gone: it's user-uid,
//! already attached to the user's terminal, and the only stax
//! process that needs the target's stdout/stdin in its own console.
//!
//! Flow:
//!
//!   1. `posix_spawn(POSIX_SPAWN_START_SUSPENDED)` the target with
//!      a fresh PTY as stdin/stdout/stderr.
//!   2. Hand the resulting PID to `RunControl::start_attach` so
//!      stax-server tells staxd to sample it.
//!   3. `SIGCONT` the target once the server confirmed the run is
//!      started (we don't have a finer-grained "staxd ready" hook
//!      to wait on from the CLI side — start_attach returning is
//!      the closest signal). The brief window where the target is
//!      live without active sampling is bounded by the staxd
//!      session warm-up; we'll tighten this with a server-side
//!      ready notification if it matters.
//!   4. Pump PTY ↔ CLI stdio locally on dedicated threads. No vox
//!      indirection.
//!   5. `waitpid` for target exit; on stop, SIGKILL if it hasn't
//!      exited yet.

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::os::raw::c_char;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

/// One launched-and-suspended target. The PID has already been
/// handed to the server; the caller resumes the target by calling
/// `resume()` once the recording is in motion.
pub struct Launched {
    pub pid: u32,
    master_fd: RawFd,
    slave_fd_at_spawn: RawFd,
    pty_pump: Option<PtyPump>,
}

impl Launched {
    pub fn resume(&self) -> std::io::Result<()> {
        let r = unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGCONT) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Hand the target's PTY traffic to local stdin/stdout. Spawns
    /// pump threads on the OS scheduler (no tokio). Idempotent if
    /// called twice — only the first hand-off takes effect.
    pub fn start_pty_pump(&mut self) {
        if self.pty_pump.is_some() {
            return;
        }
        self.pty_pump = Some(PtyPump::start(self.master_fd));
    }

    /// Push the current terminal size down to the PTY master so
    /// the target sees a SIGWINCH-equivalent. Best-effort.
    pub fn resize(&self, size: TerminalSize) {
        if self.master_fd < 0 {
            return;
        }
        set_pty_size(self.master_fd, size);
    }

    /// SIGKILL + reap. Called on stop when the target hasn't exited
    /// on its own (typically: user Ctrl-C while the target is still
    /// running, or start_attach failing after spawn).
    pub fn terminate(&self) {
        let mut status = 0;
        let r =
            unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, libc::WNOHANG) };
        if r == self.pid as libc::pid_t {
            return;
        }
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
            libc::waitpid(self.pid as libc::pid_t, &mut status, 0);
        }
    }
}

impl Drop for Launched {
    fn drop(&mut self) {
        // Stop the pump threads (closes the master fd they're
        // reading from, which makes them exit).
        if let Some(pump) = self.pty_pump.take() {
            pump.shutdown();
        } else {
            // Pump never started — close the master fd ourselves.
            if self.master_fd >= 0 {
                unsafe { libc::close(self.master_fd) };
            }
        }
        // slave_fd was closed inside posix_spawn_file_actions; we
        // tracked it for symmetry but don't own it post-spawn.
        let _ = self.slave_fd_at_spawn;
    }
}

pub fn posix_spawn_suspended(
    argv: &[String],
    cwd: Option<&str>,
    terminal_size: Option<TerminalSize>,
) -> std::io::Result<Launched> {
    if argv.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "launch argv is empty",
        ));
    }

    let program = CString::new(argv[0].as_str())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "argv[0] has NUL"))?;
    let argv_c: Vec<CString> = argv
        .iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "argv has NUL"))?;
    let mut argv_p: Vec<*mut c_char> = argv_c
        .iter()
        .map(|c| c.as_ptr() as *mut c_char)
        .collect();
    argv_p.push(ptr::null_mut());

    let (master_fd, slave_fd) = open_pty(terminal_size)?;

    let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
    let r = unsafe { libc::posix_spawnattr_init(&mut attr) };
    if r != 0 {
        unsafe { libc::close(master_fd); libc::close(slave_fd) };
        return Err(std::io::Error::from_raw_os_error(r));
    }
    let flags = libc::POSIX_SPAWN_START_SUSPENDED | libc::POSIX_SPAWN_SETSIGDEF;
    let r = unsafe { libc::posix_spawnattr_setflags(&mut attr, flags as libc::c_short) };
    if r != 0 {
        unsafe {
            libc::posix_spawnattr_destroy(&mut attr);
            libc::close(master_fd);
            libc::close(slave_fd);
        }
        return Err(std::io::Error::from_raw_os_error(r));
    }

    let mut actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
    let r = unsafe { libc::posix_spawn_file_actions_init(&mut actions) };
    if r != 0 {
        unsafe {
            libc::posix_spawnattr_destroy(&mut attr);
            libc::close(master_fd);
            libc::close(slave_fd);
        }
        return Err(std::io::Error::from_raw_os_error(r));
    }
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        let r = unsafe { libc::posix_spawn_file_actions_adddup2(&mut actions, slave_fd, fd) };
        if r != 0 {
            unsafe {
                libc::posix_spawn_file_actions_destroy(&mut actions);
                libc::posix_spawnattr_destroy(&mut attr);
                libc::close(master_fd);
                libc::close(slave_fd);
            }
            return Err(std::io::Error::from_raw_os_error(r));
        }
    }
    if slave_fd > libc::STDERR_FILENO {
        let r = unsafe { libc::posix_spawn_file_actions_addclose(&mut actions, slave_fd) };
        if r != 0 {
            unsafe {
                libc::posix_spawn_file_actions_destroy(&mut actions);
                libc::posix_spawnattr_destroy(&mut attr);
                libc::close(master_fd);
                libc::close(slave_fd);
            }
            return Err(std::io::Error::from_raw_os_error(r));
        }
    }
    let r = unsafe { libc::posix_spawn_file_actions_addclose(&mut actions, master_fd) };
    if r != 0 {
        unsafe {
            libc::posix_spawn_file_actions_destroy(&mut actions);
            libc::posix_spawnattr_destroy(&mut attr);
            libc::close(master_fd);
            libc::close(slave_fd);
        }
        return Err(std::io::Error::from_raw_os_error(r));
    }

    let prev_cwd = cwd.and_then(|target| {
        let prev = std::env::current_dir().ok();
        let target_c = CString::new(target).ok()?;
        unsafe { libc::chdir(target_c.as_ptr()) };
        prev
    });

    let mut pid: libc::pid_t = 0;
    let r = unsafe {
        libc::posix_spawnp(
            &mut pid,
            program.as_ptr(),
            &actions,
            &attr,
            argv_p.as_ptr(),
            extern_environ(),
        )
    };

    if let Some(prev) = prev_cwd {
        let _ = std::env::set_current_dir(prev);
    }

    unsafe {
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);
    }

    if r != 0 {
        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
        return Err(std::io::Error::from_raw_os_error(r));
    }

    // Parent doesn't need the slave end (dup'd into the child).
    unsafe { libc::close(slave_fd) };

    Ok(Launched {
        pid: pid as u32,
        master_fd,
        slave_fd_at_spawn: slave_fd,
        pty_pump: None,
    })
}

fn open_pty(size: Option<TerminalSize>) -> std::io::Result<(RawFd, RawFd)> {
    let mut master = -1;
    let mut slave = -1;
    let r = if let Some(size) = size {
        let mut winsize = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut winsize,
            )
        }
    } else {
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        }
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((master, slave))
}

pub fn set_pty_size(fd: RawFd, size: TerminalSize) {
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

unsafe extern "C" {
    static environ: *mut *mut std::os::raw::c_char;
}

fn extern_environ() -> *const *mut std::os::raw::c_char {
    unsafe { environ as *const _ }
}

/// Bidirectional pump between the PTY master and local stdin/stdout.
/// Owns the master fd; closing the pump closes the fd, which makes
/// the reader thread see EOF and exit.
struct PtyPump {
    shutdown_flag: Arc<AtomicBool>,
    master_fd: RawFd,
}

impl PtyPump {
    fn start(master_fd: RawFd) -> Self {
        let shutdown_flag = Arc::new(AtomicBool::new(false));

        // Master → stdout (blocking on a thread).
        let reader_master = master_fd;
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut stdout = std::io::stdout();
            loop {
                let n = unsafe { libc::read(reader_master, buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    let _ = stdout.write_all(&buf[..n as usize]);
                    let _ = stdout.flush();
                    continue;
                }
                if n == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EIO) => break, // slave side hung up
                    _ => break,
                }
            }
        });

        // stdin → master (blocking on a thread). Closes when stdin
        // hits EOF or the shutdown flag flips.
        let writer_master = unsafe { libc::dup(master_fd) };
        let writer_flag = shutdown_flag.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 8192];
            loop {
                if writer_flag.load(Ordering::Relaxed) {
                    break;
                }
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut off = 0;
                        while off < n {
                            let w = unsafe {
                                libc::write(
                                    writer_master,
                                    buf[off..n].as_ptr().cast(),
                                    n - off,
                                )
                            };
                            if w <= 0 {
                                let err = std::io::Error::last_os_error();
                                if err.raw_os_error() == Some(libc::EINTR) {
                                    continue;
                                }
                                return;
                            }
                            off += w as usize;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            unsafe { libc::close(writer_master) };
        });

        Self {
            shutdown_flag,
            master_fd,
        }
    }

    fn shutdown(self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        // Closing the master fd unblocks the reader thread's `read`.
        if self.master_fd >= 0 {
            unsafe { libc::close(self.master_fd) };
        }
    }
}
