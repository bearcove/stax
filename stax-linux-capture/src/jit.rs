//! JIT code on Linux: anonymous executable mappings, named from
//! `/tmp/perf-<pid>.map` and `/tmp/jit-<pid>.dump`.
//!
//! JIT'd code lives in mappings with no on-disk ELF (`//anon`, a named
//! `[anon:…]` region, or a `/memfd:` file), so `emit_image` has nothing
//! to read. Instead we keep those mappings as *regions*, and publish
//! each region to the sink as a synthetic `[jit]` image whose symbol
//! table is every perf-map / jitdump entry that falls inside it.
//! `base_avma == text_svma`, so a symbol's SVMA is simply its runtime
//! address.
//!
//! The live `BinaryRegistry` keys images by `base_avma` and resolves an
//! address through the last image whose base is `<= address`. Images
//! must therefore never overlap, and an image must never move its base
//! (the registry would keep the stale one; Linux sinks don't remove on
//! unload). [`JitImages`] guarantees both: when the kernel reports a
//! mapping that overlaps known regions (VMA merges after `mprotect`),
//! only the uncovered sub-ranges become new regions. When symbols arrive
//! for a region it is simply re-emitted at the same base with the larger
//! symbol table, which replaces the previous one.
//!
//! Both symbol files grow while the target runs, so they are tailed: the
//! perf map by [`PerfMapTailer`], the jitdump by the shared
//! [`stax_mac_capture::jitdump_tail::JitdumpTailer`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use stax_mac_capture::jitdump_tail::JitdumpTailer;
use stax_mac_capture::proc_maps::MachOSymbol;
use tracing::{debug, info, warn};

/// Path the synthetic images are published under; `stax top` shows it in
/// the binary column.
pub(crate) const JIT_IMAGE_PATH: &str = "[jit]";

/// How often the drain loop re-reads the symbol files.
const POLL_PERIOD: Duration = Duration::from_millis(250);

/// Is this executable mapping JIT code (no on-disk ELF we could read)?
/// `[vdso]`, `[vsyscall]` and friends are not: they are kernel-provided.
pub(crate) fn is_jit_mapping(path: &str) -> bool {
    path.is_empty() || path == "//anon" || path.starts_with("[anon:") || path.starts_with("/memfd:")
}

/// One region ready to be published as a `[jit]` image.
pub(crate) struct JitImage {
    pub start: u64,
    pub end: u64,
    pub symbols: Vec<MachOSymbol>,
}

/// Disjoint JIT regions plus the JIT symbols known so far.
#[derive(Default)]
pub(crate) struct JitImages {
    /// Region start -> end. Disjoint; a region's start never changes.
    regions: BTreeMap<u64, u64>,
    /// Symbol start -> (end, name). A later entry at the same start wins
    /// (code freed and re-JIT'd at the same address).
    symbols: BTreeMap<u64, (u64, Box<str>)>,
    /// Region starts whose image must be (re-)published.
    dirty: BTreeSet<u64>,
    /// Symbol starts that intersect no region yet.
    orphans: Vec<u64>,
}

impl JitImages {
    /// Starts of the regions intersecting `[start, end)`, ascending.
    fn intersecting(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        // Regions are disjoint and sorted by start, so their ends are
        // sorted too: walk back from the last region starting before
        // `end` until one ends at or before `start`.
        let mut out: Vec<(u64, u64)> = self
            .regions
            .range(..end)
            .rev()
            .take_while(|(_, &e)| e > start)
            .map(|(&s, &e)| (s, e))
            .collect();
        out.reverse();
        out
    }

    /// An executable mapping `[start, end)` exists. Every part of it not
    /// already covered by a region becomes a new region. Returns how many
    /// regions were added.
    pub fn add_mapping(&mut self, start: u64, end: u64) -> usize {
        if end <= start {
            return 0;
        }
        let mut gaps = Vec::new();
        let mut cursor = start;
        for (s, e) in self.intersecting(start, end) {
            if s > cursor {
                gaps.push((cursor, s));
            }
            cursor = cursor.max(e);
        }
        if cursor < end {
            gaps.push((cursor, end));
        }
        for &(s, e) in &gaps {
            self.regions.insert(s, e);
            self.dirty.insert(s);
        }
        if !gaps.is_empty() && !self.orphans.is_empty() {
            // Symbols that were waiting for a region may now have one.
            let orphans = std::mem::take(&mut self.orphans);
            for sym in orphans {
                if let Some(end) = self.symbols.get(&sym).map(|(e, _)| *e) {
                    if !self.mark_dirty(sym, end) {
                        self.orphans.push(sym);
                    }
                }
            }
        }
        gaps.len()
    }

