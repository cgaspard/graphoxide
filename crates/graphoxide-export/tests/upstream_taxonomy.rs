use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_export::{derive_topic_tree, TopicTree};
use std::collections::BTreeMap;

fn graph(nodes: &[(&str, &str, i64)], links: &[(&str, &str, f64)]) -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: nodes
            .iter()
            .map(|(id, label, community)| Node {
                id: (*id).into(),
                label: (*label).into(),
                file_type: "concept".into(),
                source_file: String::new(),
                source_location: None,
                community: Some(*community),
                extra: BTreeMap::new(),
            })
            .collect(),
        links: links
            .iter()
            .map(|(source, target, weight)| Edge {
                source: (*source).into(),
                target: (*target).into(),
                relation: "relates".into(),
                confidence: Confidence::Extracted,
                source_file: String::new(),
                extra: BTreeMap::from([("weight".into(), (*weight).into())]),
            })
            .collect(),
        ..KnowledgeGraph::default()
    }
}

fn mark_link(graph: &mut KnowledgeGraph, id: &str) {
    graph
        .nodes
        .iter_mut()
        .find(|node| node.id == id)
        .unwrap()
        .extra
        .insert("type".into(), "html_link".into());
}

fn assert_complete(tree: &TopicTree, communities: &[i64]) {
    let mut actual = tree
        .topics
        .iter()
        .flat_map(|topic| topic.communities.iter().copied())
        .collect::<Vec<_>>();
    actual.sort_unstable();
    assert_eq!(actual, communities);
    assert!(tree.topics.iter().all(|topic| topic.label != "Other"));
    assert_eq!(tree.community_paths.len(), communities.len());
    assert!(tree
        .community_paths
        .values()
        .all(|path| path.len() == 1 && path[0].starts_with("topic-")));
}

#[test]
fn shuffled_graph_input_has_the_same_taxonomy() {
    let graph = graph(
        &[
            ("a", "alpha", 7),
            ("b", "beta", 7),
            ("c", "gamma", 11),
            ("d", "delta", 13),
        ],
        &[
            ("a", "c", 1e16),
            ("b", "c", 1.0),
            ("a", "c", 1.0),
            ("c", "d", 1.0),
        ],
    );
    let expected = derive_topic_tree(&graph).unwrap();

    let mut shuffled = graph;
    shuffled.nodes.reverse();
    shuffled.links.reverse();
    let actual = derive_topic_tree(&shuffled).unwrap();

    assert_eq!(actual, expected);
    assert_complete(&actual, &[7, 11, 13]);
}

#[test]
fn disconnected_community_is_retained_in_the_taxonomy() {
    let tree = derive_topic_tree(&graph(
        &[("a", "alpha", 4), ("b", "beta", 8), ("c", "gamma", 15)],
        &[("a", "b", 4.0)],
    ))
    .unwrap();

    assert_complete(&tree, &[4, 8, 15]);
    assert!(tree
        .topics
        .iter()
        .any(|topic| topic.communities == vec![15]));
}

#[test]
fn weighted_cross_community_links_project_the_taxonomy_once() {
    let tree = derive_topic_tree(&graph(
        &[("a", "alpha", 1), ("b", "beta", 2)],
        &[("a", "b", 3.0), ("b", "a", 5.0)],
    ))
    .unwrap();

    assert_complete(&tree, &[1, 2]);
    assert!(tree
        .topics
        .iter()
        .any(|topic| topic.communities == vec![1, 2]));
}

#[test]
fn finite_cross_community_weight_overflow_keeps_the_taxonomy_complete() {
    let tree = derive_topic_tree(&graph(
        &[("a", "alpha", 1), ("b", "beta", 2)],
        &[("a", "b", f64::MAX), ("b", "a", f64::MAX)],
    ))
    .unwrap();

    assert_complete(&tree, &[1, 2]);
}

#[test]
fn relative_cross_community_weights_keep_dense_taxonomy_pairs_together() {
    let tree = derive_topic_tree(&graph(
        &[
            ("a", "alpha", 1),
            ("b", "beta", 2),
            ("c", "gamma", 3),
            ("d", "delta", 4),
        ],
        &[("a", "b", 10.0), ("c", "d", 10.0), ("b", "c", 0.01)],
    ))
    .unwrap();

    assert!(tree
        .topics
        .iter()
        .any(|topic| topic.communities == vec![1, 2]));
    assert!(tree
        .topics
        .iter()
        .any(|topic| topic.communities == vec![3, 4]));
}

