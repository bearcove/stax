use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use stax_live_proto::{
    AppEventEntry, AppEventValue, ContractListUpdate, ContractStatusEntry, CounterQueryParams,
    CounterSampleEntry, CounterSeriesEntry, CounterSeriesUpdate, EventListUpdate, EventQueryParams,
    IncidentCounterEvidence, IncidentQueryParams, IncidentSchedulerEvidence, IncidentStackEvidence,
    IncidentUpdate, ObservabilityDiagnostics, OffCpuReason, SavedObservability,
    SavedTargetContract, SavedTargetCounterSet, SavedTargetEventKind, SymbolRef,
    TargetContractDuty, TargetContractId, TargetContractKind, TargetContractRecord,
    TargetContractSeverity, TargetCounterSamplePoint, TargetCounterSampleRecord,
    TargetCounterScalar, TargetCounterSetId, TargetCounterSetRecord, TargetEventKindId,
    TargetEventKindRecord, TargetEventRecord, TargetSignalBatch, TargetSignalSelector,
    TargetViolation, TargetViolationId, TimeRange, ViolationListUpdate, ViolationQueryParams,
};

use crate::{Aggregator, BinaryRegistry, IntervalKind};

const MAX_EVENTS: usize = 250_000;
const MAX_COUNTER_SAMPLES: usize = 250_000;
const SYNTH_TID_BASE: u32 = 0xFFF0_0000;
const MAX_NAME_BYTES: usize = 128;
const MAX_DESCRIPTION_BYTES: usize = 512;
const MAX_FIELDS: usize = 16;
const PET_JOIN_DISTANCE_NS: u64 = 100_000_000;
const TIMESTAMP_BOUND_SLOP_NS: u64 = 5_000_000_000;

#[derive(Default)]
pub struct ObservabilityStore {
    event_kinds: HashMap<(u32, TargetEventKindId), TargetEventKindRecord>,
    events: VecDeque<TargetEventRecord>,
    counter_sets: HashMap<(u32, TargetCounterSetId), TargetCounterSetRecord>,
    counter_samples: VecDeque<TargetCounterSampleRecord>,
    contracts: HashMap<(u32, TargetContractId), TargetContractRecord>,
    diagnostics: ObservabilityDiagnostics,
}

impl ObservabilityStore {
    pub fn to_saved(&self) -> SavedObservability {
        let mut event_kinds: Vec<_> = self
            .event_kinds
            .iter()
            .map(|(&(source_pid, _), definition)| SavedTargetEventKind {
                source_pid,
                definition: definition.clone(),
            })
            .collect();
        event_kinds.sort_by_key(|entry| (entry.source_pid, entry.definition.event_kind_id.raw));
        let mut counter_sets: Vec<_> = self
            .counter_sets
            .iter()
            .map(|(&(source_pid, _), definition)| SavedTargetCounterSet {
                source_pid,
                definition: definition.clone(),
            })
            .collect();
        counter_sets.sort_by_key(|entry| (entry.source_pid, entry.definition.counter_set_id.raw));
        let mut contracts: Vec<_> = self
            .contracts
            .iter()
            .map(|(&(source_pid, _), definition)| SavedTargetContract {
                source_pid,
                definition: definition.clone(),
            })
            .collect();
        contracts.sort_by_key(|entry| (entry.source_pid, entry.definition.contract_id.raw));
        SavedObservability {
            event_kinds,
            events: self.events.iter().cloned().collect(),
            counter_sets,
            counter_samples: self.counter_samples.iter().cloned().collect(),
            contracts,
            diagnostics: self.diagnostics.clone(),
        }
    }

    pub fn replace_from_saved(&mut self, saved: SavedObservability) {
        self.event_kinds = saved
            .event_kinds
            .into_iter()
            .map(|entry| {
                (
                    (entry.source_pid, entry.definition.event_kind_id),
                    entry.definition,
                )
            })
            .collect();
        self.events = saved.events.into();
        self.counter_sets = saved
            .counter_sets
            .into_iter()
            .map(|entry| {
                (
                    (entry.source_pid, entry.definition.counter_set_id),
                    entry.definition,
                )
            })
            .collect();
        self.counter_samples = saved.counter_samples.into();
        self.contracts = saved
            .contracts
            .into_iter()
            .map(|entry| {
                (
                    (entry.source_pid, entry.definition.contract_id),
                    entry.definition,
                )
            })
            .collect();
        self.diagnostics = saved.diagnostics;
        self.sort_samples();
    }

    pub fn diagnostics(&self) -> &ObservabilityDiagnostics {
        &self.diagnostics
    }

    pub fn record_dropped_no_active_run(&mut self, signals: u64) {
        self.diagnostics.signal_batches_dropped_no_active_run += 1;
        self.diagnostics.signals_dropped_no_active_run += signals;
    }

    pub fn record_dropped_wrong_pid(&mut self, signals: u64) {
        self.diagnostics.signal_batches_dropped_wrong_pid += 1;
        self.diagnostics.signals_dropped_wrong_pid += signals;
    }

