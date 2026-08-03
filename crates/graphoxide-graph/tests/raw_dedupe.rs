use graphoxide_core::{Confidence, Edge, Extraction, Node};
use graphoxide_graph::dedupe_raw_extractions;
use serde_json::json;
use std::collections::BTreeMap;

fn node(id: &str, label: &str) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: "app.py".into(),
        source_location: None,
        community: None,
        extra: BTreeMap::new(),
    }
}

fn edge(marker: u64) -> Edge {
    Edge {
        source: "app".into(),
        target: "helper".into(),
        relation: "calls".into(),
        confidence: Confidence::Extracted,
        source_file: "app.py".into(),
        extra: BTreeMap::from([("marker".into(), json!(marker))]),
    }
}

#[test]
fn raw_dedupe_counts_unique_nodes_and_keeps_expected_records() {
    let chunks = [
        Extraction {
            nodes: vec![node("app", "old"), node("helper", "helper")],
            edges: vec![edge(1)],
            hyperedges: vec![json!({"id": "flow"})],
        },
        Extraction {
            nodes: vec![node("app", "fresh")],
            edges: vec![edge(2)],
            hyperedges: vec![],
        },
    ];

    let deduped = dedupe_raw_extractions(&chunks);

    assert_eq!(deduped.nodes.len(), 2);
    assert_eq!(deduped.nodes[0].id, "app");
    assert_eq!(deduped.nodes[0].label, "fresh");
    assert_eq!(deduped.nodes[1].id, "helper");
    assert_eq!(deduped.edges.len(), 1);
    assert_eq!(deduped.edges[0].extra["marker"], json!(1));
    assert_eq!(deduped.hyperedges, vec![json!({"id": "flow"})]);
}

#[test]
fn raw_dedupe_matches_upstream_literal_endpoint_keys() {
    let mut first = edge(1);
    first.extra.insert("_src".into(), json!("owner"));
    first.extra.insert("_tgt".into(), json!("callee"));
    let mut duplicate = edge(2);
    duplicate.extra.insert("_src".into(), json!("other_owner"));
    duplicate.extra.insert("_tgt".into(), json!("other_callee"));
    let mut distinct = edge(3);
    distinct.source = "other".into();
    distinct.target = "target".into();
    distinct.extra.insert("_src".into(), json!("owner"));
    distinct.extra.insert("_tgt".into(), json!("callee"));

    let deduped = dedupe_raw_extractions(&[Extraction {
        edges: vec![first, duplicate, distinct],
        ..Extraction::default()
    }]);

    assert_eq!(deduped.edges.len(), 2);
    assert_eq!(deduped.edges[0].extra["marker"], json!(1));
    assert_eq!(deduped.edges[1].extra["marker"], json!(3));
}
