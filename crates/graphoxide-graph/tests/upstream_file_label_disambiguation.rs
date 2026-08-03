use graphoxide_core::{Extraction, Node};
use graphoxide_graph::{
    disambiguate_file_labels_in_extractions, disambiguate_file_labels_in_nodes, is_file_node_label,
    shortest_unique_suffix,
};
use std::collections::{BTreeMap, BTreeSet};

fn node(id: &str, label: &str, source_file: &str) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: None,
        community: None,
        extra: BTreeMap::new(),
    }
}

#[test]
fn test_disambiguate_raw_node_list_for_no_cluster_path() {
    let mut extractions = vec![Extraction {
        nodes: vec![
            node("po", "index.ts", "fn/process-order/index.ts"),
            node("sr", "index.ts", "fn/send-receipt/index.ts"),
            node("m", "main.ts", "main.ts"),
            node("sym", "handler", "fn/process-order/index.ts"),
        ],
        ..Extraction::default()
    }];
    disambiguate_file_labels_in_extractions(&mut extractions);
    let labels: BTreeMap<_, _> = extractions[0]
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect();
    assert_eq!(labels["po"], "process-order/index.ts");
    assert_eq!(labels["sr"], "send-receipt/index.ts");
    assert_eq!(labels["m"], "main.ts");
    assert_eq!(labels["sym"], "handler");
}

#[test]
fn test_is_file_node_label_and_suffix_helpers() {
    assert!(is_file_node_label("index.ts", "a/b/index.ts"));
    assert!(is_file_node_label("b/index.ts", "a/b/index.ts"));
    assert!(!is_file_node_label("index", "a/b/index.ts"));
    assert!(!is_file_node_label("helper()", "a/b/index.ts"));
    let paths = BTreeSet::from([
        "supabase/functions/process-order/index.ts".to_owned(),
        "supabase/functions/send-receipt/index.ts".to_owned(),
    ]);
    assert_eq!(
        shortest_unique_suffix("supabase/functions/process-order/index.ts", &paths),
        "process-order/index.ts"
    );
    assert_eq!(
        shortest_unique_suffix(
            "index.ts",
            &BTreeSet::from(["index.ts".to_owned(), "a/index.ts".to_owned()])
        ),
        "index.ts"
    );
}

#[test]
fn test_colliding_file_labels_are_qualified_uniques_left_bare() {
    let mut nodes = vec![
        node(
            "po",
            "index.ts",
            "supabase/functions/process-order/index.ts",
        ),
        node("sr", "index.ts", "supabase/functions/send-receipt/index.ts"),
        node("main", "main.ts", "src/main.ts"),
        node(
            "sym",
            "handler",
            "supabase/functions/process-order/index.ts",
        ),
    ];
    disambiguate_file_labels_in_nodes(&mut nodes);
    let labels: BTreeMap<_, _> = nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect();
    assert_eq!(labels["po"], "process-order/index.ts");
    assert_eq!(labels["sr"], "send-receipt/index.ts");
    assert_eq!(labels["main"], "main.ts");
    assert_eq!(labels["sym"], "handler");
}

#[test]
fn test_disambiguation_is_idempotent() {
    let mut nodes = vec![
        node("a", "index.ts", "x/a/index.ts"),
        node("b", "index.ts", "x/b/index.ts"),
    ];
    disambiguate_file_labels_in_nodes(&mut nodes);
    let first: Vec<_> = nodes.iter().map(|node| node.label.clone()).collect();
    disambiguate_file_labels_in_nodes(&mut nodes);
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.label.clone())
            .collect::<Vec<_>>(),
        first
    );
    assert_eq!(first, ["a/index.ts", "b/index.ts"]);
}

#[test]
fn test_three_way_collision_grows_suffix_until_unique() {
    let mut nodes = vec![
        node("a", "index.ts", "a/x/index.ts"),
        node("b", "index.ts", "b/x/index.ts"),
    ];
    disambiguate_file_labels_in_nodes(&mut nodes);
    assert_eq!(nodes[0].label, "a/x/index.ts");
    assert_eq!(nodes[1].label, "b/x/index.ts");
}
