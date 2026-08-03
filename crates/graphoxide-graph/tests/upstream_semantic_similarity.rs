use graphoxide_core::{Confidence, Edge, Extraction, KnowledgeGraph, Node};
use graphoxide_graph::{build_graph, surprise_score};
use std::collections::BTreeMap;

fn extraction() -> Extraction {
    serde_json::from_value(serde_json::json!({
        "nodes": [
            {"id": "a_validate_input", "label": "validate_input", "file_type": "code", "source_file": "auth/validators.py", "source_location": "L5"},
            {"id": "b_check_input", "label": "check_input", "file_type": "code", "source_file": "api/checks.py", "source_location": "L12"}
        ],
        "edges": [{
            "source": "a_validate_input", "target": "b_check_input",
            "relation": "semantically_similar_to", "confidence": "INFERRED",
            "confidence_score": 0.82, "source_file": "auth/validators.py", "weight": 0.82
        }]
    })).unwrap()
}

fn semantic_graph() -> KnowledgeGraph {
    build_graph(&[extraction()]).unwrap()
}

fn node(id: &str, label: &str, source: &str) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source.into(),
        source_location: None,
        community: None,
        extra: BTreeMap::new(),
    }
}

fn edge(source: &str, target: &str, relation: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Inferred,
        source_file: String::new(),
        extra: BTreeMap::new(),
    }
}

fn two_edge_graph() -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: vec![
            node("a", "ValidateInput", "auth/validators.py"),
            node("b", "CheckInput", "api/checks.py"),
            node("c", "LoadConfig", "config/loader.py"),
            node("d", "ReadConfig", "utils/reader.py"),
        ],
        links: vec![
            edge("a", "b", "semantically_similar_to"),
            edge("c", "d", "references"),
        ],
        ..KnowledgeGraph::default()
    }
}

#[test]
fn test_semantic_edge_survives_build_from_json() {
    let graph = semantic_graph();
    assert_eq!(graph.links.len(), 1);
    assert_eq!(graph.links[0].relation, "semantically_similar_to");
}

#[test]
fn test_semantic_edge_nodes_present() {
    let graph = semantic_graph();
    assert!(graph.nodes.iter().any(|node| node.id == "a_validate_input"));
    assert!(graph.nodes.iter().any(|node| node.id == "b_check_input"));
}

#[test]
fn test_semantic_edge_confidence_score_preserved() {
    let graph = semantic_graph();
    let edge = &graph.links[0];
    assert_eq!(edge.confidence, Confidence::Inferred);
    assert_eq!(edge.extra["confidence_score"].as_f64(), Some(0.82));
}

#[test]
fn test_semantic_edge_scores_higher_than_references() {
    let graph = two_edge_graph();
    let communities = BTreeMap::from([
        ("a".into(), 0),
        ("b".into(), 0),
        ("c".into(), 1),
        ("d".into(), 1),
    ]);
    let semantic = surprise_score(
        &graph,
        "a",
        "b",
        &graph.links[0],
        &communities,
        "auth/validators.py",
        "api/checks.py",
        None,
    )
    .0;
    let reference = surprise_score(
        &graph,
        "c",
        "d",
        &graph.links[1],
        &communities,
        "config/loader.py",
        "utils/reader.py",
        None,
    )
    .0;
    assert!(semantic > reference);
}

#[test]
fn test_semantic_edge_reason_mentions_similarity() {
    let graph = two_edge_graph();
    let communities = BTreeMap::from([("a".into(), 0), ("b".into(), 0)]);
    let reasons = surprise_score(
        &graph,
        "a",
        "b",
        &graph.links[0],
        &communities,
        "auth/validators.py",
        "api/checks.py",
        None,
    )
    .1;
    assert!(reasons.iter().any(|reason| reason.contains("similar")));
}
