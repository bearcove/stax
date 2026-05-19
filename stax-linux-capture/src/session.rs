//! The drain loop: open a sampling ring per CPU, poll, parse
//! `PERF_RECORD_*`, and drive the `SampleSink` with the same event
//! sequence the macOS backend emits.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use stax_mac_capture::{
    BinaryLoadedEvent, SampleEvent, SampleSink, ThreadNameEvent,
};
use tracing::{debug, info, warn};

use crate::sys::{
    PERF_CONTEXT_KERNEL, PERF_CONTEXT_MAX, PERF_CONTEXT_USER, PerfRing, online_cpus, open_cpu,
};
use crate::{RecordOptions, RecordSummary};

// perf_event.h record types we handle.
const PERF_RECORD_MMAP: u32 = 1;
const PERF_RECORD_LOST: u32 = 2;
const PERF_RECORD_COMM: u32 = 3;
const PERF_RECORD_MMAP2: u32 = 10;
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_RECORD_MISC_MMAP_BUILD_ID: u16 = 1 << 14;

/// Little-endian cursor over a single record's bytes (perf records are
/// native-endian; every Linux target stax runs on is LE).
struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn u32(&mut self) -> Option<u32> {
        let e = self.p + 4;
        let v = u32::from_le_bytes(self.b.get(self.p..e)?.try_into().ok()?);
        self.p = e;
        Some(v)
    }
    fn u64(&mut self) -> Option<u64> {
        let e = self.p + 8;
        let v = u64::from_le_bytes(self.b.get(self.p..e)?.try_into().ok()?);
        self.p = e;
        Some(v)
    }
    fn skip(&mut self, n: usize) {
        self.p += n;
    }
    fn cstr(&mut self) -> &'a [u8] {
        let rest = &self.b[self.p.min(self.b.len())..];
        let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
        &rest[..end]
    }
}

#[derive(Default)]
struct ImageRegistry {
    /// Mappings we've already announced, keyed by runtime base addr.
    seen: HashMap<u64, ()>,
}

struct Session<'s> {
    opts: RecordOptions,
    sink: &'s mut dyn SampleSink,
    images: ImageRegistry,
    thread_names: HashMap<u32, String>,
    summary: RecordSummary,
}

