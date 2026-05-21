//! On-disk ELF → the OS-neutral `MachOSymbol` list, plus the program
//! headers we need to turn a `PERF_RECORD_MMAP2` (runtime addr + file
//! offset) into the link-time SVMA the symbols are relative to.
//!
//! This is the ELF analogue of `stax-mac-kperf-parse::image_scan` (which
//! does the same job for Mach-O `LC_SYMTAB`).

use std::io::Read;
use std::ops::Range;

use object::read::elf::{ElfFile64, FileHeader, ProgramHeader};
use object::{Object, ObjectSection, ObjectSymbol, ObjectKind};
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
    /// `.text` SVMA range + bytes. Populated when the section exists;
    /// `None` for purely data libraries. framehop uses both the range
    /// (to detect "is this PC inside text?") and the bytes (for
    /// prologue/epilogue instruction analysis).
    pub text: Option<SectionSlice>,
    /// `.eh_frame` SVMA range + bytes. This is the DWARF CFI that
    /// userspace unwinders walk to recover return addresses through
    /// `-fomit-frame-pointer` code. `None` ⇒ no DWARF unwinding for
    /// this image (framehop falls back to frame-pointer chasing).
    pub eh_frame: Option<SectionSlice>,
    /// `.eh_frame_hdr` SVMA range + bytes, when present. The
    /// pre-built binary search index over `.eh_frame`; framehop uses
    /// it when available and synthesises one otherwise.
    pub eh_frame_hdr: Option<SectionSlice>,
}

/// One ELF section's link-time address range and the bytes that live
/// at it. Cloned into framehop's per-module storage, so it owns the
/// data for the session.
#[derive(Clone, Debug)]
pub struct SectionSlice {
    pub svma: Range<u64>,
    pub bytes: Vec<u8>,
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

    // Sections the DWARF unwinder needs. Each is optional: many
    // shared libraries don't ship a `.eh_frame_hdr`, and tiny shims
    // can lack `.text`. framehop tolerates any combination — what
    // it can't do is unwind through code with no `.eh_frame`.
    let text = read_section(&file, b".text");
    let eh_frame = read_section(&file, b".eh_frame");
    let eh_frame_hdr = read_section(&file, b".eh_frame_hdr");

    Some(ElfImage {
        arch: arch_str(&file),
        is_executable,
        build_id,
        build_id_full: build_id_bytes,
        symbols,
        loads,
        text,
        eh_frame,
        eh_frame_hdr,
    })
}

/// x86_64 frame-pointer prologue: `push %rbp` (`55`) immediately
/// followed by `mov %rsp,%rbp` (`48 89 e5`). A `-fno-omit-frame-pointer`
/// build opens (almost) every function with this; an
/// `-fomit-frame-pointer` build essentially never does — a function
/// there may still `push %rbp` to *save* the callee-saved register,
/// but it won't follow with the `mov`, so the 4-byte pair is the
/// unambiguous tell.
const FP_PROLOGUE: [u8; 4] = [0x55, 0x48, 0x89, 0xe5];
/// x86_64 CET landing pad `endbr64`. When a function is an indirect-
/// branch target the compiler emits this first; the FP prologue (if
/// any) follows it.
const ENDBR64: [u8; 4] = [0xf3, 0x0f, 0x1e, 0xfa];

/// How many inspected `.text` functions opened with a frame-pointer
/// prologue, out of how many we could read. Produced by
/// [`frame_pointer_stats`] and consumed by the `--dwarf-unwind` auto
/// mode (`stax_linux_capture::scan_frame_pointers`).
#[derive(Clone, Copy, Debug)]
pub struct FramePointerStats {
    /// Functions whose first bytes we successfully inspected.
    pub scanned: usize,
    /// Of those, how many opened with `push %rbp; mov %rsp,%rbp`.
    pub with_prologue: usize,
}

