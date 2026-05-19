//! Wire schema for the staxd RPC.
//!
//! Two platform protocols, same daemon role (do the privileged thing
//! so `stax record …` needs no sudo), different shape because the OS
//! primitives differ:
//!
//! * **macOS** ([`macos`]) — xnu has no descriptor to share, so the
//!   daemon owns kperf+kdebug and *streams* raw `KdBuf` batches to the
//!   client, which runs the parser.
//! * **Linux** ([`linux`]) — `perf_event_open` *is* a descriptor, so
//!   the daemon is a one-shot **fd broker**: it does only the
//!   privileged per-CPU open and hands the fds (`vox::Fd`, SCM_RIGHTS)
//!   plus the scalars to mmap/parse them to the unprivileged caller,
//!   which drains the rings itself.
//!
//! Either way the wire stays stable: the things that change rapidly
//! during development (sample shape, off-CPU classification, view
//! models) never reach the daemon. What *can* change here is the OS
//! primitive (Apple changing kdebug; a new perf knob), in lockstep on
//! both ends, or a forward-compatible config addition.
//!
//! The matching daemon binary lives in `staxd`. The matching consumer
//! is `staxd-client` (macOS) / `stax-linux-capture` (Linux).

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
