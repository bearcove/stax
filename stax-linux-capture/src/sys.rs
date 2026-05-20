//! Thin safe-ish layer over `perf_event_open` + the mmap ring buffer.
//!
//! We open one sampling event per online CPU, system-wide (`pid = -1`),
//! and filter to the target in userspace. That captures every thread of
//! the target — including ones that already existed and short-lived
//! children — which a single `pid > 0` event would miss. (`paranoid`
//! must allow it; on a locked-down host the daemon phase opens these
//! with privilege instead.)

use std::collections::HashMap;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::ptr;
use std::sync::atomic::{Ordering, fence};

use perf_event_open_sys as pe;
use staxd_proto::WakingFieldOffsets;

/// `PERF_CONTEXT_*` sentinels that split a callchain into kernel/user
/// regions. They are huge unsigned values (negative `i64` cast to
/// `u64`); anything `>= PERF_CONTEXT_MAX` is a marker, not an address.
pub const PERF_CONTEXT_KERNEL: u64 = (-128i64) as u64;
pub const PERF_CONTEXT_USER: u64 = (-512i64) as u64;
/// Largest reserved marker value; real addresses are always below this.
pub const PERF_CONTEXT_MAX: u64 = (-4095i64) as u64;

/// `<asm/perf_regs.h>` indices for the x86_64 user GPRs that framehop
/// asks for (and only those three — DWARF rules can reference other
/// regs but framehop deliberately doesn't recover them since they
/// aren't needed for return addresses). perf streams the captured
/// values in ascending bit-index order, so with this mask the on-wire
/// triple is `bp, sp, ip`.
#[cfg(target_arch = "x86_64")]
pub const PERF_REG_X86_BP: u32 = 6;
#[cfg(target_arch = "x86_64")]
pub const PERF_REG_X86_SP: u32 = 7;
#[cfg(target_arch = "x86_64")]
pub const PERF_REG_X86_IP: u32 = 8;

/// Bitmask we pass in `perf_event_attr.sample_regs_user`. Only the
/// three regs framehop needs.
#[cfg(target_arch = "x86_64")]
pub const DWARF_USER_REGS_MASK: u64 =
    (1u64 << PERF_REG_X86_BP) | (1u64 << PERF_REG_X86_SP) | (1u64 << PERF_REG_X86_IP);

/// `abi` value the kernel writes at the head of a `PERF_SAMPLE_REGS_USER`
/// block to say "the regs are 64-bit". `_NONE` (0) means the kernel
/// couldn't capture them (e.g. sample taken in kernel mode for a
/// 32-bit task on a 64-bit kernel).
pub const PERF_SAMPLE_REGS_ABI_64: u64 = 2;

/// Bytes of user stack we ask the kernel to snapshot per sample. 8 KiB
/// covers a typical "go through libc into a Rust callback" frame chain
/// without blowing the per-CPU ring on a hot loop.
pub const DWARF_USER_STACK_SIZE: u32 = 8 * 1024;

/// Which kind of perf event a [`PerfRing`] is draining. Determines
/// how the drain loop dispatches a `PERF_RECORD_SAMPLE` from it: the
/// per-event `sample_type` differs (the sampling ring has
/// PERF_SAMPLE_READ + a callchain; the waking ring has CALLCHAIN +
/// PERF_SAMPLE_RAW), and only the switch ring emits
/// `PERF_RECORD_SWITCH_CPU_WIDE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PerfRingKind {
    /// `PERF_TYPE_SOFTWARE`/`SW_CPU_CLOCK` PET sampler. Drives the
    /// flamegraph; the SAMPLE_READ block carries the PMU group.
    Sampling,
    /// `PERF_TYPE_SOFTWARE`/`SW_DUMMY` + `context_switch=1`. Emits
    /// `PERF_RECORD_SWITCH_CPU_WIDE` for off-CPU attribution; no
    /// samples.
    Switch,
    /// `PERF_TYPE_TRACEPOINT` on `sched:sched_waking`. Each sample
    /// names a waker (the TID field), its stack (CALLCHAIN), and the
    /// wakee tid (PERF_SAMPLE_RAW, demultiplexed via
    /// `waking_field_offsets`).
    Waking,
}

