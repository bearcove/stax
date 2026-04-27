use std::collections::HashMap;

use nperf_live_proto::TopEntry;

#[derive(Clone, Copy, Default)]
pub struct PmcAccum {
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,
}

impl PmcAccum {
    fn add(&mut self, s: &PmuSample) {
        self.cycles = self.cycles.saturating_add(s.cycles);
        self.instructions = self.instructions.saturating_add(s.instructions);
        self.l1d_misses = self.l1d_misses.saturating_add(s.l1d_misses);
        self.branch_mispreds = self
            .branch_mispreds
            .saturating_add(s.branch_mispreds);
    }

    fn add_other(&mut self, other: &PmcAccum) {
        self.cycles = self.cycles.saturating_add(other.cycles);
        self.instructions = self.instructions.saturating_add(other.instructions);
        self.l1d_misses = self.l1d_misses.saturating_add(other.l1d_misses);
        self.branch_mispreds = self
            .branch_mispreds
            .saturating_add(other.branch_mispreds);
    }
}

/// Per-sample PMU values handed in via `Aggregator::record`. Shaped
/// identically to `PmcAccum` but distinct so the API is "totals
/// vs. one sample" at a glance.
#[derive(Clone, Copy, Default)]
pub struct PmuSample {
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,
}

/// One row in `Aggregator::top_wakers`: how many times waker_tid
/// (with leaf frame at `waker_leaf_address`) woke the queried thread.
/// Wakeups are counts, not durations -- they're discrete events.
#[derive(Clone, Copy)]
pub struct RawWakerEntry {
    pub waker_tid: u32,
    pub waker_leaf_address: u64,
    pub count: u64,
}

#[derive(Clone, Copy)]
pub struct RawTopEntry {
    pub address: u64,
    /// Wall-clock time attributed to this address as the leaf frame,
    /// in nanoseconds (sum across every sample whose stack ended in
    /// this address).
    pub self_duration_ns: u64,
    /// Wall-clock time attributed to this address anywhere on the
    /// stack, in nanoseconds.
    pub total_duration_ns: u64,
    pub self_pmc: PmcAccum,
    pub total_pmc: PmcAccum,
}

/// Result of `Aggregator::aggregate_filtered`: same shape as the
/// per-thread fields but built fresh from raw samples that pass a
/// predicate. The flame root is owned (no Cow) since it's freshly
/// constructed.
pub struct FilteredAggregation {
    /// `address -> ns of wall-clock attributed to that address as a
    /// leaf frame` (sum of `RawSample::duration_ns` across samples
    /// whose stack ended in that address).
    pub self_durations: HashMap<u64, u64>,
    /// Same idea but attribution is "any frame on the stack",
    /// matching `total_pmc` semantics.
    pub total_durations: HashMap<u64, u64>,
    pub self_pmc: HashMap<u64, PmcAccum>,
    pub total_pmc: HashMap<u64, PmcAccum>,
    /// Total wall-clock time covered by samples that passed the
    /// predicate, in nanoseconds.
    pub total_duration_ns: u64,
    pub flame_root: StackNode,
}

impl FilteredAggregation {
    pub fn top_raw(&self, limit: usize) -> Vec<RawTopEntry> {
        let mut entries: Vec<RawTopEntry> = self
            .total_durations
            .iter()
            .map(|(&address, &total_duration_ns)| RawTopEntry {
                address,
                self_duration_ns: self.self_durations.get(&address).copied().unwrap_or(0),
                total_duration_ns,
                self_pmc: self.self_pmc.get(&address).copied().unwrap_or_default(),
                total_pmc: self.total_pmc.get(&address).copied().unwrap_or_default(),
            })
            .collect();
        entries.sort_by(|a, b| {
            b.self_duration_ns
                .cmp(&a.self_duration_ns)
                .then_with(|| b.total_duration_ns.cmp(&a.total_duration_ns))
        });
        entries.truncate(limit);
        entries
    }

    pub fn self_duration_ns(&self, address: u64) -> u64 {
        self.self_durations.get(&address).copied().unwrap_or(0)
    }
}

