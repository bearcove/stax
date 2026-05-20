//! Userspace DWARF stack unwinder, fed by `PERF_SAMPLE_REGS_USER` +
//! `PERF_SAMPLE_STACK_USER` records.
//!
//! Why this exists: the kernel's `PERF_SAMPLE_CALLCHAIN` walks user
//! stacks via frame pointers. Most distro libraries (glibc,
//! libstdc++, OpenSSL) and any `-O2 -fomit-frame-pointer` Rust/C/C++
//! binary do not maintain an `%rbp` frame chain, so the kernel
//! callchain truncates at the first non-FP frame. Replaying the
//! unwind in userspace via `.eh_frame` CFI — what `perf record
//! --call-graph dwarf` does — restores the full chain.
//!
//! framehop wants three things per module: its mapped address range,
//! its `.text` + `.eh_frame` (+ optional `.eh_frame_hdr`), and the
//! image's *base SVMA* (zero for ELF). At sample time, framehop wants
//! `ip/sp/bp`, a cache, and a `read_stack` closure that returns the
//! `u64` at any stack address. We have all of that: the broker fds
//! ship the regs + stack snapshot per sample, the ELF parse already
//! ran in [`crate::elf::scan`].
//!
//! The unwinder is x86_64-only this round. aarch64-Linux's AAPCS64
//! keeps `x29` as a frame pointer in practice, so the kernel
//! CALLCHAIN already works there.

#![cfg(target_arch = "x86_64")]

use std::ops::Range;

use framehop::x86_64::{CacheX86_64, UnwindRegsX86_64, UnwinderX86_64};
use framehop::{ExplicitModuleSectionInfo, FrameAddress, Module, Unwinder};

/// Owns framehop's per-process state: the module map (one entry per
/// loaded image with `.eh_frame`) and the unwind-rule cache. One
/// instance per `Session`, populated by `emit_image` and queried by
/// `on_sample` whenever the kernel hands us a regs+stack block.
pub struct DwarfUnwinder {
    unwinder: UnwinderX86_64<Vec<u8>>,
    cache: CacheX86_64,
}

impl Default for DwarfUnwinder {
    fn default() -> Self {
        Self::new()
    }
}

impl DwarfUnwinder {
    pub fn new() -> Self {
        Self {
            unwinder: UnwinderX86_64::new(),
            cache: CacheX86_64::new(),
        }
    }

    /// Register one loaded image. `mapping_avma` is where the
    /// executable LOAD landed at runtime (the perf MMAP2 `addr`);
    /// `image_base_avma` is the AVMA the image would have if its
    /// SVMA-0 byte were mapped — for a PIE that's
    /// `mapping_avma - mapping_pgoff`. With `base_svma = 0` on the
    /// framehop side, all section SVMAs are then just offsets from
    /// `image_base_avma`, so each FDE's PC range translates trivially.
    /// `vmsize` is the executable LOAD's runtime size (the `len` field
    /// of MMAP2); it bounds the AVMA range framehop uses to ask "is
    /// this PC inside this module?".
    #[allow(clippy::too_many_arguments)]
    pub fn add_image(
        &mut self,
        path: &str,
        mapping_avma: u64,
        image_base_avma: u64,
        vmsize: u64,
        text_svma: Option<Range<u64>>,
        text: Option<Vec<u8>>,
        eh_frame_svma: Range<u64>,
        eh_frame: Vec<u8>,
        eh_frame_hdr_svma: Option<Range<u64>>,
        eh_frame_hdr: Option<Vec<u8>>,
    ) {
        let info = ExplicitModuleSectionInfo {
            base_svma: 0,
            text_svma,
            text,
            eh_frame_svma: Some(eh_frame_svma),
            eh_frame: Some(eh_frame),
            eh_frame_hdr_svma,
            eh_frame_hdr,
            ..Default::default()
        };
        let module = Module::new(
            path.to_string(),
            mapping_avma..mapping_avma.saturating_add(vmsize),
            image_base_avma,
            info,
        );
        self.unwinder.add_module(module);
    }

    /// Walk the stack starting from `(ip, sp, bp)`, reading return
    /// addresses out of the captured `stack_bytes`. `stack_base_addr`
    /// is the SP at capture (the lowest address in `stack_bytes`);
    /// `stack_bytes.len()` is what perf actually filled (`dyn_size`).
    /// Returns a flat `Vec<u64>` of IPs: the instruction pointer
    /// first, then each return address. Empty on hard failure (which
    /// the caller treats as "fall back to the kernel CALLCHAIN").
    pub fn unwind(
        &mut self,
        ip: u64,
        sp: u64,
        bp: u64,
        stack_base_addr: u64,
        stack_bytes: &[u8],
    ) -> Vec<u64> {
        let regs = UnwindRegsX86_64::new(ip, sp, bp);
        let mut read_stack = |addr: u64| -> Result<u64, ()> {
            if addr < stack_base_addr {
                return Err(());
            }
            let off = (addr - stack_base_addr) as usize;
            let end = off.checked_add(8).ok_or(())?;
            if end > stack_bytes.len() {
                return Err(());
            }
            let bytes: [u8; 8] = stack_bytes[off..end].try_into().map_err(|_| ())?;
            Ok(u64::from_le_bytes(bytes))
        };

        let mut frames: Vec<u64> = Vec::with_capacity(32);
        let mut iter = self
            .unwinder
            .iter_frames(ip, regs, &mut self.cache, &mut read_stack);
        // `iter.next()` returns `Ok(Some(_))` per frame, `Ok(None)` on
        // successful end-of-stack, `Err(_)` on truncation (no CFI for
        // the next return address, stack snapshot ran out, …). A
        // truncated unwind still yields useful prefix frames — keep
        // what we got and stop.
        loop {
            match iter.next() {
                Ok(Some(frame)) => {
                    let pc = match frame {
                        FrameAddress::InstructionPointer(p) => p,
                        FrameAddress::ReturnAddress(p) => p.get(),
                    };
                    frames.push(pc);
                    if frames.len() > 256 {
                        break; // defensive runaway guard on corrupt CFI
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        frames
    }
}