impl FramePointerStats {
    /// `true` when the binary looks built `-fomit-frame-pointer`:
    /// fewer than half the inspected functions keep a frame pointer.
    /// (Real builds are ~all-or-nothing — the compiler flag is
    /// per-translation-unit — so the 50% line has a wide margin.)
    pub fn omits_frame_pointers(&self) -> bool {
        self.with_prologue * 2 < self.scanned
    }
}

/// Inspect the function prologues in `text` for every symbol that
/// falls inside it. `None` when there isn't enough to decide — no
/// `.text`, or fewer than 8 inspectable functions (a stripped or tiny
/// binary, where a handful of hand-written asm stubs could skew any
/// ratio).
pub fn frame_pointer_stats(
    symbols: &[MachOSymbol],
    text: &SectionSlice,
) -> Option<FramePointerStats> {
    let mut scanned = 0usize;
    let mut with_prologue = 0usize;
    for sym in symbols {
        // Only symbols inside `.text` are functions with a prologue
        // to read (`.dynsym` also names PLT stubs, data, …).
        if sym.start_svma < text.svma.start || sym.start_svma >= text.svma.end {
            continue;
        }
        let off = (sym.start_svma - text.svma.start) as usize;
        // 8 bytes covers an optional `endbr64` (4) + the prologue (4).
        let win = match text.bytes.get(off..off + 8) {
            Some(w) => w,
            None => continue,
        };
        scanned += 1;
        let body = if win[..4] == ENDBR64 { &win[4..8] } else { &win[..4] };
        if body == FP_PROLOGUE {
            with_prologue += 1;
        }
    }
    if scanned < 8 {
        return None;
    }
    Some(FramePointerStats {
        scanned,
        with_prologue,
    })
}