/// Aggregated state for one specific thread, plus a capped log of raw
/// samples. The pre-aggregated maps + tree make no-filter queries
/// cheap; the raw log lets us re-aggregate over a time slice or
/// exclude/focus on stacks containing a given symbol.
///
/// Every accumulated value is in nanoseconds of wall-clock time --
/// the aggregator's unit is duration, not sample count.
pub struct ThreadStats {
    self_durations: HashMap<u64, u64>,
    total_durations: HashMap<u64, u64>,
    /// Wall-clock time covered by samples for this thread, in ns.
    total_duration_ns: u64,
    /// Wakeups received by this thread, capped FIFO so memory is
    /// bounded the same way `samples` is.
    pub(crate) wakeups: std::collections::VecDeque<RawWakeup>,
    /// Same maps, but counting only off-CPU samples. Lets queries
    /// pick "wall clock" / "on-CPU only" / "off-CPU only" views by
    /// subtracting one from the other (no extra raw-log scan).
    offcpu_self_durations: HashMap<u64, u64>,
    offcpu_total_durations: HashMap<u64, u64>,
    offcpu_duration_ns: u64,
    /// PMU counter accumulators (cycles + instructions retired)
    /// keyed by symbol address. `self_pmc` is attributed to the leaf
    /// frame of each sample; `total_pmc` includes the sample at every
    /// frame on its stack (matching `total_durations` semantics).
    self_pmc: HashMap<u64, PmcAccum>,
    total_pmc: HashMap<u64, PmcAccum>,
    pub(crate) flame_root: StackNode,
    /// FIFO ring of raw samples. Capped at MAX_SAMPLES_PER_THREAD so
    /// memory doesn't grow unbounded; when full, we drop the oldest
    /// (the pre-aggregations stay intact).
    pub(crate) samples: std::collections::VecDeque<RawSample>,
}

impl Default for ThreadStats {
    fn default() -> Self {
        Self {
            self_durations: HashMap::new(),
            total_durations: HashMap::new(),
            total_duration_ns: 0,
            wakeups: std::collections::VecDeque::new(),
            offcpu_self_durations: HashMap::new(),
            offcpu_total_durations: HashMap::new(),
            offcpu_duration_ns: 0,
            self_pmc: HashMap::new(),
            total_pmc: HashMap::new(),
            flame_root: StackNode::default(),
            samples: std::collections::VecDeque::new(),
        }
    }
}

/// One captured sample. Stack is leaf-first (matches what the sampler
/// feeds in). Kept boxed so the VecDeque stores fixed-size handles.
pub struct RawSample {
    pub timestamp_ns: u64,
    /// How much wall-clock time this sample accounts for. Sampling
    /// period for on-CPU PET ticks (1ms at 1kHz); interval length
    /// for off-CPU intervals.
    pub duration_ns: u64,
    pub stack: Box<[u64]>,
    /// `true` if this sample stands in for an off-CPU interval (the
    /// thread was blocked at this stack for `duration_ns`). Lets the
    /// UI filter wall-clock vs strict on-CPU views.
    pub is_offcpu: bool,
    /// PMU counter deltas at this PET tick. All-zero for synthesised
    /// off-CPU samples and on the Linux backend.
    pub pmc: PmuSample,
}

/// One observed wakeup edge. We keep them in a FIFO ring per wakee
/// and rebuild aggregations on subscription, the same way RawSample
/// works for the on-CPU flame graph.
pub struct RawWakeup {
    pub timestamp_ns: u64,
    pub waker_tid: u32,
    pub waker_user_stack: Box<[u64]>,
    pub waker_kernel_stack: Box<[u64]>,
}

/// Per-thread cap on the raw sample log. ~100k * (avg ~30 frames * 8B
/// + 24B header) ≈ 26 MB worst case, before slack — comfortable for
/// live sessions of several minutes. FIFO drop above this cap.
const MAX_SAMPLES_PER_THREAD: usize = 100_000;

