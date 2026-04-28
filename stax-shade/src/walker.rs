//! Framehop-driven user-stack walker.
//!
//! ## Why
//!
//! kperf walks user stacks in-kernel using frame pointers. That
//! works for code that keeps FPs (most C/C++/Swift). It silently
//! truncates or skips frames in code that doesn't:
//!
//! - Rust release builds without `-C force-frame-pointers=yes`
//! - JIT'd code with custom prologues (vox-jit, cranelift output,
//!   any tracing JIT we'd want to profile)
//! - Hand-written assembly
//!
//! Framehop reconstructs stacks from the on-disk unwind tables
//! (`__unwind_info`, `__compact_unwind`, `.eh_frame`) without
//! needing FPs to be present. To drive it we need three things:
//!
//! 1. **Initial register state** — IP/SP/FP/LR/etc at the moment
//!    of the sample. Got via `thread_get_state(ARM_THREAD_STATE64)`
//!    on a suspended thread.
//! 2. **Stack memory** — for the unwinder to read CFA expressions
//!    and saved registers off the stack. Got via `mach_vm_read`
//!    against the target task port.
//! 3. **Per-module unwind sections** — `__unwind_info` /
//!    `__compact_unwind` / `.eh_frame` blobs for every binary
//!    loaded in the target, addressable by AVMA. We don't have
//!    those yet on the shade side; the next commit wires
//!    target-image enumeration (read `dyld_all_image_infos` from
//!    the target's memory and lazily fetch unwind sections per
//!    image).
//!
//! ## What this commit ships
//!
//! Foundational types so the framehop dep lands and the
//! integration shape is in tree:
//!
//! - `MachStackReader` — `framehop::MemoryRead`-style
//!   accessor backed by `mach_vm_read`. Hot-path: read 8 bytes
//!   at a time from the suspended target.
//! - `walk_thread_snapshot` — public entry point that, given a
//!   target task port + thread port, would suspend the thread,
//!   pull `ARM_THREAD_STATE64`, and feed framehop. Currently a
//!   stub that returns the IP only — wiring the per-module
//!   unwinder is the next slice.
//!
//! No periodic walking yet, no integration with the
//! `stax-shade-proto::Shade` service, no streaming back to
//! stax-server. Those land on top.

#![cfg(target_os = "macos")]

use mach2::kern_return::KERN_SUCCESS;
use mach2::port::mach_port_t;

/// Memory accessor backed by `mach_vm_read_overwrite` against
/// the target task. Holds the task port (a Mach right we acquired
/// via `task_for_pid`) by value. Cheap to copy — `mach_port_t` is
/// a `u32` underneath; the right itself is reference-counted in
/// the kernel.
#[derive(Copy, Clone)]
#[allow(dead_code)] // wired in the periodic-walker commit
pub struct MachStackReader {
    pub task: mach_port_t,
}

impl MachStackReader {
    /// Read exactly `buf.len()` bytes starting at `addr` (target
    /// AVMA) into `buf`. Returns `false` on partial reads or any
    /// kernel-side failure (unmapped page, protection violation,
    /// task port revoked, …) — framehop treats unreadable stack
    /// memory as a hard wall, which is the correct conservative
    /// answer.
    #[allow(dead_code)] // wired in the periodic-walker commit
    pub fn read_exact(&self, addr: u64, buf: &mut [u8]) -> bool {
        use mach2::vm::mach_vm_read_overwrite;
        use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};

        let mut got: mach_vm_size_t = 0;
        // SAFETY: buf is a unique mut slice; addr is treated as
        // an opaque integer by the kernel; got is an out-pointer.
        let kr = unsafe {
            mach_vm_read_overwrite(
                self.task,
                addr as mach_vm_address_t,
                buf.len() as mach_vm_size_t,
                buf.as_mut_ptr() as mach_vm_address_t,
                &mut got,
            )
        };
        kr == KERN_SUCCESS && got as usize == buf.len()
    }

    /// Convenience: 8-byte aligned u64 read. The unwinder calls
    /// this for nearly every CFA / saved-register lookup, so
    /// optimising it later (vmap a stack window once per sample
    /// instead of one syscall per quad) is on the table.
    #[allow(dead_code)] // wired in the periodic-walker commit
    pub fn read_u64(&self, addr: u64) -> Option<u64> {
        let mut buf = [0u8; 8];
        if self.read_exact(addr, &mut buf) {
            Some(u64::from_le_bytes(buf))
        } else {
            None
        }
    }
}

