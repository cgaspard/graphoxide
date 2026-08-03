use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_export::render_cypher;
use std::collections::BTreeMap;

fn graph() -> KnowledgeGraph {
    KnowledgeGraph {
        directed: true,
        multigraph: true,
        nodes: vec![node("source", "Source"), node("target", "Target")],
        links: vec![Edge {
            source: "source".into(),
            target: "target".into(),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: "sample.py".into(),
            extra: BTreeMap::new(),
        }],
        ..Default::default()
    }
}

fn node(id: &str, label: &str) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: "sample.py".into(),
        source_location: None,
        community: None,
        extra: BTreeMap::new(),
    }
}

#[test]
fn test_push_to_falkordb_creates_expected_graph() {
    let graph = graph();
    let cypher = render_cypher(&graph);
    assert_eq!(
        cypher
            .lines()
            .filter(|line| line.starts_with("MERGE (n:GraphoxideNode"))
            .count(),
        graph.nodes.len()
    );
    assert_eq!(
        cypher
            .lines()
            .filter(|line| line.starts_with("MATCH (a:GraphoxideNode"))
            .count(),
        graph.links.len()
    );
}

#[test]
fn test_push_to_falkordb_is_idempotent() {
    let first = render_cypher(&graph());
    let second = render_cypher(&graph());
    assert_eq!(first, second);
    assert!(first
        .lines()
        .next()
        .is_some_and(|line| line.contains("CREATE CONSTRAINT") && line.contains("IF NOT EXISTS")));
    assert!(first.lines().skip(1).all(|line| line.contains("MERGE")));
}
