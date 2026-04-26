//! Off-CPU tracking via `DBG_MACH_SCHED` kdebug records.
//!
//! Each context switch fires one of:
//!   * `MACH_SCHED` (subclass=0x40, code=0x0) -- a thread-thread switch.
//!     `arg2` holds the on-coming thread's tid.
//!   * `MACH_STKHANDOFF` (subclass=0x40, code=0x8) -- the same thing
//!     but on the fast path used by `thread_handoff`.
//!   * `MACH_MAKERUNNABLE` (subclass=0x40, code=0x4) -- a blocked
//!     thread became runnable. `arg1` holds the woken-up tid.
//!
//! For each (off, on) tid pair we know:
//!   - who came on-CPU at this timestamp (record's `arg2`)
//!   - who went off-CPU is the *previous* on-going thread on the
//!     same cpuid; we track the per-cpu current tid for that.
//!
//! From this we can derive per-thread off-CPU intervals: time from
//! when a thread last went off-CPU until it next came back. Sum
//! those and you get total off-CPU time per thread.
//!
//! This module currently only tracks the bookkeeping + summary. The
//! actual emission of off-CPU samples (with stacks borrowed from the
//! preceding PET tick) is a follow-up.

use std::collections::HashMap;

use crate::kdebug::{kdbg_code, kdbg_subclass, mach_sched, KdBuf, KDBG_TIMESTAMP_MASK};

#[derive(Default)]
pub struct OffCpuTracker {
    /// Last-known on-CPU thread per cpuid. Mapping cpuid -> tid.
    on_cpu: HashMap<u32, u64>,
    /// Per-thread accumulated off-CPU duration (ns) and the timestamp
    /// of the most recent off->on transition we still need to close
    /// (None once it's already on-CPU).
    threads: HashMap<u64, ThreadState>,
    sched_count: u64,
    stkhandoff_count: u64,
    makerunnable_count: u64,
}

#[derive(Default)]
struct ThreadState {
    total_off_ns: u64,
    /// Timestamp the thread last went off-CPU. `None` while on-CPU.
    last_off_ns: Option<u64>,
}

impl OffCpuTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, rec: &KdBuf) {
        let subclass = kdbg_subclass(rec.debugid);
        if subclass != crate::kdebug::DBG_MACH_SCHED {
            return;
        }
        let code = kdbg_code(rec.debugid);
        let ts = rec.timestamp & KDBG_TIMESTAMP_MASK;
        match code {
            mach_sched::SCHED | mach_sched::STKHANDOFF => {
                if code == mach_sched::SCHED {
                    self.sched_count += 1;
                } else {
                    self.stkhandoff_count += 1;
                }
                let new_tid = rec.arg2;
                let cpu = rec.cpuid;

                // Whoever was on this cpu before is now off.
                if let Some(prev_tid) = self.on_cpu.insert(cpu, new_tid) {
                    if prev_tid != 0 && prev_tid != new_tid {
                        let st = self.threads.entry(prev_tid).or_default();
                        st.last_off_ns = Some(ts);
                    }
                }

                // The new on-coming thread closes its off-CPU
                // interval (if any).
                if new_tid != 0 {
                    let st = self.threads.entry(new_tid).or_default();
                    if let Some(off_ns) = st.last_off_ns.take() {
                        st.total_off_ns = st.total_off_ns.saturating_add(ts.saturating_sub(off_ns));
                    }
                }
            }
            mach_sched::MAKERUNNABLE => {
                self.makerunnable_count += 1;
            }
            _ => {}
        }
    }

    pub fn log_summary(&self) {
        log::info!(
            "off-cpu: SCHED={} STKHANDOFF={} MAKERUNNABLE={} threads_seen={}",
            self.sched_count,
            self.stkhandoff_count,
            self.makerunnable_count,
            self.threads.len()
        );
        // Top-10 threads by total off-CPU time.
        let mut by_off: Vec<(u64, u64)> =
            self.threads.iter().map(|(&t, st)| (t, st.total_off_ns)).collect();
        by_off.sort_by(|a, b| b.1.cmp(&a.1));
        for (tid, total_off_ns) in by_off.iter().take(10) {
            let total_off_ms = (*total_off_ns as f64) / 1_000_000.0;
            log::info!("  tid={tid} off-cpu={total_off_ms:.2}ms");
        }
    }
}