/// Pull the ARM64 register state for one thread of `task`. The
/// thread must already be suspended (or in a state where the
/// kernel will return a coherent register set — a thread on its
/// own kernel stack returns the user-space state at the syscall
/// boundary, which is what we want for a profiler).
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)] // wired in the periodic-walker commit
pub fn thread_state_arm64(thread: mach_port_t) -> Option<mach2::structs::arm_thread_state64_t> {
    use mach2::structs::arm_thread_state64_t;
    use mach2::thread_act::thread_get_state;
    use mach2::thread_status::ARM_THREAD_STATE64;

    let mut state: arm_thread_state64_t = unsafe { std::mem::zeroed() };
    let mut count = (std::mem::size_of::<arm_thread_state64_t>() / std::mem::size_of::<u32>())
        as mach2::message::mach_msg_type_number_t;
    // SAFETY: state is a fresh zeroed struct of the right size;
    // count is set to its u32-word length per the Mach contract.
    let kr = unsafe {
        thread_get_state(
            thread,
            ARM_THREAD_STATE64,
            (&mut state) as *mut _ as *mut u32,
            &mut count,
        )
    };
    if kr == KERN_SUCCESS {
        Some(state)
    } else {
        None
    }
}

/// Build a framehop `UnwinderAarch64` populated with one
/// `Module` per parsed image.
///
/// Each `MachoSections` carries AVMA ranges (already slid) plus
/// the bytes of the small unwind sections; framehop, conversely,
/// keys section ranges by SVMA and consumes raw bytes by `Deref<[u8]>`.
/// We translate via the recorded slide.
///
/// Modules whose Mach-O parse failed (`sections == None`) or that
/// lack a `__TEXT` range are skipped — without an avma_range there's
/// nothing to register.
///
/// The unwinder takes ownership of section bytes (we move them out
/// of the input). Callers that want to retain them elsewhere should
/// clone first.
#[cfg(target_arch = "aarch64")]
pub fn build_unwinder(
    images: Vec<stax_target_images::ImageEntry>,
) -> (
    framehop::aarch64::UnwinderAarch64<Vec<u8>>,
    ImageMap,
    UnwinderStats,
) {
    use framehop::aarch64::UnwinderAarch64;
    use framehop::{ExplicitModuleSectionInfo, Module, Unwinder};

    let mut unwinder: UnwinderAarch64<Vec<u8>> = UnwinderAarch64::new();
    let mut stats = UnwinderStats::default();
    let mut map_entries: Vec<ImageMapEntry> = Vec::with_capacity(images.len());
    stats.images_total = images.len();

    for img in images {
        let Some(sections) = img.sections else {
            stats.skipped_no_sections += 1;
            continue;
        };
        let Some(text_avma) = sections.text_avma.clone() else {
            stats.skipped_no_text += 1;
            continue;
        };
        map_entries.push(ImageMapEntry {
            path: img.path.clone(),
            avma_range: text_avma.clone(),
            load_address: img.load_address,
        });
        let text_svma = sections.avma_to_svma(&text_avma);
        let base_svma = text_svma.start;

        // Compute SVMA ranges first (immutable borrows), then move
        // bytes out of the section data.
        let got_svma = sections
            .got_avma
            .as_ref()
            .map(|r| sections.avma_to_svma(r));
        let eh_frame_svma = sections
            .eh_frame
            .as_ref()
            .map(|s| sections.avma_to_svma(&s.avma));
        let eh_frame_hdr_svma = sections
            .eh_frame_hdr
            .as_ref()
            .map(|s| sections.avma_to_svma(&s.avma));

        let info = ExplicitModuleSectionInfo {
            base_svma,
            text_segment_svma: Some(text_svma.clone()),
            got_svma,
            unwind_info: sections.unwind_info.map(|s| s.bytes),
            eh_frame_svma,
            eh_frame: sections.eh_frame.map(|s| s.bytes),
            eh_frame_hdr_svma,
            eh_frame_hdr: sections.eh_frame_hdr.map(|s| s.bytes),
            ..Default::default()
        };

        let has_unwind = info.unwind_info.is_some();
        let has_eh_frame = info.eh_frame.is_some();

        let module = Module::new(img.path, text_avma, img.load_address, info);
        unwinder.add_module(module);
        stats.modules_added += 1;
        if has_unwind {
            stats.with_unwind_info += 1;
        }
        if has_eh_frame {
            stats.with_eh_frame += 1;
        }
    }

    map_entries.sort_by_key(|e| e.avma_range.start);
    let image_map = ImageMap {
        entries: map_entries,
    };
    (unwinder, image_map, stats)
}

