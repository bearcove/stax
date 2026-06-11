//! The drain loop: open a sampling ring per CPU, poll, parse
//! `PERF_RECORD_*`, and drive the `SampleSink` with the same event
//! sequence the macOS backend emits.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use stax_mac_capture::{
    BinaryLoadedEvent, CpuIntervalEvent, CpuIntervalKind, SampleEvent, SampleSink, ThreadNameEvent,
    WakeupEvent,
};
use staxd_proto::WakingFieldOffsets;
use tracing::{debug, info, warn};

#[cfg(target_arch = "x86_64")]
use crate::sys::PERF_SAMPLE_REGS_ABI_64;
use crate::sys::{
    online_cpus, open_cpu, open_cpu_pmu_siblings, open_cpu_switch, open_cpu_waking,
    read_sched_waking_tracepoint, PerfRing, PerfRingKind, PmuGroup, PmuKind, PERF_CONTEXT_KERNEL,
    PERF_CONTEXT_MAX, PERF_CONTEXT_USER,
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
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let e = self.p + n;
        let s = self.b.get(self.p..e)?;
        self.p = e;
        Some(s)
    }
}

#[derive(Default)]
struct ImageRegistry {
    /// Mappings we've already announced, keyed by runtime base addr.
    seen: HashMap<u64, ()>,
}

/// A `sched:sched_waking` event we've seen but not yet matched against
/// a switch-in. Keyed in [`Session::recent_waking`] by the wakee tid.
struct RecentWaking {
    waker_tid: u32,
    waker_user_stack: Box<[u64]>,
    waker_kernel_stack: Box<[u64]>,
    time_ns: u64,
}

/// Drop a `RecentWaking` entry whose `sched_waking` event predates the
/// switch-in by more than this. Wakeups are normally consumed within
/// microseconds (try_to_wake_up -> the CPU running it picks the wakee
/// up almost immediately); a generous window covers loaded scheduler
/// latency without misattributing ancient unrelated wakeups.
const WAKING_STALENESS_NS: u64 = 100_000_000;

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
    /// Byte offsets of `sched:sched_waking`'s `pid` field inside its
    /// RAW payload, supplied by whoever opened the tracepoint (the
    /// daemon, or the local in-process attempt). `None` = no wakeup
    /// rings were attached, so `on_sample_from_waking` is never
    /// called and the off-CPU close path skips waker attribution.
    waking_offsets: Option<WakingFieldOffsets>,
    /// `wakee tid → most recent waking event`, awaiting its
    /// switch-in. Bounded in practice by the number of distinct tids
    /// (one entry per tid; later wakeups overwrite). Stale entries
    /// (> [`WAKING_STALENESS_NS`] before the switch-in) are ignored
    /// at lookup so a missed switch-in can't ascribe an ancient wake
    /// to a later off-CPU.
    recent_waking: HashMap<u32, RecentWaking>,
    /// debuginfod HTTPS lookup config, built once per session from
    /// the environment (`DEBUGINFOD_URLS` + `/etc/debuginfod/*.urls`)
    /// and cached on disk under `~/.cache/stax/debuginfod/`. `None` =
    /// no debuginfod configured; the lookup chain stops at the local
    /// `/usr/lib/debug/.build-id/` tree.
    debuginfod: Option<crate::elf::DebuginfodConfig>,
    /// Userspace DWARF unwinder, populated by `emit_image` as each
    /// binary's `.eh_frame` lands. `Some` iff
    /// `opts.dwarf_unwind` was honoured by the broker — when the
    /// kernel paired REGS_USER + STACK_USER with each sample.
    #[cfg(target_arch = "x86_64")]
    dwarf: Option<crate::dwarf::DwarfUnwinder>,
    summary: RecordSummary,
}

