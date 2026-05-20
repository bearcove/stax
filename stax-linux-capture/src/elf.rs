//! On-disk ELF → the OS-neutral `MachOSymbol` list, plus the program
//! headers we need to turn a `PERF_RECORD_MMAP2` (runtime addr + file
//! offset) into the link-time SVMA the symbols are relative to.
//!
//! This is the ELF analogue of `stax-mac-kperf-parse::image_scan` (which
//! does the same job for Mach-O `LC_SYMTAB`).

use object::read::elf::{ElfFile64, FileHeader, ProgramHeader};
use object::{Object, ObjectSymbol, ObjectKind};
use stax_mac_capture::proc_maps::MachOSymbol;

/// One executable `PT_LOAD` segment: enough to map a file offset back
/// to the address the linker assigned (`svma = off - p_offset + p_vaddr`).
#[derive(Clone, Copy, Debug)]
pub struct LoadSeg {
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_filesz: u64,
}

pub struct ElfImage {
    pub arch: Option<&'static str>,
    pub is_executable: bool,
    /// First 16 bytes of the GNU build-id, used like a Mach-O `LC_UUID`
    /// for image identity. `None` if the binary has no build-id note.
    pub build_id: Option<[u8; 16]>,
    /// Full GNU build-id (typically 20 bytes / SHA-1) — kept around in
    /// addition to the truncated [`Self::build_id`] because the
    /// debuginfo lookup
    /// (`/usr/lib/debug/.build-id/XX/YYY...YY.debug`) keys on the full
    /// hex, not the truncated identity-only prefix.
    pub build_id_full: Vec<u8>,
    /// Function symbols, addresses as SVMAs, sorted by `start_svma`.
    pub symbols: Vec<MachOSymbol>,
    pub loads: Vec<LoadSeg>,
}

impl ElfImage {
    /// SVMA the linker assigned to the byte at file offset `file_off`,
    /// i.e. which segment maps it. Returns `None` for offsets outside
    /// every `PT_LOAD` (e.g. a mapping of a non-code section).
    pub fn svma_for_file_off(&self, file_off: u64) -> Option<u64> {
        // The kernel maps each segment page-aligned, so a mapping's
        // `pgoff` is `p_offset` rounded *down* to the page (e.g. seg at
        // file 0xddba0 → r-xp mapping pgoff 0xdd000). ELF guarantees
        // `p_vaddr ≡ p_offset (mod p_align)` and `p_align` is a multiple
        // of the page size, so the linear relation `svma = off -
        // p_offset + p_vaddr` stays exact across that rounded prefix —
        // we just have to widen the lower bound to the page floor.
        // (4 KiB covers x86_64 and 4K-page arm64; 16K-page arm64 is a
        // Phase-B refinement — it would only over-match unused padding.)
        const PAGE: u64 = 0x1000;
        for s in &self.loads {
            let lo = s.p_offset & !(PAGE - 1);
            if file_off >= lo && file_off < s.p_offset + s.p_filesz {
                return Some(file_off.wrapping_sub(s.p_offset).wrapping_add(s.p_vaddr));
            }
        }
        None
    }
}

fn arch_str(file: &ElfFile64) -> Option<&'static str> {
    match file.architecture() {
        object::Architecture::Aarch64 => Some("aarch64"),
        object::Architecture::X86_64 => Some("x86_64"),
        object::Architecture::I386 => Some("i386"),
        object::Architecture::Arm => Some("arm"),
        _ => None,
    }
}

