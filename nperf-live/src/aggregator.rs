use std::collections::HashMap;

use nperf_live_proto::TopEntry;

#[derive(Clone, Copy, Default)]
pub struct PmcAccum {
    pub cycles: u64,
    pub instructions: u64,
}

impl PmcAccum {
    fn add(&mut self, cycles: u64, instructions: u64) {
        self.cycles = self.cycles.saturating_add(cycles);
        self.instructions = self.instructions.saturating_add(instructions);
    }
}

#[derive(Clone, Copy)]
pub struct RawTopEntry {
    pub address: u64,
    pub self_count: u64,
    pub total_count: u64,
    pub self_pmc: PmcAccum,
    pub total_pmc: PmcAccum,
}

/// Result of `Aggregator::aggregate_filtered`: same shape as the
/// per-thread fields but built fresh from raw samples that pass a
/// predicate. The flame root is owned (no Cow) since it's freshly
/// constructed.
pub struct FilteredAggregation {
    pub self_counts: HashMap<u64, u64>,
    pub total_counts: HashMap<u64, u64>,
    pub self_pmc: HashMap<u64, PmcAccum>,
    pub total_pmc: HashMap<u64, PmcAccum>,
    pub total_samples: u64,
    pub flame_root: StackNode,
}

impl FilteredAggregation {
    pub fn top_raw(&self, limit: usize) -> Vec<RawTopEntry> {
        let mut entries: Vec<RawTopEntry> = self
            .total_counts
            .iter()
            .map(|(&address, &total_count)| RawTopEntry {
                address,
                self_count: self.self_counts.get(&address).copied().unwrap_or(0),
                total_count,
                self_pmc: self.self_pmc.get(&address).copied().unwrap_or_default(),
                total_pmc: self.total_pmc.get(&address).copied().unwrap_or_default(),
            })
            .collect();
        entries.sort_by(|a, b| {
            b.self_count
                .cmp(&a.self_count)
                .then_with(|| b.total_count.cmp(&a.total_count))
        });
        entries.truncate(limit);
        entries
    }

    pub fn self_count(&self, address: u64) -> u64 {
        self.self_counts.get(&address).copied().unwrap_or(0)
    }
}

/// Aggregated state for one specific thread, plus a capped log of raw
/// samples (timestamp + stack). The pre-aggregated maps + tree make
/// no-filter queries cheap; the raw log lets us re-aggregate over a
/// time slice or, eventually, exclude/focus on stacks containing a
/// given symbol.
pub struct ThreadStats {
    self_counts: HashMap<u64, u64>,
    total_counts: HashMap<u64, u64>,
    total_samples: u64,
    /// Same maps, but counting only off-CPU samples. Lets queries
    /// pick "wall clock" / "on-CPU only" / "off-CPU only" views by
    /// subtracting one from the other (no extra raw-log scan).
    offcpu_self_counts: HashMap<u64, u64>,
    offcpu_total_counts: HashMap<u64, u64>,
    offcpu_samples: u64,
    /// PMU counter accumulators (cycles + instructions retired)
    /// keyed by symbol address. `self_pmc` is attributed to the leaf
    /// frame of each sample; `total_pmc` includes the sample at every
    /// frame on its stack (matching `total_counts` semantics).
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
            self_counts: HashMap::new(),
            total_counts: HashMap::new(),
            total_samples: 0,
            offcpu_self_counts: HashMap::new(),
            offcpu_total_counts: HashMap::new(),
            offcpu_samples: 0,
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
    pub stack: Box<[u64]>,
    /// `true` if this sample was synthesised to fill an off-CPU
    /// interval (the thread was blocked, stack frozen from the last
    /// on-CPU sample). Lets the UI filter wall-clock vs strict
    /// on-CPU views.
    pub is_offcpu: bool,
    /// Apple Silicon fixed PMU counter deltas at this PET tick. 0
    /// for synthesised off-CPU samples (we have no real counter
    /// reading there) and 0 on the Linux backend.
    pub cycles: u64,
    pub instructions: u64,
}

/// Per-thread cap on the raw sample log. ~100k * (avg ~30 frames * 8B
/// + 24B header) ≈ 26 MB worst case, before slack — comfortable for
/// live sessions of several minutes. FIFO drop above this cap.
const MAX_SAMPLES_PER_THREAD: usize = 100_000;