#[test]
fn topic_uses_the_most_cross_community_connected_child_label() {
    let mut graph = graph(
        &[
            ("document", "capture-20260825.html", 1),
            ("document-child-a", "Document child A", 1),
            ("document-child-b", "Document child B", 1),
            ("semantic", "Semantic child", 2),
            ("other", "Other child", 3),
        ],
        &[
            ("document", "document-child-a", 1.0),
            ("document", "document-child-b", 1.0),
            ("document", "semantic", 6.0),
            ("semantic", "other", 5.0),
        ],
    );
    graph.nodes[0].source_file = "raw/capture-20260825.html".into();

    let tree = derive_topic_tree(&graph).unwrap();
    let topic = tree
        .topics
        .iter()
        .find(|topic| topic.communities == vec![1, 2, 3])
        .expect("connected communities share a topic");

    assert_eq!(topic.label, "Semantic child");
}

#[test]
fn topic_prefers_non_fallback_child_before_cross_community_degree() {
    let mut graph = graph(
        &[
            ("artifact", "/feed", 1),
            ("semantic", "Supply Chain", 2),
            ("other", "Other child", 3),
        ],
        &[("artifact", "semantic", 10.0), ("artifact", "other", 5.0)],
    );
    mark_link(&mut graph, "artifact");

    let tree = derive_topic_tree(&graph).unwrap();
    let topic = tree
        .topics
        .iter()
        .find(|topic| topic.communities == vec![1, 2, 3])
        .expect("connected communities share a topic");

    assert_eq!(topic.label, "Supply Chain");
    assert_eq!(topic.communities, vec![1, 2, 3]);
    assert_eq!(tree.community_paths[&1], vec![topic.id.clone()]);
    assert_eq!(tree.community_paths[&2], vec![topic.id.clone()]);
    assert_eq!(tree.community_paths[&3], vec![topic.id.clone()]);
}

#[test]
fn all_artifact_singleton_topic_uses_explicit_topic_fallback() {
    let mut graph = graph(
        &[
            ("capture", "capture-20260510t093300z.txt", 7),
            ("fragment", "#fragment", 7),
            ("relative", "../path", 7),
        ],
        &[("capture", "fragment", 1.0), ("fragment", "relative", 1.0)],
    );
    graph.nodes[0].source_file = "raw/capture-20260510t093300z.txt".into();
    mark_link(&mut graph, "fragment");
    mark_link(&mut graph, "relative");

    let tree = derive_topic_tree(&graph).unwrap();

    assert_eq!(tree.topics[0].id, "topic-0");
    assert_eq!(tree.topics[0].label, "Topic 0");
    assert_eq!(tree.topics[0].communities, vec![7]);
    assert_eq!(tree.community_paths[&7], vec!["topic-0"]);
}

#[test]
fn navigation_labels_do_not_change_topic_placement() {
    let mut graph = graph(
        &[("a", "#fragment", 1), ("b", "beta", 2), ("c", "gamma", 3)],
        &[("a", "b", 4.0), ("b", "c", 3.0)],
    );
    mark_link(&mut graph, "a");
    let before = derive_topic_tree(&graph).unwrap();
    let mut relabeled = graph;
    relabeled.nodes[0].label = "alpha".into();
    relabeled.nodes[1].label = "../reference".into();
    mark_link(&mut relabeled, "b");
    let after = derive_topic_tree(&relabeled).unwrap();

    assert_ne!(
        before
            .topics
            .iter()
            .map(|topic| &topic.label)
            .collect::<Vec<_>>(),
        after
            .topics
            .iter()
            .map(|topic| &topic.label)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        before
            .topics
            .iter()
            .map(|topic| (&topic.id, &topic.communities))
            .collect::<Vec<_>>(),
        after
            .topics
            .iter()
            .map(|topic| (&topic.id, &topic.communities))
            .collect::<Vec<_>>()
    );
    assert_eq!(before.community_paths, after.community_paths);
}
