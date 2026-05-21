+++
title = "Concepts"
sort_by = "weight"
weight = 2
insert_anchor_links = "heading"
+++

The [Guide](@/guide/_index.md) tells you *what to type*. These pages explain
*how stax works* — enough that, when something is missing from a profile or a
number looks off, you know where to look.

- [Architecture](@/concepts/architecture.md) — the three processes, the two daemons, and the sockets between them
- [Platform Support](@/concepts/platform-support.md) — what macOS and Linux each capture, and what they don't
- [Stack Unwinding](@/concepts/stack-unwinding.md) — why your build needs frame pointers
- [Sampling](@/concepts/sampling.md) — what a profiler actually measures, on-CPU and off-CPU

If you only read one, read [Stack Unwinding](@/concepts/stack-unwinding.md) —
a build without frame pointers produces a profile with no call stacks, and
that surprises people.
