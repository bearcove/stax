use super::*;
use serde_json::json;

fn capture(events: Vec<Value>, footer: bool) -> String {
    let mut rows = vec![
        json!({"event":"header","format":"vix-eval-graph","version":1,"semantics":"lazy-dataflow","records":1}),
    ];
    rows.extend(events);
    if footer {
        rows.push(json!({"event":"footer","complete":true,"records":rows.len()+1}));
    }
    for (i, v) in rows.iter_mut().enumerate() {
        v["seq"] = json!(i + 1);
        v["ts_ns"] = json!(i);
    }
    rows.into_iter().map(|v| v.to_string() + "\n").collect()
}
fn graph(s: &str) -> Graph {
    Graph::read(s.as_bytes(), 1, "fixture.jsonl".into(), Limits::default()).unwrap()
}
fn site() -> Vec<Value> {
    vec![
        json!({"event":"module","module":1}),
        json!({"event":"site","module":1,"site":1,"kind":"function","name":"f\"\\\nλ","file":"a\".vix","start":0,"end":20,"line":1,"column":1,"params":["x"]}),
    ]
}
fn inv(n: u64) -> Value {
    json!({"event":"invocation","invocation":n,"module":1,"site":1,"arity":1})
}
fn alloc(n: u64) -> Value {
    json!({"event":"allocation","thunk":n,"words":8})
}
fn force(n: u64) -> Value {
    json!({"event":"force","invocation":n,"thunk":{"id":1,"identity":"allocated"},"state":0})
}
fn argument(inv: u64, thunk: u64) -> Value {
    json!({"event":"argument","invocation":inv,"index":0,"thunk":{"id":thunk,"identity":"allocated"},"state":0,"preview":{"text":"same \"\\\nλ","truncated":false}})
}