impl ThreadStats {
    pub fn record(
        &mut self,
        timestamp_ns: u64,
        user_addrs: &[u64],
        is_offcpu: bool,
        cycles: u64,
        instructions: u64,
    ) {
        self.total_samples += 1;
        if is_offcpu {
            self.offcpu_samples += 1;
        }
        if let Some(&leaf) = user_addrs.first() {
            *self.self_counts.entry(leaf).or_insert(0) += 1;
            self.self_pmc
                .entry(leaf)
                .or_default()
                .add(cycles, instructions);
            if is_offcpu {
                *self.offcpu_self_counts.entry(leaf).or_insert(0) += 1;
            }
        }
        let mut seen: smallset::SmallSet = Default::default();
        for &addr in user_addrs {
            if seen.insert(addr) {
                *self.total_counts.entry(addr).or_insert(0) += 1;
                self.total_pmc
                    .entry(addr)
                    .or_default()
                    .add(cycles, instructions);
                if is_offcpu {
                    *self.offcpu_total_counts.entry(addr).or_insert(0) += 1;
                }
            }
        }
        // Build the call tree: user_addrs is leaf-first, walk reversed
        // so children are callees of their parent.
        let mut node = &mut self.flame_root;
        for &addr in user_addrs.iter().rev() {
            node = node.children.entry(addr).or_default();
            node.count += 1;
            node.pmc.add(cycles, instructions);
            if is_offcpu {
                node.offcpu_count += 1;
            }
        }

        // Append to the raw log; FIFO-drop the oldest when over cap.
        if self.samples.len() >= MAX_SAMPLES_PER_THREAD {
            self.samples.pop_front();
        }
        self.samples.push_back(RawSample {
            timestamp_ns,
            stack: user_addrs.to_vec().into_boxed_slice(),
            is_offcpu,
            cycles,
            instructions,
        });
    }

    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    pub fn self_count(&self, address: u64) -> u64 {
        self.self_counts.get(&address).copied().unwrap_or(0)
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
    pub(crate) count: u64,
    /// Subset of `count` from off-CPU samples; subtract from `count`
    /// to get on-CPU samples. Carried per-node so flame-graph views
    /// can pivot on it without rescanning the raw log.
    pub(crate) offcpu_count: u64,
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
        user_addrs: &[u64],
        is_offcpu: bool,
        cycles: u64,
        instructions: u64,
    ) {
        if self.session_start_ns.is_none() {
            self.session_start_ns = Some(timestamp_ns);
        }
        self.last_sample_ns = Some(timestamp_ns);
        self.threads.entry(tid).or_default().record(
            timestamp_ns,
            user_addrs,
            is_offcpu,
            cycles,
            instructions,
        );
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
        let mut self_counts: HashMap<u64, u64> = HashMap::new();
        let mut total_counts: HashMap<u64, u64> = HashMap::new();
        let mut self_pmc: HashMap<u64, PmcAccum> = HashMap::new();
        let mut total_pmc: HashMap<u64, PmcAccum> = HashMap::new();
        let mut total_samples: u64 = 0;
        let mut flame_root = StackNode::default();

        for (_tid, sample) in self.iter_samples(tid) {
            if !predicate(sample) {
                continue;
            }
            total_samples += 1;
            if let Some(&leaf) = sample.stack.first() {
                *self_counts.entry(leaf).or_insert(0) += 1;
                self_pmc
                    .entry(leaf)
                    .or_default()
                    .add(sample.cycles, sample.instructions);
            }
            let mut seen: smallset::SmallSet = Default::default();
            for &addr in sample.stack.iter() {
                if seen.insert(addr) {
                    *total_counts.entry(addr).or_insert(0) += 1;
                    total_pmc
                        .entry(addr)
                        .or_default()
                        .add(sample.cycles, sample.instructions);
                }
            }
            // Build the call tree rooted at the synthetic node, leaf-first
            // input → walk reversed for caller-first descent.
            let mut node = &mut flame_root;
            for &addr in sample.stack.iter().rev() {
                node = node.children.entry(addr).or_default();
                node.count += 1;
                node.pmc.add(sample.cycles, sample.instructions);
            }
        }

        FilteredAggregation {
            self_counts,
            total_counts,
            self_pmc,
            total_pmc,
            total_samples,
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

    /// Iterate (tid, sample_count) pairs for the live thread list.
    pub fn iter_threads(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.threads.iter().map(|(&tid, t)| (tid, t.total_samples))
    }

    /// Total samples across all threads (or just one when filtered).
    pub fn total_samples(&self, tid: Option<u32>) -> u64 {
        match tid {
            Some(tid) => self
                .threads
                .get(&tid)
                .map(|t| t.total_samples)
                .unwrap_or(0),
            None => self.threads.values().map(|t| t.total_samples).sum(),
        }
    }

    /// Self-count for `address`, optionally restricted to one thread.
    pub fn self_count(&self, address: u64, tid: Option<u32>) -> u64 {
        match tid {
            Some(tid) => self
                .threads
                .get(&tid)
                .map(|t| t.self_count(address))
                .unwrap_or(0),
            None => self.threads.values().map(|t| t.self_count(address)).sum(),
        }
    }

    pub fn top(&self, limit: usize) -> Vec<TopEntry> {
        self.top_raw(limit, None)
            .into_iter()
            .map(|e| TopEntry {
                address: e.address,
                self_count: e.self_count,
                total_count: e.total_count,
                function_name: None,
                binary: None,
                is_main: false,
                language: "unknown".to_owned(),
                self_cycles: e.self_pmc.cycles,
                self_instructions: e.self_pmc.instructions,
                total_cycles: e.total_pmc.cycles,
                total_instructions: e.total_pmc.instructions,
            })
            .collect()
    }

    /// Top-N as raw addresses + counts, optionally filtered to one
    /// thread. When `tid` is `None` we union all threads' counts.
    pub fn top_raw(&self, limit: usize, tid: Option<u32>) -> Vec<RawTopEntry> {
        let mut entries: Vec<RawTopEntry> = match tid {
            Some(tid) => match self.threads.get(&tid) {
                Some(t) => collect_top(
                    &t.self_counts,
                    &t.total_counts,
                    &t.self_pmc,
                    &t.total_pmc,
                ),
                None => Vec::new(),
            },
            None => {
                // Merge across threads.
                let mut self_counts: HashMap<u64, u64> = HashMap::new();
                let mut total_counts: HashMap<u64, u64> = HashMap::new();
                let mut self_pmc: HashMap<u64, PmcAccum> = HashMap::new();
                let mut total_pmc: HashMap<u64, PmcAccum> = HashMap::new();
                for t in self.threads.values() {
                    for (&a, &c) in &t.self_counts {
                        *self_counts.entry(a).or_insert(0) += c;
                    }
                    for (&a, &c) in &t.total_counts {
                        *total_counts.entry(a).or_insert(0) += c;
                    }
                    for (&a, &p) in &t.self_pmc {
                        self_pmc.entry(a).or_default().add(p.cycles, p.instructions);
                    }
                    for (&a, &p) in &t.total_pmc {
                        total_pmc.entry(a).or_default().add(p.cycles, p.instructions);
                    }
                }
                collect_top(&self_counts, &total_counts, &self_pmc, &total_pmc)
            }
        };
        entries.sort_by(|a, b| {
            b.self_count
                .cmp(&a.self_count)
                .then_with(|| b.total_count.cmp(&a.total_count))
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
        self.count += other.count;
        self.offcpu_count += other.offcpu_count;
        self.pmc.add(other.pmc.cycles, other.pmc.instructions);
        for (&addr, child) in &other.children {
            self.children.entry(addr).or_default().merge(child);
        }
    }
}

impl Clone for StackNode {
    fn clone(&self) -> Self {
        Self {
            count: self.count,
            offcpu_count: self.offcpu_count,
            pmc: self.pmc,
            children: self.children.clone(),
        }
    }
}

fn collect_top(
    self_counts: &HashMap<u64, u64>,
    total_counts: &HashMap<u64, u64>,
    self_pmc: &HashMap<u64, PmcAccum>,
    total_pmc: &HashMap<u64, PmcAccum>,
) -> Vec<RawTopEntry> {
    total_counts
        .iter()
        .map(|(&address, &total_count)| RawTopEntry {
            address,
            self_count: self_counts.get(&address).copied().unwrap_or(0),
            total_count,
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