impl ThreadStats {
    pub fn record(
        &mut self,
        timestamp_ns: u64,
        duration_ns: u64,
        user_addrs: &[u64],
        is_offcpu: bool,
        pmc: PmuSample,
    ) {
        self.total_duration_ns = self.total_duration_ns.saturating_add(duration_ns);
        if is_offcpu {
            self.offcpu_duration_ns =
                self.offcpu_duration_ns.saturating_add(duration_ns);
        }
        if let Some(&leaf) = user_addrs.first() {
            *self.self_durations.entry(leaf).or_insert(0) += duration_ns;
            self.self_pmc.entry(leaf).or_default().add(&pmc);
            if is_offcpu {
                *self.offcpu_self_durations.entry(leaf).or_insert(0) += duration_ns;
            }
        }
        let mut seen: smallset::SmallSet = Default::default();
        for &addr in user_addrs {
            if seen.insert(addr) {
                *self.total_durations.entry(addr).or_insert(0) += duration_ns;
                self.total_pmc.entry(addr).or_default().add(&pmc);
                if is_offcpu {
                    *self.offcpu_total_durations.entry(addr).or_insert(0) += duration_ns;
                }
            }
        }
        // Build the call tree: user_addrs is leaf-first, walk reversed
        // so children are callees of their parent.
        let mut node = &mut self.flame_root;
        for &addr in user_addrs.iter().rev() {
            node = node.children.entry(addr).or_default();
            node.duration_ns = node.duration_ns.saturating_add(duration_ns);
            node.pmc.add(&pmc);
            if is_offcpu {
                node.offcpu_duration_ns =
                    node.offcpu_duration_ns.saturating_add(duration_ns);
            }
        }

        // Append to the raw log; FIFO-drop the oldest when over cap.
        if self.samples.len() >= MAX_SAMPLES_PER_THREAD {
            self.samples.pop_front();
        }
        self.samples.push_back(RawSample {
            timestamp_ns,
            duration_ns,
            stack: user_addrs.to_vec().into_boxed_slice(),
            is_offcpu,
            pmc,
        });
    }

    pub fn total_duration_ns(&self) -> u64 {
        self.total_duration_ns
    }

    pub fn self_duration_ns(&self, address: u64) -> u64 {
        self.self_durations.get(&address).copied().unwrap_or(0)
    }
}

/// Process-wide aggregator: per-thread state plus thread name lookup.
/// "All-threads" queries iterate and merge across threads on demand —
/// avoids keeping a duplicate combined index.
#[derive(Default)]
pub struct Aggregator {
    threads: HashMap<u32, ThreadStats>,
    thread_names: HashMap<u32, String>,
    /// First sample timestamp we ever saw, in ns. Used as the timeline
    /// origin so the UI shows "0s" at the start of recording rather
    /// than a giant Mach absolute time.
    session_start_ns: Option<u64>,
    /// Most recent sample timestamp; gives the timeline a known end.
    last_sample_ns: Option<u64>,
}

#[derive(Default)]
pub struct StackNode {
    /// Wall-clock time attributed to this node, in nanoseconds. Sum
    /// of `RawSample::duration_ns` across every sample whose stack
    /// passed through this node.
    pub(crate) duration_ns: u64,
    /// Subset of `duration_ns` from off-CPU samples; subtract from
    /// `duration_ns` to get on-CPU duration. Carried per-node so
    /// flame-graph views can pivot on it without rescanning the raw
    /// log.
    pub(crate) offcpu_duration_ns: u64,
    /// Sum of cycle/instruction counter deltas across every sample
    /// that traversed this node. Lets per-node IPC fall straight out
    /// of the call tree.
    pub(crate) pmc: PmcAccum,
    pub(crate) children: HashMap<u64, StackNode>,
}

impl Aggregator {
    pub fn record(
        &mut self,
        tid: u32,
        timestamp_ns: u64,
        duration_ns: u64,
        user_addrs: &[u64],
        is_offcpu: bool,
        pmc: PmuSample,
    ) {
        if self.session_start_ns.is_none() {
            self.session_start_ns = Some(timestamp_ns);
        }
        self.last_sample_ns = Some(timestamp_ns);
        self.threads
            .entry(tid)
            .or_default()
            .record(timestamp_ns, duration_ns, user_addrs, is_offcpu, pmc);
    }