#[test]
fn shared_identity_multiplicity_and_exact_recorded_counts() {
    let mut e = site();
    e.extend([alloc(1), inv(1), inv(2), argument(1, 1), argument(2, 1)]);
    for _ in 0..25 {
        e.push(force(1));
    }
    e.push(force(2));
    e.push(json!({"event":"cache","invocation":1,"result":0}));
    e.push(json!({"event":"cache","invocation":2,"result":1}));
    let g = graph(&capture(e, true));
    assert!(g.summary.complete, "{:?}", g.summary.diagnostics);
    assert_eq!(
        (
            g.summary.invocations,
            g.summary.force_requests,
            g.summary.cache_computes,
            g.summary.cache_hits
        ),
        (2, 26, 1, 1)
    );
    let n = g.neighborhood("thunk:1", "incoming", 0, 200).unwrap();
    assert_eq!(n.nodes.len(), 3);
    assert_eq!(n.edges.len(), 4);
    assert_eq!(
        n.edges
            .iter()
            .filter(|e| e.relation == "force")
            .map(|e| e.count)
            .sum::<u64>(),
        26
    );
    let c = g.callables("", 0, 10).unwrap();
    assert_eq!(c.callables[0].invocations, 2);
    assert_eq!(g.invocations(1, 1, 0, 1).unwrap().total, 2);
    assert_eq!(
        g.inspect("invocation:1").unwrap().site.unwrap().name,
        "f\"\\\nλ"
    );
}
#[test]
fn equal_previews_are_not_identity_and_json_escapes_roundtrip() {
    let mut e = site();
    e.extend([
        alloc(1),
        alloc(2),
        inv(1),
        inv(2),
        argument(1, 1),
        argument(2, 2),
    ]);
    let g = graph(&capture(e, true));
    assert!(g.summary.complete);
    let a = g.inspect("thunk:1").unwrap();
    let b = g.inspect("thunk:2").unwrap();
    assert_eq!(a.preview, b.preview);
    assert_ne!(a.id, b.id);
    assert_eq!(a.preview.as_deref(), Some("same \"\\\nλ"));
    assert_eq!(g.search("same", "thunk", 0, 1).unwrap().total, 2);
}
#[test]
fn forward_references_resolve_but_missing_endpoint_is_partial() {
    let mut e = vec![
        argument(1, 1),
        force(1),
        json!({"event":"cache","invocation":1,"result":0}),
        alloc(1),
        inv(1),
    ];
    e.extend(site());
    let g = graph(&capture(e, true));
    assert!(g.summary.complete, "{:?}", g.summary.diagnostics);
    assert_eq!(
        g.inspect("thunk:1").unwrap().preview.as_deref(),
        Some("same \"\\\nλ")
    );
    assert!(
        g.inspect("invocation:1")
            .unwrap()
            .attributes
            .iter()
            .any(|a| a.name == "cache_compute" && a.value == "1")
    );
    let g = graph(&capture(vec![force(1)], true));
    assert!(!g.summary.complete);
    assert_eq!(g.summary.missing_endpoints, 2);
    assert!(!g.inspect("thunk:1").unwrap().defined);
}
#[test]
fn missing_footer_and_truncated_json_are_partial() {
    let s = capture(site(), false);
    assert!(!graph(&s).summary.complete);
    let g = graph(&(s + "{\"event\":\"alloc"));
    assert!(!g.summary.complete);
    assert_eq!(g.summary.diagnostic_count, 2);
}
#[test]
fn wrong_version_or_semantics_is_rejected() {
    let s = capture(vec![], true).replace("\"version\":1", "\"version\":2");
    assert!(Graph::read(s.as_bytes(), 1, String::new(), Limits::default()).is_err());
    assert!(Graph::read(b"".as_slice(), 1, String::new(), Limits::default()).is_err());
}
#[test]
fn sequence_count_time_duplicate_and_trailing_data_diagnostics() {
    let s = capture(site(), true).replace("\"seq\":2", "\"seq\":9");
    assert!(!graph(&s).summary.complete);
    let s = capture(site(), true).replace("\"records\":4", "\"records\":99");
    assert!(!graph(&s).summary.complete);
    let s = capture(site(), true).replace("\"ts_ns\":2", "\"ts_ns\":0");
    assert!(!graph(&s).summary.complete);
    let s = capture(vec![alloc(1), alloc(1)], true);
    assert!(!graph(&s).summary.complete);
    let s = capture(vec![], true) + "{\"event\":\"module\",\"module\":1,\"seq\":3,\"ts_ns\":3}\n";
    assert!(!graph(&s).summary.complete);
}
#[test]
fn unresolved_identity_is_explicit_not_merged() {
    let mut e = site();
    e.push(inv(1));
    for identity in ["unknown_external", "unknown_external", "null"] {
        e.push(json!({"event":"created","invocation":1,"thunk":{"id":null,"identity":identity}}));
    }
    let g = graph(&capture(e, true));
    assert!(!g.summary.complete);
    assert_eq!(g.search("", "unresolved", 0, 10).unwrap().total, 3);
}
#[test]
fn imports_isolate_local_ids_and_queries_are_bounded() {
    let s = capture(vec![alloc(1)], true);
    let a = graph(&s);
    let b = Graph::read(s.as_bytes(), 2, "other".into(), Limits::default()).unwrap();
    assert_ne!(a.summary.capture_id, b.summary.capture_id);
    assert_eq!(
        a.inspect("thunk:1").unwrap().id,
        b.inspect("thunk:1").unwrap().id
    );
    assert!(a.search("", "", 0, 201).is_err());
    assert!(a.neighborhood("thunk:1", "wrong", 0, 10).is_err());
    assert_eq!(a.search("", "", 100, 1).unwrap().nodes.len(), 0);
}
#[test]
fn explicit_limits_reject_instead_of_eviction() {
    let s = capture(vec![alloc(1), alloc(2)], true);
    for limits in [
        Limits {
            nodes: 1,
            ..Limits::default()
        },
        Limits {
            bytes: 1,
            ..Limits::default()
        },
        Limits {
            records: 2,
            ..Limits::default()
        },
        Limits {
            record_bytes: 5,
            ..Limits::default()
        },
    ] {
        assert!(Graph::read(s.as_bytes(), 1, "".into(), limits).is_err());
    }
}
#[test]
fn malformed_known_events_and_unknown_events_are_partial() {
    for event in [
        json!({"event":"future"}),
        json!({"event":"allocation","thunk":0,"words":1}),
        json!({"event":"force","invocation":1,"thunk":{"id":1,"identity":"address"},"state":0}),
        json!({"event":"cache","invocation":1,"result":9}),
    ] {
        assert!(!graph(&capture(vec![event], true)).summary.complete);
    }
}
#[test]
fn cache_only_is_not_silently_counted_as_invocation() {
    let g = graph(&capture(
        vec![json!({"event":"cache","invocation":1,"result":0})],
        true,
    ));
    assert!(!g.summary.complete);
    assert_eq!(g.summary.invocations, 0);
    assert_eq!(g.summary.cache_computes, 1);
}

