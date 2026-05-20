//! Precise launch for `stax record -- <argv>` on Linux.
//!
//! `std::process::Command::spawn` synchronises with the child on its
//! own CLOEXEC pipe so the caller knows whether `exec` succeeded
//! — but that means any `pre_exec` `SIGSTOP` to "spawn suspended"
//! deadlocks `spawn()` itself. Linux has no `posix_spawn(START_SUSPENDED)`
//! either. So we drop down to raw `fork()` + a parent→child
//! "go" pipe, matching the pattern `perf record` uses:
//!
//!   1. Parent `pipe2(O_CLOEXEC)` and `fork()`.
//!   2. Child closes the write end and `read()`s the read end — the
//!      thread blocks until the parent signals "go". The child does
//!      *not* `exec()` yet, so no target instructions execute.
//!   3. Parent closes the read end, hands the child's pid to the
//!      recorder, and waits for perf to attach.
//!   4. Parent writes a "go" byte (and closes the pipe). The child's
//!      `read()` returns, then it `execvp()`s the target. The read
//!      end is `CLOEXEC`, so it goes away on exec — no leaked fd.
//!   5. From the kernel's perspective the very first instruction of
//!      the post-exec target is sampled: perf was already attached to
//!      the (pre-exec) pid, and `PERF_RECORD_MMAP*` for the new
//!      program text fires through the existing events.
//!
//! Everything between `fork()` and `execvp()` in the child runs in
//! the "async-signal-safe" window — so the child uses only raw libc
//! calls and pre-built `CString`s; no allocations, no Drop, no Rust
//! globals.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::ptr;

/// One launched-and-paused child. The pid exists in `/proc/<pid>`
/// (so the recorder can `perf_event_open` against it) but the child
/// is blocked on `read()` — its first executed target instruction is
/// the one after [`Self::resume`].
pub struct LinuxLaunched {
    pub pid: u32,
    /// Write end of the go-pipe; writing to it (or closing it)
    /// unblocks the child's `read()`. Set to `-1` once consumed by
    /// `resume` / `Drop` so the same fd is never closed twice.
    go_write: RawFd,
}