/// Parse `path` as a 64-bit ELF. `None` if it isn't one (anonymous /
/// JIT mappings, `[vdso]`, deleted files, non-ELF, etc.).
pub fn scan(bytes: &[u8]) -> Option<ElfImage> {
    let file: ElfFile64 = ElfFile64::parse(bytes).ok()?;

    let endian = file.endian();
    let header = file.elf_header();
    let mut loads = Vec::new();
    if let Ok(phdrs) = header.program_headers(endian, bytes) {
        for ph in phdrs {
            if ph.p_type(endian) == object::elf::PT_LOAD
                && ph.p_flags(endian) & object::elf::PF_X != 0
            {
                loads.push(LoadSeg {
                    p_offset: ph.p_offset(endian),
                    p_vaddr: ph.p_vaddr(endian),
                    p_filesz: ph.p_filesz(endian),
                });
            }
        }
    }

    let is_executable = matches!(file.kind(), ObjectKind::Executable);
    let build_id_bytes: Vec<u8> = file
        .build_id()
        .ok()
        .flatten()
        .map(|id| id.to_vec())
        .unwrap_or_default();
    let build_id = if build_id_bytes.is_empty() {
        None
    } else {
        let mut out = [0u8; 16];
        let n = build_id_bytes.len().min(16);
        out[..n].copy_from_slice(&build_id_bytes[..n]);
        Some(out)
    };

    // Function/code symbols only, addresses as link-time SVMAs (ELF
    // `st_value` already is one). Pulled into a helper so the
    // separate-debug lookup path can produce the same shape.
    let symbols = extract_symbols(&file);

    Some(ElfImage {
        arch: arch_str(&file),
        is_executable,
        build_id,
        build_id_full: build_id_bytes,
        symbols,
        loads,
    })
}

/// Extract function symbols from `.symtab` + `.dynsym` of `bytes`,
/// returning them in the same SVMA-sorted shape [`scan`] produces.
/// Shared between the primary image scan and the separate-debug
/// lookup so the two paths can be merged.
fn extract_symbols(file: &ElfFile64) -> Vec<MachOSymbol> {
    let mut symbols: Vec<MachOSymbol> = Vec::new();
    for sym in file.symbols().chain(file.dynamic_symbols()) {
        if !sym.is_definition() {
            continue;
        }
        let addr = sym.address();
        if addr == 0 {
            continue;
        }
        if let Ok(name) = sym.name_bytes() {
            if name.is_empty() {
                continue;
            }
            symbols.push(MachOSymbol {
                start_svma: addr,
                end_svma: addr.saturating_add(sym.size().max(4)),
                name: name.to_vec(),
            });
        }
    }
    symbols.sort_by_key(|s| s.start_svma);
    symbols.dedup_by_key(|s| s.start_svma);
    for i in 0..symbols.len() {
        let next = symbols.get(i + 1).map(|n| n.start_svma);
        if let Some(next) = next {
            if next > symbols[i].start_svma {
                symbols[i].end_svma = next;
            }
        }
    }
    symbols
}

/// Where the system stashes detached debug info, keyed by GNU build-id.
/// `/usr/lib/debug/.build-id/XX/YYY...YY.debug` is the cross-distro
/// convention (Debian, Ubuntu, Fedora, Arch — all the same). The
/// `XX/` directory is the first byte of the build-id as two hex
/// digits; the filename is the remaining hex + `.debug`.
const DEBUG_BUILD_ID_ROOT: &str = "/usr/lib/debug/.build-id";

/// Render `bytes` as lowercase ASCII hex. Plain helper — no `hex` crate
/// dep just for one stringify of a 20-byte SHA-1.
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// Try to load detached debug symbols for `build_id_full` from the
/// local `/usr/lib/debug/.build-id/` tree. Returns the parsed symbols
/// on hit, `None` on miss / parse failure (in which case the caller
/// just uses whatever symbols the primary image had).
///
/// This is the first lookup in the chain a Linux profiler does on
/// stripped distro libraries: `libc.so.6`, `libstdc++.so.6`,
/// `ld-linux.so.2` etc. all ship without `.symtab` but their
/// `*-dbg` / `*-debuginfo` package drops a fully-symboled `.debug`
/// file under this path. (debuginfod HTTP lookup is a follow-on for
/// hosts where the package isn't installed but the network is.)
pub fn load_separate_debug_by_build_id(build_id_full: &[u8]) -> Option<Vec<MachOSymbol>> {
    if build_id_full.len() < 2 {
        return None;
    }
    let hex = hex_lower(build_id_full);
    let path = format!(
        "{}/{}/{}.debug",
        DEBUG_BUILD_ID_ROOT,
        &hex[..2],
        &hex[2..]
    );
    let bytes = std::fs::read(&path).ok()?;
    let file: ElfFile64 = ElfFile64::parse(&*bytes).ok()?;
    Some(extract_symbols(&file))
}