impl Session<'_> {
    fn handle(&mut self, ty: u32, misc: u16, body: &[u8], kind: PerfRingKind) {
        match ty {
            PERF_RECORD_SAMPLE => match kind {
                // Each ring's `sample_type` is different, so the
                // parser must too: the sampling ring has
                // PERF_SAMPLE_READ + a callchain; the waking
                // tracepoint ring has CALLCHAIN + PERF_SAMPLE_RAW.
                PerfRingKind::Sampling => self.on_sample(body),
                PerfRingKind::Waking => self.on_sample_from_waking(body),
                // Switch rings (SW_DUMMY) don't emit SAMPLE.
                PerfRingKind::Switch => {}
            },
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
        // PERF_RECORD_SAMPLE layout per kernel `perf_output_sample`:
        // IDENTIFIER, IP, TID, TIME, ADDR, ID, STREAM_ID, CPU, PERIOD,
        // READ, CALLCHAIN, RAW — each only present when its
        // `PERF_SAMPLE_*` bit is set in `sample_type`. Ours has
        // TID|TIME|CPU|READ|CALLCHAIN, so on the wire: pid/tid, time,
        // cpu/res, read block, callchain.
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

        // Optional REGS_USER + STACK_USER blocks (only present when
        // opts.dwarf_unwind was honoured by the broker). Layout:
        //   regs_user: u64 abi; u64 regs[popcount(mask)]
        //     — our mask is bp|sp|ip so three u64s in ascending
        //       bit-index order: bp, sp, ip.
        //   stack_user: u64 size; u8 data[size]; (u64 dyn_size if size!=0)
        // If we get a usable triple + non-empty stack snapshot, ask
        // framehop to replace the user half of the callchain with a
        // DWARF-unwound version. Falls back silently to the kernel's
        // frame-pointer CALLCHAIN on any irregularity.
        #[cfg(target_arch = "x86_64")]
        if self.opts.dwarf_unwind {
            let abi = c.u64().unwrap_or(0);
            if abi == PERF_SAMPLE_REGS_ABI_64 {
                let bp = c.u64().unwrap_or(0);
                let sp = c.u64().unwrap_or(0);
                let ip = c.u64().unwrap_or(0);
                let stack_size = c.u64().unwrap_or(0) as usize;
                let stack = c.bytes(stack_size);
                let dyn_size = if stack_size != 0 {
                    c.u64().unwrap_or(0) as usize
                } else {
                    0
                };
                if let (Some(stack), Some(uw)) = (stack, self.dwarf.as_mut()) {
                    let filled = dyn_size.min(stack.len());
                    if filled >= 16 && ip != 0 && sp != 0 {
                        let unwound = uw.unwind(ip, sp, bp, sp, &stack[..filled]);
                        // Trust framehop only when it produced more
                        // frames than the kernel's FP walk did — that's
                        // the regime where we beat the fallback. For
                        // FP-built binaries (Fedora glibc, Apple-style
                        // Rust builds with `force-frame-pointers`) the
                        // kernel walk and DWARF agree, and we keep
                        // either; the test is just a robustness check
                        // so a no-op DWARF run can't shorten a stack.
                        if unwound.len() > user.len() {
                            user = unwound;
                        }
                    }
                }
            } else if abi != 0 {
                // 32-bit task on a 64-bit kernel, or some other oddity
                // — skip without burning a warn per sample.
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
            self.last_user_stack.insert(tid, user.into_boxed_slice());
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
                // Wakeup attribution: if `sched:sched_waking` for this
                // tid landed recently (within the staleness window),
                // the sample's waker_tid + stack identifies who woke
                // it. The staleness filter rejects ancient unrelated
                // wakes if a switch-in went missing.
                let waking = self
                    .recent_waking
                    .remove(&sid_tid)
                    .filter(|w| time.saturating_sub(w.time_ns) <= WAKING_STALENESS_NS);
                let (waker_tid, waker_user_stack) = match &waking {
                    Some(w) => (Some(w.waker_tid), Some(w.waker_user_stack.as_ref())),
                    None => (None, None),
                };
                self.sink.on_cpu_interval(CpuIntervalEvent {
                    pid: self.opts.pid,
                    tid: sid_tid,
                    start_ns: start,
                    end_ns: time,
                    kind: CpuIntervalKind::OffCpu {
                        stack: &stack,
                        waker_tid,
                        waker_user_stack,
                    },
                });
                self.summary.off_cpu_intervals = self.summary.off_cpu_intervals.saturating_add(1);
            }
        }
    }

    /// One `sched:sched_waking` tracepoint sample. The sample's TID
    /// is the **waker** (whoever was on-CPU when `try_to_wake_up` ran);
    /// the CALLCHAIN is the waker's stack; and `PERF_SAMPLE_RAW`
    /// carries the wakee tid at the offset
    /// [`Session::waking_offsets`] tells us. We stream a `WakeupEvent`
    /// to the sink (for the aggregator's wakeup view) and stash the
    /// record by wakee tid in [`Session::recent_waking`] for the next
    /// switch-in to pick up as `OffCpu.waker_tid`.
    fn on_sample_from_waking(&mut self, body: &[u8]) {
        let Some(offsets) = self.waking_offsets else {
            return;
        };
        // PERF_RECORD_SAMPLE layout per kernel `perf_output_sample`:
        // TID|TIME|CPU|CALLCHAIN|RAW → on the wire: pid/tid, time,
        // cpu/res, callchain (u64 nr; ips[nr]), raw (u32 size; data).
        let mut c = Cur::new(body);
        let _waker_pid = match c.u32() {
            Some(v) => v,
            None => return,
        };
        let waker_tid = c.u32().unwrap_or(0);
        let time = c.u64().unwrap_or(0);
        let _cpu = c.u32();
        let _res = c.u32();
        let nr = match c.u64() {
            Some(n) => n,
            None => return,
        };
        let mut user: Vec<u64> = Vec::new();
        let mut kernel: Vec<u64> = Vec::new();
        let mut in_kernel = false;
        for _ in 0..nr {
            let ip = match c.u64() {
                Some(v) => v,
                None => return,
            };
            if ip >= PERF_CONTEXT_MAX {
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
        // RAW: u32 size; u8 data[size]
        let raw_size = match c.u32() {
            Some(s) => s,
            None => return,
        } as usize;
        let raw = match c.bytes(raw_size) {
            Some(b) => b,
            None => return,
        };
        let off = offsets.wakee_pid_offset as usize;
        let sz = offsets.wakee_pid_size as usize;
        if raw.len() < off.saturating_add(sz) {
            return;
        }
        let wakee_tid: u32 = match sz {
            4 => i32::from_le_bytes(raw[off..off + 4].try_into().unwrap()) as u32,
            8 => u64::from_le_bytes(raw[off..off + 8].try_into().unwrap()) as u32,
            _ => return,
        };
        if time != 0 {
            self.last_ts = self.last_ts.max(time);
        }

        let waker_user_stack: Box<[u64]> = user.into_boxed_slice();
        let waker_kernel_stack: Box<[u64]> = kernel.into_boxed_slice();

        // Stream the wakeup itself (aggregator's wakeup view).
        self.sink.on_wakeup(WakeupEvent {
            timestamp_ns: time,
            pid: self.opts.pid,
            waker_tid,
            wakee_tid,
            waker_user_stack: &waker_user_stack,
            waker_kernel_stack: &waker_kernel_stack,
        });

        // Stash for the matching switch-in to consume. A later wakeup
        // for the same wakee overwrites — the freshest wins, which is
        // the correct association when a thread is woken multiple
        // times before it actually gets scheduled.
        self.recent_waking.insert(
            wakee_tid,
            RecentWaking {
                waker_tid,
                waker_user_stack,
                waker_kernel_stack,
                time_ns: time,
            },
        );
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
                self.summary.off_cpu_intervals = self.summary.off_cpu_intervals.saturating_add(1);
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
        let mut img = match crate::elf::scan(&bytes) {
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
        // Detached debug info: distro libraries (libc, libstdc++,
        // ld-linux, …) ship stripped. Two-step lookup:
        //   1. `/usr/lib/debug/.build-id/XX/YYY...YY.debug` —
        //      installed by the matching `*-dbg`/`*-debuginfo`
        //      package. Cheap when missing (one stat).
        //   2. debuginfod HTTPS GET (if configured) — covers hosts
        //      where the dbg package isn't installed but the network
        //      can reach `https://debuginfod.debian.net/` etc.
        //      Disk-cached on hit + negative-cached on miss so the
        //      second session is instant.
        // Whichever path returns first wins; both produce a sorted
        // symbol list we merge + dedup into the primary image's set.
        let mut debug_added = 0usize;
        let mut debug_source: Option<&'static str> = None;
        if !img.build_id_full.is_empty() {
            let extra = crate::elf::load_separate_debug_by_build_id(&img.build_id_full)
                .map(|s| ("local", s))
                .or_else(|| {
                    self.debuginfod
                        .as_ref()
                        .and_then(|cfg| crate::elf::debuginfod_fetch(cfg, &img.build_id_full))
                        .map(|s| ("debuginfod", s))
                });
            if let Some((src, extra)) = extra {
                let before = img.symbols.len();
                img.symbols.extend(extra);
                img.symbols.sort_by_key(|s| s.start_svma);
                img.symbols.dedup_by_key(|s| s.start_svma);
                debug_added = img.symbols.len().saturating_sub(before);
                debug_source = Some(src);
            }
        }
        // Hand the binary's `.eh_frame` to framehop so subsequent
        // samples whose RIP lives in this image can be DWARF-unwound.
        // No-op when the image lacks `.eh_frame` (data-only libs, or
        // a build stripped of unwind info) or when this session
        // didn't enable DWARF unwinding.
        //
        // `base_avma` here is the AVMA of the mmapped executable
        // LOAD, not the image's SVMA-0 base. framehop's address
        // translation is `avma - module_base_avma = svma - base_svma`,
        // so with `base_svma = 0` we have to subtract `pgoff` to get
        // back to the SVMA-0 anchor — every section's SVMA is then
        // exactly the offset from there.
        #[cfg(target_arch = "x86_64")]
        if let (Some(uw), Some(eh)) = (self.dwarf.as_mut(), img.eh_frame.as_ref()) {
            let text = img.text.as_ref();
            let image_base_avma = base_avma.saturating_sub(pgoff);
            uw.add_image(
                path,
                base_avma,
                image_base_avma,
                vmsize,
                text.map(|t| t.svma.clone()),
                text.map(|t| t.bytes.clone()),
                eh.svma.clone(),
                eh.bytes.clone(),
                img.eh_frame_hdr.as_ref().map(|h| h.svma.clone()),
                img.eh_frame_hdr.as_ref().map(|h| h.bytes.clone()),
            );
        }

        self.summary.binaries = self.summary.binaries.saturating_add(1);
        if debug_added > 0 {
            info!(
                path,
                base_avma = format_args!("{base_avma:#x}"),
                text_svma = format_args!("{text_svma:#x}"),
                syms = img.symbols.len(),
                from_debug = debug_added,
                source = debug_source.unwrap_or("?"),
                "image loaded (merged separate debug info)"
            );
        } else {
            info!(
                path,
                base_avma = format_args!("{base_avma:#x}"),
                text_svma = format_args!("{text_svma:#x}"),
                syms = img.symbols.len(),
                "image loaded"
            );
        }
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
        sess.handle(ty, misc, &scratch[off + 8..off + size], r.kind);
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
        let leader = match open_cpu(
            *cpu,
            opts.frequency_hz,
            opts.kernel_stacks,
            opts.dwarf_unwind,
        ) {
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
    // sched:sched_waking tracepoint rings for wakeup attribution.
    // Best-effort in-process: tracefs is typically root-only, so this
    // generally fails outside the daemon path. When it does, wakeups
    // are simply unattributed (the off-CPU intervals still flow).
    let (waking_rings, waking_offsets) = match read_sched_waking_tracepoint() {
        Ok((id, offsets)) => {
            let mut wr: Vec<PerfRing> = Vec::with_capacity(cpus.len());
            let mut failed = false;
            for cpu in &cpus {
                match open_cpu_waking(*cpu, id, opts.kernel_stacks) {
                    Ok(r) => wr.push(r),
                    Err(e) => {
                        warn!(
                            %e,
                            cpu = *cpu,
                            "sched_waking ring open failed; wakeups disabled"
                        );
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                (Vec::new(), None)
            } else {
                (wr, Some(offsets))
            }
        }
        Err(e) => {
            debug!(
                %e,
                "sched_waking tracepoint unavailable in-process \
                 (tracefs is usually root-only); wakeups attributed only \
                 over the staxd broker"
            );
            (Vec::new(), None)
        }
    };

    run_with_rings(
        opts,
        sink,
        should_stop,
        rings,
        switch_rings,
        waking_rings,
        waking_offsets,
        pmu,
    )
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
    mut waking_rings: Vec<PerfRing>,
    waking_offsets: Option<WakingFieldOffsets>,
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

    for r in rings
        .iter()
        .chain(switch_rings.iter())
        .chain(waking_rings.iter())
    {
        r.enable()?;
    }
    info!(
        pid = opts.pid,
        freq_hz = opts.frequency_hz,
        cpus = rings.len(),
        off_cpu = !switch_rings.is_empty(),
        wakeups = !waking_rings.is_empty(),
        "linux perf capture started"
    );

    let mut pollfds: Vec<libc::pollfd> = rings
        .iter()
        .chain(switch_rings.iter())
        .chain(waking_rings.iter())
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
        waking_offsets,
        recent_waking: HashMap::new(),
        debuginfod: crate::elf::DebuginfodConfig::from_env(),
        #[cfg(target_arch = "x86_64")]
        dwarf: if opts.dwarf_unwind {
            Some(crate::dwarf::DwarfUnwinder::new())
        } else {
            None
        },
        summary: RecordSummary::default(),
    };
    if let Some(cfg) = &sess.debuginfod {
        info!(
            urls = cfg.urls.len(),
            cache = %cfg.cache_dir.display(),
            timeout_ms = cfg.timeout.as_millis() as u64,
            "debuginfod lookup enabled"
        );
    }

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
        for r in rings
            .iter_mut()
            .chain(switch_rings.iter_mut())
            .chain(waking_rings.iter_mut())
        {
            drain_dispatch(r, &mut scratch, &mut sess);
        }
    }

    for r in rings
        .iter()
        .chain(switch_rings.iter())
        .chain(waking_rings.iter())
    {
        r.disable();
    }
    // Final sweep so the tail of the recording isn't lost.
    for r in rings
        .iter_mut()
        .chain(switch_rings.iter_mut())
        .chain(waking_rings.iter_mut())
    {
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
