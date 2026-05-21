+++
title = "Platform Support"
weight = 1
insert_anchor_links = "heading"
+++

stax runs on **macOS** and **Linux**. The analysis half — the aggregator,
flamegraphs, top-N, per-thread breakdowns, annotated disassembly, the CLI, the
web UI — is one codebase and behaves identically on both. What differs is the
*capture* backend underneath, and therefore some of what each platform can see.

## At a glance

| capability                          | macOS                        | Linux                          |
|-------------------------------------|------------------------------|--------------------------------|
| capture backend                     | `kperf` / `kdebug`           | `perf_event_open`              |
| privileged helper (`staxd`)         | LaunchDaemon — **required**  | systemd service — **optional** |
| record without `sudo`               | ✅                           | ✅                             |
| on-CPU sampling                     | ✅                           | ✅                             |
| off-CPU intervals                   | ✅                           | ✅                             |
| per-thread blocked reason           | ✅                           | ✅                             |
| wakeup attribution                  | ✅                           | ✅ with `staxd`                |
| user call stacks                    | ✅                           | ✅                             |
| kernel call stacks                  | ✅                           | ✅ when the host permits       |
| frame-pointer unwinding             | ✅ (in-kernel)               | ✅ (in-kernel)                 |
| DWARF `.eh_frame` unwinding          | —                            | ✅ x86-64                      |
| PMU / hardware counters             | ✅                           | ✅ with `staxd`                |
| debuginfod + separate debug files   | —                            | ✅                             |
| dyld shared-cache symbols           | ✅                           | —                              |
| JIT (jitdump) symbolication         | ✅                           | —                              |
| annotated disassembly               | ✅ (`x86_64` + `aarch64`)    | ✅ (`x86_64` + `aarch64`)      |

`✅` works today, `—` not applicable to that platform.

## macOS

On macOS, stax uses Apple's private **`kperf`** and **`kdebug`** frameworks —
the low-level interfaces the OS exposes for periodic sampling and scheduler
tracing.

- **The PET sampler.** `kperf`'s *periodic event timer* fires on every thread
  at the configured frequency. The kernel walks the stack and writes records
  into the `kdebug` trace buffer.
- **User *and* kernel stacks.** Each tick yields both a user-space backtrace
  and a kernel-space one.
- **Off-CPU intervals and wakeups.** stax subscribes to `kdebug` scheduler
  events (`MACH_SCHED`, `MACH_MAKERUNNABLE`), so it knows when a thread went
  *off* the CPU, for how long, why, and — for the wakeups it catches — which
  thread woke it.
- **PMU counters.** `kperf` exposes the CPU's performance counters; stax
  records per-thread cycles and instructions, plus L1-data-cache misses and
  branch mispredicts.
- **JIT and system libraries.** stax tails a [perf jitdump](@/guide/profiling-jit-code.md)
  file to symbolicate JIT'd code, and parses the **dyld shared cache** so
  addresses inside system libraries resolve to names.

These frameworks need root, so they live behind
[`staxd`](@/concepts/architecture.md) — a LaunchDaemon you install once with
`sudo stax setup`. On macOS `staxd` is **required**: it is the only path to
kperf. Once installed, `stax record` itself is unprivileged. Both Apple
Silicon and Intel Macs are supported.

> **Code signing is required.** `cargo xtask install` codesigns the binaries;
> the daemons will not run otherwise — see
> [Getting Started](@/guide/getting-started.md). **Hardened-runtime
> targets** — apps with the hardened runtime and no `get-task-allow`
> entitlement — cannot be attached to.

## Linux

On Linux, stax uses **`perf_event_open`** directly — the same kernel
interface `perf` itself uses. It opens a sampling event per CPU, maps the
kernel's ring buffers, and drains and parses `PERF_RECORD_*` records.

- **On-CPU sampling** via a frequency-driven software clock event with
  `PERF_SAMPLE_CALLCHAIN`.
- **Off-CPU intervals** from a side-band `PERF_RECORD_SWITCH` context-switch
  ring. A voluntary switch-out opens an off-CPU span; the matching switch-in
  closes it with a true scheduler duration.
- **Per-thread blocked reasons** from `/proc/<tid>/wchan` — the kernel
  wait-site the thread is parked on leads its off-CPU stack, and stax
  classifies it into the same buckets as macOS.
- **Wakeup attribution** from the `sched:sched_waking` tracepoint. The
  tracepoint lives in tracefs, which is root-only — so wakeup attribution
  needs the `staxd` broker (below). Without it, off-CPU intervals still flow,
  they are just not attributed to a waker.
- **Kernel call stacks** when the host permits it (see
  [`perf_event_paranoid`](#linux--perf-event-paranoid)); user stacks always.
- **PMU counters** — cycles, instructions, L1-data read misses, branch
  mispredicts — opened as a hardware-counter group. Like the tracepoint,
  hardware counters on a locked-down host need the `staxd` broker.
- **Stripped binaries.** Linux ships most system libraries stripped; stax
  recovers their symbols from local separate-debug files and **debuginfod** —
  see [Symbolication](@/concepts/symbolication.md).
- **DWARF unwinding.** On x86-64, stax replays the unwind in userspace
  against `.eh_frame` by default — the system `libc` omits frame pointers, so
  the kernel walk truncates without it. See
  [Stack Unwinding](@/concepts/stack-unwinding.md).

### Two recording modes

Linux can record **with or without** the `staxd` daemon:

- **In-process** — when `perf_event_paranoid` is permissive, `stax-server`
  opens `perf_event_open` itself. No daemon needed.
- **Via the `staxd` broker** — `staxd` does the one privileged
  `perf_event_open` per CPU and hands the file descriptors back to the
  unprivileged side over a Unix socket. From that point the daemon is out of
  the data path entirely.

The broker is what makes stax work on a host with a restrictive
`perf_event_paranoid`, and it is also what unlocks **hardware counters** and
**wakeup attribution** (the tracepoint), which a non-root process cannot open.
Installing it is the same one-time `sudo stax setup` as on macOS — see
[Getting Started](@/guide/getting-started.md) and
[Architecture](@/concepts/architecture.md).

### perf_event_paranoid {#perf-event-paranoid}

The kernel's `kernel.perf_event_paranoid` sysctl gates what
`perf_event_open` allows a non-root process to do:

```bash
cat /proc/sys/kernel/perf_event_paranoid
# lower it for the session (kernel stacks need <= 1):
sudo sysctl kernel.perf_event_paranoid=1
```

A restrictive value is the usual reason an in-process Linux recording comes
back with shallow stacks, no kernel frames, or no hardware counters. The fix
is either to lower `perf_event_paranoid`, or — better — to install the
`staxd` broker, which holds the privilege so the host setting stops mattering.

## What's shared

Everything *above* the capture backend is one codebase. Both platforms feed
the same OS-neutral event stream — on-CPU samples, off-CPU intervals,
wakeups, image loads, thread names — into the same aggregator. A flamegraph,
a `stax top` table, an annotated disassembly, the web UI: all look and behave
identically regardless of where the recording happened. The differences are
only ever about what the OS *let* stax capture in the first place.

## See also

- [Architecture](@/concepts/architecture.md) — what `staxd` does on each platform.
- [Stack Unwinding](@/concepts/stack-unwinding.md) — frame pointers and DWARF.
- [Symbolication](@/concepts/symbolication.md) — debuginfod and friends.
- [Sampling](@/concepts/sampling.md) — on-CPU vs off-CPU, explained.
