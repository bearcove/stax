//! The drain loop: open a sampling ring per CPU, poll, parse
//! `PERF_RECORD_*`, and drive the `SampleSink` with the same event
//! sequence the macOS backend emits.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use stax_mac_capture::{
    BinaryLoadedEvent, CpuIntervalEvent, CpuIntervalKind, SampleEvent, SampleSink, ThreadNameEvent,
};
use tracing::{debug, info, warn};

use crate::sys::{
    PERF_CONTEXT_KERNEL, PERF_CONTEXT_MAX, PERF_CONTEXT_USER, PerfRing, PmuGroup, PmuKind,
    online_cpus, open_cpu, open_cpu_pmu_siblings, open_cpu_switch,
};
use crate::{RecordOptions, RecordSummary};

// perf_event.h record types we handle.
const PERF_RECORD_MMAP: u32 = 1;
const PERF_RECORD_LOST: u32 = 2;
const PERF_RECORD_COMM: u32 = 3;
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_RECORD_MMAP2: u32 = 10;
/// Context-switch record emitted by a cpu-wide event with
/// `context_switch` set (we open per-CPU/system-wide, so it's the
/// CPU_WIDE variant, which carries the *other* side's pid/tid).
const PERF_RECORD_SWITCH_CPU_WIDE: u32 = 15;
const PERF_RECORD_MISC_MMAP_BUILD_ID: u16 = 1 << 14;
/// Set on a SWITCH record when the thread is leaving the CPU (vs being
/// scheduled in).
const PERF_RECORD_MISC_SWITCH_OUT: u16 = 1 << 13;
/// Set on a switch-out when it was an involuntary preemption (the
/// thread stayed runnable) rather than a voluntary block/sleep.
const PERF_RECORD_MISC_SWITCH_OUT_PREEMPT: u16 = 1 << 14;

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
    /// Last sample timestamp seen per tid. Each new sample closes an
    /// on-CPU interval `[prev, now)` for that thread (see `on_sample`).
    last_sample_ts: HashMap<u32, u64>,
    /// Cap on a synthesized on-CPU interval. A gap longer than this
    /// between two samples of a thread means the thread was almost
    /// certainly off-CPU (perf doesn't sample parked threads), so we
    /// only credit one capped slice rather than fabricating seconds of
    /// on-CPU time. The matching real off-CPU span comes from the
    /// context-switch ring (see `on_switch`).
    max_on_cpu_gap_ns: u64,
    /// Last user stack seen per tid (leaf-first). When a thread blocks
    /// (voluntary switch-out) this is its "parked" stack, credited to
    /// the off-CPU interval — same model as the macOS backend.
    last_user_stack: HashMap<u32, Box<[u64]>>,
    /// Threads currently off-CPU: tid -> (switch-out ns, parked stack).
    /// Closed into an off-CPU interval on the matching switch-in.
    off_start: HashMap<u32, (u64, Box<[u64]>)>,
    /// Kernel symbol name -> address, for turning a blocked thread's
    /// `/proc/.../wchan` into the off-CPU leaf (the block reason).
    kallsyms: HashMap<String, u64>,
    /// Newest timestamp seen on any ring; bounds still-blocked threads
    /// at teardown.
    last_ts: u64,
    /// `perf-event-id → counter kind` for the PMU sibling group. Empty
    /// when no PMU group is attached (the daemon-broker path today) —
    /// the SampleEvent contract documents 0 as "not available".
    pmu_id_to_kind: HashMap<u64, PmuKind>,
    /// Per-CPU running PMU counts at the last sample seen on that CPU.
    /// Per-sample deltas are `cur - prev[cpu][k]`, then `prev = cur`.
    /// Indexed by [`PmuKind::index`].
    prev_pmu_per_cpu: HashMap<u32, [u64; 4]>,
    summary: RecordSummary,
}

