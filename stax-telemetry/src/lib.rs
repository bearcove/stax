use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use facet::Facet;

const DEFAULT_EVENT_CAPACITY: usize = 256;
const DEFAULT_HISTOGRAM_BUCKETS: &[u64] = &[
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
];

#[derive(Clone, Debug, Facet)]
pub struct TelemetrySnapshot {
    pub component: String,
    pub generated_at_unix_ns: u64,
    pub counters: Vec<CounterSnapshot>,
    pub gauges: Vec<GaugeSnapshot>,
    pub histograms: Vec<HistogramSnapshot>,
    pub phases: Vec<PhaseSnapshot>,
    pub recent_events: Vec<RecentEventSnapshot>,
}

#[derive(Clone, Debug, Facet)]
pub struct CounterSnapshot {
    pub name: String,
    pub value: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct GaugeSnapshot {
    pub name: String,
    pub value: i64,
}

#[derive(Clone, Debug, Facet)]
pub struct HistogramSnapshot {
    pub name: String,
    pub count: u64,
    pub sum: u64,
    pub max: u64,
    pub buckets: Vec<HistogramBucketSnapshot>,
    pub overflow: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct HistogramBucketSnapshot {
    pub le: u64,
    pub count: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct PhaseSnapshot {
    pub name: String,
    pub state: String,
    pub detail: String,
    pub entered_at_unix_ns: u64,
    pub elapsed_ns: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct RecentEventSnapshot {
    pub at_unix_ns: u64,
    pub name: String,
    pub detail: String,
}

#[derive(Clone)]
pub struct TelemetryRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    component: String,
    counters: Mutex<HashMap<String, Arc<AtomicU64>>>,
    gauges: Mutex<HashMap<String, Arc<AtomicI64>>>,
    histograms: Mutex<HashMap<String, Arc<HistogramInner>>>,
    phases: Mutex<HashMap<String, Arc<PhaseInner>>>,
    events: Mutex<VecDeque<RecentEventSnapshot>>,
    event_capacity: usize,
}

struct HistogramInner {
    buckets: Vec<u64>,
    counts: Vec<AtomicU64>,
    overflow: AtomicU64,
    count: AtomicU64,
    sum: AtomicU64,
    max: AtomicU64,
}

struct PhaseInner {
    state: Mutex<PhaseState>,
}

struct PhaseState {
    state: String,
    detail: String,
    entered_at_unix_ns: u64,
    entered_at: Instant,
}

#[derive(Clone)]
pub struct CounterHandle {
    value: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct GaugeHandle {
    value: Arc<AtomicI64>,
}

#[derive(Clone)]
pub struct HistogramHandle {
    inner: Arc<HistogramInner>,
}

#[derive(Clone)]
pub struct PhaseHandle {
    inner: Arc<PhaseInner>,
}

impl TelemetryRegistry {
    pub fn new(component: impl Into<String>) -> Self {
        Self::with_event_capacity(component, DEFAULT_EVENT_CAPACITY)
    }

    pub fn with_event_capacity(component: impl Into<String>, event_capacity: usize) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                component: component.into(),
                counters: Mutex::new(HashMap::new()),
                gauges: Mutex::new(HashMap::new()),
                histograms: Mutex::new(HashMap::new()),
                phases: Mutex::new(HashMap::new()),
                events: Mutex::new(VecDeque::with_capacity(event_capacity)),
                event_capacity,
            }),
        }
    }

    pub fn counter(&self, name: impl Into<String>) -> CounterHandle {
        let name = name.into();
        let mut counters = self
            .inner
            .counters
            .lock()
            .expect("telemetry counters mutex poisoned");
        let value = counters
            .entry(name)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        CounterHandle { value }
    }

    pub fn gauge(&self, name: impl Into<String>) -> GaugeHandle {
        let name = name.into();
        let mut gauges = self
            .inner
            .gauges
            .lock()
            .expect("telemetry gauges mutex poisoned");
        let value = gauges
            .entry(name)
            .or_insert_with(|| Arc::new(AtomicI64::new(0)))
            .clone();
        GaugeHandle { value }
    }