    pub fn ingest(
        &mut self,
        batch: TargetSignalBatch,
        session_start: Option<u64>,
        last_event: Option<u64>,
    ) -> Option<(u64, u64)> {
        self.diagnostics.signal_batches += 1;
        self.diagnostics.events_received += batch.events.len() as u64;
        self.diagnostics.counter_samples_received += batch.counter_samples.len() as u64;
        for definition in batch.event_kinds {
            if !valid_event_definition(&definition) {
                self.diagnostics.definitions_conflicting += 1;
                continue;
            }
            insert_definition(
                &mut self.event_kinds,
                (batch.pid, definition.event_kind_id),
                definition,
                &mut self.diagnostics,
            );
        }
        for definition in batch.counter_sets {
            if !valid_counter_definition(&definition) {
                self.diagnostics.definitions_conflicting += 1;
                continue;
            }
            insert_definition(
                &mut self.counter_sets,
                (batch.pid, definition.counter_set_id),
                definition,
                &mut self.diagnostics,
            );
        }
        for definition in batch.contracts {
            if !valid_contract_definition(&definition) {
                self.diagnostics.definitions_conflicting += 1;
                continue;
            }
            insert_definition(
                &mut self.contracts,
                (batch.pid, definition.contract_id),
                definition,
                &mut self.diagnostics,
            );
        }
        let mut accepted_bounds: Option<(u64, u64)> = None;
        for event in batch.events {
            let Some(definition) = self.event_kinds.get(&(batch.pid, event.event_kind_id)) else {
                self.diagnostics.samples_unknown_definition += 1;
                continue;
            };
            if event.values.len() != definition.fields.len() {
                self.diagnostics.samples_bad_value_count += 1;
                continue;
            }
            if invalid_tid(batch.pid, event.source_pid, event.tid) {
                self.diagnostics.samples_invalid_tid += 1;
                continue;
            }
            if invalid_timestamp(event.timestamp_ns, session_start, last_event) {
                self.diagnostics.samples_bad_timestamp += 1;
                continue;
            }
            extend_bounds(&mut accepted_bounds, event.timestamp_ns);
            push_bounded(
                &mut self.events,
                event,
                MAX_EVENTS,
                &mut self.diagnostics.events_dropped_store_full,
            );
            self.diagnostics.events_recorded += 1;
        }
        for sample in batch.counter_samples {
            let Some(definition) = self
                .counter_sets
                .get(&(batch.pid, sample.counter_set_id))
                .cloned()
            else {
                self.diagnostics.samples_unknown_definition += 1;
                continue;
            };

            if sample.values.len() != definition.counters.len() {
                self.diagnostics.samples_bad_value_count += 1;
                continue;
            }
            if invalid_tid(batch.pid, sample.source_pid, sample.tid) {
                self.diagnostics.samples_invalid_tid += 1;
                continue;
            }
            let Some(timestamp) = sample_timestamp(&sample) else {
                self.diagnostics.samples_bad_timestamp += 1;
                continue;
            };
            if invalid_timestamp(timestamp, session_start, last_event) {
                self.diagnostics.samples_bad_timestamp += 1;
                continue;
            }
            self.check_monotonic_regression(batch.pid, &definition, &sample);

            push_bounded(
                &mut self.counter_samples,
                sample,
                MAX_COUNTER_SAMPLES,
                &mut self.diagnostics.counter_samples_dropped_store_full,
            );
            self.diagnostics.counter_samples_recorded += 1;
            extend_bounds(&mut accepted_bounds, timestamp);
        }
        self.sort_samples();
        accepted_bounds
    }

    fn check_monotonic_regression(
        &mut self,
        pid: u32,
        definition: &TargetCounterSetRecord,
        sample: &TargetCounterSampleRecord,
    ) {
        for (index, value) in sample.values.iter().enumerate() {
            if !definition
                .counters
                .get(index)
                .is_some_and(|counter| counter.monotonic)
            {
                continue;
            }
            let previous = self
                .counter_samples
                .iter()
                .rev()
                .find(|previous| {
                    previous.source_pid == sample.source_pid
                        && previous.counter_set_id == sample.counter_set_id
                        && previous.values.get(index).is_some()
                })
                .and_then(|previous| previous.values.get(index));
            if previous.is_some_and(|previous| scalar_cmp(value, previous) == Some(Ordering::Less))
            {
                self.diagnostics.monotonic_regressions += 1;
            }
        }
        let _ = pid;
    }

    fn sort_samples(&mut self) {
        self.events
            .make_contiguous()
            .sort_by_key(|event| (event.timestamp_ns, event.event_id.raw));
        self.counter_samples
            .make_contiguous()
            .sort_by_key(|sample| {
                (
                    sample_timestamp(sample).unwrap_or(0),
                    sample.counter_sample_id.raw,
                )
            });
    }

    pub fn events_update(&self, params: &EventQueryParams, session_start: u64) -> EventListUpdate {
        let mut events = Vec::new();
        let mut total = 0u64;
        let limit = params.limit.max(1) as usize;
        for event in &self.events {
            let relative = event.timestamp_ns.saturating_sub(session_start);
            if !in_window(relative, params.window.as_ref())
                || params.tid.is_some_and(|tid| event.tid != Some(tid))
            {
                continue;
            }
            let Some(definition) = self
                .event_kinds
                .get(&(event.source_pid, event.event_kind_id))
                .or_else(|| {
                    self.event_kinds
                        .values()
                        .find(|d| d.event_kind_id == event.event_kind_id)
                })
            else {
                continue;
            };
            if params
                .name_contains
                .as_ref()
                .is_some_and(|needle| !definition.name.contains(needle))
            {
                continue;
            }
            total += 1;
            if events.len() < limit {
                events.push(decode_event(event, definition, session_start));
            }
        }
        EventListUpdate {
            truncated: total as usize > events.len(),
            total_matching: total,
            events,
        }
    }

