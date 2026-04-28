//! Minimal framehop wiring used by the race-against-return probe.
//!
//! Mirrors the public surface of `stax-shade::walker` (image
//! enumeration → framehop modules; a `MachStackReader` that
//! `mach_vm_read_overwrite`s into the target task; a single-stack
//! walk that returns the list of return addresses framehop
//! produced). Pulled in here so staxd doesn't depend on
//! stax-shade — the two are otherwise unrelated.

#![cfg(target_os = "macos")]

use framehop::aarch64::{CacheAarch64, UnwinderAarch64};
use mach2::kern_return::KERN_SUCCESS;
use mach2::port::mach_port_t;

/// Memory accessor for framehop. `mach_port_t` is a u32 — copying
/// it doesn't duplicate the underlying right.
#[derive(Copy, Clone)]
pub struct MachStackReader {
    pub task: mach_port_t,
}

impl MachStackReader {
    fn read_exact(&self, addr: u64, buf: &mut [u8]) -> bool {
        use mach2::vm::mach_vm_read_overwrite;
        use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};

        let mut got: mach_vm_size_t = 0;
        // SAFETY: buf is a unique mut slice; addr is opaque integer
        // to the kernel; got is an out-pointer.
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

    fn read_u64(&self, addr: u64) -> Option<u64> {
        let mut buf = [0u8; 8];
        if self.read_exact(addr, &mut buf) {
            Some(u64::from_le_bytes(buf))
        } else {
            None
        }
    }
}

/// Snapshot of the target's loaded modules + the framehop unwinder
/// built from them. Owned by the probe worker for the lifetime of
/// the session.
pub struct TargetUnwinder {
    pub unwinder: UnwinderAarch64<Vec<u8>>,
    pub cache: CacheAarch64,
    pub reader: MachStackReader,
    pub stats: UnwinderStats,
    pub symbolicator: Symbolicator,
}

/// Address-to-symbol resolver covering both the target's on-disk
/// binaries (parsed via the `object` crate) and the host's
/// dyld_shared_cache (via stax-mac-shared-cache). Built once per
/// session; lookup is `O(log N)` per address.
pub struct Symbolicator {
    on_disk: Vec<DiskBinary>,
    shared_cache: Option<std::sync::Arc<stax_mac_shared_cache::SharedCache>>,
}

struct DiskBinary {
    basename: String,
    avma_range: core::ops::Range<u64>,
    slide: i64,
    /// Sorted by `start_svma` for binary-search lookup.
    symbols: Vec<stax_mac_capture::proc_maps::MachOSymbol>,
}

#[derive(Default, Debug)]
pub struct UnwinderStats {
    pub images_total: usize,
    pub modules_added: usize,
    pub with_unwind_info: usize,
    pub with_eh_frame: usize,
}

/// Enumerate target images via stax-target-images and build a
/// framehop unwinder + symbolicator. Returns `None` if image
/// enumeration failed outright (probe falls back to FP-walk and
/// raw addresses in that case).
pub fn build(task: mach_port_t) -> Option<TargetUnwinder> {
    use framehop::Unwinder;
    use framehop::{ExplicitModuleSectionInfo, Module};

    let walker = stax_target_images::TargetImageWalker::new(task);
    let images = match walker.enumerate() {
        Ok(images) => images,
        Err(e) => {
            tracing::warn!("probe: dyld walk failed: {e}");
            return None;
        }
    };

    let mut unwinder: UnwinderAarch64<Vec<u8>> = UnwinderAarch64::new();
    let mut stats = UnwinderStats {
        images_total: images.len(),
        ..Default::default()
    };
    let mut on_disk: Vec<DiskBinary> = Vec::new();

    for img in images {
        // Symbolicator side: try to parse the on-disk binary for
        // its symbol table. Failures are logged-and-ignored — the
        // image still gets registered with framehop. Shared-cache
        // dylibs (paths like /usr/lib/libsystem_*.dylib) won't
        // exist on disk on Apple Silicon; the SharedCache below
        // covers them.
        if let Some(sections) = img.sections.as_ref() {
            if let Some(text_avma) = sections.text_avma.as_ref() {
                if let Some(symbols) = parse_disk_symbols(&img.path) {
                    let basename = std::path::Path::new(&img.path)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&img.path)
                        .to_owned();
                    on_disk.push(DiskBinary {
                        basename,
                        avma_range: text_avma.clone(),
                        slide: sections.slide,
                        symbols,
                    });
                }
            }
        }

        let Some(sections) = img.sections else { continue };
        let Some(text_avma) = sections.text_avma.clone() else {
            continue;
        };
        let text_svma = sections.avma_to_svma(&text_avma);
        let base_svma = text_svma.start;

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

    on_disk.sort_by_key(|b| b.avma_range.start);

    let shared_cache = stax_mac_shared_cache::SharedCache::for_host().map(std::sync::Arc::new);
    let symbolicator = Symbolicator {
        on_disk,
        shared_cache,
    };

    Some(TargetUnwinder {
        unwinder,
        cache: CacheAarch64::new(),
        reader: MachStackReader { task },
        stats,
        symbolicator,
    })
}