#[test]
fn checked_in_fixture_is_complete_and_shared() {
    let g = graph(include_str!("../../test-data/eval-graph/shared.jsonl"));
    assert!(g.summary.complete, "{:?}", g.summary.diagnostics);
    assert_eq!(
        (
            g.summary.invocations,
            g.summary.force_requests,
            g.summary.cache_computes,
            g.summary.cache_hits
        ),
        (3, 4, 2, 1)
    );
    assert_eq!(
        g.neighborhood("thunk:1", "incoming", 0, 200)
            .unwrap()
            .nodes
            .len(),
        3
    );
}
#[test]
fn states_are_signed_and_execution_status_is_not_stream_completeness() {
    let mut e = site();
    e.extend([alloc(1), inv(1), force(1)]);
    e.last_mut().unwrap()["state"] = json!(-7);
    e.push(json!({"event":"execution","workers":4,"status":"out_of_fuel"}));
    let g = graph(&capture(e, true));
    assert!(g.summary.complete);
    assert_eq!(g.summary.executions[0].status, "out_of_fuel");
    assert_eq!(
        g.neighborhood("thunk:1", "incoming", 0, 10).unwrap().edges[0].state,
        Some(-7)
    );
}

#[test]
fn header_elapsed_timestamp_need_not_be_zero() {
    let s = capture(vec![], true).replace("\"ts_ns\":0", "\"ts_ns\":1");
    assert!(graph(&s).summary.complete);
}

#[test]
fn actual_vix_four_worker_capture_preserves_shared_bindings() {
    let g = graph(include_str!("../../test-data/eval-graph/vix-shared.jsonl"));
    assert!(g.summary.complete, "{:?}", g.summary.diagnostics);
    assert_eq!(g.summary.records, 40);
    let rows = g.callables("", 0, 200).unwrap().callables;
    let twice = rows.iter().find(|c| c.site.name == "vix::twice").unwrap();
    let payload = rows.iter().find(|c| c.site.name == "vix::payload").unwrap();
    assert_eq!(twice.invocations, 2);
    assert_eq!(payload.invocations, 1);
    let invocations = g
        .invocations(twice.site.module, twice.site.id, 0, 200)
        .unwrap();
    let args: Vec<_> = invocations
        .nodes
        .iter()
        .map(|n| {
            g.neighborhood(&n.id, "outgoing", 0, 200)
                .unwrap()
                .edges
                .into_iter()
                .find(|e| e.relation == "argument" && e.argument_index == Some(0))
                .unwrap()
                .to
        })
        .collect();
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], args[1]);
}

#[test]
fn selected_budget_stop_is_partial_with_explicit_reason_not_malformed() {
    let text = capture(vec![alloc(100)], true)
        .replace(
            "\"records\":1",
            "\"include\":\"solve,planner.vix\",\"max_bytes\":268435456,\"records\":1",
        )
        .replace(
            "\"complete\":true",
            "\"complete\":false,\"reason\":\"byte_budget\"",
        );
    let g = graph(&text);
    assert!(!g.summary.complete);
    assert_eq!(g.summary.partial_reason.as_deref(), Some("byte_budget"));
    assert_eq!(g.summary.selection.as_deref(), Some("solve,planner.vix"));
    assert_eq!(g.summary.max_bytes, Some(268435456));
    assert!(g.summary.diagnostics[0].contains("intentionally"));
    assert!(g.inspect("thunk:100").unwrap().defined);
}
#[test]
fn allocation_ids_can_gap_and_emit_out_of_order_with_full_u128_birth() {
    let text = capture(vec![alloc(90), alloc(3)], true).replace(
        "\"words\":8",
        "\"born_ns\":340282366920938463463374607431768211455,\"words\":8",
    );
    let g = graph(&text);
    assert!(g.summary.complete, "{:?}", g.summary.diagnostics);
    assert_eq!(
        g.inspect("thunk:90")
            .unwrap()
            .attributes
            .iter()
            .find(|a| a.name == "born_ns")
            .unwrap()
            .value,
        u128::MAX.to_string()
    );
}

