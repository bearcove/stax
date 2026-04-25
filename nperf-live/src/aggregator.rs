use std::collections::HashMap;

use nperf_live_proto::TopEntry;

#[derive(Clone, Copy)]
pub struct RawTopEntry {
    pub address: u64,
    pub self_count: u64,
    pub total_count: u64,
}

#[derive(Default)]
pub struct Aggregator {
    /// Self-count: the leaf frame of each sample.
    self_counts: HashMap<u64, u64>,
    /// Total-count: any time an address appears anywhere in the stack.
    total_counts: HashMap<u64, u64>,
    total_samples: u64,
}

impl Aggregator {
    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    pub fn self_count(&self, address: u64) -> u64 {
        self.self_counts.get(&address).copied().unwrap_or(0)
    }

    pub fn record(&mut self, user_addrs: &[u64]) {
        self.total_samples += 1;
        if let Some(&leaf) = user_addrs.first() {
            *self.self_counts.entry(leaf).or_insert(0) += 1;
        }

        // Each address contributes once per sample to total, even if it
        // appears multiple times (recursion).
        let mut seen: smallset::SmallSet = Default::default();
        for &addr in user_addrs {
            if seen.insert(addr) {
                *self.total_counts.entry(addr).or_insert(0) += 1;
            }
        }
    }

    pub fn top(&self, limit: usize) -> Vec<TopEntry> {
        self.top_raw(limit)
            .into_iter()
            .map(|e| TopEntry {
                address: e.address,
                self_count: e.self_count,
                total_count: e.total_count,
                function_name: None,
                binary: None,
            })
            .collect()
    }

    /// Top-N as raw addresses + counts, for callers (the live server)
    /// that want to layer symbol resolution on top.
    pub fn top_raw(&self, limit: usize) -> Vec<RawTopEntry> {
        let mut entries: Vec<RawTopEntry> = self
            .self_counts
            .iter()
            .map(|(&address, &self_count)| RawTopEntry {
                address,
                self_count,
                total_count: self.total_counts.get(&address).copied().unwrap_or(0),
            })
            .collect();
        entries.sort_by(|a, b| b.self_count.cmp(&a.self_count));
        entries.truncate(limit);
        entries
    }
}

mod smallset {
    /// Tiny set optimised for typical stack depths (<32). Linear search; no allocs
    /// unless a stack is huge.
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