    /// Append one wakeup edge into the wakee's per-thread ledger.
    /// FIFO-cap matches `MAX_SAMPLES_PER_THREAD` so memory stays
    /// bounded for long-lived recordings.
    pub fn record_wakeup(
        &mut self,
        timestamp_ns: u64,
        waker_tid: u32,
        wakee_tid: u32,
        waker_user_stack: Vec<u64>,
        waker_kernel_stack: Vec<u64>,
    ) {
        let stats = self.threads.entry(wakee_tid).or_default();
        if stats.wakeups.len() >= MAX_SAMPLES_PER_THREAD {
            stats.wakeups.pop_front();
        }
        stats.wakeups.push_back(RawWakeup {
            timestamp_ns,
            waker_tid,
            waker_user_stack: waker_user_stack.into_boxed_slice(),
            waker_kernel_stack: waker_kernel_stack.into_boxed_slice(),
        });
    }

    /// Aggregate wakers for a given wakee tid: top-N (waker_tid +
    /// waker leaf-frame) groups by count. Used by the live UI's
    /// "who woke me?" panel.
    pub fn top_wakers(&self, wakee_tid: u32, limit: usize) -> Vec<RawWakerEntry> {
        let Some(stats) = self.threads.get(&wakee_tid) else {
            return Vec::new();
        };
        let mut groups: HashMap<(u32, u64), RawWakerEntry> = HashMap::new();
        for w in &stats.wakeups {
            // Pick the leaf user frame as the representative
            // address, falling back to the leaf kernel frame.
            let leaf = w
                .waker_user_stack
                .first()
                .copied()
                .or_else(|| w.waker_kernel_stack.first().copied())
                .unwrap_or(0);
            let key = (w.waker_tid, leaf);
            groups
                .entry(key)
                .and_modify(|e| e.count += 1)
                .or_insert(RawWakerEntry {
                    waker_tid: w.waker_tid,
                    waker_leaf_address: leaf,
                    count: 1,
                });
        }
        let mut out: Vec<RawWakerEntry> = groups.into_values().collect();
        out.sort_by(|a, b| b.count.cmp(&a.count));
        out.truncate(limit);
        out
    }

    pub fn session_start_ns(&self) -> Option<u64> {
        self.session_start_ns
    }

    pub fn last_sample_ns(&self) -> Option<u64> {
        self.last_sample_ns
    }

    /// Filter-aware re-aggregation. Walks the raw sample log,
    /// applies the predicate to each sample, and rebuilds the
    /// aggregations we need for top-N / flamegraph / neighbors. When
    /// the predicate accepts every sample the result is identical to
    /// the pre-aggregated state (just slower); the fast path bypasses
    /// this.
    pub fn aggregate_filtered<P>(
        &self,
        tid: Option<u32>,
        mut predicate: P,
    ) -> FilteredAggregation
    where
        P: FnMut(&RawSample) -> bool,
    {
        let mut self_durations: HashMap<u64, u64> = HashMap::new();
        let mut total_durations: HashMap<u64, u64> = HashMap::new();
        let mut self_pmc: HashMap<u64, PmcAccum> = HashMap::new();
        let mut total_pmc: HashMap<u64, PmcAccum> = HashMap::new();
        let mut total_duration_ns: u64 = 0;
        let mut flame_root = StackNode::default();

        for (_tid, sample) in self.iter_samples(tid) {
            if !predicate(sample) {
                continue;
            }
            total_duration_ns = total_duration_ns.saturating_add(sample.duration_ns);
            if let Some(&leaf) = sample.stack.first() {
                *self_durations.entry(leaf).or_insert(0) += sample.duration_ns;
                self_pmc.entry(leaf).or_default().add(&sample.pmc);
            }
            let mut seen: smallset::SmallSet = Default::default();
            for &addr in sample.stack.iter() {
                if seen.insert(addr) {
                    *total_durations.entry(addr).or_insert(0) += sample.duration_ns;
                    total_pmc.entry(addr).or_default().add(&sample.pmc);
                }
            }
            // Build the call tree rooted at the synthetic node, leaf-first
            // input → walk reversed for caller-first descent.
            let mut node = &mut flame_root;
            for &addr in sample.stack.iter().rev() {
                node = node.children.entry(addr).or_default();
                node.duration_ns = node.duration_ns.saturating_add(sample.duration_ns);
                node.pmc.add(&sample.pmc);
                if sample.is_offcpu {
                    node.offcpu_duration_ns = node
                        .offcpu_duration_ns
                        .saturating_add(sample.duration_ns);
                }
            }
        }

        FilteredAggregation {
            self_durations,
            total_durations,
            self_pmc,
            total_pmc,
            total_duration_ns,
            flame_root,
        }
    }