    /// Mark every region intersecting `[start, end)` dirty; `false` when
    /// there is none.
    fn mark_dirty(&mut self, start: u64, end: u64) -> bool {
        let hits = self.intersecting(start, end);
        for &(s, _) in &hits {
            self.dirty.insert(s);
        }
        !hits.is_empty()
    }

    /// A JIT symbol `[start, start+size)` named `name`.
    pub fn add_symbol(&mut self, start: u64, size: u64, name: &str) {
        let end = start.saturating_add(size.max(1));
        let unchanged = self
            .symbols
            .get(&start)
            .is_some_and(|(e, n)| *e == end && &**n == name);
        if unchanged {
            return;
        }
        self.symbols.insert(start, (end, name.into()));
        if !self.mark_dirty(start, end) {
            self.orphans.push(start);
        }
    }

    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    pub fn has_orphans(&self) -> bool {
        !self.orphans.is_empty()
    }

    /// Give every orphan symbol a region of its own (exactly its range,
    /// minus whatever other regions already cover). Used once we've
    /// re-read `/proc/<pid>/maps` and still found no mapping for it:
    /// the code is real (the runtime told us), we just never saw its
    /// mapping, e.g. because it is file-backed by something we skip, or
    /// the process has exited.
    pub fn adopt_orphans(&mut self) {
        let orphans = std::mem::take(&mut self.orphans);
        for sym in orphans {
            if let Some(end) = self.symbols.get(&sym).map(|(e, _)| *e) {
                self.add_mapping(sym, end);
                self.mark_dirty(sym, end);
            }
        }
        self.orphans.clear();
    }

    /// The regions that changed since the last call, with their symbol
    /// tables, ready to publish.
    pub fn take_dirty(&mut self) -> Vec<JitImage> {
        let dirty = std::mem::take(&mut self.dirty);
        let mut out = Vec::with_capacity(dirty.len());
        for start in dirty {
            let Some(&end) = self.regions.get(&start) else {
                continue;
            };
            let mut symbols = Vec::new();
            // A symbol that starts before this region but runs into it
            // (a region split by a merge) still names its tail.
            if let Some((&s, (e, name))) = self.symbols.range(..start).next_back() {
                if *e > start {
                    symbols.push(sym(s, *e, name));
                }
            }
            for (&s, (e, name)) in self.symbols.range(start..end) {
                symbols.push(sym(s, *e, name));
            }
            out.push(JitImage {
                start,
                end,
                symbols,
            });
        }
        out
    }
}

fn sym(start: u64, end: u64, name: &str) -> MachOSymbol {
    MachOSymbol {
        start_svma: start,
        end_svma: end,
        name: name.as_bytes().to_vec(),
    }
}

/// Incremental reader of a perf map (`/tmp/perf-<pid>.map`): one
/// `START SIZE NAME` line per symbol, START and SIZE hexadecimal (with
/// or without `0x`), NAME the rest of the line (it may contain spaces).
/// See linux/tools/perf/Documentation/jit-interface.txt.
pub(crate) struct PerfMapTailer {
    path: PathBuf,
    file: Option<File>,
    /// Bytes consumed so far: always the end of a complete line.
    offset: u64,
}

impl PerfMapTailer {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            offset: 0,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Entries appended since the previous call. A line still being
    /// written (no trailing newline yet) is left for the next call.
    pub fn tick(&mut self) -> std::io::Result<Vec<(u64, u64, String)>> {
        if self.file.is_none() {
            match File::open(&self.path) {
                Ok(f) => {
                    info!(path = %self.path.display(), "perf map found; tailing JIT symbols");
                    self.file = Some(f);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(e),
            }
        }
        let file = self.file.as_mut().expect("just opened");
        let len = file.metadata()?.len();
        if len < self.offset {
            // Truncated / rewritten: start over.
            self.offset = 0;
        }
        if len == self.offset {
            return Ok(Vec::new());
        }
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        file.seek(SeekFrom::Start(self.offset))?;
        file.by_ref()
            .take(len - self.offset)
            .read_to_end(&mut buf)?;
        let complete = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => return Ok(Vec::new()),
        };
        self.offset += complete as u64;
        let text = String::from_utf8_lossy(&buf[..complete]);
        Ok(text.lines().filter_map(parse_perf_map_line).collect())
    }
}

fn parse_hex(s: &str) -> Option<u64> {
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u64::from_str_radix(s, 16).ok()
}