impl Session<'_> {
    fn handle(&mut self, ty: u32, misc: u16, body: &[u8]) {
        match ty {
            PERF_RECORD_SAMPLE => self.on_sample(body),
            PERF_RECORD_MMAP2 => self.on_mmap2(misc, body),
            PERF_RECORD_MMAP => self.on_mmap(body),
            PERF_RECORD_COMM => self.on_comm(body),
            PERF_RECORD_LOST => {
                let mut c = Cur::new(body);
                let _id = c.u64();
                if let Some(lost) = c.u64() {
                    self.summary.lost_records = self.summary.lost_records.saturating_add(lost);
                }
            }
            _ => {}
        }
    }

    fn on_sample(&mut self, body: &[u8]) {
        // sample_type = TID | TIME | CPU | CALLCHAIN (fixed order).
        let mut c = Cur::new(body);
        let pid = match c.u32() {
            Some(v) => v,
            None => return,
        };
        let tid = c.u32().unwrap_or(0);
        if pid != self.opts.pid {
            return; // system-wide ring; keep only the target.
        }
        let time = c.u64().unwrap_or(0);
        let _cpu = c.u32();
        let _res = c.u32();
        let nr = match c.u64() {
            Some(n) => n,
            None => return,
        };
        let mut user: Vec<u64> = Vec::new();
        let mut kernel: Vec<u64> = Vec::new();
        // Default region: kernel callchains begin with a
        // PERF_CONTEXT_KERNEL marker; anything before the first marker
        // (rare) is treated as user.
        let mut in_kernel = false;
        for _ in 0..nr {
            let ip = match c.u64() {
                Some(v) => v,
                None => break,
            };
            if ip >= PERF_CONTEXT_MAX {
                // Reserved marker, not an address.
                if ip == PERF_CONTEXT_KERNEL {
                    in_kernel = true;
                } else if ip == PERF_CONTEXT_USER {
                    in_kernel = false;
                }
                continue;
            }
            if in_kernel {
                kernel.push(ip);
            } else {
                user.push(ip);
            }
        }
        self.summary.samples = self.summary.samples.saturating_add(1);
        self.sink.on_sample(SampleEvent {
            timestamp_ns: time,
            pid,
            tid,
            backtrace: &user,
            kernel_backtrace: &kernel,
            // PMU counters are a later sub-phase; the SampleEvent
            // contract documents 0 as "not available (Linux backend)".
            cycles: 0,
            instructions: 0,
            l1d_misses: 0,
            branch_mispreds: 0,
        });
    }

    fn on_mmap2(&mut self, misc: u16, body: &[u8]) {
        let mut c = Cur::new(body);
        let pid = c.u32().unwrap_or(0);
        let _tid = c.u32();
        if pid != self.opts.pid {
            return;
        }
        let addr = match c.u64() {
            Some(v) => v,
            None => return,
        };
        let len = c.u64().unwrap_or(0);
        let pgoff = c.u64().unwrap_or(0);
        // Either {maj,min,ino,ino_generation} (24B) or the build-id
        // variant (also 24B) depending on misc.
        let _ = misc & PERF_RECORD_MISC_MMAP_BUILD_ID;
        c.skip(24);
        let prot = c.u32().unwrap_or(0);
        let _flags = c.u32();
        let path = c.cstr();
        // Only executable file mappings become images.
        const PROT_EXEC: u32 = 0x4;
        if prot & PROT_EXEC == 0 {
            return;
        }
        let path = match std::str::from_utf8(path) {
            Ok(p) => p,
            Err(_) => return,
        };
        if path.is_empty() || path.starts_with('[') || path == "//anon" {
            return; // anonymous / JIT / vdso: no on-disk ELF.
        }
        if self.images.seen.contains_key(&addr) {
            return;
        }
        self.images.seen.insert(addr, ());
        self.emit_image(addr, len, pgoff, path);
    }

    // Legacy PERF_RECORD_MMAP (no maj/min/prot) — some kernels still
    // emit it for the executable; treat it as an exec mapping.
    fn on_mmap(&mut self, body: &[u8]) {
        let mut c = Cur::new(body);
        let pid = c.u32().unwrap_or(0);
        let _tid = c.u32();
        if pid != self.opts.pid {
            return;
        }
        let addr = match c.u64() {
            Some(v) => v,
            None => return,
        };
        let len = c.u64().unwrap_or(0);
        let pgoff = c.u64().unwrap_or(0);
        let path = c.cstr();
        let path = match std::str::from_utf8(path) {
            Ok(p) => p,
            Err(_) => return,
        };
        if path.is_empty() || path.starts_with('[') {
            return;
        }
        if self.images.seen.contains_key(&addr) {
            return;
        }
        self.images.seen.insert(addr, ());
        self.emit_image(addr, len, pgoff, path);
    }

    fn emit_image(&mut self, base_avma: u64, vmsize: u64, pgoff: u64, path: &str) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                debug!(path, %e, "image mapped but unreadable; skipping symbols");
                return;
            }
        };
        let img = match crate::elf::scan(&bytes) {
            Some(i) => i,
            None => return,
        };
        // The SVMA the linker assigned to the first byte of this
        // mapping; symbol addresses are in the same SVMA space, so the
        // analysis side recovers a static address as
        //   pc - base_avma + text_svma.
        let text_svma = img.svma_for_file_off(pgoff).unwrap_or_else(|| {
            // Shouldn't happen for a normal code mapping; keep a quiet
            // breadcrumb rather than silently mis-symbolizing.
            debug!(
                path,
                pgoff = format_args!("{pgoff:#x}"),
                loads = ?img.loads,
                "no PT_LOAD(x) covers pgoff; text_svma=0"
            );
            0
        });
        self.summary.binaries = self.summary.binaries.saturating_add(1);
        info!(
            path,
            base_avma = format_args!("{base_avma:#x}"),
            text_svma = format_args!("{text_svma:#x}"),
            syms = img.symbols.len(),
            "image loaded"
        );
        self.sink.on_binary_loaded(BinaryLoadedEvent {
            pid: self.opts.pid,
            base_avma,
            vmsize,
            text_svma,
            path,
            uuid: img.build_id,
            arch: img.arch,
            is_executable: img.is_executable,
            symbols: &img.symbols,
            text_bytes: None,
        });
    }

    fn on_comm(&mut self, body: &[u8]) {
        let mut c = Cur::new(body);
        let pid = c.u32().unwrap_or(0);
        let tid = c.u32().unwrap_or(0);
        if pid != self.opts.pid {
            return;
        }
        let name = String::from_utf8_lossy(c.cstr()).into_owned();
        if self.thread_names.get(&tid).map(|s| s.as_str()) == Some(name.as_str()) {
            return;
        }
        self.thread_names.insert(tid, name.clone());
        self.sink.on_thread_name(ThreadNameEvent {
            pid,
            tid,
            name: &name,
        });
    }
}

