use std::collections::HashMap;

use nperf_live_proto::TopEntry;

#[derive(Clone, Copy)]
pub struct RawTopEntry {
    pub address: u64,
    pub self_count: u64,
    pub total_count: u64,
}

/// Aggregated state for one specific thread.
#[derive(Default)]
pub struct ThreadStats {
    self_counts: HashMap<u64, u64>,
    total_counts: HashMap<u64, u64>,
    total_samples: u64,
    pub(crate) flame_root: StackNode,
}

impl ThreadStats {
    pub fn record(&mut self, user_addrs: &[u64]) {
        self.total_samples += 1;
        if let Some(&leaf) = user_addrs.first() {
            *self.self_counts.entry(leaf).or_insert(0) += 1;
        }
        let mut seen: smallset::SmallSet = Default::default();
        for &addr in user_addrs {
            if seen.insert(addr) {
                *self.total_counts.entry(addr).or_insert(0) += 1;
            }
        }
        // Build the call tree: user_addrs is leaf-first, walk reversed
        // so children are callees of their parent.
        let mut node = &mut self.flame_root;
        for &addr in user_addrs.iter().rev() {
            node = node.children.entry(addr).or_default();
            node.count += 1;
        }
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
}

#[derive(Default)]
pub(crate) struct StackNode {
    pub(crate) count: u64,
    pub(crate) children: HashMap<u64, StackNode>,
}

impl Aggregator {
    pub fn record(&mut self, tid: u32, user_addrs: &[u64]) {
        self.threads.entry(tid).or_default().record(user_addrs);
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
            })
            .collect()
    }

    /// Top-N as raw addresses + counts, optionally filtered to one
    /// thread. When `tid` is `None` we union all threads' counts.
    pub fn top_raw(&self, limit: usize, tid: Option<u32>) -> Vec<RawTopEntry> {
        let mut entries: Vec<RawTopEntry> = match tid {
            Some(tid) => match self.threads.get(&tid) {
                Some(t) => collect_top(&t.self_counts, &t.total_counts),
                None => Vec::new(),
            },
            None => {
                // Merge across threads.
                let mut self_counts: HashMap<u64, u64> = HashMap::new();
                let mut total_counts: HashMap<u64, u64> = HashMap::new();
                for t in self.threads.values() {
                    for (&a, &c) in &t.self_counts {
                        *self_counts.entry(a).or_insert(0) += c;
                    }
                    for (&a, &c) in &t.total_counts {
                        *total_counts.entry(a).or_insert(0) += c;
                    }
                }
                collect_top(&self_counts, &total_counts)
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
        for (&addr, child) in &other.children {
            self.children.entry(addr).or_default().merge(child);
        }
    }
}

impl Clone for StackNode {
    fn clone(&self) -> Self {
        Self {
            count: self.count,
            children: self.children.clone(),
        }
    }
}

fn collect_top(
    self_counts: &HashMap<u64, u64>,
    total_counts: &HashMap<u64, u64>,
) -> Vec<RawTopEntry> {
    total_counts
        .iter()
        .map(|(&address, &total_count)| RawTopEntry {
            address,
            self_count: self_counts.get(&address).copied().unwrap_or(0),
            total_count,
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