    pub fn histogram(&self, name: impl Into<String>) -> HistogramHandle {
        self.histogram_with_buckets(name, DEFAULT_HISTOGRAM_BUCKETS.iter().copied())
    }

    pub fn histogram_with_buckets(
        &self,
        name: impl Into<String>,
        buckets: impl IntoIterator<Item = u64>,
    ) -> HistogramHandle {
        let name = name.into();
        let mut histograms = self
            .inner
            .histograms
            .lock()
            .expect("telemetry histograms mutex poisoned");
        let inner = histograms
            .entry(name)
            .or_insert_with(|| Arc::new(HistogramInner::new(buckets)))
            .clone();
        HistogramHandle { inner }
    }

    pub fn phase(&self, name: impl Into<String>) -> PhaseHandle {
        let name = name.into();
        let mut phases = self
            .inner
            .phases
            .lock()
            .expect("telemetry phases mutex poisoned");
        let inner = phases
            .entry(name)
            .or_insert_with(|| Arc::new(PhaseInner::new("idle", "")))
            .clone();
        PhaseHandle { inner }
    }

    pub fn event(&self, name: impl Into<String>, detail: impl Into<String>) {
        let mut events = self
            .inner
            .events
            .lock()
            .expect("telemetry events mutex poisoned");
        if self.inner.event_capacity == 0 {
            return;
        }
        while events.len() >= self.inner.event_capacity {
            events.pop_front();
        }
        events.push_back(RecentEventSnapshot {
            at_unix_ns: now_unix_ns(),
            name: name.into(),
            detail: detail.into(),
        });
    }

    pub fn snapshot(&self) -> TelemetrySnapshot {
        let mut counters: Vec<_> = self
            .inner
            .counters
            .lock()
            .expect("telemetry counters mutex poisoned")
            .iter()
            .map(|(name, value)| CounterSnapshot {
                name: name.clone(),
                value: value.load(Ordering::Relaxed),
            })
            .collect();
        counters.sort_by(|a, b| a.name.cmp(&b.name));

        let mut gauges: Vec<_> = self
            .inner
            .gauges
            .lock()
            .expect("telemetry gauges mutex poisoned")
            .iter()
            .map(|(name, value)| GaugeSnapshot {
                name: name.clone(),
                value: value.load(Ordering::Relaxed),
            })
            .collect();
        gauges.sort_by(|a, b| a.name.cmp(&b.name));

        let mut histograms: Vec<_> = self
            .inner
            .histograms
            .lock()
            .expect("telemetry histograms mutex poisoned")
            .iter()
            .map(|(name, inner)| inner.snapshot(name.clone()))
            .collect();
        histograms.sort_by(|a, b| a.name.cmp(&b.name));

        let mut phases: Vec<_> = self
            .inner
            .phases
            .lock()
            .expect("telemetry phases mutex poisoned")
            .iter()
            .map(|(name, inner)| inner.snapshot(name.clone()))
            .collect();
        phases.sort_by(|a, b| a.name.cmp(&b.name));

        let recent_events = self
            .inner
            .events
            .lock()
            .expect("telemetry events mutex poisoned")
            .iter()
            .cloned()
            .collect();

        TelemetrySnapshot {
            component: self.inner.component.clone(),
            generated_at_unix_ns: now_unix_ns(),
            counters,
            gauges,
            histograms,
            phases,
            recent_events,
        }
    }
}

