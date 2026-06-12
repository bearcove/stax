+++
title = "Sampling"
weight = 3
insert_anchor_links = "heading"
+++

stax is a *sampling* profiler. It does not instrument your code or count
every function call. Instead it interrupts your program many times a second
and writes down where it is. This page explains what that buys you, what it
costs, and why stax measures two different things at once.

## How sampling works

A sampling profiler sets a timer. Every time it fires, the profiler records
the current call stack of each thread, then lets the program continue. After
a while you have thousands of these snapshots.

The key idea is **statistical**: if a function appears in 30% of the samples,
it was running roughly 30% of the time. You never see *every* call, but with
enough samples the proportions converge on the truth — and the overhead stays
low and roughly constant, because the cost is set by the sampling rate, not
by how much your code does between samples.

On macOS the timer is `kperf`'s **PET** — *periodic event timer*. On Linux it
is a frequency-driven `perf_event_open` software clock event. Either way, the
rate is what you set with [`stax record -F`](@/guide/recording.md#flags--f-frequency-hz).

## Frequency: the one knob

`-F, --frequency` is sampling rate in hertz — samples per second per thread.
**The default is 900.**

It is a straight trade-off:

- **Higher** (e.g. `1999`) — finer detail, short-lived functions are more
  likely to be caught, more overhead, more data.
- **Lower** (e.g. `499`) — coarser, cheaper, you may miss brief functions
  entirely.

900–1999 Hz suits most work. Raise it when you are chasing something brief;
lower it when overhead matters more than fine detail.

## On-CPU: where the CPU time goes

The sampling timer only fires for a thread **while that thread is actually
running on a CPU**. A thread blocked on a lock, asleep, or waiting on I/O is
not scheduled — so it produces no samples for as long as it waits.

That is a feature, not a gap. It means the on-CPU profile measures **CPU
time**, not wall-clock time. The flamegraph from
[`stax flame`](@/guide/inspecting-a-run.md#stax-flame) and the table from
[`stax top`](@/guide/inspecting-a-run.md#stax-top) answer the question *"what
is burning CPU?"* — and a thread that spends its life blocked correctly
contributes almost nothing to it.

Cooperating target spans are the other exact-duration input to those same
views. When a target reports GPU, accelerator, or executor spans through
`stax-target`, stax records them as synthetic lane intervals rather than CPU
samples. In `threads`, `top`, `flame`, and the web UI, those spans show up as
explicit `target` time/span counts alongside active time. The legacy active
field includes target duration so existing flame widths still work; it does
not imply the CPU was executing that work.

## Off-CPU: why threads wait

But "what is burning CPU" is only half of "why is this slow". A program can
be slow because it waits — and waiting, by definition, produces no on-CPU
samples. So stax measures the gaps too.

Whenever a thread goes **off** the CPU, stax records an **off-CPU
interval**: how long the thread was descheduled, and *why*. macOS reads this
from `kdebug` scheduler events; Linux from `PERF_RECORD_SWITCH`
context-switch records, with the kernel wait-site from `/proc/<tid>/wchan`
naming the cause. Either way the reason is bucketed into one of:

`idle` · `lock` · `sem` · `ipc` · `ioR` · `ioW` · `ready` · `sleep` ·
`conn` · `other`

Off-CPU intervals are not sampled — they are exact, measured durations with a
cause attached. That is what fills the `off-CPU ms` and `blocked` columns of
[`stax threads`](@/guide/inspecting-a-run.md#stax-threads).

### Wakeups

stax also tracks **wakeup edges** — which thread *woke* a sleeping thread.
When thread A releases a lock that thread B was blocked on, that is a wakeup
from A to B. It is the thread of causality that turns "B waited 200 ms on a
lock" into "…and A is what kept it waiting." On macOS this comes for free
with the scheduler events; on Linux it needs the `staxd` broker, because the
underlying tracepoint is root-only — see
[Platform Support](@/concepts/platform-support.md). Off-CPU intervals
themselves are recorded either way.

## Self vs total

Each sample is a whole stack, so any one function can be counted two ways:

- **self** — the function was the *leaf*, the innermost frame: the CPU was
  executing *its* instructions.
- **total** — the function was *somewhere* on the stack: its own code, or
  anything it called, was running.

`stax top --sort self` ranks where the CPU literally is. `--sort total` ranks
which work is *responsible* — and naturally floats callers like `main` or a
runtime's poll loop to the top, because all the hot leaves run beneath them.
Both are useful; they answer different questions.

## How many samples is enough?

Because sampling is statistical, a profile from a handful of samples is
noise. A function that truly takes 1% of the time might show up as 0% or 3%
until you have collected enough.

This is exactly what [`stax wait --for-samples N`](@/guide/run-lifecycle.md#stax-wait)
is for: start a recording, block until N samples have landed, *then* query.
Tens of thousands of samples give a stable picture; a few hundred do not. As
a rule of thumb, the rarer the thing you are hunting, the more samples you
need before the numbers settle.

## See also

- [Recording a Run](@/guide/recording.md) — setting the frequency.
- [Stack Unwinding](@/concepts/stack-unwinding.md) — turning each sample's PC
  into a full stack.
- [Inspecting a Run](@/guide/inspecting-a-run.md) — the on-CPU and off-CPU
  views.
