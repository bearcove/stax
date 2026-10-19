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