impl Session<'_> {
    fn handle(&mut self, ty: u32, misc: u16, body: &[u8]) {
        match ty {
            PERF_RECORD_SAMPLE => self.on_sample(body),
            PERF_RECORD_MMAP2 => self.on_mmap2(misc, body),
            PERF_RECORD_MMAP => self.on_mmap(body),
            PERF_RECORD_COMM => self.on_comm(body),
            PERF_RECORD_SWITCH_CPU_WIDE => self.on_switch(misc, body),
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
        // sample_type = TID | TIME | CPU | READ | CALLCHAIN. The kernel
        // emits fields in ascending bit order with CPU before READ.
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
        let cpu = c.u32().unwrap_or(0);
        let _res = c.u32();

        // PERF_SAMPLE_READ block with PERF_FORMAT_GROUP | PERF_FORMAT_ID:
        // u64 nr_values; { u64 value; u64 id }[nr_values]
        // `nr_values == 1` on the daemon-brokered path (leader only),
        // `nr_values == 5` when [`open_cpu_pmu_siblings`] attached the
        // 4-counter HW group. Unknown ids (the leader's SW_CPU_CLOCK,
        // any sibling we didn't open) just get ignored.
        let mut pmu_deltas = [0u64; 4];
        let nr_pmu = c.u64().unwrap_or(0);
        for _ in 0..nr_pmu {
            let value = match c.u64() {
                Some(v) => v,
                None => return,
            };
            let id = match c.u64() {
                Some(v) => v,
                None => return,
            };
            if let Some(&kind) = self.pmu_id_to_kind.get(&id) {
                let idx = kind.index();
                let prev_slot = self.prev_pmu_per_cpu.entry(cpu).or_insert([0; 4]);
                let prev = prev_slot[idx];
                prev_slot[idx] = value;
                pmu_deltas[idx] = value.saturating_sub(prev);
            }
        }

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
            // Deltas from the per-CPU running counters in the
            // PERF_SAMPLE_READ block; 0 when no PMU group is attached
            // or the host couldn't open this counter.
            cycles: pmu_deltas[PmuKind::Cycles.index()],
            instructions: pmu_deltas[PmuKind::Instructions.index()],
            l1d_misses: pmu_deltas[PmuKind::L1dMisses.index()],
            branch_mispreds: pmu_deltas[PmuKind::BranchMisses.index()],
        });

        // Synthesize the on-CPU interval the aggregator needs for time
        // attribution. macOS gets ground-truth slices from MACH_SCHED;
        // a perf frequency profiler instead lets each sample own the
        // CPU time until the thread's next sample. We close the
        // *previous* sample's interval `[prev, now)` now that we know
        // when it ended, capping the duration so a long gap (thread was
        // parked, perf doesn't sample parked threads) isn't mis-booked
        // as on-CPU. The aggregator finds exactly the prev sample
        // inside `[prev, prev+dur)` (end exclusive) and credits it the
        // whole slice. Out-of-order arrivals across per-CPU rings are
        // tolerated by only advancing on a strictly newer timestamp,
        // keeping per-thread interval starts monotonic.
        if time != 0 {
            let prev = self.last_sample_ts.get(&tid).copied();
            if let Some(prev) = prev {
                if time > prev {
                    let dur = (time - prev).min(self.max_on_cpu_gap_ns);
                    if dur > 0 {
                        self.sink.on_cpu_interval(CpuIntervalEvent {
                            pid,
                            tid,
                            start_ns: prev,
                            end_ns: prev + dur,
                            kind: CpuIntervalKind::OnCpu,
                        });
                        self.summary.intervals = self.summary.intervals.saturating_add(1);
                    }
                }
            }
            if prev.is_none_or(|p| time > p) {
                self.last_sample_ts.insert(tid, time);
            }
            self.last_ts = self.last_ts.max(time);
        }

        // Remember this thread's most recent *user* stack so a later
        // voluntary switch-out can credit the off-CPU span to where the
        // thread was when it parked (matches the macOS "cached PET
        // stack" model). Skip in-kernel samples with no user frames so
        // we don't clobber a good stack with an empty one.
        if !user.is_empty() {
            self.last_user_stack
                .insert(tid, user.into_boxed_slice());
        }
    }

    /// A `PERF_RECORD_SWITCH_CPU_WIDE`: the kernel telling us a thread
    /// left or entered the CPU. A *voluntary* switch-out (not a
    /// preemption) opens an off-CPU span with the thread's parked
    /// stack; the matching switch-in closes it with the real scheduler
    /// duration. The trailing `sample_id` (sample_id_all=1, sample_type
    /// TID|TIME|CPU) identifies the task this record is *about* (the
    /// outgoing task on switch-out, the incoming one on switch-in).
    fn on_switch(&mut self, misc: u16, body: &[u8]) {
        let mut c = Cur::new(body);
        // CPU_WIDE body: next_prev_pid/tid (the *other* side) ...
        let _next_prev_pid = c.u32();
        let _next_prev_tid = c.u32();
        // ... then the sample_id trailer in sample_type order.
        let sid_pid = c.u32().unwrap_or(0);
        let sid_tid = c.u32().unwrap_or(0);
        let time = c.u64().unwrap_or(0);
        if time != 0 {
            self.last_ts = self.last_ts.max(time);
        }
        if sid_pid != self.opts.pid {
            return; // system-wide ring; keep only the target.
        }
        if misc & PERF_RECORD_MISC_SWITCH_OUT != 0 {
            // Preemption leaves the thread runnable — that's not the
            // blocking off-CPU we attribute (it's CPU contention, and
            // counting it would double-book against on-CPU).
            if misc & PERF_RECORD_MISC_SWITCH_OUT_PREEMPT != 0 {
                return;
            }
            // Lead the off-CPU stack with the kernel wait site (from
            // wchan) so the aggregator classifies the reason and the
            // off-CPU flame shows `<wait fn> -> <user call path>`.
            // Falls back to just the parked user stack when wchan
            // isn't resolvable.
            let parked = self.last_user_stack.get(&sid_tid);
            let stack: Box<[u64]> = match wchan_addr(self.opts.pid, sid_tid, &self.kallsyms) {
                Some(waddr) => {
                    let mut v = Vec::with_capacity(1 + parked.map_or(0, |s| s.len()));
                    v.push(waddr);
                    if let Some(s) = parked {
                        v.extend_from_slice(s);
                    }
                    v.into_boxed_slice()
                }
                None => parked.cloned().unwrap_or_default(),
            };
            self.off_start.insert(sid_tid, (time, stack));
        } else if let Some((start, stack)) = self.off_start.remove(&sid_tid) {
            if time > start {
                self.sink.on_cpu_interval(CpuIntervalEvent {
                    pid: self.opts.pid,
                    tid: sid_tid,
                    start_ns: start,
                    end_ns: time,
                    kind: CpuIntervalKind::OffCpu {
                        stack: &stack,
                        waker_tid: None,
                        waker_user_stack: None,
                    },
                });
                self.summary.off_cpu_intervals =
                    self.summary.off_cpu_intervals.saturating_add(1);
            }
        }
    }

    /// At teardown, close out threads still parked: emit their open
    /// off-CPU span ending at the last timestamp we saw, so a snapshot
    /// taken right after stop still reflects idle worker pools etc.
    fn flush_off_cpu(&mut self) {
        let end = self.last_ts;
        let pid = self.opts.pid;
        for (tid, (start, stack)) in std::mem::take(&mut self.off_start) {
            if end > start {
                self.sink.on_cpu_interval(CpuIntervalEvent {
                    pid,
                    tid,
                    start_ns: start,
                    end_ns: end,
                    kind: CpuIntervalKind::OffCpu {
                        stack: &stack,
                        waker_tid: None,
                        waker_user_stack: None,
                    },
                });
                self.summary.off_cpu_intervals =
                    self.summary.off_cpu_intervals.saturating_add(1);
            }
        }
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

/// Parse `/proc/kallsyms` into a kernel-text name -> address map.
/// Only `t`/`T`/`w`/`W` symbols in the kernel half are kept — that's
/// the universe `/proc/<tid>/wchan` reports.
fn parse_kallsyms_names(bytes: &[u8]) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    let Ok(text) = std::str::from_utf8(bytes) else {
        return map;
    };
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(addr), Some(kind), Some(name)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if !matches!(kind, "t" | "T" | "w" | "W") {
            continue;
        }
        if let Ok(a) = u64::from_str_radix(addr, 16) {
            if a >= 0xffff_0000_0000_0000 {
                map.entry(name.to_owned()).or_insert(a);
            }
        }
    }
    map
}

