//! Recording driver. Configures kperf's PET (Profile Every Thread)
//! mode for a target PID, enables the kdebug ringbuffer, drains it
//! on a schedule, and emits parsed samples to a [`SampleSink`].
//!
//! On drop, the [`Session`] guard tears down kperf + kdebug state in
//! the same order as mperf's `profiling_cleanup` so the host kernel
//! is left in a clean state even on panic.

use std::time::{Duration, Instant};

use nerf_mac_capture::SampleSink;

use nerf_mac_kperf_parse::pipeline::{Pipeline, PipelineConfig};
use nerf_mac_kperf_sys::bindings::{self, sampler, Frameworks};
use nerf_mac_kperf_sys::error::Error;
use nerf_mac_kperf_sys::kdebug::{self, KdBuf, KdRegtype};
use nerf_mac_kperf_sys::pmu_events::{self, ConfiguredPmu, PmuSlot};

/// Configuration for a kperf-driven recording session.
pub struct RecordOptions {
    /// PID to attach to.
    pub pid: u32,
    /// Sampling frequency in Hz. Translated to a PET timer period.
    pub frequency_hz: u32,
    /// If `Some`, stop recording after this duration.
    pub duration: Option<Duration>,
    /// Number of records the kdebug ringbuffer is sized for. mperf
    /// uses 1_000_000; that's a few tens of MB and is fine for
    /// short-to-medium captures.
    pub kdebug_buf_records: i32,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            pid: 0,
            frequency_hz: 1000,
            duration: None,
            kdebug_buf_records: 1_000_000,
        }
    }
}

/// Sampler bitmask. `TH_INFO` lets us correlate a record back to a
/// tid; `USTACK`/`KSTACK` are the user/kernel callchains; `PMC_THREAD`
/// asks kperf to read per-thread CPU performance counters at each
/// PET tick (cycles + instructions retired on Apple Silicon's fixed
/// counters).
const STACK_SAMPLER_BITS: u32 =
    sampler::TH_INFO | sampler::USTACK | sampler::KSTACK | sampler::PMC_THREAD;

/// Drive a recording session. Blocks until `should_stop` returns true,
/// the duration elapses, or an unrecoverable error occurs.
pub fn record<S: SampleSink>(
    opts: RecordOptions,
    sink: &mut S,
    mut should_stop: impl FnMut() -> bool,
) -> Result<(), Error> {
    let fw = bindings::load()?;

    // The earliest, cheapest way to confirm we have root: this
    // sysctl is gated on the same privilege check as the rest of
    // the kpc surface.
    let mut force_ctrs: i32 = 0;
    let rc = unsafe { (fw.kpc_force_all_ctrs_get)(&mut force_ctrs) };
    if rc != 0 {
        return Err(Error::NotRoot);
    }

    // Wipe any stale kperf/ktrace state from a previous half-finished
    // run. Without this, `kdebug::reset()` below trips EINVAL when
    // ktrace is still owned by KTRACE_KPERF from a previous session.
    unsafe {
        let _ = (fw.kperf_sample_set)(0);
        let _ = (fw.kperf_reset)();
    }
    let _ = kdebug::set_lightweight_pet(0);
    let _ = kdebug::enable(false);
    let _ = kdebug::reset();

    // Open the shared cache once and share it via Arc — the image
    // scanner needs it for symbol enumeration, and the live sink
    // needs the same parsed cache as a `MachOByteSource` for
    // system-code disassembly. Single parse, two consumers.
    //
    // No `task_for_pid` here on purpose: AMFI denies it from root
    // against a privilege-dropped child on Apple Silicon, and we
    // don't actually need a Mach task port. libproc gives us the
    // image regions and thread names by PID, gated only by read
    // permission.
    let shared_cache: Option<std::sync::Arc<nperf_mac_shared_cache::SharedCache>> =
        nperf_mac_shared_cache::SharedCache::for_host().map(std::sync::Arc::new);

    // Configure additional PMU events (cache misses, branch
    // mispredicts) before session start so kpc_set_config sees the
    // configurable counter requests. Falls back gracefully to the
    // FIXED-only path if the lookups don't resolve on this chip.
    let configured_pmu = pmu_events::configure(&fw);

    // Pipeline owns parser/offcpu/image-scan/thread-name/jitdump/
    // kernel-image+slide and runs all the periodic tasks.
    // nperfd-client uses the same Pipeline to consume `KdBuf`s
    // streamed from the daemon — feature parity by construction.
    let mut pipeline = Pipeline::new(
        PipelineConfig {
            pid: opts.pid,
            frequency_hz: opts.frequency_hz,
            pmc_idx_l1d: configured_pmu
                .as_ref()
                .and_then(|c| c.slot_indices[PmuSlot::L1DCacheMissLoad as usize]),
            pmc_idx_brmiss: configured_pmu
                .as_ref()
                .and_then(|c| c.slot_indices[PmuSlot::BranchMispredict as usize]),
        },
        shared_cache,
        sink,
    );

    let t0 = Instant::now();
    let mut session = Session::start(&fw, &opts, configured_pmu.as_ref())?;
    session.enable_kdebug(&opts)?;
    session.arm()?;
    log::info!("kperf+kdebug arming took {:?}", t0.elapsed());

    drain_loop(&opts, sink, &mut pipeline, &mut should_stop)?;

    drop(session);

    pipeline.finish(sink);
    Ok(())
}

