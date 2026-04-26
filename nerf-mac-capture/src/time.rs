// CLOCK_MONOTONIC matches what perf-jitdump producers (and the spec) use, so
// sample timestamps stay comparable with embedded jitdump CodeLoad timestamps.
// On macOS, CLOCK_MONOTONIC includes time spent asleep, while
// mach_absolute_time / CLOCK_UPTIME_RAW does not -- using the latter here can
// leave the two clocks differing by hours or days after a sleep cycle.

pub fn get_monotonic_timestamp() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}