/// AVMA → (image path, offset) lookup. Built alongside the
/// unwinder so we can render walked frames as
/// `<install_name>+<hex_offset>`, which downstream symbolicators
/// (stax-server's BinaryRegistry, `atos`, `addr2line`, …) can
/// resolve. Cheap binary search; the entry list is sorted by
/// `avma_range.start` and ranges of mach-O text segments don't
/// overlap on macOS.
#[derive(Debug, Default)]
pub struct ImageMap {
    entries: Vec<ImageMapEntry>,
}

#[derive(Debug, Clone)]
pub struct ImageMapEntry {
    pub path: String,
    pub avma_range: core::ops::Range<u64>,
    pub load_address: u64,
}

impl ImageMap {
    pub fn lookup(&self, addr: u64) -> Option<&ImageMapEntry> {
        // partition_point gives the first entry whose
        // avma_range.start > addr; the candidate is the one before
        // it. Then verify addr is actually inside the range
        // (catches addresses in the holes between modules).
        let i = self.entries.partition_point(|e| e.avma_range.start <= addr);
        if i == 0 {
            return None;
        }
        let e = &self.entries[i - 1];
        if e.avma_range.contains(&addr) {
            Some(e)
        } else {
            None
        }
    }

}

/// Coverage breakdown for a freshly-built unwinder. Logged once
/// at attach time so we can spot regressions in image discovery
/// or section parsing.
#[derive(Default, Debug)]
pub struct UnwinderStats {
    pub images_total: usize,
    pub modules_added: usize,
    pub with_unwind_info: usize,
    pub with_eh_frame: usize,
    pub skipped_no_sections: usize,
    pub skipped_no_text: usize,
}

/// One captured stack: instruction pointer at the leaf, plus the
/// list of return addresses framehop walked back to. Empty `frames`
/// means we got the IP but framehop refused to step (no module, no
/// unwind info at PC, etc).
#[derive(Debug, Clone)]
#[cfg(target_arch = "aarch64")]
pub struct ThreadSample {
    pub thread: mach_port_t,
    pub pc: u64,
    pub frames: Vec<u64>,
    pub error: Option<String>,
}

