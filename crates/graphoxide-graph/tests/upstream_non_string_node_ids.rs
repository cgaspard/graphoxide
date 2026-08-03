use graphoxide_core::{coerce_non_string_ids, KnowledgeGraph};
use graphoxide_graph::{build_graph_from_value, BuildOptions};
use serde_json::{json, Value};

fn node(id: Value, label: &str) -> Value {
    json!({"id": id, "label": label, "file_type": "concept", "source_file": "a.py"})
}

fn edge(source: Value, target: Value) -> Value {
    json!({"source": source, "target": target, "relation": "uses", "confidence": "EXTRACTED"})
}

fn build(value: Value) -> KnowledgeGraph {
    build_graph_from_value(&value, BuildOptions::default(), None)
        .unwrap()
        .0
}

fn has_edge(graph: &KnowledgeGraph, source: &str, target: &str) -> bool {
    graph.links.iter().any(|edge| {
        (edge.true_source() == source && edge.true_target() == target)
            || (edge.true_source() == target && edge.true_target() == source)
    })
}

#[test]
fn test_pick_winner_survives_int_id_in_duplicate_group() {
    let graph = build(
        json!({"nodes": [node(json!(10), "Alpha"), node(json!("alpha_c1"), "Alpha")], "edges": []}),
    );
    assert!(graph.nodes.iter().all(|node| !node.id.is_empty()));
}

#[test]
fn test_build_accepts_a_single_int_id_node_with_no_duplicate() {
    let graph = build(json!({
        "nodes": [node(json!(10), "Alpha"), node(json!("b"), "Beta")],
        "edges": [edge(json!(10), json!("b"))]
    }));
    assert!(graph.nodes.iter().any(|node| node.id == "10"));
}

#[test]
fn test_int_id_endpoints_stay_connected_after_coercion() {
    let graph = build(json!({
        "nodes": [node(json!(10), "Alpha"), node(json!(20), "Beta")],
        "edges": [edge(json!(10), json!(20))]
    }));
    assert!(has_edge(&graph, "10", "20"));
}

#[test]
fn test_int_id_survives_a_fuzzy_dedup_group() {
    let graph = build(json!({
        "nodes": [node(json!(10), "PaymentProcessor"), node(json!("b"), "PaymentProcessors")],
        "edges": [edge(json!(10), json!("b"))]
    }));
    assert!(graph.nodes.iter().all(|node| !node.id.is_empty()));
}

#[test]
fn test_float_id_is_coerced_too() {
    let graph = build(json!({
        "nodes": [node(json!(1.5), "Alpha"), node(json!("b"), "Beta")],
        "edges": [edge(json!(1.5), json!("b"))]
    }));
    assert!(has_edge(&graph, "1.5", "b"));
}

#[test]
fn test_legacy_from_to_endpoints_are_coerced() {
    let graph = build(json!({
        "nodes": [node(json!(10), "Alpha"), node(json!("b"), "Beta")],
        "edges": [{"from": 10, "to": "b", "relation": "uses", "confidence": "EXTRACTED"}]
    }));
    assert!(has_edge(&graph, "10", "b"));
}

#[test]
fn test_hyperedge_members_are_coerced_with_their_nodes() {
    let graph = build(json!({
        "nodes": [node(json!(10), "Alpha"), node(json!("b"), "Beta")],
        "edges": [],
        "hyperedges": [{"id": "he1", "label": "grp", "nodes": [10, "b"]}]
    }));
    assert_eq!(graph.hyperedges[0]["nodes"], json!(["10", "b"]));
}

#[test]
fn test_build_from_json_coerces_on_the_direct_entry() {
    let graph = build(json!({"nodes": [node(json!(10), "Alpha")], "edges": []}));
    assert_eq!(
        graph
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["10"]
    );
}

#[test]
fn test_numeric_endpoint_with_no_matching_node_matches_the_string_case() {
    let graph_for = |target: Value| {
        let graph = build(json!({
            "nodes": [node(json!("a"), "Alpha")],
            "edges": [edge(json!("a"), target)]
        }));
        (graph.nodes.len(), graph.links.len())
    };
    assert_eq!(graph_for(json!(99)), graph_for(json!("99")));
}

#[test]
fn test_non_scalar_ids_are_left_for_validation_none() {
    let mut extraction = json!({"nodes": [{"id": null, "label": "Alpha"}], "edges": []});
    coerce_non_string_ids(&mut extraction);
    assert!(extraction["nodes"][0]["id"].is_null());
}

#[test]
fn test_non_scalar_ids_are_left_for_validation_bad1() {
    let mut extraction = json!({"nodes": [{"id": ["x"], "label": "Alpha"}], "edges": []});
    let before = extraction["nodes"][0]["id"].clone();
    coerce_non_string_ids(&mut extraction);
    assert_eq!(extraction["nodes"][0]["id"], before);
}

#[test]
fn test_non_scalar_ids_are_left_for_validation_bad2() {
    let mut extraction = json!({"nodes": [{"id": {"k": "v"}, "label": "Alpha"}], "edges": []});
    let before = extraction["nodes"][0]["id"].clone();
    coerce_non_string_ids(&mut extraction);
    assert_eq!(extraction["nodes"][0]["id"], before);
}

#[test]
fn test_bool_id_is_not_coerced() {
    let mut extraction = json!({"nodes": [{"id": true, "label": "Alpha"}], "edges": []});
    coerce_non_string_ids(&mut extraction);
    assert_eq!(extraction["nodes"][0]["id"], true);
}

#[test]
fn test_string_ids_are_untouched() {
    let graph = build(json!({
        "nodes": [node(json!("a"), "Alpha"), node(json!("b"), "Beta")],
        "edges": [edge(json!("a"), json!("b"))]
    }));
    assert!(graph.nodes.iter().any(|node| node.id == "a"));
    assert!(graph.nodes.iter().any(|node| node.id == "b"));
    assert!(has_edge(&graph, "a", "b"));
}