/// Pull one section's `(svma, bytes)` out of an ELF, or `None` if it
/// isn't present / has no on-disk data (e.g. `SHT_NOBITS`).
fn read_section(file: &ElfFile64, name: &[u8]) -> Option<SectionSlice> {
    let section = file
        .sections()
        .find(|s| s.name_bytes().ok().is_some_and(|n| n == name))?;
    let data = section.data().ok()?;
    if data.is_empty() {
        return None;
    }
    let addr = section.address();
    Some(SectionSlice {
        svma: addr..addr.saturating_add(data.len() as u64),
        bytes: data.to_vec(),
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
/// file under this path. The [debuginfod HTTP fallback](`debuginfod_fetch`)
/// covers hosts without the dbg packages.
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

/// debuginfod HTTPS lookup config: where to ask, where to cache, how
/// long to wait. One built per session via [`Self::from_env`] and
/// passed into [`debuginfod_fetch`].
#[derive(Clone, Debug)]
pub struct DebuginfodConfig {
    /// Base URLs to try in order (e.g.
    /// `https://debuginfod.debian.net`). Empty = "no debuginfod
    /// configured" — the helper short-circuits to `None` without any
    /// network I/O.
    pub urls: Vec<String>,
    /// On-disk cache root. Hits are stored as
    /// `<cache_dir>/<XX>/<YYY...>.debug`; misses (negative cache)
    /// drop a zero-byte `.miss` sentinel so we don't re-fetch each
    /// session.
    pub cache_dir: std::path::PathBuf,
    /// Per-request HTTP timeout. Image-loaded events fire on the
    /// drain thread, so this directly caps how long a stripped image
    /// can pause sampling. The first session pays the latency; the
    /// disk cache makes every subsequent session instant.
    pub timeout: std::time::Duration,
}

impl DebuginfodConfig {
    /// Read the standard debuginfod configuration sources, returning
    /// `None` when there is nothing to query (no env, no Debian-style
    /// `/etc/debuginfod/*.urls`). Sources, in order:
    ///   1. `DEBUGINFOD_URLS` env var (space/semicolon-separated).
    ///   2. Every `*.urls` file under `/etc/debuginfod/` — one URL
    ///      per non-comment line. The Debian `libdebuginfod-common`
    ///      package drops `elfutils.urls` here.
    pub fn from_env() -> Option<Self> {
        let mut urls: Vec<String> = Vec::new();
        if let Ok(s) = std::env::var("DEBUGINFOD_URLS") {
            for url in s.split([' ', ';', '\t']) {
                let url = url.trim();
                if !url.is_empty() {
                    urls.push(url.to_string());
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir("/etc/debuginfod") {
            for ent in entries.flatten() {
                let p = ent.path();
                if p.extension().and_then(|e| e.to_str()) != Some("urls") {
                    continue;
                }
                let Ok(s) = std::fs::read_to_string(&p) else { continue };
                for line in s.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if !urls.iter().any(|u| u == line) {
                        urls.push(line.to_string());
                    }
                }
            }
        }
        if urls.is_empty() {
            return None;
        }

        // `$XDG_CACHE_HOME/stax/debuginfod`, falling back to
        // `~/.cache/stax/debuginfod`. Same shape as elfutils'
        // `debuginfod-client` so users can share with `debuginfod-find`
        // if they want to (we don't read its cache yet, but the layout
        // matches so a future opt-in is mechanical).
        let cache_root = std::env::var_os("XDG_CACHE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
        let cache_dir = cache_root.join("stax").join("debuginfod");

        Some(Self {
            urls,
            cache_dir,
            timeout: std::time::Duration::from_secs(5),
        })
    }

    /// Cache path for a given build-id hex. The shape mirrors
    /// `/usr/lib/debug/.build-id/`.
    fn cache_path(&self, hex: &str, suffix: &str) -> std::path::PathBuf {
        self.cache_dir.join(&hex[..2]).join(format!("{}{suffix}", &hex[2..]))
    }
}

/// Look up `build_id_full` against the configured debuginfod servers
/// (or the local on-disk cache), returning the parsed symbols on hit.
///
/// The on-disk cache is consulted first, so warm starts are a single
/// `read()` regardless of network state. A miss is recorded as an
/// empty `.miss` sentinel next to where a hit would have lived; the
/// next session sees it and short-circuits without re-trying the
/// servers. (Cache-busting: delete the `<XX>/` subdir.)
///
/// Synchronous, blocking — image-load events come from the drain
/// thread, and the natural unit of work is "one image, one HTTPS GET".
/// Per-request timeout comes from [`DebuginfodConfig::timeout`]; on
/// most programs the per-process image loads happen at process start
/// so the latency is paid once, up front.
pub fn debuginfod_fetch(
    cfg: &DebuginfodConfig,
    build_id_full: &[u8],
) -> Option<Vec<MachOSymbol>> {
    if cfg.urls.is_empty() || build_id_full.len() < 2 {
        return None;
    }
    let hex = hex_lower(build_id_full);
    let hit_path = cfg.cache_path(&hex, ".debug");
    let miss_path = cfg.cache_path(&hex, ".miss");

    // Warm cache hit.
    if let Ok(bytes) = std::fs::read(&hit_path) {
        let file: ElfFile64 = ElfFile64::parse(&*bytes).ok()?;
        return Some(extract_symbols(&file));
    }
    // Negative cache: don't hammer the server for a known miss.
    if miss_path.exists() {
        return None;
    }

    // Cold lookup. Try every configured URL until one returns a
    // parseable ELF; first hit wins.
    let agent = ureq::AgentBuilder::new()
        .timeout(cfg.timeout)
        .user_agent(concat!("stax/", env!("CARGO_PKG_VERSION")))
        .build();
    for base in &cfg.urls {
        let url = format!(
            "{}/buildid/{hex}/debuginfo",
            base.trim_end_matches('/')
        );
        let resp = match agent.get(&url).call() {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(url = %url, %e, "debuginfod GET failed");
                continue;
            }
        };
        if resp.status() != 200 {
            tracing::debug!(url = %url, status = resp.status(), "debuginfod miss");
            continue;
        }
        let mut bytes: Vec<u8> = Vec::new();
        if let Err(e) = resp.into_reader().read_to_end(&mut bytes) {
            tracing::debug!(url = %url, %e, "debuginfod body read failed");
            continue;
        }
        if bytes.len() < 4 || &bytes[..4] != b"\x7fELF" {
            tracing::debug!(url = %url, bytes = bytes.len(), "debuginfod body not an ELF");
            continue;
        }

        // Persist the hit before symbol extraction — even if our parse
        // fails we want the cache populated for the next attempt.
        if let Some(parent) = hit_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = hit_path.with_extension("debug.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &hit_path);
        }

        let file: ElfFile64 = match ElfFile64::parse(&*bytes) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(url = %url, %e, "debuginfod ELF parse failed");
                continue;
            }
        };
        return Some(extract_symbols(&file));
    }

    // All URLs missed — drop a sentinel so we don't ask again this
    // (or next) session. Best-effort; if the FS write fails we just
    // pay the lookup again later.
    if let Some(parent) = miss_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&miss_path, b"");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic `.text` of `n` functions, each 16 bytes,
    /// where the first `fp` of them open with `endbr64` + the
    /// frame-pointer prologue and the rest open with `endbr64` + a
    /// non-prologue instruction. Returns the section + one symbol per
    /// function.
    fn synth(n: usize, fp: usize) -> (Vec<MachOSymbol>, SectionSlice) {
        const STRIDE: u64 = 16;
        const BASE: u64 = 0x1000;
        let mut bytes = Vec::new();
        let mut symbols = Vec::new();
        for i in 0..n {
            let mut func = Vec::with_capacity(STRIDE as usize);
            func.extend_from_slice(&ENDBR64);
            if i < fp {
                func.extend_from_slice(&FP_PROLOGUE);
            } else {
                // `sub $0x18,%rsp` — a typical omit-FP prologue.
                func.extend_from_slice(&[0x48, 0x83, 0xec, 0x18]);
            }
            func.resize(STRIDE as usize, 0x90); // pad with NOPs
            let start = BASE + i as u64 * STRIDE;
            symbols.push(MachOSymbol {
                start_svma: start,
                end_svma: start + STRIDE,
                name: format!("fn{i}").into_bytes(),
            });
            bytes.extend_from_slice(&func);
        }
        let text = SectionSlice {
            svma: BASE..BASE + bytes.len() as u64,
            bytes,
        };
        (symbols, text)
    }

    #[test]
    fn detects_frame_pointer_build() {
        let (syms, text) = synth(40, 40); // every function keeps FP
        let stats = frame_pointer_stats(&syms, &text).expect("enough functions");
        assert_eq!(stats.scanned, 40);
        assert_eq!(stats.with_prologue, 40);
        assert!(!stats.omits_frame_pointers());
    }

    #[test]
    fn detects_omit_frame_pointer_build() {
        let (syms, text) = synth(40, 0); // no function keeps FP
        let stats = frame_pointer_stats(&syms, &text).expect("enough functions");
        assert_eq!(stats.with_prologue, 0);
        assert!(stats.omits_frame_pointers());
    }

    #[test]
    fn too_few_functions_is_undecidable() {
        let (syms, text) = synth(5, 5);
        assert!(frame_pointer_stats(&syms, &text).is_none());
    }

    #[test]
    fn symbols_outside_text_are_ignored() {
        // A `.dynsym`-style entry pointing into `.plt` (before `.text`)
        // must not be counted as a scanned function.
        let (mut syms, text) = synth(20, 20);
        syms.push(MachOSymbol {
            start_svma: 0x10, // well below the .text base
            end_svma: 0x20,
            name: b"plt_stub".to_vec(),
        });
        let stats = frame_pointer_stats(&syms, &text).expect("enough functions");
        assert_eq!(stats.scanned, 20);
    }
}