    /// Iterate raw samples (timestamped + stacks) for a single thread,
    /// or for every thread when `tid` is `None`. Used for filter-aware
    /// queries that the pre-aggregated state can't answer.
    pub fn iter_samples<'a>(
        &'a self,
        tid: Option<u32>,
    ) -> Box<dyn Iterator<Item = (u32, &'a RawSample)> + 'a> {
        match tid {
            Some(tid) => match self.threads.get(&tid) {
                Some(t) => Box::new(t.samples.iter().map(move |s| (tid, s))),
                None => Box::new(std::iter::empty()),
            },
            None => Box::new(
                self.threads
                    .iter()
                    .flat_map(|(&tid, t)| t.samples.iter().map(move |s| (tid, s))),
            ),
        }
    }

    pub fn set_thread_name(&mut self, tid: u32, name: String) {
        self.thread_names.insert(tid, name);
    }

    pub fn thread_name(&self, tid: u32) -> Option<&str> {
        self.thread_names.get(&tid).map(|s| s.as_str())
    }

    /// Iterate (tid, total_duration_ns) pairs for the live thread list.
    pub fn iter_threads(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.threads.iter().map(|(&tid, t)| (tid, t.total_duration_ns))
    }

    /// Total wall-clock duration across all threads (or just one when
    /// filtered), in nanoseconds.
    pub fn total_duration_ns(&self, tid: Option<u32>) -> u64 {
        match tid {
            Some(tid) => self
                .threads
                .get(&tid)
                .map(|t| t.total_duration_ns)
                .unwrap_or(0),
            None => self.threads.values().map(|t| t.total_duration_ns).sum(),
        }
    }

    /// Self-duration for `address`, optionally restricted to one
    /// thread. In nanoseconds.
    pub fn self_duration_ns(&self, address: u64, tid: Option<u32>) -> u64 {
        match tid {
            Some(tid) => self
                .threads
                .get(&tid)
                .map(|t| t.self_duration_ns(address))
                .unwrap_or(0),
            None => self
                .threads
                .values()
                .map(|t| t.self_duration_ns(address))
                .sum(),
        }
    }

    pub fn top(&self, limit: usize) -> Vec<TopEntry> {
        self.top_raw(limit, None)
            .into_iter()
            .map(|e| TopEntry {
                address: e.address,
                self_duration_ns: e.self_duration_ns,
                total_duration_ns: e.total_duration_ns,
                function_name: None,
                binary: None,
                is_main: false,
                language: "unknown".to_owned(),
                self_cycles: e.self_pmc.cycles,
                self_instructions: e.self_pmc.instructions,
                self_l1d_misses: e.self_pmc.l1d_misses,
                self_branch_mispreds: e.self_pmc.branch_mispreds,
                total_cycles: e.total_pmc.cycles,
                total_instructions: e.total_pmc.instructions,
                total_l1d_misses: e.total_pmc.l1d_misses,
                total_branch_mispreds: e.total_pmc.branch_mispreds,
            })
            .collect()
    }

    /// Top-N as raw addresses + durations, optionally filtered to one
    /// thread. When `tid` is `None` we union all threads' durations.
    pub fn top_raw(&self, limit: usize, tid: Option<u32>) -> Vec<RawTopEntry> {
        let mut entries: Vec<RawTopEntry> = match tid {
            Some(tid) => match self.threads.get(&tid) {
                Some(t) => collect_top(
                    &t.self_durations,
                    &t.total_durations,
                    &t.self_pmc,
                    &t.total_pmc,
                ),
                None => Vec::new(),
            },
            None => {
                // Merge across threads.
                let mut self_durations: HashMap<u64, u64> = HashMap::new();
                let mut total_durations: HashMap<u64, u64> = HashMap::new();
                let mut self_pmc: HashMap<u64, PmcAccum> = HashMap::new();
                let mut total_pmc: HashMap<u64, PmcAccum> = HashMap::new();
                for t in self.threads.values() {
                    for (&a, &d) in &t.self_durations {
                        *self_durations.entry(a).or_insert(0) += d;
                    }
                    for (&a, &d) in &t.total_durations {
                        *total_durations.entry(a).or_insert(0) += d;
                    }
                    for (&a, &p) in &t.self_pmc {
                        self_pmc.entry(a).or_default().add_other(&p);
                    }
                    for (&a, &p) in &t.total_pmc {
                        total_pmc.entry(a).or_default().add_other(&p);
                    }
                }
                collect_top(&self_durations, &total_durations, &self_pmc, &total_pmc)
            }
        };
        entries.sort_by(|a, b| {
            b.self_duration_ns
                .cmp(&a.self_duration_ns)
                .then_with(|| b.total_duration_ns.cmp(&a.total_duration_ns))
        });
        entries.truncate(limit);
        entries
    }

    /// Build the call-tree root for the flamegraph view.
    /// When `tid` is `None`, return a fresh tree merged across threads.
    pub(crate) fn flame_root(&self, tid: Option<u32>) -> std::borrow::Cow<'_, StackNode> {
        match tid {
            Some(tid) => match self.threads.get(&tid) {
                Some(t) => std::borrow::Cow::Borrowed(&t.flame_root),
                None => std::borrow::Cow::Owned(StackNode::default()),
            },
            None => {
                let mut merged = StackNode::default();
                for t in self.threads.values() {
                    merged.merge(&t.flame_root);
                }
                std::borrow::Cow::Owned(merged)
            }
        }
    }
}

