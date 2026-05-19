//! Thin safe-ish layer over `perf_event_open` + the mmap ring buffer.
//!
//! We open one sampling event per online CPU, system-wide (`pid = -1`),
//! and filter to the target in userspace. That captures every thread of
//! the target — including ones that already existed and short-lived
//! children — which a single `pid > 0` event would miss. (`paranoid`
//! must allow it; on a locked-down host the daemon phase opens these
//! with privilege instead.)

use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::ptr;
use std::sync::atomic::{Ordering, fence};

use perf_event_open_sys as pe;

/// `PERF_CONTEXT_*` sentinels that split a callchain into kernel/user
/// regions. They are huge unsigned values (negative `i64` cast to
/// `u64`); anything `>= PERF_CONTEXT_MAX` is a marker, not an address.
pub const PERF_CONTEXT_KERNEL: u64 = (-128i64) as u64;
pub const PERF_CONTEXT_USER: u64 = (-512i64) as u64;
/// Largest reserved marker value; real addresses are always below this.
pub const PERF_CONTEXT_MAX: u64 = (-4095i64) as u64;

/// One online CPU's sampling fd plus its mmap'd ring buffer.
pub struct PerfRing {
    pub fd: RawFd,
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
const DATA_PAGES: usize = 512;

fn page_size() -> usize {
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
fn sampling_attr(freq_hz: u32, kernel_stacks: bool) -> pe::bindings::perf_event_attr {
    // SAFETY: perf_event_attr is a plain-old-data struct; zeroing it is
    // the documented way to start (size field then declares the ABI).
    let mut attr: pe::bindings::perf_event_attr = unsafe { mem::zeroed() };
    attr.type_ = pe::bindings::PERF_TYPE_SOFTWARE;
    attr.size = mem::size_of::<pe::bindings::perf_event_attr>() as u32;
    attr.config = pe::bindings::PERF_COUNT_SW_CPU_CLOCK as u64;
    // Frequency mode: kernel auto-tunes the period to hit ~freq_hz.
    attr.__bindgen_anon_1.sample_freq = freq_hz.max(1) as u64;
    attr.sample_type = (pe::bindings::PERF_SAMPLE_TID
        | pe::bindings::PERF_SAMPLE_TIME
        | pe::bindings::PERF_SAMPLE_CPU
        | pe::bindings::PERF_SAMPLE_CALLCHAIN) as u64;
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
fn map_ring(fd: RawFd) -> io::Result<PerfRing> {
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
        base: base as *mut u8,
        mmap_len,
        // First page is the user/kernel control header; data follows.
        data_offset: ps,
        data_size: DATA_PAGES * ps,
        tail: 0,
    })
}

/// Open the per-CPU `perf_event_open` fd for `attr` (system-wide:
/// `pid = -1`, `cpu = N`, no group, cloexec) and mmap its ring.
fn open_ring(attr: &mut pe::bindings::perf_event_attr, cpu: u32) -> io::Result<PerfRing> {
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
    map_ring(fd as RawFd)
}

/// Open + mmap one sampling ring on `cpu`. `pid = -1` → system-wide.
pub fn open_cpu(cpu: u32, freq_hz: u32, kernel_stacks: bool) -> io::Result<PerfRing> {
    open_ring(&mut sampling_attr(freq_hz, kernel_stacks), cpu)
}

/// Open + mmap one context-switch (off-CPU) tracking ring on `cpu`.
pub fn open_cpu_switch(cpu: u32) -> io::Result<PerfRing> {
    open_ring(&mut switch_attr(), cpu)
}

impl PerfRing {
    fn meta(&self) -> *mut pe::bindings::perf_event_mmap_page {
        self.base as *mut pe::bindings::perf_event_mmap_page
    }

    pub fn enable(&self) -> io::Result<()> {
        // SAFETY: ioctl on our own perf fd.
        let rc = unsafe { pe::ioctls::ENABLE(self.fd, 0) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn disable(&self) {
        // SAFETY: ioctl on our own perf fd; errors are non-actionable
        // during teardown.
        unsafe {
            pe::ioctls::DISABLE(self.fd, 0);
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
