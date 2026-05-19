//! Linux sampling backend for stax — the `perf_event_open` counterpart
//! of the macOS kperf path.
//!
//! Same role as the macOS `staxd-client`: produce the OS-neutral
//! [`SampleSink`] event stream (on-CPU stacks, image loads, thread
//! names, kallsyms) from a running target. Where macOS owns kperf in a
//! privileged daemon and streams raw `KdBuf` records to a client-side
//! parser, here we open `perf_event_open` system-wide per-CPU, drain
//! the mmap ring buffers, and parse `PERF_RECORD_*` in process. The
//! daemon/systemd split (for hosts with a restrictive
//! `perf_event_paranoid`) is layered on in a later phase; the parsing
//! core lives here so both paths share it.
//!
//! Off-CPU intervals and wakeup attribution (the `CpuIntervalEvent` /
//! `WakeupEvent` parity, via `sched_*` tracepoints) are a deliberate
//! follow-on — this is the on-CPU flamegraph spine.

#![cfg(target_os = "linux")]

mod elf;
mod proc;
mod session;
mod sys;

use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub use stax_mac_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, JitdumpEvent, SampleEvent, SampleSink, ThreadNameEvent,
    WakeupEvent,
};

/// User-facing recording options. Mirrors the shape of
/// `staxd_client::RemoteOptions` so the `stax record` plumbing is
/// mechanical, minus the macOS-only `task` port and the daemon socket
/// (in-process path).
#[derive(Clone, Debug)]
pub struct RecordOptions {
    /// Target pid. All of its threads (current and future) are
    /// followed; samples from other processes are filtered out.
    pub pid: u32,
    /// Sampling frequency in Hz (per the kernel's `freq` mode).
    pub frequency_hz: u32,
    /// Stop after this long, if set. Independent of `should_stop`.
    pub duration: Option<Duration>,
    /// Include kernel-side stack frames (requires the host to allow it;
    /// `perf_event_paranoid <= 1`). User frames are always captured.
    pub kernel_stacks: bool,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            pid: 0,
            frequency_hz: 999,
            duration: None,
            kernel_stacks: true,
        }
    }
}

/// What a finished session produced — enough for the caller to log a
/// one-line summary, matching `staxd_proto::RecordSummary` in spirit.
#[derive(Clone, Copy, Debug, Default)]
pub struct RecordSummary {
    pub samples: u64,
    /// Ring-buffer records the kernel reported as lost (overruns).
    pub lost_records: u64,
    pub binaries: u64,
    pub session_ns: u64,
}

/// Open `perf_event_open` against `opts.pid`, drain until the duration
/// elapses or `should_stop` flips, driving `sink` with the same event
/// sequence the macOS backend produces.
pub fn record(
    opts: &RecordOptions,
    sink: &mut dyn SampleSink,
    should_stop: &AtomicBool,
) -> eyre::Result<RecordSummary> {
    session::run(opts, sink, should_stop)
}
