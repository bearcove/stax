+++
title = "Platform Support"
weight = 1
insert_anchor_links = "heading"
+++

stax runs on **macOS** and **Linux**. The analysis side — the aggregator,
flamegraphs, top-N, annotated disassembly, the CLI, the web UI — is identical
on both. What differs is the *capture* backend underneath, and therefore what
each platform can actually see.

## At a glance

| capability                     | macOS                          | Linux                          |
|--------------------------------|--------------------------------|--------------------------------|
| capture backend                | `kperf` / `kdebug` via `staxd` | `perf_event_open`, in-process  |
| privileged daemon              | `staxd` (LaunchDaemon)         | none — direct syscalls         |
| on-CPU sampling                | ✅                             | ✅                             |
| off-CPU intervals              | ✅                             | ⏳ follow-on                   |
| wakeup attribution             | ✅                             | ⏳ follow-on                   |
| per-thread `blocked` reason    | ✅                             | ⏳ follow-on                   |
| user call stacks               | ✅                             | ✅                             |
| kernel call stacks             | ✅                             | opt-in (`perf_event_paranoid`) |
| per-thread PMU counters        | ✅                             | —                              |
| JIT (jitdump) auto-discovery   | ✅                             | ⏳                             |
| annotated disassembly          | ✅ (`aarch64` + `x86_64`)      | ✅ (`aarch64` + `x86_64`)      |

`✅` works today, `⏳` is a planned follow-on, `—` is not applicable.

## macOS

On macOS, stax uses Apple's private **`kperf`** and **`kdebug`**
frameworks — the same machinery *Instruments.app* is built on. That is what
lets stax see things `samply` cannot.

- **The PET sampler.** `kperf`'s *periodic event timer* fires on every
  thread at the configured frequency. `staxd` arms it; the kernel walks the
  stack and writes records into the `kdebug` trace buffer, which `staxd`
  streams out.
- **User *and* kernel stacks.** Each PET tick yields both a user-space
  backtrace and a kernel-space one.
- **Off-CPU intervals and wakeups.** stax also subscribes to `kdebug`
  scheduler events, so it knows when a thread went *off* the CPU, for how
  long, and why — lock, sleep, I/O, semaphore, IPC. This is what fills the
  `blocked` column in [`stax threads`](@/guide/inspecting-a-run.md#stax-threads).
- **PMU counters.** `kperf` also exposes the CPU's performance counters; stax
  records per-thread counter deltas at each tick.

Because these frameworks need root, they live behind
[`staxd`](@/concepts/architecture.md) — installed once with
`sudo stax setup`. Both Apple Silicon and Intel Macs are supported.

> **Code signing is required.** `cargo xtask install` codesigns the binaries;
> the daemons will not run otherwise. See
> [Getting Started](@/guide/getting-started.md). **Hardened-runtime
> targets** — apps with the hardened runtime and no `get-task-allow`
> entitlement — cannot be attached to; the helper is same-uid and aimed at
> ordinary local developer processes.

## Linux

On Linux, stax uses **`perf_event_open`** directly — the same kernel
interface `perf` itself uses. There is no privileged daemon: the recording
task opens a sampling event per CPU, maps the kernel's ring buffers, and
drains and parses `PERF_RECORD_*` records in-process.

- **On-CPU sampling.** A frequency-driven `PERF_COUNT_SW_CPU_CLOCK` event
  with `PERF_SAMPLE_CALLCHAIN` — the kernel walks the stack at each tick.
  This is the on-CPU flamegraph spine.
- **User stacks always; kernel stacks if allowed.** User frames are always
  captured. Kernel frames require the host to permit it
  (`perf_event_paranoid <= 1`); when it doesn't, stax records user frames
  only.
- **Image loads and thread names.** `mmap` / `mmap2` records (including
  build-ids) track loaded binaries; `comm` records track thread names.
- **Off-CPU is a follow-on.** Off-CPU intervals and wakeup attribution — the
  `sched_*` tracepoint correlation — are deliberately *not* in the current
  Linux backend. Today the Linux path is the on-CPU flamegraph; the
  `blocked`-reason columns in `stax threads` are a macOS feature for now.

### perf_event_paranoid

The kernel's `kernel.perf_event_paranoid` sysctl gates what
`perf_event_open` will allow:

```bash
cat /proc/sys/kernel/perf_event_paranoid
# lower it for the session (kernel stacks need <= 1):
sudo sysctl kernel.perf_event_paranoid=1
```

A restrictive value is the usual reason a Linux recording comes back with
shallow or kernel-frame-less stacks. A daemon/systemd split that holds the
privilege the way `staxd` does on macOS is planned for hosts that keep
`perf_event_paranoid` locked down.

## What's shared

Everything *above* the capture backend is one codebase. Both platforms feed
the same OS-neutral event stream — on-CPU samples, image loads, thread names
— into the same aggregator. So a flamegraph, a `stax top` table, or an
annotated disassembly looks and behaves identically regardless of where the
recording happened. The differences are only ever about what the OS *let*
stax capture in the first place.

## See also

- [Architecture](@/concepts/architecture.md) — where `staxd` fits.
- [Stack Unwinding](@/concepts/stack-unwinding.md) — why both backends need
  frame pointers in the target.
- [Sampling](@/concepts/sampling.md) — on-CPU vs off-CPU, explained.
