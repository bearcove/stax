//! `staxd` — root daemon for stax.
//!
//! Does the privileged profiling step so `stax record …` needs no
//! sudo. Two platform shapes (see `staxd-proto`), one role:
//!
//! * **macOS** ([`macos`]) — owns kperf+kdebug and *streams* raw
//!   `KdBuf` records to the client over a vox local socket.
//! * **Linux** ([`linux`]) — a one-shot **fd broker**: does the
//!   privileged per-CPU `perf_event_open` and hands the descriptors
//!   (`vox::Fd`, SCM_RIGHTS) to the unprivileged client, which mmaps
//!   and drains the rings itself.
//!
//! `main` is a thin platform dispatcher; each backend brings its own
//! `#[tokio::main]` entry point.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod session;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
fn main() -> eyre::Result<()> {
    macos::main()
}

#[cfg(target_os = "linux")]
fn main() -> eyre::Result<()> {
    linux::main()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn main() {
    eprintln!("staxd: unsupported platform (macOS or Linux only)");
    std::process::exit(1);
}