fn counts_capture(events: Vec<Value>, footer: bool) -> String {
    capture(events, footer).replace(
        "\"semantics\":\"lazy-dataflow\"",
        "\"semantics\":\"source-counts\",\"capture_mode\":\"counts\"",
    )
}
fn execution(execution: u64, status: &str) -> Value {
    json!({"event":"execution","execution":execution,"module":1,"workers":4,"status":status})
}
fn counts(execution: u64, site: u64, n: u64) -> Value {
    json!({"event":"counts","execution":execution,"module":1,"site":site,"body_starts":n,"force_requests":n,"created":n,"cache_compute":n,"cache_hit":n,"cache_wait":n,"overflow":false})
}
fn complete_counts(n: u64) -> Vec<Value> {
    let mut e = site();
    e.extend([execution(1, "start"), counts(1, 1, n), execution(1, "done")]);
    e
}

#[test]
fn counts_nested_executions_sum_all_metrics_without_graph_identities() {
    let mut e = site();
    let mut zero_site = e[1].clone();
    zero_site["site"] = json!(2);
    zero_site["name"] = json!("unused");
    e.push(zero_site);
    e.extend([
        execution(10, "start"),
        execution(11, "start"),
        counts(11, 1, 3),
        counts(11, 2, 0),
        execution(11, "poison"),
        counts(10, 1, 7),
        counts(10, 2, 0),
        execution(10, "out_of_fuel"),
    ]);
    let g = graph(&counts_capture(e, true));
    assert!(g.summary.complete, "{:?}", g.summary.diagnostics);
    assert!(g.summary.counts_exact);
    assert_eq!(g.summary.capture_mode, "counts");
    assert_eq!(g.summary.semantics, "source-counts");
    assert_eq!((g.summary.nodes, g.summary.relations), (0, 0));
    assert!(g.nodes.is_empty() && g.edges.is_empty());
    let totals = g.summary.counts.as_ref().unwrap();
    assert_eq!(
        (totals.body_starts, totals.force_requests, totals.created),
        (10, 10, 10)
    );
    assert_eq!(
        (totals.cache_compute, totals.cache_hit, totals.cache_wait),
        (10, 10, 10)
    );
    assert_eq!((g.summary.invocations, g.summary.cache_computes), (10, 10));
    let page = g.callables("", 0, 1).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.callables[0].invocations, 10);
    assert_eq!(page.callables[0].counts.as_ref().unwrap().body_starts, 10);
    assert_eq!(
        g.callables("unused", 0, 10).unwrap().callables[0].invocations,
        0
    );
    assert_eq!(g.callables("", 1, 1).unwrap().callables[0].site.id, 2);
    assert_eq!(g.summary.executions[1].execution, Some(11));
    assert_eq!(g.summary.executions[1].module, Some(1));
    for result in [
        g.inspect("invocation:1").map(|_| ()),
        g.search("", "all", 0, 10).map(|_| ()),
        g.invocations(1, 1, 0, 10).map(|_| ()),
        g.neighborhood("thunk:1", "both", 0, 10).map(|_| ()),
    ] {
        assert!(
            result
                .unwrap_err()
                .contains("no invocation/argument detail")
        );
    }
}