    pub fn counters_update(
        &self,
        params: &CounterQueryParams,
        session_start: u64,
    ) -> CounterSeriesUpdate {
        let mut groups: BTreeMap<
            (u32, TargetCounterSetId, usize),
            Vec<&TargetCounterSampleRecord>,
        > = BTreeMap::new();
        for sample in &self.counter_samples {
            let Some(timestamp) = sample_timestamp(sample) else {
                continue;
            };
            let relative = timestamp.saturating_sub(session_start);
            if !in_window(relative, params.window.as_ref()) {
                continue;
            }
            for index in 0..sample.values.len() {
                groups
                    .entry((sample.source_pid, sample.counter_set_id, index))
                    .or_default()
                    .push(sample);
            }
        }
        let mut counters = Vec::new();
        let limit = params.limit.max(1) as usize;
        let mut truncated = false;
        for ((source_pid, set_id, index), samples) in groups {
            let Some(set) = self.counter_sets.get(&(source_pid, set_id)).or_else(|| {
                self.counter_sets
                    .values()
                    .find(|d| d.counter_set_id == set_id)
            }) else {
                continue;
            };
            let Some(definition) = set.counters.get(index) else {
                continue;
            };
            let full_name = format!("{}.{}", set.name, definition.name);
            if params
                .name_contains
                .as_ref()
                .is_some_and(|needle| !full_name.contains(needle))
            {
                continue;
            }
            if counters.len() >= limit {
                truncated = true;
                break;
            }
            let values: Vec<_> = samples
                .iter()
                .filter_map(|sample| sample.values.get(index))
                .cloned()
                .collect();
            let first = values.first().cloned();
            let last = values.last().cloned();
            let min = values.iter().cloned().min_by(scalar_total_cmp);
            let max = values.iter().cloned().max_by(scalar_total_cmp);
            let delta = first
                .as_ref()
                .zip(last.as_ref())
                .and_then(|(first, last)| scalar_sub(last, first));
            let last_change_ns = samples.windows(2).rev().find_map(|pair| {
                let a = pair[0].values.get(index)?;
                let b = pair[1].values.get(index)?;
                (scalar_cmp(a, b) != Some(Ordering::Equal)).then(|| {
                    sample_timestamp(pair[1])
                        .unwrap_or(0)
                        .saturating_sub(session_start)
                })
            });
            let exact = if params.include_samples {
                samples
                    .iter()
                    .map(|sample| counter_sample_entry(sample, index, session_start))
                    .collect()
            } else {
                Vec::new()
            };
            counters.push(CounterSeriesEntry {
                source_pid,
                counter_set_id: set_id,
                counter_index: index as u32,
                name: full_name,
                description: definition.description.clone(),
                unit: definition.unit,
                monotonic: definition.monotonic,
                count: samples.len() as u64,
                first,
                last,
                min,
                max,
                delta,
                last_change_ns,
                samples: exact,
            });
        }
        CounterSeriesUpdate {
            counters,
            truncated,
        }
    }

    pub fn violations(
        &self,
        aggregator: &Aggregator,
        binaries: &BinaryRegistry,
    ) -> Vec<TargetViolation> {
        let session_start = aggregator.session_start_ns().unwrap_or(0);
        let run_end = aggregator.last_event_ns().unwrap_or(session_start);
        let mut violations = Vec::new();
        for (&(pid, _), contract) in &self.contracts {
            let duty = self.duty_windows(pid, &contract.duty, session_start, run_end);
            let mut raw = match &contract.kind {
                TargetContractKind::MaxOffCpuInterval {
                    tid,
                    max_ns,
                    reasons,
                } => self.eval_off_cpu(
                    pid, contract, *tid, *max_ns, reasons, duty, aggregator, binaries, run_end,
                ),
                TargetContractKind::MaxSignalGap { signal, max_ns } => {
                    self.eval_signal_gap(pid, contract, signal, *max_ns, duty, session_start)
                }
                TargetContractKind::MaxLatency {
                    start_event,
                    end_event,
                    max_ns,
                } => self.eval_latency(
                    pid,
                    contract,
                    *start_event,
                    *end_event,
                    *max_ns,
                    duty,
                    session_start,
                    run_end,
                ),
            };
            coalesce(&mut raw, contract_threshold(contract));
            violations.extend(raw);
        }
        violations.sort_by_key(|violation| (violation.start_ns, violation.violation_id.raw));
        violations
    }

    pub fn violations_update(
        &self,
        params: &ViolationQueryParams,
        aggregator: &Aggregator,
        binaries: &BinaryRegistry,
    ) -> ViolationListUpdate {
        let mut violations: Vec<_> = self
            .violations(aggregator, binaries)
            .into_iter()
            .filter(|violation| {
                in_range(violation.start_ns, violation.end_ns, params.window.as_ref())
                    && params.minimum_severity.is_none_or(|severity| {
                        severity_rank(violation.severity) >= severity_rank(severity)
                    })
            })
            .collect();
        violations.sort_by(|a, b| {
            b.open
                .cmp(&a.open)
                .then_with(|| b.excess_ns.cmp(&a.excess_ns))
                .then_with(|| a.start_ns.cmp(&b.start_ns))
        });
        let total = violations.len();
        violations.truncate(params.limit.max(1) as usize);
        ViolationListUpdate {
            truncated: total > violations.len(),
            total_matching: total as u64,
            violations,
        }
    }

    pub fn contracts_update(
        &self,
        aggregator: &Aggregator,
        binaries: &BinaryRegistry,
    ) -> ContractListUpdate {
        let violations = self.violations(aggregator, binaries);
        let contracts = self
            .contracts
            .iter()
            .map(|(&(source_pid, contract_id), contract)| {
                let matching: Vec<_> = violations
                    .iter()
                    .filter(|violation| {
                        violation.source_pid == source_pid && violation.contract_id == contract_id
                    })
                    .collect();
                ContractStatusEntry {
                    source_pid,
                    contract: contract.clone(),
                    evaluations: matching.len() as u64,
                    violations: matching.len() as u64,
                    currently_violating: matching.iter().any(|violation| violation.open),
                }
            })
            .collect();
        ContractListUpdate { contracts }
    }