pub fn run(
    opts: &RecordOptions,
    sink: &mut dyn SampleSink,
    should_stop: &AtomicBool,
) -> eyre::Result<RecordSummary> {
    let start = Instant::now();

    // Kernel symbols up front, same as the kperf backend, so the
    // analysis side can resolve kernel_backtrace addresses.
    match std::fs::read("/proc/kallsyms") {
        Ok(k) => sink.on_kallsyms(&k),
        Err(e) => warn!(%e, "could not read /proc/kallsyms; kernel frames stay raw"),
    }

    let cpus = online_cpus();
    let mut rings: Vec<PerfRing> = Vec::with_capacity(cpus.len());
    for cpu in &cpus {
        match open_cpu(*cpu, opts.frequency_hz, opts.kernel_stacks) {
            Ok(r) => rings.push(r),
            Err(e) => {
                return Err(eyre::eyre!(
                    "perf_event_open on cpu {cpu} failed: {e} \
                     (need perf_event_paranoid low enough, or the daemon)"
                ));
            }
        }
    }
    for r in &rings {
        r.enable()?;
    }
    info!(
        pid = opts.pid,
        freq_hz = opts.frequency_hz,
        cpus = rings.len(),
        "linux perf capture started"
    );

    let mut pollfds: Vec<libc::pollfd> = rings
        .iter()
        .map(|r| libc::pollfd {
            fd: r.fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();

    let mut sess = Session {
        opts: opts.clone(),
        sink,
        images: ImageRegistry::default(),
        thread_names: HashMap::new(),
        summary: RecordSummary::default(),
    };

    // Synthesize the pre-existing state the kernel won't replay: every
    // executable mapping and thread name that predates our attach.
    for (tid, name) in crate::proc::threads(opts.pid) {
        if sess.thread_names.get(&tid).map(|s| s.as_str()) != Some(name.as_str()) {
            sess.thread_names.insert(tid, name.clone());
            sess.sink.on_thread_name(ThreadNameEvent {
                pid: opts.pid,
                tid,
                name: &name,
            });
        }
    }
    for m in crate::proc::maps(opts.pid) {
        if !sess.images.seen.contains_key(&m.base_avma) {
            sess.images.seen.insert(m.base_avma, ());
            sess.emit_image(m.base_avma, m.vmsize, m.pgoff, &m.path);
        }
    }

    let deadline = opts.duration.map(|d| start + d);
    let mut scratch: Vec<u8> = Vec::with_capacity(1 << 16);

    loop {
        if should_stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                break;
            }
        }
        // SAFETY: poll over our own valid pollfds for up to 100ms.
        unsafe {
            libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 100);
        }
        for r in &mut rings {
            scratch.clear();
            r.drain(&mut scratch);
            let mut off = 0usize;
            while off + 8 <= scratch.len() {
                let ty = u32::from_le_bytes(scratch[off..off + 4].try_into().unwrap());
                let misc = u16::from_le_bytes(scratch[off + 4..off + 6].try_into().unwrap());
                let size =
                    u16::from_le_bytes(scratch[off + 6..off + 8].try_into().unwrap()) as usize;
                if size < 8 || off + size > scratch.len() {
                    break;
                }
                sess.handle(ty, misc, &scratch[off + 8..off + size]);
                off += size;
            }
        }
    }

    for r in &rings {
        r.disable();
    }
    // Final sweep so the tail of the recording isn't lost.
    for r in &mut rings {
        scratch.clear();
        r.drain(&mut scratch);
        let mut off = 0usize;
        while off + 8 <= scratch.len() {
            let ty = u32::from_le_bytes(scratch[off..off + 4].try_into().unwrap());
            let misc = u16::from_le_bytes(scratch[off + 4..off + 6].try_into().unwrap());
            let size = u16::from_le_bytes(scratch[off + 6..off + 8].try_into().unwrap()) as usize;
            if size < 8 || off + size > scratch.len() {
                break;
            }
            sess.handle(ty, misc, &scratch[off + 8..off + size]);
            off += size;
        }
    }

    sess.summary.session_ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let summary = sess.summary;
    info!(
        samples = summary.samples,
        binaries = summary.binaries,
        lost = summary.lost_records,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "linux perf capture finished"
    );
    Ok(summary)
}
