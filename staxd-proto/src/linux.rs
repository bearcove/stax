//! Linux staxd protocol: a one-shot **fd broker**.
//!
//! Unlike macOS — where xnu has no descriptor to share so the daemon
//! streams `KdBuf` records — Linux `perf_event_open` *is* a file
//! descriptor. So the privileged daemon does only the privileged part
//! (the per-CPU `perf_event_open`) and hands the resulting descriptors
//! back to the unprivileged caller, which then mmaps the rings and
//! drains/parses them itself (reusing the exact in-process capture
//! core). The descriptors travel as [`vox::Fd`] in `SCM_RIGHTS`
//! ancillary data over the Unix-domain link; the daemon is out of the
//! data path the instant it replies.
//!
//! This keeps the wire stable for the same reason the macOS side is
//! stable: everything that turns records into samples, attributes
//! off-CPU intervals, resolves symbols, and renders the UI lives in
//! the unprivileged client. The only privileged surface is "open
//! these N perf events", which never changes shape.

use facet::Facet;

/// Default Unix-domain socket the systemd unit binds and the client
/// dials. Production deployments may override via `--socket`.
pub const STAXD_LINUX_SOCKET_DEFAULT: &str = "/run/staxd.sock";

/// What the unprivileged client asks the privileged daemon to open.
/// Mirrors `stax_linux_capture::RecordOptions` minus the bits the
/// client handles itself after it has the fds (duration, stop flag).
#[derive(Clone, Debug, Facet)]
pub struct PerfSessionConfig {
    /// Target pid. System-wide events are opened; the client filters
    /// to this pid in userspace (so all of its threads, including
    /// pre-existing and short-lived ones, are captured).
    pub target_pid: u32,
    /// Sampling frequency in Hz (kernel `freq` mode).
    pub frequency_hz: u32,
    /// Include kernel-side stack frames (`exclude_kernel = 0`). The
    /// daemon is privileged so this generally succeeds; the client
    /// degrades gracefully if a ring lacks kernel frames.
    pub kernel_stacks: bool,
    /// Also broker per-CPU `sched:sched_waking` tracepoint rings so
    /// the client can attribute `OffCpu.waker_tid`. The tracepoint
    /// id/format lives in root-only tracefs, so the unprivileged side
    /// can't open these itself — this is the whole reason the daemon
    /// exists. Best-effort on the daemon side: if tracefs is
    /// unreadable or the tracepoint isn't available, `waking` comes
    /// back empty and wakeups stay unattributed.
    pub request_waking: bool,
    /// Also attach the HW counter group (cycles, instructions, L1D
    /// read misses, branch mispredicts) as siblings of each per-CPU
    /// sampling leader, and broker their fds + perf event ids. On a
    /// locked-down host (`perf_event_paranoid >= 2`) the unprivileged
    /// caller can't `perf_event_open` HW counters itself, so the
    /// daemon is the only path to populating
    /// `SampleEvent::{cycles, instructions, l1d_misses, branch_mispreds}`.
    /// Best-effort: a host without an exposed vPMU just gets zeros
    /// for those fields; the rest of the sample (callchain, off-CPU,
    /// wakeups) is unaffected.
    pub request_pmu: bool,
    /// Open the per-CPU sampling rings with `PERF_SAMPLE_REGS_USER` +
    /// `PERF_SAMPLE_STACK_USER` so each sample carries the user
    /// rip/rsp/rbp plus an 8 KiB stack snapshot. The unprivileged
    /// client uses these to DWARF-unwind through `-fomit-frame-pointer`
    /// binaries (libc, OpenSSL, most Rust release builds, …) where
    /// the kernel's frame-pointer CALLCHAIN truncates early. Perf attrs
    /// are immutable post-open, so the bit has to ride the broker
    /// request rather than be flipped on the unprivileged side. The
    /// flag is a no-op on non-x86_64 daemons (FP-by-default ABIs).
    pub request_dwarf_unwind: bool,
}

/// Where the wakee tid lives inside the `sched:sched_waking`
/// tracepoint RAW payload. The kernel writes the fields at host-
/// specific byte offsets that come from
/// `/sys/kernel/tracing/events/sched/sched_waking/format`; the
/// privileged daemon parses that file (root-only on most hosts) and
/// hands the offsets to the unprivileged client. There is no stable
/// across-kernels layout, so the format MUST come from the live
/// kernel that issued the fds.
#[derive(Clone, Copy, Debug, Default, Facet)]
pub struct WakingFieldOffsets {
    /// Byte offset of `pid_t pid` (the wakee tid) inside the RAW
    /// payload. Note: *not* `common_pid` (that's the waker, which we
    /// get for free as the sample's TID).
    pub wakee_pid_offset: u32,
    /// Size of the wakee field in bytes (typically 4 for `pid_t`).
    pub wakee_pid_size: u32,
}