#[test]
fn counts_overflow_is_lower_bound_even_when_stream_footer_is_complete() {
    let mut e = complete_counts(u64::MAX);
    e.extend([execution(2, "start"), counts(2, 1, 1), execution(2, "done")]);
    let g = graph(&counts_capture(e, true));
    assert!(g.summary.complete, "overflow is not a stream truncation");
    assert!(!g.summary.counts_exact);
    let c = &g.callables("", 0, 10).unwrap().callables[0];
    assert_eq!(c.invocations, u64::MAX);
    assert!(c.counts.as_ref().unwrap().overflow);
    assert!(g.summary.counts.as_ref().unwrap().overflow);
    assert_eq!(g.summary.counts.as_ref().unwrap().created, u64::MAX);

    let mut e = complete_counts(5);
    e[3]["overflow"] = json!(true);
    let g = graph(&counts_capture(e, true));
    assert!(g.summary.complete);
    assert!(!g.summary.counts_exact);
    assert!(
        g.callables("", 0, 10).unwrap().callables[0]
            .counts
            .as_ref()
            .unwrap()
            .overflow
    );
}

#[test]
fn counts_global_overflow_across_distinct_sites_does_not_mark_each_site_overflowed() {
    let mut e = site();
    let mut second = e[1].clone();
    second["site"] = json!(2);
    e.push(second);
    e.extend([
        execution(1, "start"),
        counts(1, 1, u64::MAX),
        counts(1, 2, 1),
        execution(1, "done"),
    ]);
    let g = graph(&counts_capture(e, true));
    assert!(!g.summary.counts_exact);
    assert!(g.summary.counts.as_ref().unwrap().overflow);
    assert!(
        g.callables("", 0, 10).unwrap().callables.iter().all(|c| !c
            .counts
            .as_ref()
            .unwrap()
            .overflow)
    );
}

#[test]
fn counts_truncation_missing_rows_and_malformed_rows_never_claim_exactness() {
    let text = counts_capture(complete_counts(2), false);
    let g = graph(&text);
    assert!(!g.summary.complete && !g.summary.counts_exact);
    let g = graph(&(text + "{\"event\":\"counts\""));
    assert!(!g.summary.complete && !g.summary.counts_exact);
    assert_eq!(g.summary.invocations, 2);

    let mut e = site();
    e.extend([execution(1, "start"), execution(1, "done")]);
    let g = graph(&counts_capture(e, true));
    assert!(!g.summary.complete && !g.summary.counts_exact);
    assert!(
        g.summary
            .diagnostics
            .iter()
            .any(|d| d.contains("missing counts rows"))
    );

    for field in [
        "body_starts",
        "force_requests",
        "created",
        "cache_compute",
        "cache_hit",
        "cache_wait",
        "overflow",
        "execution",
    ] {
        let mut e = complete_counts(9);
        e[3].as_object_mut().unwrap().remove(field);
        let g = graph(&counts_capture(e, true));
        assert!(!g.summary.complete && !g.summary.counts_exact, "{field}");
        assert_eq!(
            g.summary.invocations, 0,
            "malformed row partially applied: {field}"
        );
    }
    let mut e = complete_counts(4);
    e[3]["created"] = json!(-1);
    assert!(!graph(&counts_capture(e, true)).summary.counts_exact);
}

#[test]
fn counts_lifecycle_duplicate_rows_and_detail_events_are_diagnosed_not_synthesized() {
    let valid = complete_counts(3);
    for (index, event) in [
        (4, counts(1, 1, 99)),      // duplicate row
        (5, counts(1, 1, 99)),      // row after terminal
        (2, counts(1, 1, 99)),      // row before start
        (3, execution(1, "start")), // duplicate start
        (5, execution(1, "done")),  // duplicate terminal
        (3, execution(7, "done")),  // no start
        (3, alloc(1)),
        (3, inv(1)),
        (3, argument(1, 1)),
        (3, force(1)),
        (
            3,
            json!({"event":"created","invocation":1,"thunk":{"id":1,"identity":"allocated"}}),
        ),
        (3, json!({"event":"cache","invocation":1,"result":1})),
    ] {
        let mut e = valid.clone();
        e.insert(index, event);
        let g = graph(&counts_capture(e, true));
        assert!(
            !g.summary.complete && !g.summary.counts_exact,
            "{:?}",
            g.summary.diagnostics
        );
        assert_eq!(g.summary.invocations, 3);
        assert_eq!((g.summary.nodes, g.summary.relations), (0, 0));
    }
    let mut e = complete_counts(3);
    e.pop();
    let g = graph(&counts_capture(e, true));
    assert!(!g.summary.counts_exact);
    assert!(
        g.summary
            .diagnostics
            .iter()
            .any(|d| d.contains("no terminal status"))
    );
    for (field, value) in [
        ("workers", json!(2)),
        ("module", json!(2)),
        ("execution", json!(0)),
        ("status", json!("unknown")),
    ] {
        let mut e = valid.clone();
        e[4][field] = value;
        assert!(!graph(&counts_capture(e, true)).summary.counts_exact);
    }
}

