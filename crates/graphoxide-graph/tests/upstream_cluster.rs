use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_graph::{cluster, cohesion_score, communities, remap_community_map, score_all};
use std::collections::{BTreeMap, BTreeSet};

fn node(id: &str) -> Node {
    Node {
        id: id.into(),
        label: id.into(),
        file_type: "code".into(),
        source_file: format!("{id}.py"),
        source_location: None,
        community: None,
        extra: BTreeMap::new(),
    }
}

fn edge(source: &str, target: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "references".into(),
        confidence: Confidence::Extracted,
        source_file: String::new(),
        extra: BTreeMap::new(),
    }
}

fn graph() -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: ["a", "b", "c", "d"].into_iter().map(node).collect(),
        links: vec![edge("a", "b"), edge("b", "c"), edge("c", "d")],
        ..KnowledgeGraph::default()
    }
}

#[test]
fn test_cluster_returns_dict() {
    let mut graph = graph();
    cluster(&mut graph).unwrap();
    let _: BTreeMap<i64, Vec<String>> = communities(&graph);
}

#[test]
fn test_cluster_covers_all_nodes() {
    let mut graph = graph();
    cluster(&mut graph).unwrap();
    let covered = communities(&graph)
        .into_values()
        .flatten()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        covered,
        graph.nodes.iter().map(|node| node.id.clone()).collect()
    );
}

#[test]
fn test_cohesion_score_complete_graph() {
    let ids = ["0", "1", "2", "3"];
    let graph = KnowledgeGraph {
        nodes: ids.into_iter().map(node).collect(),
        links: (0..ids.len())
            .flat_map(|left| ((left + 1)..ids.len()).map(move |right| edge(ids[left], ids[right])))
            .collect(),
        ..KnowledgeGraph::default()
    };
    assert_eq!(cohesion_score(&graph, &ids.map(str::to_owned)), 1.0);
}

#[test]
fn test_cohesion_score_single_node() {
    let graph = KnowledgeGraph {
        nodes: vec![node("a")],
        ..KnowledgeGraph::default()
    };
    assert_eq!(cohesion_score(&graph, &["a".into()]), 1.0);
}

#[test]
fn test_cohesion_score_disconnected() {
    let graph = KnowledgeGraph {
        nodes: ["a", "b", "c"].into_iter().map(node).collect(),
        ..KnowledgeGraph::default()
    };
    assert_eq!(
        cohesion_score(&graph, &["a".into(), "b".into(), "c".into()]),
        0.0
    );
}

#[test]
fn test_cohesion_score_range() {
    let mut graph = graph();
    cluster(&mut graph).unwrap();
    for members in communities(&graph).values() {
        assert!((0.0..=1.0).contains(&cohesion_score(&graph, members)));
    }
}

#[test]
fn test_score_all_keys_match_communities() {
    let mut graph = graph();
    cluster(&mut graph).unwrap();
    let groups = communities(&graph);
    assert_eq!(
        score_all(&graph, &groups)
            .keys()
            .copied()
            .collect::<BTreeSet<_>>(),
        groups.keys().copied().collect()
    );
}

#[test]
fn test_cluster_does_not_write_to_stdout() {
    let mut graph = graph();
    cluster(&mut graph).unwrap();
}

#[test]
fn test_cluster_does_not_write_to_stderr() {
    let mut graph = graph();
    cluster(&mut graph).unwrap();
}

#[test]
fn test_remap_communities_to_previous_reuses_old_ids() {
    let groups = BTreeMap::from([
        (10, vec!["a".into(), "b".into(), "c".into()]),
        (11, vec!["d".into(), "e".into()]),
    ]);
    let previous = BTreeMap::from([
        ("a".into(), 5),
        ("b".into(), 5),
        ("c".into(), 5),
        ("d".into(), 1),
        ("e".into(), 1),
    ]);
    let remapped = remap_community_map(&groups, &previous);
    assert_eq!(
        remapped.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([1, 5])
    );
    assert_eq!(remapped[&5], ["a", "b", "c"]);
    assert_eq!(remapped[&1], ["d", "e"]);
}

#[test]
fn test_remap_communities_to_previous_assigns_deterministic_new_ids() {
    let groups = BTreeMap::from([
        (7, vec!["x".into(), "y".into(), "z".into()]),
        (8, vec!["m".into()]),
    ]);
    let previous = BTreeMap::from([("a".into(), 3)]);
    let remapped = remap_community_map(&groups, &previous);
    assert_eq!(remapped.keys().copied().collect::<Vec<_>>(), [0, 1]);
    assert_eq!(remapped[&0], ["x", "y", "z"]);
    assert_eq!(remapped[&1], ["m"]);
}