    pub fn incident(
        &self,
        params: &IncidentQueryParams,
        aggregator: &Aggregator,
        binaries: &BinaryRegistry,
    ) -> IncidentUpdate {
        let Some(violation) = self
            .violations(aggregator, binaries)
            .into_iter()
            .find(|violation| violation.violation_id == params.violation_id)
        else {
            return IncidentUpdate::default();
        };
        let contract = self
            .contracts
            .get(&(violation.source_pid, violation.contract_id))
            .cloned();
        let margin = params
            .margin_ns
            .unwrap_or_else(|| 50_000_000.max(violation.actual_ns / 10));
        let window = TimeRange {
            start_ns: violation.start_ns.saturating_sub(margin),
            end_ns: violation.end_ns.saturating_add(margin),
        };
        let session_start = aggregator.session_start_ns().unwrap_or(0);
        let absolute_start = session_start.saturating_add(window.start_ns);
        let absolute_end = session_start.saturating_add(window.end_ns);
        let scheduler = violation.tid.and_then(|tid| {
            aggregator
                .iter_intervals(Some(tid))
                .find_map(|(_, interval)| {
                    let end = if interval.end_ns == 0 {
                        aggregator.last_event_ns().unwrap_or(interval.start_ns)
                    } else {
                        interval.end_ns
                    };
                    if interval.start_ns >= absolute_end || end <= absolute_start {
                        return None;
                    }
                    let IntervalKind::OffCpu {
                        stack,
                        waker_tid,
                        waker_user_stack,
                    } = &interval.kind
                    else {
                        return None;
                    };
                    Some(IncidentSchedulerEvidence {
                        tid,
                        start_ns: interval.start_ns.saturating_sub(session_start),
                        end_ns: end.saturating_sub(session_start),
                        reason: classify_stack(stack, binaries),
                        blocking_stack: resolve_stack(stack, binaries),
                        waker_tid: *waker_tid,
                        waker_stack: waker_user_stack
                            .as_deref()
                            .map(|stack| resolve_stack(stack, binaries))
                            .unwrap_or_default(),
                    })
                })
        });
        let mut nearest_pet = Vec::new();
        if let Some(tid) = violation.tid {
            for timestamp_ns in [
                absolute_start,
                session_start.saturating_add(violation.start_ns),
                absolute_end,
            ] {
                if let Ok(nearest) = aggregator.nearest_pet_stack_with_distance(
                    tid,
                    timestamp_ns,
                    PET_JOIN_DISTANCE_NS,
                ) {
                    nearest_pet.push(IncidentStackEvidence {
                        timestamp_ns: timestamp_ns.saturating_sub(session_start),
                        tid,
                        distance_ns: Some(nearest.distance_ns),
                        frames: resolve_stack(&nearest.stack, binaries),
                    });
                }
            }
        }
        let mut other_thread_stacks = Vec::new();
        for tid in aggregator
            .iter_threads()
            .filter(|tid| Some(*tid) != violation.tid)
            .take(16)
        {
            if let Ok(nearest) = aggregator.nearest_pet_stack_with_distance(
                tid,
                session_start.saturating_add(violation.start_ns),
                PET_JOIN_DISTANCE_NS,
            ) {
                other_thread_stacks.push(IncidentStackEvidence {
                    timestamp_ns: violation.start_ns,
                    tid,
                    distance_ns: Some(nearest.distance_ns),
                    frames: resolve_stack(&nearest.stack, binaries),
                });
            }
        }
        let events = self
            .events_update(
                &EventQueryParams {
                    run: None,
                    tid: None,
                    window: Some(window.clone()),
                    name_contains: None,
                    limit: 10_000,
                },
                session_start,
            )
            .events;
        let counters = self.incident_counters(&window, session_start);
        let markers = aggregator
            .markers()
            .iter()
            .filter_map(|marker| {
                let relative = marker.timestamp_ns.saturating_sub(session_start);
                in_window(relative, Some(&window)).then(|| stax_live_proto::RunMarker {
                    timestamp_ns: relative,
                    label: marker.label.clone(),
                })
            })
            .collect();
        let diagnostics = diagnostics_strings(&self.diagnostics);
        IncidentUpdate {
            violation: Some(violation),
            contract,
            window: Some(window),
            scheduler,
            nearest_pet,
            other_thread_stacks,
            target_spans: Vec::new(),
            events,
            counters,
            markers,
            diagnostics,
        }
    }

    fn incident_counters(
        &self,
        window: &TimeRange,
        session_start: u64,
    ) -> Vec<IncidentCounterEvidence> {
        let mut result = Vec::new();
        for (&(pid, set_id), set) in &self.counter_sets {
            for (index, definition) in set.counters.iter().enumerate() {
                let samples: Vec<_> = self
                    .counter_samples
                    .iter()
                    .filter(|sample| {
                        sample.source_pid == pid
                            && sample.counter_set_id == set_id
                            && sample.values.get(index).is_some()
                    })
                    .collect();
                if samples.is_empty() {
                    continue;
                }
                let before = samples
                    .iter()
                    .rev()
                    .find(|sample| {
                        sample_timestamp(sample)
                            .unwrap_or(0)
                            .saturating_sub(session_start)
                            < window.start_ns
                    })
                    .map(|sample| counter_sample_entry(sample, index, session_start));
                let inside: Vec<_> = samples
                    .iter()
                    .filter(|sample| {
                        in_window(
                            sample_timestamp(sample)
                                .unwrap_or(0)
                                .saturating_sub(session_start),
                            Some(window),
                        )
                    })
                    .map(|sample| counter_sample_entry(sample, index, session_start))
                    .collect();
                let after = samples
                    .iter()
                    .find(|sample| {
                        sample_timestamp(sample)
                            .unwrap_or(0)
                            .saturating_sub(session_start)
                            >= window.end_ns
                    })
                    .map(|sample| counter_sample_entry(sample, index, session_start));
                if before.is_none() && inside.is_empty() && after.is_none() {
                    continue;
                }
                let first_value = before
                    .as_ref()
                    .map(|entry| &entry.value)
                    .or_else(|| inside.first().map(|entry| &entry.value));
                let last_value = inside
                    .last()
                    .map(|entry| &entry.value)
                    .or_else(|| after.as_ref().map(|entry| &entry.value));
                let unchanged_ns = definition
                    .monotonic
                    .then(|| {
                        (first_value.zip(last_value).is_some_and(|(first, last)| {
                            scalar_cmp(first, last) == Some(Ordering::Equal)
                        }))
                        .then_some(window.end_ns - window.start_ns)
                    })
                    .flatten();
                result.push(IncidentCounterEvidence {
                    name: format!("{}.{}", set.name, definition.name),
                    unit: definition.unit,
                    monotonic: definition.monotonic,
                    before,
                    samples: inside,
                    after,
                    unchanged_ns,
                });
            }
        }
        result
    }