/// One online CPU's sampling fd plus its mmap'd ring buffer.
pub struct PerfRing {
    pub fd: RawFd,
    pub kind: PerfRingKind,
    base: *mut u8,
    mmap_len: usize,
    data_offset: usize,
    data_size: usize,
    /// Our consumed cursor (mirrors the kernel's `data_tail`).
    tail: u64,
}

// The ring is only ever touched from the single draining thread.
unsafe impl Send for PerfRing {}

/// `1 + 2^DATA_PAGES` pages per ring. 512 data pages * 4K = 2 MiB of
/// kernel-side buffering per CPU — comfortably rides out GC pauses /
/// scheduling gaps without dropping records at a few-kHz sample rate.
pub const DATA_PAGES: usize = 512;

pub fn page_size() -> usize {
    // SAFETY: sysconf with a constant query is always valid.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

pub fn online_cpus() -> Vec<u32> {
    // SAFETY: constant query.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    let n = if n < 1 { 1 } else { n as u32 };
    (0..n).collect()
}

/// Build the sampling `perf_event_attr`: a frequency-driven software
/// CPU-clock event with TID/TIME/CPU/CALLCHAIN sample fields, plus
/// `mmap2`/`comm`/`task` records so we learn about images and threads.
///
/// `dwarf_unwind = true` additionally requests `PERF_SAMPLE_REGS_USER`
/// (rip/rsp/rbp) + `PERF_SAMPLE_STACK_USER` (8 KiB) so userspace can
/// replay the unwind through `.eh_frame` CFI for binaries without
/// frame pointers. x86_64-only: on other arches the flag is a no-op
/// (the kernel CALLCHAIN already works on FP-by-default ABIs).
fn sampling_attr(
    freq_hz: u32,
    kernel_stacks: bool,
    dwarf_unwind: bool,
) -> pe::bindings::perf_event_attr {
    // SAFETY: perf_event_attr is a plain-old-data struct; zeroing it is
    // the documented way to start (size field then declares the ABI).
    let mut attr: pe::bindings::perf_event_attr = unsafe { mem::zeroed() };
    attr.type_ = pe::bindings::PERF_TYPE_SOFTWARE;
    attr.size = mem::size_of::<pe::bindings::perf_event_attr>() as u32;
    attr.config = pe::bindings::PERF_COUNT_SW_CPU_CLOCK as u64;
    // Frequency mode: kernel auto-tunes the period to hit ~freq_hz.
    attr.__bindgen_anon_1.sample_freq = freq_hz.max(1) as u64;
    let mut sample_type = (pe::bindings::PERF_SAMPLE_TID
        | pe::bindings::PERF_SAMPLE_TIME
        | pe::bindings::PERF_SAMPLE_CPU
        | pe::bindings::PERF_SAMPLE_READ
        | pe::bindings::PERF_SAMPLE_CALLCHAIN) as u64;
    #[cfg(target_arch = "x86_64")]
    if dwarf_unwind {
        sample_type |= (pe::bindings::PERF_SAMPLE_REGS_USER
            | pe::bindings::PERF_SAMPLE_STACK_USER) as u64;
        attr.sample_regs_user = DWARF_USER_REGS_MASK;
        attr.sample_stack_user = DWARF_USER_STACK_SIZE;
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = dwarf_unwind; // No-op on non-x86_64 — FP-by-default ABIs.
    attr.sample_type = sample_type;
    // Group-read format so each sample carries `{u64 nr; (u64 value,
    // u64 id)[nr]}` for the whole counter group. With no siblings
    // (daemon-brokered path, today) `nr == 1` and we just ignore the
    // leader's own SW_CPU_CLOCK value; with siblings (in-process,
    // [`open_cpu_pmu_siblings`]) `nr == 5` and we attribute deltas per
    // counter via the queried perf-event ID.
    attr.read_format = (pe::bindings::PERF_FORMAT_GROUP | pe::bindings::PERF_FORMAT_ID) as u64;
    attr.set_disabled(1);
    attr.set_freq(1);
    attr.set_exclude_kernel(if kernel_stacks { 0 } else { 1 });
    attr.set_exclude_hv(1);
    // Image / thread bookkeeping records.
    attr.set_mmap(1);
    attr.set_mmap2(1);
    attr.set_comm(1);
    attr.set_task(1);
    // Keep non-SAMPLE records free of a trailing sample_id block so
    // MMAP2/COMM parsing stays a fixed layout.
    attr.set_sample_id_all(0);
    // Wake the poll() side every N events rather than per-sample.
    attr.__bindgen_anon_2.wakeup_events = 64;
    attr
}

/// Side-band-only attr for context-switch tracking: a `DUMMY` software
/// event (counts nothing, emits no samples) with `context_switch` set,
/// so the kernel writes `PERF_RECORD_SWITCH_CPU_WIDE` records into the
/// ring on every on/off-CPU transition. `sample_id_all` makes those
/// records carry the TID/TIME/CPU trailer we need to attribute them.
///
/// This is the unprivileged off-CPU path: it needs only
/// `perf_event_paranoid` low enough (same as the sampling event) — no
/// tracefs / `sched:sched_switch` tracepoint id (that lives in
/// root-only `tracing/`). No `mmap`/`comm`/`task`: the sampling ring
/// already owns image + thread bookkeeping.
fn switch_attr() -> pe::bindings::perf_event_attr {
    // SAFETY: POD struct; zeroing then setting `size` is the documented
    // way to initialise a perf_event_attr.
    let mut attr: pe::bindings::perf_event_attr = unsafe { mem::zeroed() };
    attr.type_ = pe::bindings::PERF_TYPE_SOFTWARE;
    attr.size = mem::size_of::<pe::bindings::perf_event_attr>() as u32;
    attr.config = pe::bindings::PERF_COUNT_SW_DUMMY as u64;
    attr.sample_type = (pe::bindings::PERF_SAMPLE_TID
        | pe::bindings::PERF_SAMPLE_TIME
        | pe::bindings::PERF_SAMPLE_CPU) as u64;
    attr.set_disabled(1);
    attr.set_context_switch(1);
    // Append the sample_type trailer to the SWITCH records.
    attr.set_sample_id_all(1);
    attr
}

/// mmap the ring for an already-opened perf fd and wrap it in a
/// `PerfRing`. Shared by the sampling and context-switch openers.
fn map_ring(fd: RawFd, kind: PerfRingKind) -> io::Result<PerfRing> {
    let ps = page_size();
    let mmap_len = (1 + DATA_PAGES) * ps;
    // SAFETY: mapping the perf ring for `fd`; len is the documented
    // (1 + 2^n) pages. MAP_SHARED is required for the kernel writer.
    let base = unsafe {
        libc::mmap(
            ptr::null_mut(),
            mmap_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        let e = io::Error::last_os_error();
        // SAFETY: closing the fd we just opened on the error path.
        unsafe { libc::close(fd) };
        return Err(e);
    }

    Ok(PerfRing {
        fd,
        kind,
        base: base as *mut u8,
        mmap_len,
        // First page is the user/kernel control header; data follows.
        data_offset: ps,
        data_size: DATA_PAGES * ps,
        tail: 0,
    })
}

/// The privileged step, in isolation: `perf_event_open` the per-CPU
/// event for `attr` (system-wide: `pid = -1`, `cpu = N`, no group,
/// cloexec) and hand back the owned descriptor. No mmap — the caller
/// (or, across the staxd fd broker, the unprivileged peer) maps it.
fn perf_event_open_cpu(
    attr: &mut pe::bindings::perf_event_attr,
    cpu: u32,
) -> io::Result<OwnedFd> {
    // SAFETY: FFI; attr is a valid initialized struct, fd args follow
    // the perf_event_open contract (pid=-1, cpu=N, no group, cloexec).
    let fd = unsafe {
        pe::perf_event_open(
            attr,
            -1,
            cpu as i32,
            -1,
            pe::bindings::PERF_FLAG_FD_CLOEXEC as u64,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel just handed us ownership of `fd`.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open the per-CPU `perf_event_open` fd for `attr` and mmap its ring
/// in one step (the in-process path: same process opens and drains).
fn open_ring(
    attr: &mut pe::bindings::perf_event_attr,
    cpu: u32,
    kind: PerfRingKind,
) -> io::Result<PerfRing> {
    ring_from_fd(perf_event_open_cpu(attr, cpu)?, kind)
}

/// `perf_event_open` one **sampling** event on `cpu` (system-wide),
/// returning just the descriptor — the privileged half of the staxd
/// Linux fd broker. The event is created **disabled**; whoever maps
/// it enables it.
///
/// `dwarf_unwind` adds the REGS_USER + STACK_USER sample bits so the
/// unprivileged peer can DWARF-unwind through `-fomit-frame-pointer`
/// binaries (x86_64 only; ignored on other arches).
pub fn open_cpu_fd(
    cpu: u32,
    freq_hz: u32,
    kernel_stacks: bool,
    dwarf_unwind: bool,
) -> io::Result<OwnedFd> {
    perf_event_open_cpu(&mut sampling_attr(freq_hz, kernel_stacks, dwarf_unwind), cpu)
}

/// `perf_event_open` one **context-switch** (off-CPU) event on `cpu`,
/// returning just the descriptor. Broker counterpart of
/// [`open_cpu_fd`].
pub fn open_cpu_switch_fd(cpu: u32) -> io::Result<OwnedFd> {
    perf_event_open_cpu(&mut switch_attr(), cpu)
}

/// mmap the ring for an already-open perf fd (one received over the
/// staxd fd broker, or just opened in-process) and wrap it in a
/// [`PerfRing`]. Takes ownership of the descriptor; the `PerfRing`'s
/// `Drop` closes it. `kind` tells the drain loop how to parse
/// `PERF_RECORD_SAMPLE` records out of this ring.
pub fn ring_from_fd(fd: OwnedFd, kind: PerfRingKind) -> io::Result<PerfRing> {
    map_ring(fd.into_raw_fd(), kind)
}

/// Open + mmap one sampling ring on `cpu`. `pid = -1` → system-wide.
pub fn open_cpu(
    cpu: u32,
    freq_hz: u32,
    kernel_stacks: bool,
    dwarf_unwind: bool,
) -> io::Result<PerfRing> {
    open_ring(
        &mut sampling_attr(freq_hz, kernel_stacks, dwarf_unwind),
        cpu,
        PerfRingKind::Sampling,
    )
}

/// Open + mmap one context-switch (off-CPU) tracking ring on `cpu`.
pub fn open_cpu_switch(cpu: u32) -> io::Result<PerfRing> {
    open_ring(&mut switch_attr(), cpu, PerfRingKind::Switch)
}

/// `perf_event_open` one **`sched:sched_waking` tracepoint** event on
/// `cpu` (system-wide), returning just the descriptor. Privileged
/// step of the staxd broker for wakeup attribution: the tracepoint
/// `id` (config) and field offsets live in root-only tracefs.
pub fn open_cpu_waking_fd(
    cpu: u32,
    tracepoint_id: u64,
    kernel_stacks: bool,
) -> io::Result<OwnedFd> {
    perf_event_open_cpu(&mut waking_attr(tracepoint_id, kernel_stacks), cpu)
}

/// Open + mmap one waking ring on `cpu` (in-process fallback when
/// the host's `perf_event_paranoid` permits it and tracefs is
/// readable; the broker is the locked-down-host path).
pub fn open_cpu_waking(
    cpu: u32,
    tracepoint_id: u64,
    kernel_stacks: bool,
) -> io::Result<PerfRing> {
    open_ring(
        &mut waking_attr(tracepoint_id, kernel_stacks),
        cpu,
        PerfRingKind::Waking,
    )
}

/// Read the live kernel's `sched:sched_waking` tracepoint id and the
/// byte offset/size of its `pid` field (the wakee tid) inside the
/// tracepoint RAW payload. Both files live in tracefs and are
/// typically root-only — which is the whole reason the staxd broker
/// (running as root) is the canonical caller. The in-process path
/// can try it too on a permissive host, and falls back to no
/// wakeup attribution on failure.
pub fn read_sched_waking_tracepoint() -> io::Result<(u64, WakingFieldOffsets)> {
    // Modern: /sys/kernel/tracing. Legacy: /sys/kernel/debug/tracing.
    const CANDIDATES: [&str; 2] = [
        "/sys/kernel/tracing/events/sched/sched_waking",
        "/sys/kernel/debug/tracing/events/sched/sched_waking",
    ];
    let base = CANDIDATES
        .iter()
        .find(|p| std::path::Path::new(p).is_dir())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no tracefs sched_waking event (tried /sys/kernel/tracing \
                 and /sys/kernel/debug/tracing)",
            )
        })?;

    let id_str = std::fs::read_to_string(format!("{base}/id"))?;
    let id: u64 = id_str
        .trim()
        .parse()
        .map_err(|e| io::Error::other(format!("parse tracepoint id {:?}: {e}", id_str.trim())))?;

    let format = std::fs::read_to_string(format!("{base}/format"))?;
    let offsets = parse_waking_format(&format).ok_or_else(|| {
        io::Error::other("sched_waking format file missed `pid` field (kernel layout drift?)")
    })?;
    Ok((id, offsets))
}

/// Parse a tracepoint `format` file and return offset+size of the
/// `pid` field (the wakee tid for `sched:sched_waking`). Not
/// `common_pid` (that's the waker, free from the sample's TID).
fn parse_waking_format(s: &str) -> Option<WakingFieldOffsets> {
    for line in s.lines() {
        let line = line.trim();
        if !line.starts_with("field:") {
            continue;
        }
        // "field:pid_t pid;\toffset:24;\tsize:4;\tsigned:1;"
        let parts: Vec<&str> = line
            .split([';', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let decl = *parts.first()?; // "field:pid_t pid"
        let raw_name = decl.split_whitespace().next_back()?;
        let name = raw_name.split('[').next()?;
        if name != "pid" {
            continue;
        }
        let mut offset: Option<u32> = None;
        let mut size: Option<u32> = None;
        for p in &parts {
            if let Some(v) = p.strip_prefix("offset:") {
                offset = v.parse().ok();
            } else if let Some(v) = p.strip_prefix("size:") {
                size = v.parse().ok();
            }
        }
        if let (Some(o), Some(s)) = (offset, size) {
            return Some(WakingFieldOffsets {
                wakee_pid_offset: o,
                wakee_pid_size: s,
            });
        }
    }
    None
}

/// Tracepoint attr for `sched:sched_waking`. Every wakeup fires
/// (`sample_period = 1`); the sample carries the waker's TID + stack
/// (CALLCHAIN) and the wakee tid embedded in PERF_SAMPLE_RAW.
fn waking_attr(tracepoint_id: u64, kernel_stacks: bool) -> pe::bindings::perf_event_attr {
    // SAFETY: POD; zero then init `size` per ABI.
    let mut attr: pe::bindings::perf_event_attr = unsafe { mem::zeroed() };
    attr.type_ = pe::bindings::PERF_TYPE_TRACEPOINT;
    attr.size = mem::size_of::<pe::bindings::perf_event_attr>() as u32;
    attr.config = tracepoint_id;
    // Count-mode (no freq): one sample per wakeup. Wakeups are
    // bursty but bounded by scheduler activity.
    attr.__bindgen_anon_1.sample_period = 1;
    attr.sample_type = (pe::bindings::PERF_SAMPLE_TID
        | pe::bindings::PERF_SAMPLE_TIME
        | pe::bindings::PERF_SAMPLE_CPU
        | pe::bindings::PERF_SAMPLE_CALLCHAIN
        | pe::bindings::PERF_SAMPLE_RAW) as u64;
    attr.set_disabled(1);
    attr.set_exclude_kernel(if kernel_stacks { 0 } else { 1 });
    attr.set_exclude_hv(1);
    // The sampling ring owns mmap/comm/task bookkeeping.
    attr.set_sample_id_all(0);
    // Wake reader after a small batch — wakeups are frequent but each
    // sample is small.
    attr.__bindgen_anon_2.wakeup_events = 64;
    attr
}

impl PerfRing {
    fn meta(&self) -> *mut pe::bindings::perf_event_mmap_page {
        self.base as *mut pe::bindings::perf_event_mmap_page
    }

    pub fn enable(&self) -> io::Result<()> {
        // `PERF_IOC_FLAG_GROUP`: if this fd is a group leader (the
        // sampling ring is — PMU siblings attach to it), all siblings
        // start together. Harmless no-op when there is no group.
        // SAFETY: ioctl on our own perf fd.
        let rc =
            unsafe { pe::ioctls::ENABLE(self.fd, pe::bindings::PERF_IOC_FLAG_GROUP) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn disable(&self) {
        // SAFETY: ioctl on our own perf fd; errors are non-actionable
        // during teardown.
        unsafe {
            pe::ioctls::DISABLE(self.fd, pe::bindings::PERF_IOC_FLAG_GROUP);
        }
    }

    /// Copy every complete record currently in the ring into `out`
    /// (record bytes, each prefixed by its 8-byte `perf_event_header`),
    /// then publish the new tail. Handles the power-of-two wrap by
    /// linearising into a scratch buffer.
    pub fn drain(&mut self, out: &mut Vec<u8>) {
        let meta = self.meta();
        // Acquire-load data_head; pairs with the kernel's release store.
        let head = unsafe { ptr::addr_of!((*meta).data_head).read_volatile() };
        fence(Ordering::Acquire);
        let mut tail = self.tail;
        if head == tail {
            return;
        }
        let size = self.data_size as u64;
        // SAFETY: data region is [base+data_offset, +data_size).
        let data = unsafe { self.base.add(self.data_offset) };
        while tail < head {
            let off = (tail % size) as usize;
            // perf_event_header is { u32 type; u16 misc; u16 size }.
            // It never straddles the wrap (kernel guarantees records
            // are contiguous), so read the size directly.
            let rec_size = unsafe {
                let p = data.add(off) as *const u32;
                // size is the high u16 of the second u32.
                (p.add(1).read_unaligned() >> 16) as u16 as usize
            };
            if rec_size == 0 {
                break;
            }
            // Copy the record, wrapping if it crosses the ring end.
            let start = off;
            let end = start + rec_size;
            if end <= self.data_size {
                let s = unsafe { std::slice::from_raw_parts(data.add(start), rec_size) };
                out.extend_from_slice(s);
            } else {
                let first = self.data_size - start;
                let a = unsafe { std::slice::from_raw_parts(data.add(start), first) };
                let b = unsafe { std::slice::from_raw_parts(data, rec_size - first) };
                out.extend_from_slice(a);
                out.extend_from_slice(b);
            }
            tail += rec_size as u64;
        }
        // Publish consumed position so the kernel can reuse the space.
        fence(Ordering::Release);
        unsafe {
            ptr::addr_of_mut!((*meta).data_tail).write_volatile(head);
        }
        self.tail = head;
    }
}

impl Drop for PerfRing {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping / closing our own fd.
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.mmap_len);
            libc::close(self.fd);
        }
    }
}

// --- PMU counter group ------------------------------------------------------
//
// Hardware counters attached as siblings to the sampling ring so each
// PERF_RECORD_SAMPLE carries running counts via PERF_SAMPLE_READ. We pick
// four to populate the PMU fields the SampleSink contract defines for the
// macOS kperf backend, mapping them onto Linux's portable perf_event
// hardware counters.

/// Which hardware counter a perf event id corresponds to. Index order
/// (0..4) matches the position of the field in [`PmuValues`] / the
/// `SampleEvent` PMU fields, so callers can use [`PmuKind::index`] for
/// O(1) bookkeeping.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PmuKind {
    Cycles,
    Instructions,
    L1dMisses,
    BranchMisses,
}

impl PmuKind {
    /// Stable index in `[cycles, instructions, l1d_misses, branch_misses]`.
    pub fn index(self) -> usize {
        match self {
            PmuKind::Cycles => 0,
            PmuKind::Instructions => 1,
            PmuKind::L1dMisses => 2,
            PmuKind::BranchMisses => 3,
        }
    }
}

/// One hardware counter inside a sampling group: the counter's perf
/// event id (returned by `PERF_EVENT_IOC_ID`, used to demultiplex the
/// `PERF_SAMPLE_READ` block) and the owning fd that keeps it alive.
pub struct PmuMember {
    pub kind: PmuKind,
    pub id: u64,
    /// Held to keep the perf event alive for the session — closing it
    /// removes the sibling from the group. The `Drop` of [`OwnedFd`]
    /// is the load-bearing part, so the field looks unread to the
    /// dead-code lint.
    #[allow(dead_code)]
    pub fd: OwnedFd,
}

/// All sibling counters attached to one CPU's sampling-ring leader.
pub type PmuSiblings = Vec<PmuMember>;

/// The session-wide PMU state: every CPU's siblings (held to keep the
/// counters alive) plus a global `perf-event-id → kind` map for sample
/// parsing. perf event ids are globally unique, so one flat map covers
/// all CPUs. Empty `PmuGroup::default()` = "no PMU on this run" — the
/// sampling path then simply leaves cycles/instructions/etc. at 0.
pub struct PmuGroup {
    pub siblings: Vec<PmuSiblings>,
    pub id_to_kind: HashMap<u64, PmuKind>,
}

impl Default for PmuGroup {
    fn default() -> Self {
        Self {
            siblings: Vec::new(),
            id_to_kind: HashMap::new(),
        }
    }
}

/// Attach a four-counter HW group (cycles / instructions / L1D-read
/// misses / branch mispredicts) to `leader_fd` on `cpu`. Returns the
/// siblings in stable order. Best-effort: on a PMU that can't hold the
/// whole group, or when the host denies HW counters, the caller treats
/// failure as "no PMU on this CPU" and continues without it.
///
/// The siblings inherit grouping from `leader_fd`: when the leader is
/// enabled with `PERF_IOC_FLAG_GROUP` they all start, and each sample's
/// `PERF_SAMPLE_READ` block carries the group's running values.
pub fn open_cpu_pmu_siblings(
    cpu: u32,
    leader_fd: RawFd,
    kernel_stacks: bool,
) -> io::Result<PmuSiblings> {
    // L1D-read miss as the cache event (PERF_TYPE_HW_CACHE encodes
    // cache / op / result in `config`):
    //   config = L1D | (READ << 8) | (MISS << 16)
    let l1d_read_miss = (pe::bindings::PERF_COUNT_HW_CACHE_L1D as u64)
        | ((pe::bindings::PERF_COUNT_HW_CACHE_OP_READ as u64) << 8)
        | ((pe::bindings::PERF_COUNT_HW_CACHE_RESULT_MISS as u64) << 16);

    let plan: [(PmuKind, u32, u64); 4] = [
        (
            PmuKind::Cycles,
            pe::bindings::PERF_TYPE_HARDWARE,
            pe::bindings::PERF_COUNT_HW_CPU_CYCLES as u64,
        ),
        (
            PmuKind::Instructions,
            pe::bindings::PERF_TYPE_HARDWARE,
            pe::bindings::PERF_COUNT_HW_INSTRUCTIONS as u64,
        ),
        (PmuKind::L1dMisses, pe::bindings::PERF_TYPE_HW_CACHE, l1d_read_miss),
        (
            PmuKind::BranchMisses,
            pe::bindings::PERF_TYPE_HARDWARE,
            pe::bindings::PERF_COUNT_HW_BRANCH_MISSES as u64,
        ),
    ];

    let mut out: PmuSiblings = Vec::with_capacity(4);
    for (kind, ty, config) in plan {
        // SAFETY: perf_event_attr is POD; zero then init `size` per ABI.
        let mut attr: pe::bindings::perf_event_attr = unsafe { mem::zeroed() };
        attr.type_ = ty;
        attr.size = mem::size_of::<pe::bindings::perf_event_attr>() as u32;
        attr.config = config;
        // Same group-read format as the leader so the SAMPLE_READ
        // block matches; same kernel/HV exclusion as the leader's
        // callchain mask so the group is permissible (the kernel
        // refuses to group events with incompatible exclude flags).
        attr.read_format =
            (pe::bindings::PERF_FORMAT_GROUP | pe::bindings::PERF_FORMAT_ID) as u64;
        attr.set_disabled(1);
        attr.set_exclude_kernel(if kernel_stacks { 0 } else { 1 });
        attr.set_exclude_hv(1);

        // SAFETY: FFI; `attr` is fully initialised; `group_fd` is the
        // caller's live sampling fd; pid=-1 + cpu=N + cloexec match
        // the leader's per-CPU system-wide scope.
        let raw = unsafe {
            pe::perf_event_open(
                &mut attr,
                -1,
                cpu as i32,
                leader_fd,
                pe::bindings::PERF_FLAG_FD_CLOEXEC as u64,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel just handed us ownership of `raw`.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let mut id: u64 = 0;
        // SAFETY: ioctl on our own fd; writes one u64.
        let rc = unsafe { pe::ioctls::ID(fd.as_raw_fd(), &mut id as *mut u64) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        out.push(PmuMember { kind, id, fd });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dwarf_unwind = false` must produce byte-for-byte the same
    /// `perf_event_attr` as before this feature landed — anything
    /// else is a regression on every recording that doesn't use the
    /// new path.
    #[test]
    fn sampling_attr_off_path_unchanged() {
        let a = sampling_attr(999, true, false);
        let regs_user = pe::bindings::PERF_SAMPLE_REGS_USER as u64;
        let stack_user = pe::bindings::PERF_SAMPLE_STACK_USER as u64;
        assert_eq!(a.sample_type & (regs_user | stack_user), 0);
        assert_eq!(a.sample_regs_user, 0);
        assert_eq!(a.sample_stack_user, 0);
    }

    /// `dwarf_unwind = true` sets the REGS_USER + STACK_USER sample
    /// bits and the `sample_regs_user` / `sample_stack_user` fields
    /// framehop relies on. x86_64-only — non-x86_64 takes the no-op
    /// branch and is covered by `_off_path_unchanged`.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn sampling_attr_dwarf_on_sets_regs_and_stack() {
        let a = sampling_attr(999, true, true);
        let regs_user = pe::bindings::PERF_SAMPLE_REGS_USER as u64;
        let stack_user = pe::bindings::PERF_SAMPLE_STACK_USER as u64;
        assert_eq!(a.sample_type & regs_user, regs_user);
        assert_eq!(a.sample_type & stack_user, stack_user);
        assert_eq!(a.sample_regs_user, DWARF_USER_REGS_MASK);
        assert_eq!(a.sample_stack_user, DWARF_USER_STACK_SIZE);
        // Exactly three regs requested (BP, SP, IP) — framehop's full
        // set on x86_64. Adding extras would just bloat per-sample
        // payload without recovering more frames.
        assert_eq!(DWARF_USER_REGS_MASK.count_ones(), 3);
    }
}
