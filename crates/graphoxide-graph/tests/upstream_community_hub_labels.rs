use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_graph::{community_member_sigs, label_communities_by_hub};
use std::collections::BTreeMap;

fn graph(labels: &[(&str, Option<&str>)], edges: &[(&str, &str)]) -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: labels
            .iter()
            .map(|(id, label)| Node {
                id: (*id).into(),
                label: label.unwrap_or_default().into(),
                file_type: "code".into(),
                source_file: "sample.py".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::new(),
            })
            .collect(),
        links: edges
            .iter()
            .map(|(source, target)| Edge {
                source: (*source).into(),
                target: (*target).into(),
                relation: "related".into(),
                confidence: Confidence::Extracted,
                source_file: "sample.py".into(),
                extra: BTreeMap::new(),
            })
            .collect(),
        ..KnowledgeGraph::default()
    }
}

fn groups(values: &[(i64, &[&str])]) -> BTreeMap<i64, Vec<String>> {
    values
        .iter()
        .map(|(id, members)| (*id, members.iter().map(|member| (*member).into()).collect()))
        .collect()
}

#[test]
fn test_labels_by_highest_degree_hub() {
    let graph = graph(
        &[
            ("a", Some("log_action()")),
            ("b", Some("b()")),
            ("c", Some("c()")),
            ("d", Some("d()")),
        ],
        &[("a", "b"), ("a", "c"), ("a", "d")],
    );
    let labels = label_communities_by_hub(&graph, &groups(&[(0, &["a", "b", "c", "d"])]));
    assert_eq!(labels[&0], "log_action");
}

#[test]
fn test_not_a_placeholder_for_a_real_community() {
    let graph = graph(
        &[("a", Some("handler()")), ("b", Some("b()"))],
        &[("a", "b")],
    );
    let labels = label_communities_by_hub(&graph, &groups(&[(0, &["a", "b"])]));
    assert_eq!(labels[&0], "handler");
    assert_ne!(labels[&0], "Community 0");
}

#[test]
fn test_tie_breaks_deterministically_by_node_id() {
    let graph = graph(&[("z", Some("z()")), ("a", Some("a()"))], &[("z", "a")]);
    assert_eq!(
        label_communities_by_hub(&graph, &groups(&[(0, &["z", "a"])]))[&0],
        "a"
    );
    assert_eq!(
        label_communities_by_hub(&graph, &groups(&[(0, &["a", "z"])]))[&0],
        "a"
    );
}

#[test]
fn test_absent_members_fall_back_to_placeholder() {
    let graph = graph(&[("a", Some("a()"))], &[]);
    assert_eq!(
        label_communities_by_hub(&graph, &groups(&[(5, &["ghost1", "ghost2"])]))[&5],
        "Community 5"
    );
}

#[test]
fn test_node_without_label_attr_uses_id() {
    let graph = graph(
        &[("hub", None), ("x", None), ("y", None)],
        &[("hub", "x"), ("hub", "y")],
    );
    assert_eq!(
        label_communities_by_hub(&graph, &groups(&[(0, &["hub", "x", "y"])]))[&0],
        "hub"
    );
}

#[test]
fn test_multiple_communities_each_get_their_own_hub() {
    let graph = graph(
        &[
            ("h1", Some("auth()")),
            ("a1", Some("a1()")),
            ("a2", Some("a2()")),
            ("h2", Some("billing()")),
            ("b1", Some("b1()")),
            ("b2", Some("b2()")),
        ],
        &[("h1", "a1"), ("h1", "a2"), ("h2", "b1"), ("h2", "b2")],
    );
    let labels = label_communities_by_hub(
        &graph,
        &groups(&[(0, &["h1", "a1", "a2"]), (1, &["h2", "b1", "b2"])]),
    );
    assert_eq!(labels[&0], "auth");
    assert_eq!(labels[&1], "billing");
}

#[test]
fn test_community_member_sigs_are_deterministic_and_order_independent() {
    let first = community_member_sigs(&groups(&[(0, &["x", "y", "z"]), (1, &["a"])]));
    let second = community_member_sigs(&groups(&[(0, &["z", "x", "y"]), (1, &["a"])]));
    assert_eq!(first, second);
    assert_ne!(first[&0], first[&1]);
    assert_eq!(
        first.values().map(String::len).collect::<Vec<_>>(),
        [16, 16]
    );
}

#[test]
fn test_community_member_sigs_change_when_membership_changes() {
    let before = community_member_sigs(&groups(&[(0, &["x", "y", "z"])]));
    let after = community_member_sigs(&groups(&[(0, &["x", "y"])]));
    assert_ne!(before[&0], after[&0]);
}