// ---------------------------------------------------------------------------
// Session: lifecycle guard for kperf + kdebug kernel state
// ---------------------------------------------------------------------------

struct Session<'a> {
    fw: &'a Frameworks,
    #[allow(dead_code)]
    actionid: u32,
    #[allow(dead_code)]
    timerid: u32,
}

impl<'a> Session<'a> {
    /// Configure kperf actions / timers / filter. Does NOT call
    /// `kperf_sample_set(1)` -- that's deferred to `arm()` so we can
    /// finish kdebug setup first. Once `kperf_sample_set(1)` runs,
    /// kperf takes exclusive ownership of ktrace and reset/set_buf_size
    /// would fail; doing kdebug init first sidesteps that. We also
    /// leave `kperf.lightweight_pet=0` (the post-cleanup default), so
    /// PET walks user/kernel callstacks on every tick instead of just
    /// when its rate-limiter happens to fire (lightweight=1 is for
    /// counter-stat tools like mperf, not profilers).
    fn start(
        fw: &'a Frameworks,
        opts: &RecordOptions,
        configured_pmu: Option<&ConfiguredPmu>,
    ) -> Result<Self, Error> {
        // Allocate one action + one timer.
        let actionid: u32 = 1;
        let timerid: u32 = 1;

        kperf_call(unsafe { (fw.kperf_action_count_set)(bindings::KPERF_ACTION_MAX) }, "action_count_set")?;
        kperf_call(unsafe { (fw.kperf_timer_count_set)(bindings::KPERF_TIMER_MAX) }, "timer_count_set")?;

        // Stack samplers — kernel does the FP-walk for us.
        kperf_call(
            unsafe { (fw.kperf_action_samplers_set)(actionid, STACK_SAMPLER_BITS) },
            "action_samplers_set",
        )?;
        kperf_call(
            unsafe {
                (fw.kperf_action_filter_set_by_pid)(actionid, opts.pid as i32)
            },
            "action_filter_set_by_pid",
        )?;

        let period_ns = if opts.frequency_hz == 0 {
            1_000_000
        } else {
            1_000_000_000u64 / opts.frequency_hz as u64
        };
        let ticks = unsafe { (fw.kperf_ns_to_ticks)(period_ns) };
        kperf_call(
            unsafe { (fw.kperf_timer_period_set)(actionid, ticks) },
            "timer_period_set",
        )?;
        kperf_call(
            unsafe { (fw.kperf_timer_action_set)(actionid, timerid) },
            "timer_action_set",
        )?;
        kperf_call(unsafe { (fw.kperf_timer_pet_set)(timerid) }, "timer_pet_set")?;

        // Enable PMU counter classes. Apple Silicon's FIXED class
        // exposes cycles + instructions retired with no per-event
        // config; the CONFIGURABLE class lets us program ~8 counters
        // for events like L1D misses or branch mispredicts. We always
        // turn FIXED on; if `configured_pmu` resolved a configurable
        // event we extend the class mask + push the event encodings
        // via kpc_set_config.
        let class_mask = configured_pmu
            .map(|c| c.class_mask)
            .unwrap_or(bindings::KPC_CLASS_FIXED_MASK);
        if let Some(c) = configured_pmu {
            // `kpc_set_config` writes an array of u64 event
            // configs into the kernel; the FIXED class needs zero
            // entries (it's pre-determined), so the array length we
            // pass is whatever the kpep_config built.
            let mut configs = c.configs.clone();
            kperf_call(
                unsafe { (fw.kpc_set_config)(class_mask, configs.as_mut_ptr()) },
                "kpc_set_config(FIXED+CONFIGURABLE)",
            )?;
        }
        kperf_call(
            unsafe { (fw.kpc_set_counting)(class_mask) },
            "kpc_set_counting",
        )?;
        kperf_call(
            unsafe { (fw.kpc_set_thread_counting)(class_mask) },
            "kpc_set_thread_counting",
        )?;

        Ok(Self { fw, actionid, timerid })
    }

