//! Executable port of upstream `test_hypergraph.py` (20 cases).

use graphoxide_core::{read_graph, write_graph_atomic, Extraction, KnowledgeGraph};
use graphoxide_export::render_report;
use graphoxide_graph::{
    analyze, attach_hyperedges, build_graph, build_graph_with_report, build_graph_with_root,
    HyperedgeRepairReason,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs};
use tempfile::tempdir;

fn sample_extraction() -> Extraction {
    serde_json::from_value(json!({
        "nodes": [
            {"id": "BasicAuth", "label": "BasicAuth", "file_type": "code", "source_file": "auth.py"},
            {"id": "DigestAuth", "label": "DigestAuth", "file_type": "code", "source_file": "auth.py"},
            {"id": "Request", "label": "Request", "file_type": "code", "source_file": "http.py"},
            {"id": "Response", "label": "Response", "file_type": "code", "source_file": "http.py"},
            {"id": "BaseClient", "label": "BaseClient", "file_type": "code", "source_file": "client.py"}
        ],
        "edges": [{
            "source": "BasicAuth", "target": "Request", "relation": "uses",
            "confidence": "EXTRACTED", "confidence_score": 1.0, "source_file": "auth.py"
        }],
        "hyperedges": [{
            "id": "auth_flow", "label": "Auth Flow",
            "nodes": ["BasicAuth", "DigestAuth", "Request", "Response", "BaseClient"],
            "relation": "participate_in", "confidence": "INFERRED",
            "confidence_score": 0.75, "source_file": "auth.py"
        }]
    })).unwrap()
}

fn without_hyperedges() -> Extraction {
    let mut extraction = sample_extraction();
    extraction.hyperedges.clear();
    extraction
}

#[test]
fn build_from_json_stores_hyperedges() {
    let graph = build_graph(&[sample_extraction()]).unwrap();
    assert_eq!(graph.hyperedges.len(), 1);
    assert_eq!(graph.hyperedges[0]["id"], "auth_flow");
}

#[test]
fn build_from_json_relativizes_hyperedge_source_file() {
    let tmp = tempdir().unwrap();
    let absolute = tmp.path().join("docs/CLAUDE.md");
    let extraction: Extraction = serde_json::from_value(json!({
        "nodes": [{"id": "a", "label": "A", "file_type": "document", "source_file": absolute}],
        "edges": [],
        "hyperedges": [{"id": "arch", "label": "Architecture", "nodes": ["a"], "source_file": absolute}]
    })).unwrap();
    let graph = build_graph_with_root(&[extraction], tmp.path()).unwrap();
    assert_eq!(graph.hyperedges[0]["source_file"], "docs/CLAUDE.md");
    assert_eq!(graph.nodes[0].source_file, "docs/CLAUDE.md");
}

#[test]
fn build_from_json_no_hyperedges() {
    assert!(build_graph(&[without_hyperedges()])
        .unwrap()
        .hyperedges
        .is_empty());
}

#[test]
fn build_from_json_missing_hyperedges_key() {
    let extraction: Extraction = serde_json::from_value(json!({"nodes": [], "edges": []})).unwrap();
    assert!(build_graph(&[extraction]).unwrap().hyperedges.is_empty());
}

#[test]
fn attach_hyperedges_adds_new() {
    let mut graph = KnowledgeGraph::default();
    attach_hyperedges(
        &mut graph,
        &[json!({"id": "auth_flow", "nodes": ["A", "B", "C"]})],
    );
    assert_eq!(graph.hyperedges.len(), 1);
}

#[test]
fn attach_hyperedges_deduplicates() {
    let mut graph = KnowledgeGraph::default();
    let value = json!({"id": "auth_flow", "nodes": ["A", "B", "C"]});
    attach_hyperedges(&mut graph, std::slice::from_ref(&value));
    attach_hyperedges(&mut graph, &[value]);
    assert_eq!(graph.hyperedges.len(), 1);
}

#[test]
fn attach_hyperedges_multiple_different_ids() {
    let mut graph = KnowledgeGraph::default();
    attach_hyperedges(
        &mut graph,
        &[
            json!({"id": "flow_a", "nodes": ["A", "B", "C"]}),
            json!({"id": "flow_b", "nodes": ["D", "E", "F"]}),
        ],
    );
    assert_eq!(graph.hyperedges.len(), 2);
}

#[test]
fn attach_hyperedges_skips_entry_without_id() {
    let mut graph = KnowledgeGraph::default();
    attach_hyperedges(
        &mut graph,
        &[json!({"label": "No ID", "nodes": ["A", "B", "C"]})],
    );
    assert!(graph.hyperedges.is_empty());
}

#[test]
fn to_json_includes_hyperedges() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    let graph = build_graph(&[sample_extraction()]).unwrap();
    write_graph_atomic(&path, &graph, true).unwrap();
    let data: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(data["hyperedges"].as_array().unwrap().len(), 1);
    assert_eq!(data["hyperedges"][0]["id"], "auth_flow");
}

#[test]
fn to_json_hyperedges_empty_when_none() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    let graph = build_graph(&[without_hyperedges()]).unwrap();
    write_graph_atomic(&path, &graph, true).unwrap();
    let data: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(data["hyperedges"], json!([]));
}