    fn duty_windows(
        &self,
        pid: u32,
        duty: &TargetContractDuty,
        start: u64,
        end: u64,
    ) -> Vec<(u64, u64)> {
        match duty {
            TargetContractDuty::EntireRun {
                startup_grace_ns,
                shutdown_grace_ns,
            } => vec![(
                start.saturating_add(*startup_grace_ns),
                end.saturating_sub(*shutdown_grace_ns),
            )],
            TargetContractDuty::WhileEventRecent {
                event_kind_id,
                within_ns,
            } => {
                let mut windows: Vec<_> = self
                    .events
                    .iter()
                    .filter(|event| {
                        event.event_kind_id == *event_kind_id
                            && (event.source_pid == pid
                                || self.event_kinds.contains_key(&(pid, *event_kind_id)))
                    })
                    .map(|event| {
                        (
                            event.timestamp_ns,
                            event.timestamp_ns.saturating_add(*within_ns).min(end),
                        )
                    })
                    .collect();
                merge_windows(&mut windows);
                windows
            }
        }
    }

    fn eval_off_cpu(
        &self,
        pid: u32,
        contract: &TargetContractRecord,
        tid: u32,
        max_ns: u64,
        reasons: &[OffCpuReason],
        duty: Vec<(u64, u64)>,
        aggregator: &Aggregator,
        binaries: &BinaryRegistry,
        run_end: u64,
    ) -> Vec<TargetViolation> {
        let session_start = aggregator.session_start_ns().unwrap_or(0);
        let mut out = Vec::new();
        for (_, interval) in aggregator.iter_intervals(Some(tid)) {
            let IntervalKind::OffCpu { stack, .. } = &interval.kind else {
                continue;
            };
            let reason = classify_stack(stack, binaries);
            if !reasons.is_empty() && !reasons.contains(&reason) {
                continue;
            }
            let end = if interval.end_ns == 0 {
                run_end
            } else {
                interval.end_ns
            };
            for (duty_start, duty_end) in &duty {
                let start = interval.start_ns.max(*duty_start);
                let end = end.min(*duty_end);
                let actual = end.saturating_sub(start);
                if actual <= max_ns {
                    continue;
                }
                out.push(make_violation(
                    pid,
                    contract,
                    start.saturating_sub(session_start),
                    end.saturating_sub(session_start),
                    actual,
                    max_ns,
                    Some(tid),
                    None,
                    interval.end_ns == 0,
                    Some(reason),
                ));
            }
        }
        out
    }

    fn eval_signal_gap(
        &self,
        pid: u32,
        contract: &TargetContractRecord,
        signal: &TargetSignalSelector,
        max_ns: u64,
        duty: Vec<(u64, u64)>,
        session_start: u64,
    ) -> Vec<TargetViolation> {
        let mut points: Vec<u64> = match signal {
            TargetSignalSelector::Event { event_kind_id } => self
                .events
                .iter()
                .filter(|event| event.event_kind_id == *event_kind_id)
                .map(|event| event.timestamp_ns)
                .collect(),
            TargetSignalSelector::Counter {
                counter_set_id,
                counter_index: _,
            } => self
                .counter_samples
                .iter()
                .filter(|sample| sample.counter_set_id == *counter_set_id)
                .filter_map(sample_timestamp)
                .collect(),
        };
        points.sort_unstable();
        let mut out = Vec::new();
        for (window_start, window_end) in duty {
            let mut previous = window_start;
            for point in points
                .iter()
                .copied()
                .filter(|point| *point >= window_start && *point <= window_end)
            {
                let actual = point.saturating_sub(previous);
                if actual > max_ns {
                    out.push(make_violation(
                        pid,
                        contract,
                        previous.saturating_sub(session_start),
                        point.saturating_sub(session_start),
                        actual,
                        max_ns,
                        None,
                        None,
                        false,
                        None,
                    ));
                }
                previous = point;
            }
            let actual = window_end.saturating_sub(previous);
            if actual > max_ns {
                out.push(make_violation(
                    pid,
                    contract,
                    previous.saturating_sub(session_start),
                    window_end.saturating_sub(session_start),
                    actual,
                    max_ns,
                    None,
                    None,
                    true,
                    None,
                ));
            }
        }
        out
    }

    fn eval_latency(
        &self,
        pid: u32,
        contract: &TargetContractRecord,
        start_kind: TargetEventKindId,
        end_kind: TargetEventKindId,
        max_ns: u64,
        duty: Vec<(u64, u64)>,
        session_start: u64,
        run_end: u64,
    ) -> Vec<TargetViolation> {
        let starts: BTreeMap<_, _> = self
            .events
            .iter()
            .filter(|event| event.event_kind_id == start_kind)
            .filter_map(|event| event.correlation_id.map(|id| (id, event)))
            .collect();
        let ends: BTreeMap<_, _> = self
            .events
            .iter()
            .filter(|event| event.event_kind_id == end_kind)
            .filter_map(|event| event.correlation_id.map(|id| (id, event)))
            .collect();
        starts
            .into_iter()
            .filter_map(|(correlation_id, start)| {
                if !duty.iter().any(|(duty_start, duty_end)| {
                    start.timestamp_ns >= *duty_start && start.timestamp_ns <= *duty_end
                }) {
                    return None;
                }
                let end = ends
                    .get(&correlation_id)
                    .map(|event| event.timestamp_ns)
                    .unwrap_or(run_end);
                let actual = end.saturating_sub(start.timestamp_ns);
                (actual > max_ns).then(|| {
                    make_violation(
                        pid,
                        contract,
                        start.timestamp_ns.saturating_sub(session_start),
                        end.saturating_sub(session_start),
                        actual,
                        max_ns,
                        start.tid,
                        Some(correlation_id),
                        !ends.contains_key(&correlation_id),
                        None,
                    )
                })
            })
            .collect()
    }
}

