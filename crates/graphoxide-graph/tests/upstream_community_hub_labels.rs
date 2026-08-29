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

fn mark_link(graph: &mut KnowledgeGraph, id: &str) {
    graph
        .nodes
        .iter_mut()
        .find(|node| node.id == id)
        .unwrap()
        .extra
        .insert("type".into(), "html_link".into());
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
fn semantic_heading_beats_a_higher_degree_file_node() {
    let mut graph = graph(
        &[
            ("capture", Some("capture-20260825.html")),
            ("heading", Some("Semantic heading")),
            ("outside-a", None),
            ("outside-b", None),
        ],
        &[
            ("capture", "heading"),
            ("capture", "outside-a"),
            ("capture", "outside-b"),
        ],
    );
    graph.nodes[0].source_file = "raw/capture-20260825.html".into();

    let labels = label_communities_by_hub(&graph, &groups(&[(0, &["capture", "heading"])]));

    assert_eq!(labels[&0], "Semantic heading");
}

#[test]
fn semantic_concept_beats_a_higher_degree_absolute_locator() {
    let graph = graph(
        &[
            ("locator", Some("https://example.test/schema#/Widget")),
            ("concept", Some("Widget contract")),
            ("outside-a", None),
            ("outside-b", None),
        ],
        &[
            ("locator", "concept"),
            ("locator", "outside-a"),
            ("locator", "outside-b"),
        ],
    );

    let labels = label_communities_by_hub(&graph, &groups(&[(0, &["locator", "concept"])]));

    assert_eq!(labels[&0], "Widget contract");
}

#[test]
fn semantic_label_beats_higher_degree_structured_fragment() {
    let mut graph = graph(
        &[
            ("fragment", Some("#icon-magnify")),
            ("semantic", Some("On This Page")),
            ("outside-a", None),
            ("outside-b", None),
        ],
        &[
            ("fragment", "semantic"),
            ("fragment", "outside-a"),
            ("fragment", "outside-b"),
        ],
    );
    mark_link(&mut graph, "fragment");

    let labels = label_communities_by_hub(&graph, &groups(&[(7, &["fragment", "semantic"])]));

    assert_eq!(labels[&7], "On This Page");
}

#[test]
fn semantic_label_beats_higher_degree_structured_relative_reference() {
    let mut graph = graph(
        &[
            ("relative", Some("../v3.0.1/")),
            ("semantic", Some("Release notes")),
            ("outside-a", None),
            ("outside-b", None),
        ],
        &[
            ("relative", "semantic"),
            ("relative", "outside-a"),
            ("relative", "outside-b"),
        ],
    );
    mark_link(&mut graph, "relative");

    let labels = label_communities_by_hub(&graph, &groups(&[(7, &["relative", "semantic"])]));

    assert_eq!(labels[&7], "Release notes");
}

#[test]
fn untyped_relative_and_pointer_references_are_navigation_artifacts() {
    let graph = graph(
        &[
            ("relative", Some("./Button")),
            ("pointer", Some("#/definitions/Widget")),
        ],
        &[("relative", "pointer")],
    );

    let labels = label_communities_by_hub(&graph, &groups(&[(7, &["relative", "pointer"])]));

    assert_eq!(labels[&7], "Community 7");
}

#[test]
fn compressed_document_wrapper_label_is_a_navigation_artifact() {
    let mut graph = graph(&[("wrapper", Some("capture-20260825"))], &[]);
    graph.nodes[0].file_type = "document".into();
    graph.nodes[0].source_file = "raw/capture-20260825.tar.gz".into();

    let labels = label_communities_by_hub(&graph, &groups(&[(7, &["wrapper"])]));

    assert_eq!(labels[&7], "Community 7");
}

#[test]
fn api_route_remains_a_meaningful_navigation_label() {
    let graph = graph(&[("route", Some("/ports/count"))], &[]);

    let labels = label_communities_by_hub(&graph, &groups(&[(7, &["route"])]));

    assert_eq!(labels[&7], "/ports/count");
}

#[test]
fn hash_prefixed_code_directive_is_not_a_link_artifact() {
    let graph = graph(
        &[
            ("include", Some("#include")),
            ("define", Some("#define")),
            ("outside-a", None),
            ("outside-b", None),
        ],
        &[
            ("include", "define"),
            ("define", "outside-a"),
            ("define", "outside-b"),
        ],
    );

    let labels = label_communities_by_hub(&graph, &groups(&[(7, &["include", "define"])]));

    assert_eq!(labels[&7], "#define");
}

#[test]
fn all_artifact_group_uses_explicit_community_fallback() {
    let mut graph = graph(
        &[
            ("capture", Some("capture-20260825.md")),
            ("locator", Some("https://example.test/schema")),
            ("fragment", Some("#fragment")),
            ("relative", Some("../path")),
            ("outside", None),
        ],
        &[
            ("locator", "capture"),
            ("locator", "fragment"),
            ("locator", "relative"),
            ("locator", "outside"),
        ],
    );
    graph.nodes[0].source_file = "docs/capture-20260825.md".into();
    mark_link(&mut graph, "fragment");
    mark_link(&mut graph, "relative");
    let members = ["capture", "locator", "fragment", "relative"];

    let labels = label_communities_by_hub(&graph, &groups(&[(7, &members)]));
    assert_eq!(labels[&7], "Community 7");

    graph.nodes.reverse();
    graph.links.reverse();
    let reversed = ["relative", "fragment", "locator", "capture"];
    let labels = label_communities_by_hub(&graph, &groups(&[(7, &reversed)]));
    assert_eq!(labels[&7], "Community 7");
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