#[test]
fn hyperedges_roundtrip_via_json_file() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    let graph = build_graph(&[sample_extraction()]).unwrap();
    write_graph_atomic(&path, &graph, true).unwrap();
    let loaded = read_graph(path).unwrap();
    let rebuilt = build_graph(&[Extraction {
        nodes: loaded.nodes,
        edges: loaded.links,
        hyperedges: loaded.hyperedges,
    }])
    .unwrap();
    assert_eq!(rebuilt.hyperedges[0]["id"], "auth_flow");
}

fn report_for(extraction: Extraction) -> String {
    let mut graph = build_graph(&[extraction]).unwrap();
    for node in &mut graph.nodes {
        node.community = Some(0);
    }
    let analysis = analyze(&graph).unwrap();
    render_report(&graph, &analysis)
}

#[test]
fn report_includes_hyperedges_section() {
    let report = report_for(sample_extraction());
    assert!(report.contains("## Hyperedges (group relationships)"));
    assert!(report.contains("Auth Flow"));
    assert!(report.contains("INFERRED 0.75"));
}

#[test]
fn report_includes_hyperedge_node_list() {
    let report = report_for(sample_extraction());
    assert!(report.contains("BasicAuth"));
    assert!(report.contains("DigestAuth"));
}

#[test]
fn report_skips_hyperedges_section_when_empty() {
    assert!(!report_for(without_hyperedges()).contains("## Hyperedges"));
}

#[test]
fn report_skips_hyperedges_section_when_key_missing() {
    let extraction: Extraction = serde_json::from_value(json!({"nodes": [], "edges": []})).unwrap();
    assert!(!report_for(extraction).contains("## Hyperedges"));
}

fn alias_extraction() -> Extraction {
    serde_json::from_value(json!({
        "nodes": [
            {"id": "a", "label": "A", "file_type": "code", "source_file": "m.py"},
            {"id": "b", "label": "B", "file_type": "code", "source_file": "m.py"},
            {"id": "c", "label": "C", "file_type": "code", "source_file": "m.py"}
        ],
        "edges": [],
        "hyperedges": [
            {"id": "he_nodes", "label": "canon", "nodes": ["a", "b", "c"]},
            {"id": "he_members", "label": "alias1", "members": ["a", "b", "c"]},
            {"id": "he_node_ids", "label": "alias2", "node_ids": ["a", "b", "c"]}
        ]
    }))
    .unwrap()
}

#[test]
fn build_normalizes_member_aliases_to_nodes() {
    let graph = build_graph(&[alias_extraction()]).unwrap();
    let by_id: BTreeMap<_, _> = graph
        .hyperedges
        .iter()
        .map(|value| (value["id"].as_str().unwrap(), value))
        .collect();
    for id in ["he_nodes", "he_members", "he_node_ids"] {
        assert_eq!(by_id[id]["nodes"], json!(["a", "b", "c"]));
        assert!(by_id[id].get("members").is_none());
        assert!(by_id[id].get("node_ids").is_none());
    }
}

#[test]
fn build_dedups_alias_members_preserving_order() {
    let extraction: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id": "a", "label": "A", "file_type": "code", "source_file": "m.py"},
            {"id": "b", "label": "B", "file_type": "code", "source_file": "m.py"}
        ],
        "edges": [], "hyperedges": [{"id": "h", "members": ["a", "a", "b"]}]
    }))
    .unwrap();
    assert_eq!(
        build_graph(&[extraction]).unwrap().hyperedges[0]["nodes"],
        json!(["a", "b"])
    );
}

#[test]
fn build_canonical_nodes_wins_over_alias() {
    let extraction: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id": "a", "label": "A", "file_type": "code", "source_file": "m.py"},
            {"id": "b", "label": "B", "file_type": "code", "source_file": "m.py"},
            {"id": "x", "label": "X", "file_type": "code", "source_file": "m.py"}
        ],
        "edges": [], "hyperedges": [{"id": "h", "nodes": ["a", "b"], "members": ["x"]}]
    }))
    .unwrap();
    let hyperedge = build_graph(&[extraction]).unwrap().hyperedges.remove(0);
    assert_eq!(hyperedge["nodes"], json!(["a", "b"]));
    assert!(hyperedge.get("members").is_none());
}

#[test]
fn build_rekeys_alias_keyed_hyperedge_members() {
    let extraction: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id": "mod_foo", "label": "foo", "file_type": "code", "source_file": "pkg/mod.py"},
            {"id": "mod_bar", "label": "bar", "file_type": "code", "source_file": "pkg/mod.py"}
        ],
        "edges": [], "hyperedges": [{"id": "h", "members": ["mod_foo", "mod_bar"]}]
    }))
    .unwrap();
    assert_eq!(
        build_graph(&[extraction]).unwrap().hyperedges[0]["nodes"],
        json!(["pkg_mod_foo", "pkg_mod_bar"])
    );
}

#[test]
fn build_reports_once_per_aliased_hyperedge() {
    let (_, report) = build_graph_with_report(&[alias_extraction()]).unwrap();
    assert_eq!(
        report
            .hyperedge_repairs
            .get(&HyperedgeRepairReason::MembersAliasNormalized),
        Some(&2)
    );
}