impl LinuxLaunched {
    /// Unblock the child so it `execvp()`s the target.
    pub fn resume(&mut self) -> io::Result<()> {
        if self.go_write < 0 {
            return Ok(()); // already resumed
        }
        let byte: u8 = 1;
        let n =
            unsafe { libc::write(self.go_write, &byte as *const u8 as *const _, 1) };
        // Closing also unblocks the child (EOF), so we keep going
        // even if the write was short — the child sees the pipe go
        // away and falls through to exec.
        let write = std::mem::replace(&mut self.go_write, -1);
        unsafe { libc::close(write) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// SIGKILL + reap. Safe to call after the child has already
    /// exited — `waitpid` with `WNOHANG` short-circuits.
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

impl Drop for LinuxLaunched {
    fn drop(&mut self) {
        if self.go_write >= 0 {
            // Closing the write end is an EOF on the child's read,
            // so it unblocks — important so we don't leak a stuck
            // child if the caller never called `resume()`.
            unsafe { libc::close(self.go_write) };
            self.go_write = -1;
        }
    }
}

/// `fork()` the target in a paused state and return a handle that
/// can `resume()` it once the caller is ready.
pub fn fork_suspended(argv: &[String]) -> io::Result<LinuxLaunched> {
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    }
    // Pre-build NUL-terminated C strings — allocation is unsafe in
    // the post-fork child, so everything the child needs has to
    // exist before `fork()`.
    let program = CString::new(argv[0].as_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argv[0] has NUL"))?;
    let argv_c: Vec<CString> = argv
        .iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argv has NUL"))?;
    let mut argv_p: Vec<*const libc::c_char> =
        argv_c.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(ptr::null());

    // CLOEXEC on both ends: read_end vanishes on exec (the whole
    // point), write_end vanishes on any later exec stax itself might
    // do (we never re-exec, so this is belt + suspenders).
    let mut pipefd: [libc::c_int; 2] = [-1; 2];
    let r = unsafe { libc::pipe2(pipefd.as_mut_ptr(), libc::O_CLOEXEC) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let read_end = pipefd[0];
    let write_end = pipefd[1];

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = io::Error::last_os_error();
        unsafe {
            libc::close(read_end);
            libc::close(write_end);
        }
        return Err(e);
    }

    if pid == 0 {
        // === CHILD === async-signal-safe only.
        // We're CLOEXEC for inheritance, but explicitly close
        // write_end here so the child's read sees EOF if the parent
        // forgets the "go" write (the parent's `Drop` covers this).
        unsafe { libc::close(write_end) };
        // Block until parent signals. Tolerate EINTR.
        let mut buf: [u8; 1] = [0];
        loop {
            let n = unsafe { libc::read(read_end, buf.as_mut_ptr().cast(), 1) };
            if n >= 0 {
                break;
            }
            let errno = unsafe { *libc::__errno_location() };
            if errno != libc::EINTR {
                break;
            }
        }
        // Either we got the byte (n == 1), the parent closed the pipe (n == 0),
        // or `read` failed; in every case the answer is "exec now".
        // execvp searches PATH for argv[0]. The read_end is CLOEXEC, so
        // it disappears as part of execve's atomic fd cleanup.
        unsafe {
            libc::execvp(program.as_ptr(), argv_p.as_ptr() as *const *const libc::c_char);
        }
        // execvp only returns on error.
        let errno = unsafe { *libc::__errno_location() };
        // 127 = "command not found" by long convention; 126 = "found but
        // not executable". Anything else maps to a generic failure code.
        let code = match errno {
            libc::ENOENT => 127,
            libc::EACCES => 126,
            _ => 125,
        };
        unsafe { libc::_exit(code) };
    }

    // === PARENT ===
    unsafe { libc::close(read_end) };
    Ok(LinuxLaunched {
        pid: pid as u32,
        go_write: write_end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `comm` of a pid via `/proc/<pid>/comm`. `None` if the file is
    /// gone (process exited).
    fn read_comm(pid: u32) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// Before `resume()`, the child's `comm` should still be ours
    /// (the test binary) — it hasn't `execvp`d yet. After `resume()`,
    /// it switches to the target program's name.
    #[test]
    fn child_pauses_until_resume() {
        // Use a target whose comm differs from ours so the transition
        // is unambiguous. `/usr/bin/true` is `true`.
        let mut launched =
            fork_suspended(&["/usr/bin/true".to_string()]).expect("fork");
        let pid = launched.pid;

        // Child should be alive but pre-exec. `comm` is the executable
        // *name* (parent's, since the child inherits our task_struct
        // until exec). We assert it's NOT "true" yet.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let pre = read_comm(pid).expect("child must exist pre-resume");
        assert_ne!(
            pre, "true",
            "child execvp'd before resume() (comm={pre})"
        );

        launched.resume().expect("resume");

        // Wait for the child to actually exec + exit.
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            match read_comm(pid) {
                None => break, // exited
                Some(c) if c == "true" => {
                    // exec'd; reap and we're done
                    let mut status = 0;
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
                    return;
                }
                _ => continue,
            }
        }
        // Reaped via /proc disappearance (exited before we caught comm="true").
        let mut status = 0;
        unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    }

    /// Dropping the handle without `resume()` shouldn't leave the
    /// child stuck — the closing of `go_write` is the EOF signal.
    #[test]
    fn drop_unblocks_child() {
        let launched =
            fork_suspended(&["/usr/bin/true".to_string()]).expect("fork");
        let pid = launched.pid;
        drop(launched);

        // Child should finish on its own now (`true` exits immediately
        // after exec). Give it up to 1s.
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let mut status = 0;
            let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
            if r == pid as libc::pid_t {
                return;
            }
        }
        // Last-ditch reap so a hung child doesn't poison other tests.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        let mut status = 0;
        unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
        panic!("child still alive after Drop");
    }
}