/// Enumerate threads of `task`, suspend each in turn, walk one
/// stack via framehop, resume. Returns one `ThreadSample` per
/// thread (or skipped entries when `thread_get_state` failed).
///
/// Cost: 1× `task_threads` + per-thread `(suspend, get_state,
/// walk, resume)`. The walk itself is N small `mach_vm_read`s
/// per CFA / saved-register lookup — fine for one-shot
/// validation, future periodic pass should mmap a stack window.
///
/// `Unwinder` is taken by `&mut` because `iter_frames` borrows
/// it; the cache is created here per call (cheap — empty rule
/// table). We'll lift the cache into the periodic loop later
/// so warm rules survive across samples.
#[cfg(target_arch = "aarch64")]
pub fn snapshot_all_threads(
    task: mach_port_t,
    unwinder: &mut framehop::aarch64::UnwinderAarch64<Vec<u8>>,
) -> Vec<ThreadSample> {
    use mach2::mach_types::thread_act_array_t;
    use mach2::message::mach_msg_type_number_t;
    use mach2::task::task_threads;
    use mach2::thread_act::{thread_resume, thread_suspend};
    use mach2::vm::mach_vm_deallocate;
    use mach2::vm_types::mach_vm_address_t;

    let mut threads: thread_act_array_t = std::ptr::null_mut();
    let mut count: mach_msg_type_number_t = 0;
    // SAFETY: out-pointers; kernel allocates the array, we free it
    // via mach_vm_deallocate before returning.
    let kr = unsafe { task_threads(task, &mut threads, &mut count) };
    if kr != KERN_SUCCESS {
        tracing::warn!(kr, "task_threads failed");
        return Vec::new();
    }

    let reader = MachStackReader { task };
    let mut samples = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        // SAFETY: kernel-allocated array of `count` thread ports.
        let thread = unsafe { *threads.add(i) };

        // SAFETY: `thread` is a valid send-right we own for the
        // lifetime of this loop iteration.
        let kr = unsafe { thread_suspend(thread) };
        if kr != KERN_SUCCESS {
            tracing::debug!(thread, kr, "thread_suspend failed; skipping");
            continue;
        }

        let sample = match thread_state_arm64(thread) {
            Some(state) => walk_one(thread, state, &reader, unwinder),
            None => ThreadSample {
                thread,
                pc: 0,
                frames: Vec::new(),
                error: Some("thread_get_state failed".to_owned()),
            },
        };

        // SAFETY: counterpart to thread_suspend. Failure here is
        // logged but otherwise tolerated — the kernel will resume
        // the thread when our task exits anyway.
        let kr = unsafe { thread_resume(thread) };
        if kr != KERN_SUCCESS {
            tracing::warn!(thread, kr, "thread_resume failed");
        }

        samples.push(sample);
    }

    // Free the thread_array_t storage. Each thread port the kernel
    // gave us is also a send-right that we technically should
    // mach_port_deallocate, but xnu treats them as task-scoped: when
    // we exit they're cleaned up. For a one-shot snapshot leaking
    // them is fine; we'll plug it for the periodic loop.
    let bytes = (count as usize)
        .saturating_mul(std::mem::size_of::<mach_port_t>())
        as u64;
    // SAFETY: `threads` was allocated by the kernel via task_threads;
    // we hand the same pointer + size back.
    unsafe {
        let _ = mach_vm_deallocate(
            mach2::traps::mach_task_self(),
            threads as mach_vm_address_t,
            bytes,
        );
    }
    samples
}

#[cfg(target_arch = "aarch64")]
fn walk_one(
    thread: mach_port_t,
    state: mach2::structs::arm_thread_state64_t,
    reader: &MachStackReader,
    unwinder: &mut framehop::aarch64::UnwinderAarch64<Vec<u8>>,
) -> ThreadSample {
    use framehop::Unwinder;
    use framehop::aarch64::{CacheAarch64, UnwindRegsAarch64};

    let mut cache = CacheAarch64::<_>::new();
    let pc = state.__pc;
    let lr = state.__lr;
    let sp = state.__sp;
    let fp = state.__fp;

    let mut read_stack = |addr: u64| reader.read_u64(addr).ok_or(());

    let mut iter = unwinder.iter_frames(
        pc,
        UnwindRegsAarch64::new(lr, sp, fp),
        &mut cache,
        &mut read_stack,
    );

    let mut frames = Vec::new();
    let mut error = None;
    loop {
        match iter.next() {
            Ok(Some(frame)) => frames.push(frame.address()),
            Ok(None) => break,
            Err(e) => {
                error = Some(format!("{e}"));
                break;
            }
        }
        // Belt-and-braces cap: pathological unwind tables can loop;
        // 1024 frames is well past anything legitimate.
        if frames.len() >= 1024 {
            error = Some("frame cap reached".to_owned());
            break;
        }
    }

    ThreadSample {
        thread,
        pc,
        frames,
        error,
    }
}