impl StackNode {
    fn merge(&mut self, other: &StackNode) {
        self.duration_ns = self.duration_ns.saturating_add(other.duration_ns);
        self.offcpu_duration_ns = self
            .offcpu_duration_ns
            .saturating_add(other.offcpu_duration_ns);
        self.pmc.add_other(&other.pmc);
        for (&addr, child) in &other.children {
            self.children.entry(addr).or_default().merge(child);
        }
    }
}

impl Clone for StackNode {
    fn clone(&self) -> Self {
        Self {
            duration_ns: self.duration_ns,
            offcpu_duration_ns: self.offcpu_duration_ns,
            pmc: self.pmc,
            children: self.children.clone(),
        }
    }
}

fn collect_top(
    self_durations: &HashMap<u64, u64>,
    total_durations: &HashMap<u64, u64>,
    self_pmc: &HashMap<u64, PmcAccum>,
    total_pmc: &HashMap<u64, PmcAccum>,
) -> Vec<RawTopEntry> {
    total_durations
        .iter()
        .map(|(&address, &total_duration_ns)| RawTopEntry {
            address,
            self_duration_ns: self_durations.get(&address).copied().unwrap_or(0),
            total_duration_ns,
            self_pmc: self_pmc.get(&address).copied().unwrap_or_default(),
            total_pmc: total_pmc.get(&address).copied().unwrap_or_default(),
        })
        .collect()
}

mod smallset {
    #[derive(Default)]
    pub struct SmallSet {
        items: Vec<u64>,
    }

    impl SmallSet {
        pub fn insert(&mut self, value: u64) -> bool {
            if self.items.contains(&value) {
                false
            } else {
                self.items.push(value);
                true
            }
        }
    }
}