/// Resolve a thread's current kernel wait site via
/// `/proc/<pid>/task/<tid>/wchan` (a symbol name) into a kernel
/// address, so it can lead the off-CPU stack. World-readable and
/// symbolic when `kptr_restrict` allows; `"0"`/empty means "not
/// resolvably blocked" -> caller falls back to the parked user stack.
fn wchan_addr(pid: u32, tid: u32, kallsyms: &HashMap<String, u64>) -> Option<u64> {
    let path = format!("/proc/{pid}/task/{tid}/wchan");
    let s = std::fs::read_to_string(path).ok()?;
    let name = s.trim();
    if name.is_empty() || name == "0" {
        return None;
    }
    kallsyms.get(name).copied()
}

/// Drain one ring and dispatch every complete record in it. Shared by
/// the steady-state loop and the post-stop final sweep, across both the
/// sampling and context-switch ring sets.
fn drain_dispatch(r: &mut PerfRing, scratch: &mut Vec<u8>, sess: &mut Session) {
    scratch.clear();
    r.drain(scratch);
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

/// In-process path: open the per-CPU rings here (needs the host's
/// `perf_event_paranoid` to permit it) and drive them.
pub fn run(
    opts: &RecordOptions,
    sink: &mut dyn SampleSink,
    should_stop: &AtomicBool,
) -> eyre::Result<RecordSummary> {
    let cpus = online_cpus();
    let mut rings: Vec<PerfRing> = Vec::with_capacity(cpus.len());
    let mut pmu = PmuGroup::default();
    for cpu in &cpus {
        let leader = match open_cpu(*cpu, opts.frequency_hz, opts.kernel_stacks) {
            Ok(r) => r,
            Err(e) => {
                return Err(eyre::eyre!(
                    "perf_event_open on cpu {cpu} failed: {e} \
                     (need perf_event_paranoid low enough, or the daemon)"
                ));
            }
        };
        // Best-effort: a host without enough PMU counters (PMU
        // contention, virtualised, locked-down) just loses the
        // counters — samples/callchains still flow.
        match open_cpu_pmu_siblings(*cpu, leader.fd, opts.kernel_stacks) {
            Ok(siblings) => {
                for m in &siblings {
                    pmu.id_to_kind.insert(m.id, m.kind);
                }
                pmu.siblings.push(siblings);
            }
            Err(e) => {
                warn!(
                    %e,
                    cpu = *cpu,
                    "PMU counter group unavailable; cycles/instructions/etc. left at 0"
                );
            }
        }
        rings.push(leader);
    }
    // Side-band context-switch rings for off-CPU attribution. Best
    // effort: if these fail (older kernel without `context_switch`,
    // stricter paranoid), we still get the on-CPU profile — just no
    // off-CPU breakdown — rather than failing the whole recording.
    let mut switch_rings: Vec<PerfRing> = Vec::with_capacity(cpus.len());
    for cpu in &cpus {
        match open_cpu_switch(*cpu) {
            Ok(r) => switch_rings.push(r),
            Err(e) => {
                warn!(%e, "context-switch ring open failed; off-CPU disabled");
                switch_rings.clear();
                break;
            }
        }
    }
    run_with_rings(opts, sink, should_stop, rings, switch_rings, pmu)
}

/// Daemon path: the privileged staxd already did `perf_event_open` per
/// CPU and handed us the fds (mapped into [`PerfRing`]s via
/// [`crate::sys::ring_from_fd`]). Everything from here on —
/// `/proc/kallsyms`, the `/proc/<pid>` synthesis, enabling the events,
/// the poll/drain/parse loop — is unprivileged and identical to the
/// in-process path, so both share [`run_with_rings`].
pub fn run_with_rings(
    opts: &RecordOptions,
    sink: &mut dyn SampleSink,
    should_stop: &AtomicBool,
    mut rings: Vec<PerfRing>,
    mut switch_rings: Vec<PerfRing>,
    pmu: PmuGroup,
) -> eyre::Result<RecordSummary> {
    // Bind the PMU sibling fds for the lifetime of this function: the
    // kernel removes a sibling from its group the moment its fd
    // closes. Empty Vec on the daemon-broker path (no group attached
    // there yet) — PMU fields then stay 0, matching the contract.
    let _pmu_siblings = pmu.siblings;
    let pmu_id_to_kind = pmu.id_to_kind;
    let start = Instant::now();

    // Kernel symbols up front, same as the kperf backend, so the
    // analysis side can resolve kernel_backtrace addresses. We also
    // keep a name->addr map so a blocked thread's `/proc/.../wchan`
    // (a kernel symbol name) can be turned into an address and used
    // as the off-CPU leaf — that's what makes the reason classifier
    // (futex/pipe/poll/...) bite on Linux.
    let kallsyms = match std::fs::read("/proc/kallsyms") {
        Ok(k) => {
            sink.on_kallsyms(&k);
            parse_kallsyms_names(&k)
        }
        Err(e) => {
            warn!(%e, "could not read /proc/kallsyms; kernel frames stay raw");
            HashMap::new()
        }
    };

    for r in rings.iter().chain(switch_rings.iter()) {
        r.enable()?;
    }
    info!(
        pid = opts.pid,
        freq_hz = opts.frequency_hz,
        cpus = rings.len(),
        off_cpu = !switch_rings.is_empty(),
        "linux perf capture started"
    );

    let mut pollfds: Vec<libc::pollfd> = rings
        .iter()
        .chain(switch_rings.iter())
        .map(|r| libc::pollfd {
            fd: r.fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();

    let nominal_period_ns = if opts.frequency_hz == 0 {
        1_000_000 // 1ms fallback; freq is validated upstream but be safe.
    } else {
        1_000_000_000 / opts.frequency_hz as u64
    };
    let mut sess = Session {
        opts: opts.clone(),
        sink,
        images: ImageRegistry::default(),
        thread_names: HashMap::new(),
        last_sample_ts: HashMap::new(),
        // Allow a few missed samples (jitter, brief preemption) before
        // declaring the thread was off-CPU for the gap.
        max_on_cpu_gap_ns: nominal_period_ns.saturating_mul(4),
        last_user_stack: HashMap::new(),
        off_start: HashMap::new(),
        kallsyms,
        last_ts: 0,
        pmu_id_to_kind,
        prev_pmu_per_cpu: HashMap::new(),
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
        for r in rings.iter_mut().chain(switch_rings.iter_mut()) {
            drain_dispatch(r, &mut scratch, &mut sess);
        }
    }

    for r in rings.iter().chain(switch_rings.iter()) {
        r.disable();
    }
    // Final sweep so the tail of the recording isn't lost.
    for r in rings.iter_mut().chain(switch_rings.iter_mut()) {
        drain_dispatch(r, &mut scratch, &mut sess);
    }
    // Close out threads still parked at stop time.
    sess.flush_off_cpu();

    sess.summary.session_ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let summary = sess.summary;
    info!(
        samples = summary.samples,
        binaries = summary.binaries,
        intervals = summary.intervals,
        off_cpu_intervals = summary.off_cpu_intervals,
        lost = summary.lost_records,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "linux perf capture finished"
    );
    Ok(summary)
}
