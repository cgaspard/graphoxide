//! Library-level port of upstream `tests/test_callflow_html.py`.

use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_export::{derive_sections_from_communities, write_callflow_html};
use serde_json::json;
use std::{collections::BTreeMap, fs, sync::Mutex};
use tempfile::tempdir;

static GRAPH_CAP_LOCK: Mutex<()> = Mutex::new(());

fn graph() -> KnowledgeGraph {
    let make_node = |id: &str, label: &str, source: &str, community: i64| Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source.into(),
        source_location: None,
        community: Some(community),
        extra: BTreeMap::new(),
    };
    let make_edge = |source: &str, target: &str, relation: &str| Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: String::new(),
        extra: BTreeMap::new(),
    };
    KnowledgeGraph {
        nodes: vec![
            make_node("api", "ApiClient", "src/api.py", 0),
            make_node("run", "run()", "src/main.py", 0),
            make_node("export", "write_html()", "src/export.py", 1),
            make_node("evil", "<script>alert(1)</script>", "src/evil.py", 1),
        ],
        links: vec![
            make_edge("run", "api", "calls"),
            make_edge("api", "export", "uses"),
            make_edge("export", "evil", "calls"),
        ],
        ..Default::default()
    }
}

#[test]
fn test_write_callflow_html_creates_file_and_uses_report() {
    let tmp = tempdir().unwrap();
    let output = tmp.path().join("graphoxide-out/callflow.html");
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    let report = "# Graph Report - sample\n\n## Summary\n- 3 nodes · 2 edges · 1 communities detected\n\n## God Nodes\n1. `Transformer` - 2 edges\n";
    let labels = BTreeMap::from([(0, "Runtime".into()), (1, "Export".into())]);
    let returned = write_callflow_html(&graph(), &output, &labels, report, 4).unwrap();
    assert_eq!(returned, output);
    let html = fs::read_to_string(returned).unwrap();
    assert!(html.contains("mermaid"));
    assert!(html.contains("Graph Report Highlights"));
    assert!(html.contains("Transformer"));
    assert!(html.contains("ApiClient"));
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.contains("<script>alert(1)</script>"));
}

#[test]
fn test_derive_sections_groups_by_architecture_keywords() {
    let nodes = vec![
        json!({"id":"extract_py","label":"extract_python","source_file":"graphify/extract.py","community":0}),
        json!({"id":"extract_js","label":"extract_js","source_file":"graphify/extract.py","community":0}),
        json!({"id":"to_html","label":"to_html","source_file":"graphify/export.py","community":1}),
        json!({"id":"test_html","label":"test_export_html","source_file":"tests/test_export.py","community":2}),
    ];
    let sections = derive_sections_from_communities(&nodes, &BTreeMap::new(), 6);
    let ids: std::collections::BTreeSet<_> =
        sections.iter().map(|section| section.id.as_str()).collect();
    assert!(ids.contains("extract-pipeline"));
    assert!(ids.contains("outputs-docs"));
    assert!(ids.contains("tests-fixtures"));
}

#[test]
fn test_load_graph_rejects_oversized_file() {
    let _guard = GRAPH_CAP_LOCK.lock().unwrap();
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, r#"{"nodes":[],"links":[]}"#).unwrap();
    unsafe { std::env::set_var("GRAPHOXIDE_MAX_GRAPH_BYTES", "8") };
    let error = graphoxide_core::read_graph(&path).unwrap_err();
    unsafe { std::env::remove_var("GRAPHOXIDE_MAX_GRAPH_BYTES") };
    assert!(error.to_string().contains("exceeds"));
}
