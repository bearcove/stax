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
//! On-CPU `CpuIntervalEvent`s are synthesized from consecutive
//! per-thread samples (the perf-frequency analog of macOS's
//! ground-truth MACH_SCHED slices) so the aggregator's time
//! attribution — and therefore `stax top`/flamegraph — works.
//!
//! Off-CPU `CpuIntervalEvent`s come from a side-band
//! `PERF_RECORD_SWITCH_CPU_WIDE` ring (a `DUMMY` event with
//! `context_switch` set — no root-only tracefs `sched_switch`
//! tracepoint needed): a voluntary switch-out opens an off-CPU span
//! whose stack is the thread's last sampled stack (its "parked"
//! stack, mirroring macOS), and the matching switch-in closes it with
//! a true scheduler duration. Wakeup attribution (`waker_tid` via
//! `sched_waking`) stays a follow-on.

#![cfg(target_os = "linux")]

mod daemon;
#[cfg(target_arch = "x86_64")]
mod dwarf;
mod elf;
mod proc;
mod session;
mod sys;

pub use daemon::record_via_daemon;
pub use elf::FramePointerStats;
/// The privileged half of the Linux fd broker, used by the `staxd`
/// daemon to `perf_event_open` per CPU and report the ring geometry.
pub use sys::{
    DATA_PAGES, PmuMember, online_cpus, open_cpu_fd, open_cpu_pmu_siblings, open_cpu_switch_fd,
    open_cpu_waking_fd, page_size, read_sched_waking_tracepoint,
};

/// Inspect an executable's `.text` function prologues to guess whether
/// it was built `-fomit-frame-pointer`. Powers `stax record`'s
/// `--dwarf-unwind` auto mode: when the target omits frame pointers
/// the kernel's `CALLCHAIN` truncates and DWARF unwinding is worth its
/// per-sample cost.
///
/// `None` when the binary can't be inspected confidently — unreadable,
/// not a 64-bit ELF, or stripped down to fewer than 8 functions in
/// `.text`. Callers treat `None` as "leave DWARF unwinding off; the
/// user can still force it with `--dwarf-unwind`".
pub fn scan_frame_pointers(exe_path: &std::path::Path) -> Option<FramePointerStats> {
    let bytes = std::fs::read(exe_path).ok()?;
    let img = elf::scan(&bytes)?;
    elf::frame_pointer_stats(&img.symbols, img.text.as_ref()?)
}

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
    /// Replay user stacks via `.eh_frame` CFI in userspace, instead of
    /// (just) the kernel's frame-pointer walker. Asks each sample to
    /// also carry the user `rip/rsp/rbp` plus an 8 KiB stack snapshot;
    /// `framehop` then unwinds against the live ELF's DWARF unwind
    /// tables. Necessary for `-fomit-frame-pointer` binaries (most
    /// distro libc, libstdc++, OpenSSL, Rust release builds without
    /// `-Cforce-frame-pointers`) — without it the kernel CALLCHAIN
    /// truncates at the first non-FP frame. x86_64-only this round;
    /// silently ignored on aarch64-Linux (FP-by-default ABI).
    pub dwarf_unwind: bool,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            pid: 0,
            frequency_hz: 999,
            duration: None,
            kernel_stacks: true,
            dwarf_unwind: false,
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
    /// Synthetic on-CPU intervals emitted from consecutive samples
    /// (the perf-frequency analog of macOS MACH_SCHED slices).
    pub intervals: u64,
    /// Off-CPU intervals emitted from context-switch records.
    pub off_cpu_intervals: u64,
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