/// Parse the LC_SYMTAB of a Mach-O on disk and return its symbols
/// sorted by start_svma (suitable for binary search). Returns
/// `None` for non-Mach-O paths, paths that don't exist (shared-
/// cache dylibs, anonymous JIT regions, …), or parse errors.
/// Cribbed from `stax-mac-kperf-parse::image_scan::parse_disk_macho`.
fn parse_disk_symbols(path: &str) -> Option<Vec<stax_mac_capture::proc_maps::MachOSymbol>> {
    use object::read::macho::MachOFile64;
    use object::{Endianness, Object, ObjectSymbol};
    use stax_mac_capture::proc_maps::MachOSymbol;

    let bytes = std::fs::read(path).ok()?;
    let file = MachOFile64::<Endianness, _>::parse(&bytes[..]).ok()?;

    let mut raw: Vec<(u64, Vec<u8>)> = Vec::new();
    for sym in file.symbols() {
        let addr = sym.address();
        if addr == 0 {
            continue;
        }
        let Ok(name) = sym.name_bytes() else { continue };
        if name.is_empty() {
            continue;
        }
        raw.push((addr, name.to_vec()));
    }
    raw.sort_by_key(|(a, _)| *a);
    raw.dedup_by_key(|(a, _)| *a);

    let mut symbols: Vec<MachOSymbol> = Vec::with_capacity(raw.len());
    for i in 0..raw.len() {
        let start = raw[i].0;
        let end = raw.get(i + 1).map(|(a, _)| *a).unwrap_or(start + 4);
        let name = std::mem::take(&mut raw[i].1);
        symbols.push(MachOSymbol {
            start_svma: start,
            end_svma: end,
            name,
        });
    }
    Some(symbols)
}

impl Symbolicator {
    /// Resolve a (PAC-stripped) AVMA to a human-readable
    /// `module!symbol+offset` string. Falls back to
    /// `module+0xoffset` if the address is inside a known module
    /// but no enclosing symbol; falls back to `<unmapped:0xaddr>`
    /// if no module covers it. Demangling via stax-demangle.
    pub fn resolve(&self, addr: u64) -> String {
        if addr == 0 {
            return "<null>".to_owned();
        }
        // Disk-backed binaries first (cheap binary search).
        if let Some(s) = self.resolve_on_disk(addr) {
            return s;
        }
        // Then the dyld shared cache.
        if let Some(s) = self.resolve_shared_cache(addr) {
            return s;
        }
        format!("<unmapped:{addr:#x}>")
    }

    fn resolve_on_disk(&self, addr: u64) -> Option<String> {
        // Find the binary whose avma_range contains addr. partition_point
        // → first binary whose start > addr; candidate is one before.
        let i = self
            .on_disk
            .partition_point(|b| b.avma_range.start <= addr);
        if i == 0 {
            return None;
        }
        let b = &self.on_disk[i - 1];
        if !b.avma_range.contains(&addr) {
            return None;
        }
        let svma = (addr as i64).wrapping_sub(b.slide) as u64;
        Some(format_symbol(&b.basename, svma, &b.symbols, addr))
    }

    fn resolve_shared_cache(&self, addr: u64) -> Option<String> {
        let cache = self.shared_cache.as_ref()?;
        let img_ref = cache.lookup_address(addr)?;
        let img = img_ref.image();
        let basename = std::path::Path::new(&img.install_name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&img.install_name);
        // Cache symbols are SVMA-keyed; cache slide = runtime_avma - text_svma.
        let slide = (img.runtime_avma as i64).wrapping_sub(img.text_svma as i64);
        let svma = (addr as i64).wrapping_sub(slide) as u64;
        Some(format_symbol(basename, svma, &img.symbols, addr))
    }
}

fn format_symbol(
    module: &str,
    svma: u64,
    symbols: &[stax_mac_capture::proc_maps::MachOSymbol],
    addr: u64,
) -> String {
    let i = symbols.partition_point(|s| s.start_svma <= svma);
    if i > 0 {
        let candidate = &symbols[i - 1];
        if svma < candidate.end_svma {
            let demangled = stax_demangle::demangle_bytes(&candidate.name);
            let off = svma.saturating_sub(candidate.start_svma);
            if off == 0 {
                return format!("{module}!{}", demangled.name);
            }
            return format!("{module}!{}+{off:#x}", demangled.name);
        }
    }
    let _ = addr;
    let off = svma;
    format!("{module}+{off:#x}")
}

/// Walk a single thread's stack via framehop. `pc/lr/sp/fp` come
/// from a fresh `thread_get_state(ARM_THREAD_STATE64)` — caller is
/// responsible for having the thread suspended. Returns the list
/// of return addresses framehop produced (PAC-bearing — strip at
/// the call site if needed).
///
/// The leaf PC is *not* included in the returned list (matches FP
/// walk shape). framehop's first frame is the caller of `pc`,
/// equivalent to the saved LR.
pub fn walk(tu: &mut TargetUnwinder, pc: u64, lr: u64, sp: u64, fp: u64, max: usize) -> Vec<u64> {
    use framehop::Unwinder;
    use framehop::aarch64::UnwindRegsAarch64;

    let reader = tu.reader;
    let mut read_stack = |addr: u64| reader.read_u64(addr).ok_or(());

    let mut iter = tu.unwinder.iter_frames(
        pc,
        UnwindRegsAarch64::new(lr, sp, fp),
        &mut tu.cache,
        &mut read_stack,
    );

    let mut frames: Vec<u64> = Vec::with_capacity(max);
    let mut first = true;
    loop {
        match iter.next() {
            Ok(Some(frame)) => {
                // framehop's first frame is the leaf PC itself.
                // Skip it so the returned list is "return addresses
                // only" — matches the FP-walk shape used elsewhere.
                if first {
                    first = false;
                    continue;
                }
                frames.push(frame.address());
                if frames.len() >= max {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    frames
}