/// The fd-broker reply: per-CPU `perf_event_open` descriptors plus the
/// scalars the unprivileged side needs to mmap and parse them.
///
/// Not `Clone` — [`vox::Fd`] owns a descriptor and is consumed once,
/// when the client maps each ring.
#[derive(Debug, Facet)]
pub struct PerfSessionFds {
    /// One sampling-ring fd per online CPU, in CPU order. The events
    /// are opened **disabled**; the client enables them after mmap
    /// (an ioctl on the fd it now owns — no privilege needed).
    pub sampling: Vec<vox::Fd>,
    /// One context-switch-ring fd per online CPU, in CPU order. Empty
    /// when the kernel/host can't do `context_switch` (off-CPU
    /// attribution disabled; the on-CPU profile still works).
    pub switch: Vec<vox::Fd>,
    /// One `sched:sched_waking` tracepoint fd per online CPU, in CPU
    /// order. Populated only when the client set
    /// [`PerfSessionConfig::request_waking`] and the daemon could
    /// read the tracepoint id/format from tracefs. Empty otherwise —
    /// the client then falls back to no wakeup attribution
    /// (`OffCpu.waker_tid = None`).
    pub waking: Vec<vox::Fd>,
    /// RAW-payload field offsets for `sched:sched_waking`, parsed by
    /// the daemon from the live kernel's tracefs format file. `Some`
    /// iff [`Self::waking`] is non-empty.
    pub waking_field_offsets: Option<WakingFieldOffsets>,
    /// HW counter sibling fds for the sampling-leader group, packed
    /// per CPU in canonical [`PmuKind`] order (cycles, instructions,
    /// L1D read misses, branch mispredicts). Empty when the client
    /// didn't request the group, or when any CPU couldn't open all
    /// four — the daemon prefers no group to a partial group so the
    /// client doesn't have to special-case missing slots.
    pub pmu: Vec<vox::Fd>,
    /// `perf event id` (from `PERF_EVENT_IOC_ID`) of each entry in
    /// [`Self::pmu`], parallel to it. The client demultiplexes the
    /// leader's `PERF_SAMPLE_READ` block by id, so the daemon MUST
    /// fetch and ship these — there is no way for the client to ask
    /// the kernel about a sibling it didn't open.
    pub pmu_ids: Vec<u64>,
    /// Siblings per CPU — 4 (the full group) or 0 (no group). The
    /// client uses this to split `pmu` into per-CPU chunks.
    pub pmu_per_cpu: u32,
    /// `online_cpus().len()` the daemon used. Equals `sampling.len()`
    /// on success; the client sizes its ring arrays from this.
    pub cpu_count: u32,
    /// `sysconf(_SC_PAGESIZE)` on the daemon host. The ring mmap is
    /// `(1 + data_pages) * page_size` bytes.
    pub page_size: u32,
    /// Data pages per ring (the `2^n` in `1 + 2^n` pages).
    pub data_pages: u32,
    /// Echoed back so the client can sanity-check the handoff.
    pub target_pid: u32,
    pub frequency_hz: u32,
    pub kernel_stacks: bool,
}

/// Why the daemon could not open a perf session. Variant names point
/// at the failing step so the client can render a precise message
/// (and decide whether to fall back to the in-process wchan path).
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum PerfSessionError {
    /// The daemon can't `perf_event_open` system-wide: it is neither
    /// root nor has `CAP_PERFMON`, and `perf_event_paranoid` is too
    /// high for an unprivileged open. `detail` carries the host's
    /// paranoid level / errno for diagnostics.
    NotPrivileged { detail: String },
    /// `perf_event_open` failed on a specific CPU for a reason other
    /// than privilege (ENODEV, EMFILE, …).
    PerfEventOpen {
        cpu: u32,
        errno: i32,
        detail: String,
    },
    /// `/proc/<pid>` does not exist — the target is gone.
    NoSuchTarget(u32),
    /// The connection's peer uid is not allowed to profile the target.
    /// (Peer-credential authorisation is a follow-up; reserved here so
    /// the wire already has the variant.)
    NotAuthorized { caller_uid: u32, target_uid: u32 },
}

/// Cheap probe — what a client calls before `open_perf_session` to
/// learn whether this daemon can actually broker fds on this host.
#[derive(Clone, Debug, Facet)]
pub struct DaemonStatus {
    /// staxd version string (diagnostics only; vox handles schema
    /// evolution).
    pub version: String,
    /// Architecture the daemon runs on ("x86_64", "aarch64").
    pub host_arch: String,
    /// True when the daemon process can `perf_event_open` system-wide
    /// (running as root or holding `CAP_PERFMON`).
    pub privileged: bool,
    /// `/proc/sys/kernel/perf_event_paranoid`, or `i32::MIN` if it
    /// could not be read.
    pub perf_event_paranoid: i32,
}

/// The Linux staxd RPC. Deliberately tiny: one fd-broker call and one
/// probe. There is no streaming channel — the descriptors *are* the
/// payload, and the kernel ring buffers are the data path.
#[vox::service]
pub trait StaxdLinux {
    /// `perf_event_open` the per-CPU sampling (and best-effort
    /// context-switch) rings for `config.target_pid` and return their
    /// descriptors. The daemon retains nothing: once this replies, the
    /// caller owns the events and the daemon is free.
    async fn open_perf_session(
        &self,
        config: PerfSessionConfig,
    ) -> Result<PerfSessionFds, PerfSessionError>;

    /// Reachability + capability probe.
    async fn status(&self) -> DaemonStatus;
}