fn valid_event_definition(definition: &TargetEventKindRecord) -> bool {
    definition.name.len() <= MAX_NAME_BYTES
        && definition
            .description
            .as_ref()
            .is_none_or(|d| d.len() <= MAX_DESCRIPTION_BYTES)
        && definition.fields.len() <= MAX_FIELDS
}
fn valid_counter_definition(definition: &TargetCounterSetRecord) -> bool {
    definition.name.len() <= MAX_NAME_BYTES && definition.counters.len() <= MAX_FIELDS
}
fn valid_contract_definition(definition: &TargetContractRecord) -> bool {
    definition.name.len() <= MAX_NAME_BYTES
        && definition
            .description
            .as_ref()
            .is_none_or(|d| d.len() <= MAX_DESCRIPTION_BYTES)
}
fn insert_definition<K: Eq + Hash, V: PartialEq>(
    map: &mut HashMap<K, V>,
    key: K,
    value: V,
    diagnostics: &mut ObservabilityDiagnostics,
) {
    if let Some(existing) = map.get(&key) {
        if existing != &value {
            diagnostics.definitions_conflicting += 1;
        }
    } else {
        map.insert(key, value);
    }
}
fn invalid_tid(batch_pid: u32, source_pid: u32, tid: Option<u32>) -> bool {
    tid.is_some_and(|tid| source_pid != batch_pid || tid >= SYNTH_TID_BASE)
}
fn invalid_timestamp(timestamp: u64, start: Option<u64>, last: Option<u64>) -> bool {
    start.is_some_and(|start| timestamp.saturating_add(TIMESTAMP_BOUND_SLOP_NS) < start)
        || last.is_some_and(|last| timestamp > last.saturating_add(TIMESTAMP_BOUND_SLOP_NS))
}
fn extend_bounds(bounds: &mut Option<(u64, u64)>, timestamp: u64) {
    *bounds = Some(match *bounds {
        Some((start, end)) => (start.min(timestamp), end.max(timestamp)),
        None => (timestamp, timestamp),
    });
}
fn sample_timestamp(sample: &TargetCounterSampleRecord) -> Option<u64> {
    match sample.sample_point {
        TargetCounterSamplePoint::TimeSeries => sample.timestamp_ns,
        _ => sample.timestamp_ns,
    }
}
fn push_bounded<T>(queue: &mut VecDeque<T>, value: T, limit: usize, dropped: &mut u64) {
    if queue.len() == limit {
        queue.pop_front();
        *dropped += 1;
    }
    queue.push_back(value);
}
fn in_window(timestamp: u64, window: Option<&TimeRange>) -> bool {
    window.is_none_or(|window| timestamp >= window.start_ns && timestamp < window.end_ns)
}
fn in_range(start: u64, end: u64, window: Option<&TimeRange>) -> bool {
    window.is_none_or(|window| start < window.end_ns && end > window.start_ns)
}
fn decode_event(
    event: &TargetEventRecord,
    definition: &TargetEventKindRecord,
    session_start: u64,
) -> AppEventEntry {
    AppEventEntry {
        event_id: event.event_id,
        event_kind_id: event.event_kind_id,
        timestamp_ns: event.timestamp_ns.saturating_sub(session_start),
        source_pid: event.source_pid,
        tid: event.tid,
        name: definition.name.clone(),
        description: definition.description.clone(),
        correlation_id: event.correlation_id,
        values: definition
            .fields
            .iter()
            .zip(&event.values)
            .map(|(field, value)| AppEventValue {
                name: field.name.clone(),
                unit: field.unit,
                value: value.clone(),
            })
            .collect(),
    }
}
fn counter_sample_entry(
    sample: &TargetCounterSampleRecord,
    index: usize,
    session_start: u64,
) -> CounterSampleEntry {
    CounterSampleEntry {
        counter_sample_id: sample.counter_sample_id,
        timestamp_ns: sample_timestamp(sample)
            .unwrap_or(0)
            .saturating_sub(session_start),
        source_pid: sample.source_pid,
        tid: sample.tid,
        value: sample.values[index].clone(),
    }
}
fn scalar_cmp(a: &TargetCounterScalar, b: &TargetCounterScalar) -> Option<Ordering> {
    match (a, b) {
        (TargetCounterScalar::U64 { value: a }, TargetCounterScalar::U64 { value: b }) => {
            Some(a.cmp(b))
        }
        (TargetCounterScalar::I64 { value: a }, TargetCounterScalar::I64 { value: b }) => {
            Some(a.cmp(b))
        }
        (TargetCounterScalar::F64 { value: a }, TargetCounterScalar::F64 { value: b }) => {
            a.partial_cmp(b)
        }
        _ => None,
    }
}
fn scalar_total_cmp(a: &TargetCounterScalar, b: &TargetCounterScalar) -> Ordering {
    scalar_cmp(a, b).unwrap_or(Ordering::Equal)
}
fn scalar_sub(a: &TargetCounterScalar, b: &TargetCounterScalar) -> Option<TargetCounterScalar> {
    match (a, b) {
        (TargetCounterScalar::U64 { value: a }, TargetCounterScalar::U64 { value: b }) => {
            Some(TargetCounterScalar::I64 {
                value: (*a as i128 - *b as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64,
            })
        }
        (TargetCounterScalar::I64 { value: a }, TargetCounterScalar::I64 { value: b }) => {
            Some(TargetCounterScalar::I64 {
                value: a.saturating_sub(*b),
            })
        }
        (TargetCounterScalar::F64 { value: a }, TargetCounterScalar::F64 { value: b }) => {
            Some(TargetCounterScalar::F64 { value: a - b })
        }
        _ => None,
    }
}
fn severity_rank(severity: TargetContractSeverity) -> u8 {
    match severity {
        TargetContractSeverity::Info => 0,
        TargetContractSeverity::Warn => 1,
        TargetContractSeverity::Fail => 2,
    }
}
fn contract_threshold(contract: &TargetContractRecord) -> u64 {
    match contract.kind {
        TargetContractKind::MaxOffCpuInterval { max_ns, .. }
        | TargetContractKind::MaxSignalGap { max_ns, .. }
        | TargetContractKind::MaxLatency { max_ns, .. } => max_ns,
    }
}
fn merge_windows(windows: &mut Vec<(u64, u64)>) {
    windows.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for window in windows.drain(..) {
        if let Some(last) = merged.last_mut() {
            if window.0 <= last.1 {
                last.1 = last.1.max(window.1);
                continue;
            }
        }
        merged.push(window);
    }
    *windows = merged;
}
fn make_violation(
    pid: u32,
    contract: &TargetContractRecord,
    start_ns: u64,
    end_ns: u64,
    actual_ns: u64,
    threshold_ns: u64,
    tid: Option<u32>,
    correlation_id: Option<u64>,
    open: bool,
    reason: Option<OffCpuReason>,
) -> TargetViolation {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (pid, contract.contract_id, start_ns, end_ns, correlation_id).hash(&mut hasher);
    TargetViolation {
        violation_id: TargetViolationId::new(hasher.finish()),
        source_pid: pid,
        contract_id: contract.contract_id,
        contract_name: contract.name.clone(),
        severity: contract.severity,
        start_ns,
        end_ns,
        actual_ns,
        threshold_ns,
        excess_ns: actual_ns.saturating_sub(threshold_ns),
        tid,
        correlation_id,
        open,
        contributing_violations: 1,
        reason,
    }
}
fn coalesce(violations: &mut Vec<TargetViolation>, cooldown: u64) {
    violations.sort_by_key(|v| v.start_ns);
    let mut output: Vec<TargetViolation> = Vec::new();
    for violation in violations.drain(..) {
        if let Some(last) = output.last_mut() {
            if last.contract_id == violation.contract_id
                && last.correlation_id == violation.correlation_id
                && violation.start_ns <= last.end_ns.saturating_add(cooldown)
            {
                last.end_ns = last.end_ns.max(violation.end_ns);
                last.actual_ns = last.end_ns.saturating_sub(last.start_ns);
                last.excess_ns = last.actual_ns.saturating_sub(last.threshold_ns);
                last.open |= violation.open;
                last.contributing_violations += violation.contributing_violations;
                continue;
            }
        }
        output.push(violation);
    }
    *violations = output;
}
fn classify_stack(stack: &[u64], binaries: &BinaryRegistry) -> OffCpuReason {
    let leaf = stack
        .first()
        .and_then(|address| binaries.lookup_symbol(*address))
        .map(|symbol| symbol.function_name);
    crate::classify::classify_offcpu(leaf.as_deref())
}
fn resolve_stack(stack: &[u64], binaries: &BinaryRegistry) -> Vec<SymbolRef> {
    stack
        .iter()
        .map(|address| {
            binaries
                .lookup_symbol(*address)
                .map(|symbol| SymbolRef {
                    function_name: Some(symbol.function_name),
                    binary: Some(symbol.binary),
                })
                .unwrap_or(SymbolRef {
                    function_name: Some(format!("0x{address:x}")),
                    binary: None,
                })
        })
        .collect()
}
fn diagnostics_strings(diagnostics: &ObservabilityDiagnostics) -> Vec<String> {
    let mut out = Vec::new();
    if diagnostics.events_dropped_store_full > 0 {
        out.push(format!(
            "{} application events were evicted from bounded storage",
            diagnostics.events_dropped_store_full
        ));
    }
    if diagnostics.counter_samples_dropped_store_full > 0 {
        out.push(format!(
            "{} counter samples were evicted from bounded storage",
            diagnostics.counter_samples_dropped_store_full
        ));
    }
    if diagnostics.samples_bad_timestamp > 0 {
        out.push(format!(
            "{} signals had invalid timestamps",
            diagnostics.samples_bad_timestamp
        ));
    }
    if diagnostics.monotonic_regressions > 0 {
        out.push(format!(
            "{} monotonic counter regressions were observed",
            diagnostics.monotonic_regressions
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use stax_live_proto::{
        TargetCounterDefinition, TargetCounterUnit, TargetEventFieldDefinition, TargetEventId,
    };

    fn event_kind(id: u64, name: &str) -> TargetEventKindRecord {
        TargetEventKindRecord {
            event_kind_id: TargetEventKindId::new(id),
            name: name.to_owned(),
            description: None,
            fields: vec![TargetEventFieldDefinition {
                name: "value".to_owned(),
                unit: TargetCounterUnit::Count,
                description: None,
            }],
        }
    }

    #[test]
    fn definitions_dedupe_and_events_sort() {
        let mut store = ObservabilityStore::default();
        let definition = event_kind(1, "tick");
        store.ingest(
            TargetSignalBatch {
                pid: 7,
                event_kinds: vec![definition.clone(), definition],
                events: vec![
                    TargetEventRecord {
                        event_id: TargetEventId::new(2),
                        event_kind_id: TargetEventKindId::new(1),
                        timestamp_ns: 120,
                        source_pid: 7,
                        tid: Some(8),
                        correlation_id: None,
                        values: vec![TargetCounterScalar::U64 { value: 2 }],
                    },
                    TargetEventRecord {
                        event_id: TargetEventId::new(1),
                        event_kind_id: TargetEventKindId::new(1),
                        timestamp_ns: 110,
                        source_pid: 7,
                        tid: Some(8),
                        correlation_id: None,
                        values: vec![TargetCounterScalar::U64 { value: 1 }],
                    },
                ],
                ..TargetSignalBatch::default()
            },
            Some(100),
            Some(200),
        );
        let update = store.events_update(
            &EventQueryParams {
                run: None,
                tid: None,
                window: None,
                name_contains: None,
                limit: 10,
            },
            100,
        );
        assert_eq!(
            update
                .events
                .iter()
                .map(|event| event.timestamp_ns)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert_eq!(store.diagnostics.definitions_conflicting, 0);
    }

    #[test]
    fn early_async_events_extend_accepted_bounds() {
        let mut store = ObservabilityStore::default();
        let bounds = store.ingest(
            TargetSignalBatch {
                pid: 7,
                event_kinds: vec![event_kind(1, "tick")],
                events: vec![TargetEventRecord {
                    event_id: TargetEventId::new(1),
                    event_kind_id: TargetEventKindId::new(1),
                    timestamp_ns: 90,
                    source_pid: 7,
                    tid: Some(8),
                    correlation_id: None,
                    values: vec![TargetCounterScalar::U64 { value: 1 }],
                }],
                ..TargetSignalBatch::default()
            },
            Some(100),
            Some(200),
        );
        assert_eq!(bounds, Some((90, 90)));
        assert_eq!(store.diagnostics.samples_bad_timestamp, 0);
    }

    #[test]
    fn max_latency_reports_correlated_violation() {
        let mut store = ObservabilityStore::default();
        let start = event_kind(1, "start");
        let end = event_kind(2, "end");
        store.ingest(
            TargetSignalBatch {
                pid: 7,
                event_kinds: vec![start, end],
                events: vec![
                    TargetEventRecord {
                        event_id: TargetEventId::new(1),
                        event_kind_id: TargetEventKindId::new(1),
                        timestamp_ns: 110,
                        source_pid: 7,
                        tid: Some(8),
                        correlation_id: Some(9),
                        values: vec![TargetCounterScalar::U64 { value: 1 }],
                    },
                    TargetEventRecord {
                        event_id: TargetEventId::new(2),
                        event_kind_id: TargetEventKindId::new(2),
                        timestamp_ns: 180,
                        source_pid: 7,
                        tid: Some(8),
                        correlation_id: Some(9),
                        values: vec![TargetCounterScalar::U64 { value: 1 }],
                    },
                ],
                contracts: vec![TargetContractRecord {
                    contract_id: TargetContractId::new(3),
                    name: "latency".to_owned(),
                    description: None,
                    severity: TargetContractSeverity::Fail,
                    duty: TargetContractDuty::EntireRun {
                        startup_grace_ns: 0,
                        shutdown_grace_ns: 0,
                    },
                    kind: TargetContractKind::MaxLatency {
                        start_event: TargetEventKindId::new(1),
                        end_event: TargetEventKindId::new(2),
                        max_ns: 50,
                    },
                }],
                ..TargetSignalBatch::default()
            },
            Some(100),
            Some(200),
        );
        let mut aggregator = Aggregator::default();
        aggregator.record_pet_sample(8, 100, &[1], &[], crate::PmuSample::default());
        aggregator.record_pet_sample(8, 200, &[1], &[], crate::PmuSample::default());
        let violations = store.violations(&aggregator, &BinaryRegistry::new());
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].correlation_id, Some(9));
        assert_eq!(violations[0].actual_ns, 70);
        assert_eq!(violations[0].start_ns, 10);
        assert_eq!(violations[0].end_ns, 80);
    }

    #[test]
    fn signal_gap_uses_recording_relative_timestamps() {
        let mut store = ObservabilityStore::default();
        let tick = event_kind(1, "tick");
        store.ingest(
            TargetSignalBatch {
                pid: 7,
                event_kinds: vec![tick],
                events: vec![TargetEventRecord {
                    event_id: TargetEventId::new(1),
                    event_kind_id: TargetEventKindId::new(1),
                    timestamp_ns: 120,
                    source_pid: 7,
                    tid: Some(8),
                    correlation_id: None,
                    values: vec![TargetCounterScalar::U64 { value: 1 }],
                }],
                contracts: vec![TargetContractRecord {
                    contract_id: TargetContractId::new(3),
                    name: "cadence".to_owned(),
                    description: None,
                    severity: TargetContractSeverity::Fail,
                    duty: TargetContractDuty::EntireRun {
                        startup_grace_ns: 0,
                        shutdown_grace_ns: 0,
                    },
                    kind: TargetContractKind::MaxSignalGap {
                        signal: TargetSignalSelector::Event {
                            event_kind_id: TargetEventKindId::new(1),
                        },
                        max_ns: 10,
                    },
                }],
                ..TargetSignalBatch::default()
            },
            Some(100),
            Some(200),
        );
        let mut aggregator = Aggregator::default();
        aggregator.record_pet_sample(8, 100, &[1], &[], crate::PmuSample::default());
        aggregator.record_pet_sample(8, 200, &[1], &[], crate::PmuSample::default());
        let violations = store.violations(&aggregator, &BinaryRegistry::new());
        assert_eq!(violations.len(), 1);
        assert_eq!((violations[0].start_ns, violations[0].end_ns), (0, 100));
        assert_eq!(violations[0].contributing_violations, 2);
    }

    #[test]
    fn monotonic_counter_regression_is_diagnosed() {
        let mut store = ObservabilityStore::default();
        store.ingest(
            TargetSignalBatch {
                pid: 7,
                counter_sets: vec![TargetCounterSetRecord {
                    counter_set_id: TargetCounterSetId::new(1),
                    name: "render".to_owned(),
                    counters: vec![TargetCounterDefinition {
                        name: "F".to_owned(),
                        unit: TargetCounterUnit::Count,
                        description: None,
                        monotonic: true,
                    }],
                }],
                counter_samples: vec![
                    TargetCounterSampleRecord {
                        counter_sample_id: stax_live_proto::TargetCounterSampleId::new(1),
                        counter_set_id: TargetCounterSetId::new(1),
                        dispatch_id: None,
                        command_buffer_id: None,
                        sample_point: TargetCounterSamplePoint::TimeSeries,
                        timestamp_ns: Some(110),
                        source_pid: 7,
                        tid: Some(8),
                        values: vec![TargetCounterScalar::U64 { value: 2 }],
                        error: None,
                    },
                    TargetCounterSampleRecord {
                        counter_sample_id: stax_live_proto::TargetCounterSampleId::new(2),
                        counter_set_id: TargetCounterSetId::new(1),
                        dispatch_id: None,
                        command_buffer_id: None,
                        sample_point: TargetCounterSamplePoint::TimeSeries,
                        timestamp_ns: Some(120),
                        source_pid: 7,
                        tid: Some(8),
                        values: vec![TargetCounterScalar::U64 { value: 1 }],
                        error: None,
                    },
                ],
                ..TargetSignalBatch::default()
            },
            Some(100),
            Some(200),
        );
        assert_eq!(store.diagnostics.monotonic_regressions, 1);
    }
}