#[test]
fn counts_empty_selection_is_valid_and_modes_cannot_be_mixed() {
    let e = vec![
        json!({"event":"module","module":1}),
        execution(1, "start"),
        execution(1, "resource_pending"),
    ];
    let g = graph(&counts_capture(e, true));
    assert!(g.summary.complete && g.summary.counts_exact);
    assert_eq!(g.callables("", 0, 10).unwrap().total, 0);
    assert!(
        Graph::read(
            counts_capture(vec![], true)
                .replace("\"capture_mode\":\"counts\",", "")
                .as_bytes(),
            1,
            "bad".into(),
            Limits::default()
        )
        .is_err()
    );
    let s = capture(vec![], true).replace(
        "\"semantics\":\"lazy-dataflow\"",
        "\"semantics\":\"lazy-dataflow\",\"capture_mode\":\"counts\"",
    );
    assert!(Graph::read(s.as_bytes(), 1, "bad".into(), Limits::default()).is_err());
    let g = graph(&capture(vec![counts(1, 1, 1)], true));
    assert!(!g.summary.complete);
    let g = graph(&capture(site(), true));
    assert_eq!(g.summary.capture_mode, "detail");
    assert!(g.summary.counts.is_none());
    assert!(g.summary.counts_exact);
}

#[test]
fn checked_in_counts_fixture_preserves_totals_and_selected_source_metadata() {
    let g = graph(include_str!("../../test-data/eval-graph/counts.jsonl"));
    assert!(
        g.summary.complete && g.summary.counts_exact,
        "{:?}",
        g.summary.diagnostics
    );
    assert_eq!(g.summary.invocations, 10);
    assert_eq!(g.summary.counts.as_ref().unwrap().created, 6);
    assert_eq!(g.callables("", 0, 10).unwrap().total, 2);
    assert_eq!(
        g.callables("twice", 0, 10).unwrap().callables[0].invocations,
        10
    );
    assert_eq!((g.summary.nodes, g.summary.relations), (0, 0));
}

#[test]
fn actual_vix_four_worker_counts_capture_preserves_native_totals_and_zero_sites() {
    let g = graph(include_str!("../../test-data/eval-graph/vix-counts.jsonl"));
    assert!(
        g.summary.complete && g.summary.counts_exact,
        "{:?}",
        g.summary.diagnostics
    );
    assert_eq!(g.summary.records, 25);
    let totals = g.summary.counts.as_ref().unwrap();
    assert_eq!(
        (totals.body_starts, totals.force_requests, totals.created),
        (7, 6, 6)
    );
    assert_eq!(
        (totals.cache_compute, totals.cache_hit, totals.cache_wait),
        (2, 0, 1)
    );
    let rows = g.callables("", 0, 200).unwrap();
    assert_eq!(rows.total, 10);
    assert_eq!(rows.callables[0].site.name, "vix::item");
    assert_eq!(rows.callables[0].invocations, 3);
    assert_eq!(
        rows.callables.iter().filter(|c| c.invocations == 0).count(),
        5
    );
    assert_eq!((g.summary.nodes, g.summary.relations), (0, 0));
    assert_eq!(g.summary.executions[0].workers, 4);
    assert_eq!(g.summary.executions[1].status, "done");
}
