//! Initial-state snapshot from `/proc/<pid>/`.
//!
//! `perf_event_open` only reports `MMAP2`/`COMM` for events that happen
//! *after* the ring is live. A process that was already running has its
//! executable and shared libraries mapped, and its threads named,
//! before we attach — so the kernel never tells us about them. `perf
//! record` and samply both work around this by synthesizing the
//! existing state from `/proc`; we do the same here.

/// One pre-existing executable file mapping: same shape the `MMAP2`
/// handler consumes (`base_avma`, `vmsize`, `pgoff`, `path`).
pub struct MapRegion {
    pub base_avma: u64,
    pub vmsize: u64,
    pub pgoff: u64,
    pub path: String,
}

/// Executable file-backed regions of `pid`, oldest mapping first.
pub fn maps(pid: u32) -> Vec<MapRegion> {
    let text = match std::fs::read_to_string(format!("/proc/{pid}/maps")) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        // ADDR-ADDR perms offset dev inode pathname
        let mut it = line.split_whitespace();
        let range = match it.next() {
            Some(r) => r,
            None => continue,
        };
        let perms = match it.next() {
            Some(p) => p,
            None => continue,
        };
        if !perms.as_bytes().get(2).map(|&c| c == b'x').unwrap_or(false) {
            continue; // not executable
        }
        let offset = it.next().unwrap_or("0");
        let _dev = it.next();
        let _inode = it.next();
        let path = it.collect::<Vec<_>>().join(" ");
        if path.is_empty() || path.starts_with('[') || path == "//anon" {
            continue; // anon / [vdso] / [heap] etc. — no on-disk ELF
        }
        let (s, e) = match range.split_once('-') {
            Some((s, e)) => (s, e),
            None => continue,
        };
        let (start, end, pgoff) = match (
            u64::from_str_radix(s, 16),
            u64::from_str_radix(e, 16),
            u64::from_str_radix(offset, 16),
        ) {
            (Ok(a), Ok(b), Ok(o)) if b > a => (a, b, o),
            _ => continue,
        };
        out.push(MapRegion {
            base_avma: start,
            vmsize: end - start,
            pgoff,
            path,
        });
    }
    out
}

/// `(tid, comm)` for every thread of `pid`.
pub fn threads(pid: u32) -> Vec<(u32, String)> {
    let dir = format!("/proc/{pid}/task");
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for ent in entries.flatten() {
        let tid: u32 = match ent.file_name().to_string_lossy().parse() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let comm = std::fs::read_to_string(format!("{dir}/{tid}/comm"))
            .unwrap_or_default()
            .trim_end()
            .to_string();
        if !comm.is_empty() {
            out.push((tid, comm));
        }
    }
    out
}