    fn enable_kdebug(&mut self, opts: &RecordOptions) -> Result<(), Error> {
        kdebug::reset()?;
        kdebug::set_buf_size(opts.kdebug_buf_records)?;
        kdebug::setup()?;

        // Range filter covers DBG_MACH (class 1, where MACH_SCHED
        // context-switch events live) through DBG_PERF (class 37,
        // where kperf samples live). The filter is single-range so
        // we sweep up everything in between (DBG_NETWORK, DBG_BSD,
        // ...); the drain loop drops anything that isn't DBG_PERF
        // or DBG_MACH_SCHED before parsing. In practice the kdebug
        // ring buffer (1M records) holds several seconds of traffic
        // even on busy systems, and we drain every few ms.
        let mut filter = KdRegtype {
            ty: kdebug::KDBG_RANGETYPE,
            value1: kdebug::kdbg_eventid(kdebug::DBG_MACH, kdebug::DBG_MACH_SCHED, 0),
            value2: kdebug::kdbg_eventid(kdebug::DBG_PERF, 0xff, 0x3fff),
            value3: 0,
            value4: 0,
        };
        kdebug::set_filter(&mut filter)?;
        kdebug::enable(true)?;
        Ok(())
    }

    /// Arm kperf sampling. Must be called *after* `enable_kdebug` --
    /// `kperf_sample_set(1)` takes exclusive ownership of the ktrace
    /// subsystem, after which `kdebug::reset` and friends would EBUSY.
    /// The exclusive lock doesn't block reads (`KERN_KDREADTR`), so the
    /// drain loop keeps working.
    ///
    /// `lightweight_pet=1` is essential: in that mode PET samples only
    /// threads that are *actually running on a CPU at the moment of
    /// the tick*. Without it (lightweight_pet=0, the "heavy PET"
    /// path), kperf walks every thread in the target -- including
    /// parked ones -- and emits a sample with the thread's frozen
    /// last user PC. Those parked-thread samples sit in the syscall
    /// stub of whatever made the thread block (`__psynch_cvwait`,
    /// `mach_msg2_trap`, ...). When we then weight every sample by
    /// the sampling period, the on-CPU view shows 27s of "cvwait
    /// time" for a thread that never actually ran cvwait for 27s --
    /// it just got caught parked there 27,000 times. Off-CPU has its
    /// own real-interval channel (MACH_SCHED records); we don't need
    /// PET to also fake it.
    fn arm(&mut self) -> Result<(), Error> {
        kdebug::set_lightweight_pet(1)?;
        kperf_call(unsafe { (self.fw.kperf_sample_set)(1) }, "sample_set")?;
        Ok(())
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        // Same order as mperf's profiling_cleanup. Errors are
        // logged, not propagated — we want the rest of the cleanup
        // to run even if one step fails.
        let _ = kdebug::enable(false);
        let _ = kdebug::reset();
        unsafe {
            let _ = (self.fw.kperf_sample_set)(0);
        }
        let _ = kdebug::set_lightweight_pet(0);
        unsafe {
            let _ = (self.fw.kpc_set_counting)(0);
            let _ = (self.fw.kpc_set_thread_counting)(0);
            let _ = (self.fw.kpc_force_all_ctrs_set)(0);
            let _ = (self.fw.kperf_reset)();
        }
    }
}

// ---------------------------------------------------------------------------
// Drain loop
// ---------------------------------------------------------------------------

/// Sleep, drain `KERN_KDREADTR`, hand the records to the shared
/// `Pipeline` for parsing + sample/interval/wakeup emission and for
/// the periodic libproc / jitdump / image-scan tasks. The pipeline
/// is the single source of truth for record-consumer logic; this
/// function only owns the kdebug-side I/O loop.
fn drain_loop<S: SampleSink>(
    opts: &RecordOptions,
    sink: &mut S,
    pipeline: &mut Pipeline,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), Error> {
    let start = Instant::now();
    let drain_period = Duration::from_micros(
        ((1_000_000 / opts.frequency_hz.max(1)) * 2).into(),
    );
    let mut buf: Vec<KdBuf> = vec![empty_kdbuf(); opts.kdebug_buf_records as usize];

    loop {
        if should_stop() {
            break;
        }
        if let Some(d) = opts.duration {
            if start.elapsed() >= d {
                break;
            }
        }

        std::thread::sleep(drain_period);

        pipeline.tick(sink);

        let n = kdebug::read_trace(&mut buf)?;
        if n == 0 {
            continue;
        }
        pipeline.process_records(&buf[..n], sink);
    }
    Ok(())
}

fn empty_kdbuf() -> KdBuf {
    KdBuf {
        timestamp: 0,
        arg1: 0,
        arg2: 0,
        arg3: 0,
        arg4: 0,
        arg5: 0,
        debugid: 0,
        cpuid: 0,
        unused: 0,
    }
}


fn kperf_call(rc: i32, op: &'static str) -> Result<(), Error> {
    if rc != 0 {
        return Err(Error::Kperf { op, code: rc });
    }
    Ok(())
}