/// `START SIZE NAME` -> `(start, size, name)`.
pub(crate) fn parse_perf_map_line(line: &str) -> Option<(u64, u64, String)> {
    let line = line.trim_start();
    let (start, rest) = line.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    let (size, name) = rest.split_once(char::is_whitespace)?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some((parse_hex(start)?, parse_hex(size)?, name.to_owned()))
}

/// Everything the drain loop needs for JIT symbolication: the regions,
/// and the two symbol sources it polls.
pub(crate) struct JitTracker {
    pub images: JitImages,
    perf_map: PerfMapTailer,
    jitdump_path: PathBuf,
    jitdump: Option<JitdumpTailer>,
    next_poll: Instant,
}

impl JitTracker {
    pub fn new(pid: u32) -> Self {
        Self::with_paths(
            PathBuf::from(format!("/tmp/perf-{pid}.map")),
            PathBuf::from(format!("/tmp/jit-{pid}.dump")),
        )
    }

    pub fn with_paths(perf_map: PathBuf, jitdump: PathBuf) -> Self {
        Self {
            images: JitImages::default(),
            perf_map: PerfMapTailer::new(perf_map),
            jitdump_path: jitdump,
            jitdump: None,
            next_poll: Instant::now(),
        }
    }

    /// Read new symbols, rate-limited unless `force`. Returns `true` when
    /// some symbol is still without a region, so the caller can re-scan
    /// `/proc/<pid>/maps` (and then call [`JitImages::adopt_orphans`]).
    pub fn poll(&mut self, force: bool) -> bool {
        let now = Instant::now();
        if !force && now < self.next_poll {
            return false;
        }
        self.next_poll = now + POLL_PERIOD;

        match self.perf_map.tick() {
            Ok(entries) => {
                if !entries.is_empty() {
                    debug!(
                        path = %self.perf_map.path().display(),
                        new = entries.len(),
                        "perf map grew"
                    );
                }
                for (start, size, name) in entries {
                    self.images.add_symbol(start, size, &name);
                }
            }
            Err(e) => warn!(path = %self.perf_map.path().display(), %e, "reading perf map"),
        }

        if self.jitdump.is_none() && self.jitdump_path.exists() {
            match JitdumpTailer::open(&self.jitdump_path) {
                Ok(t) => {
                    info!(path = %self.jitdump_path.display(), "jitdump found; tailing JIT symbols");
                    self.jitdump = Some(t);
                }
                Err(e) => warn!(path = %self.jitdump_path.display(), %e, "opening jitdump"),
            }
        }
        if let Some(t) = self.jitdump.as_mut() {
            match t.tick() {
                Ok(loads) => {
                    for r in loads {
                        self.images.add_symbol(r.avma, r.code_size, &r.name);
                    }
                }
                Err(e) => warn!(path = %self.jitdump_path.display(), %e, "reading jitdump"),
            }
        }
        self.images.has_orphans()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn names(img: &JitImage) -> Vec<String> {
        img.symbols
            .iter()
            .map(|s| String::from_utf8(s.name.clone()).unwrap())
            .collect()
    }

    #[test]
    fn parses_perf_map_lines() {
        assert_eq!(
            parse_perf_map_line("7f0012340000 40 vix::cargo::plan_of"),
            Some((0x7f00_1234_0000, 0x40, "vix::cargo::plan_of".into()))
        );
        assert_eq!(
            parse_perf_map_line("0x1000 0x10 name with spaces (closure #1)"),
            Some((0x1000, 0x10, "name with spaces (closure #1)".into()))
        );
        assert_eq!(parse_perf_map_line("zz 10 bad"), None);
        assert_eq!(parse_perf_map_line("1000 10"), None);
        assert_eq!(parse_perf_map_line(""), None);
    }

    #[test]
    fn mappings_never_overlap_or_move() {
        let mut j = JitImages::default();
        assert_eq!(j.add_mapping(0x2000, 0x3000), 1);
        // A merged VMA reported after mprotect: covers the old one and
        // more on both sides. Only the uncovered parts become regions.
        assert_eq!(j.add_mapping(0x1000, 0x5000), 2);
        let mut regions: Vec<_> = j.take_dirty().iter().map(|i| (i.start, i.end)).collect();
        regions.sort();
        assert_eq!(
            regions,
            vec![(0x1000, 0x2000), (0x2000, 0x3000), (0x3000, 0x5000)]
        );
        // Fully covered: nothing new.
        assert_eq!(j.add_mapping(0x1800, 0x4000), 0);
        assert!(j.take_dirty().is_empty());
    }

    #[test]
    fn symbols_republish_their_region() {
        let mut j = JitImages::default();
        j.add_mapping(0x1000, 0x2000);
        assert_eq!(j.take_dirty().len(), 1);
        j.add_symbol(0x1000, 0x20, "vix::a");
        j.add_symbol(0x1040, 0x20, "vix::b");
        let imgs = j.take_dirty();
        assert_eq!(imgs.len(), 1);
        assert_eq!((imgs[0].start, imgs[0].end), (0x1000, 0x2000));
        assert_eq!(names(&imgs[0]), ["vix::a", "vix::b"]);
        // Same entry again: no churn.
        j.add_symbol(0x1040, 0x20, "vix::b");
        assert!(j.take_dirty().is_empty());
    }

    #[test]
    fn orphan_symbols_wait_for_a_mapping_or_get_their_own() {
        let mut j = JitImages::default();
        j.add_symbol(0x5000, 0x10, "vix::early");
        assert!(j.has_orphans());
        assert!(j.take_dirty().is_empty());
        // The mapping shows up later (e.g. from a /proc rescan).
        j.add_mapping(0x5000, 0x6000);
        assert!(!j.has_orphans());
        let imgs = j.take_dirty();
        assert_eq!(names(&imgs[0]), ["vix::early"]);

        j.add_symbol(0x9000, 0x10, "vix::nowhere");
        assert!(j.has_orphans());
        j.adopt_orphans();
        let imgs = j.take_dirty();
        assert_eq!(imgs.len(), 1);
        assert_eq!((imgs[0].start, imgs[0].end), (0x9000, 0x9010));
        assert_eq!(names(&imgs[0]), ["vix::nowhere"]);
    }

    /// A synthetic perf map written in pieces, the way a JIT appends to
    /// it, including a line split across two writes.
    #[test]
    fn tails_a_growing_perf_map() {
        let dir = std::env::temp_dir().join(format!("stax-jit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let map = dir.join("perf-1.map");
        let _ = std::fs::remove_file(&map);
        let mut t = JitTracker::with_paths(map.clone(), dir.join("jit-1.dump"));
        t.images.add_mapping(0x7000_0000, 0x7001_0000);
        t.images.take_dirty();

        // Not there yet.
        assert!(!t.poll(true));

        let mut f = std::fs::File::create(&map).unwrap();
        write!(f, "70000000 100 vix::first\n70000100 80 vix::sec").unwrap();
        f.flush().unwrap();
        t.poll(true);
        let imgs = t.images.take_dirty();
        assert_eq!(names(&imgs[0]), ["vix::first"]);

        write!(f, "ond\n70000200 40 vix::third\n").unwrap();
        f.flush().unwrap();
        t.poll(true);
        let imgs = t.images.take_dirty();
        assert_eq!(imgs.len(), 1);
        assert_eq!(names(&imgs[0]), ["vix::first", "vix::second", "vix::third"]);
        assert_eq!(imgs[0].symbols[1].start_svma, 0x7000_0100);
        assert_eq!(imgs[0].symbols[1].end_svma, 0x7000_0180);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The jitdump path goes through the shared macOS tailer.
    #[test]
    fn tails_a_jitdump() {
        let dir = std::env::temp_dir().join(format!("stax-jitdump-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dump = dir.join("jit-1.dump");
        let mut bytes = Vec::new();
        // 40-byte header: magic, version, size, elf_mach, pad, pid, ts, flags.
        bytes.extend_from_slice(&0x4A69_5444u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(&62u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        // CODE_LOAD record.
        let name = b"vix::jitted\0";
        let code = [0x90u8; 16];
        let total = 16 + 40 + name.len() + code.len();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(total as u32).to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes()); // pid
        bytes.extend_from_slice(&1u32.to_le_bytes()); // tid
        bytes.extend_from_slice(&0x8000_0000u64.to_le_bytes()); // vma
        bytes.extend_from_slice(&0x8000_0000u64.to_le_bytes()); // code_addr
        bytes.extend_from_slice(&16u64.to_le_bytes()); // code_size
        bytes.extend_from_slice(&0u64.to_le_bytes()); // code_index
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&code);
        std::fs::write(&dump, &bytes).unwrap();

        let mut t = JitTracker::with_paths(dir.join("perf-1.map"), dump);
        assert!(t.poll(true), "no mapping known yet: orphan");
        t.images.adopt_orphans();
        let imgs = t.images.take_dirty();
        assert_eq!(names(&imgs[0]), ["vix::jitted"]);
        assert_eq!((imgs[0].start, imgs[0].end), (0x8000_0000, 0x8000_0010));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