impl CounterHandle {
    pub fn inc(&self, amount: u64) {
        self.value.fetch_add(amount, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

impl GaugeHandle {
    pub fn set(&self, value: i64) {
        self.value.store(value, Ordering::Relaxed);
    }

    pub fn inc(&self, amount: i64) {
        self.value.fetch_add(amount, Ordering::Relaxed);
    }

    pub fn dec(&self, amount: i64) {
        self.value.fetch_sub(amount, Ordering::Relaxed);
    }

    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }
}

impl HistogramHandle {
    pub fn record(&self, value: u64) {
        self.inner.record(value);
    }

    pub fn record_duration(&self, duration: Duration) {
        self.record(duration.as_nanos().min(u128::from(u64::MAX)) as u64);
    }
}

impl PhaseHandle {
    pub fn enter(&self, state: impl Into<String>, detail: impl Into<String>) {
        *self
            .inner
            .state
            .lock()
            .expect("telemetry phase mutex poisoned") = PhaseState {
            state: state.into(),
            detail: detail.into(),
            entered_at_unix_ns: now_unix_ns(),
            entered_at: Instant::now(),
        };
    }
}

impl HistogramInner {
    fn new(buckets: impl IntoIterator<Item = u64>) -> Self {
        let mut buckets: Vec<u64> = buckets.into_iter().collect();
        buckets.sort_unstable();
        buckets.dedup();
        let counts = buckets.iter().map(|_| AtomicU64::new(0)).collect();
        Self {
            buckets,
            counts,
            overflow: AtomicU64::new(0),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            max: AtomicU64::new(0),
        }
    }

    fn record(&self, value: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
        self.max.fetch_max(value, Ordering::Relaxed);
        match self.buckets.iter().position(|bucket| value <= *bucket) {
            Some(index) => {
                self.counts[index].fetch_add(1, Ordering::Relaxed);
            }
            None => {
                self.overflow.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn snapshot(&self, name: String) -> HistogramSnapshot {
        let mut cumulative = 0;
        HistogramSnapshot {
            name,
            count: self.count.load(Ordering::Relaxed),
            sum: self.sum.load(Ordering::Relaxed),
            max: self.max.load(Ordering::Relaxed),
            buckets: self
                .buckets
                .iter()
                .zip(self.counts.iter())
                .map(|(le, count)| HistogramBucketSnapshot {
                    le: *le,
                    count: {
                        cumulative += count.load(Ordering::Relaxed);
                        cumulative
                    },
                })
                .collect(),
            overflow: self.overflow.load(Ordering::Relaxed),
        }
    }
}

impl PhaseInner {
    fn new(state: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            state: Mutex::new(PhaseState {
                state: state.into(),
                detail: detail.into(),
                entered_at_unix_ns: now_unix_ns(),
                entered_at: Instant::now(),
            }),
        }
    }

    fn snapshot(&self, name: String) -> PhaseSnapshot {
        let state = self.state.lock().expect("telemetry phase mutex poisoned");
        PhaseSnapshot {
            name,
            state: state.state.clone(),
            detail: state.detail.clone(),
            entered_at_unix_ns: state.entered_at_unix_ns,
            elapsed_ns: state
                .entered_at
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
        }
    }
}

pub fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_ring_is_bounded() {
        let registry = TelemetryRegistry::with_event_capacity("test", 2);
        registry.event("a", "1");
        registry.event("b", "2");
        registry.event("c", "3");

        let snapshot = registry.snapshot();
        let names: Vec<_> = snapshot
            .recent_events
            .iter()
            .map(|event| event.name.as_str())
            .collect();
        assert_eq!(names, ["b", "c"]);
    }

    #[test]
    fn histogram_records_overflow_and_max() {
        let registry = TelemetryRegistry::new("test");
        let histogram = registry.histogram_with_buckets("latency", [10, 20]);
        histogram.record(5);
        histogram.record(20);
        histogram.record(25);

        let snapshot = registry.snapshot();
        let histogram = &snapshot.histograms[0];
        assert_eq!(histogram.count, 3);
        assert_eq!(histogram.max, 25);
        assert_eq!(histogram.buckets[0].count, 1);
        assert_eq!(histogram.buckets[1].count, 2);
        assert_eq!(histogram.overflow, 1);
    }
}
