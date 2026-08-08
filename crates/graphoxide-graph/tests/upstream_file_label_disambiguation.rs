use graphoxide_core::{Extraction, Node};
use graphoxide_graph::{
    disambiguate_file_labels_in_extractions, disambiguate_file_labels_in_nodes, is_file_node_label,
    shortest_unique_suffix,
};
use serde_json::json;
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

#[test]
fn test_graphviz_file_roots_are_qualified_without_rewriting_declared_labels() {
    let mut root_a = node("root_a", "architecture.dot", "a/architecture.dot");
    root_a.file_type = "document".into();
    root_a
        .extra
        .insert("diagram_format".into(), json!("graphviz"));
    let mut root_b = node("root_b", "architecture.dot", "b/architecture.dot");
    root_b.file_type = "document".into();
    root_b
        .extra
        .insert("diagram_format".into(), json!("graphviz"));
    let mut declared_a = node("declared_a", "architecture.dot", "a/architecture.dot");
    declared_a.file_type = "document".into();
    declared_a.extra.extend([
        ("diagram_format".into(), json!("graphviz")),
        ("dot_id".into(), json!("architecture.dot")),
    ]);
    let mut declared_b = node("declared_b", "architecture.dot", "b/architecture.dot");
    declared_b.file_type = "document".into();
    declared_b.extra.extend([
        ("diagram_format".into(), json!("graphviz")),
        ("dot_id".into(), json!("architecture.dot")),
    ]);
    let mut nodes = vec![root_a, root_b, declared_a, declared_b];

    disambiguate_file_labels_in_nodes(&mut nodes);
    let labels = nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(labels["root_a"], "a/architecture.dot");
    assert_eq!(labels["root_b"], "b/architecture.dot");
    assert_eq!(labels["declared_a"], "architecture.dot");
    assert_eq!(labels["declared_b"], "architecture.dot");
}

#[test]
fn test_document_package_roots_are_qualified_without_rewriting_declared_unit_labels() {
    let mut root_a = node("root_a", "report.docx", "a/report.docx");
    root_a.file_type = "document".into();
    root_a
        .extra
        .insert("_origin".into(), json!("document_package"));
    let mut root_b = node("root_b", "report.docx", "b/report.docx");
    root_b.file_type = "document".into();
    root_b
        .extra
        .insert("_origin".into(), json!("document_package"));
    let mut unit_a = node("unit_a", "report.docx", "a/report.docx");
    unit_a.file_type = "document".into();
    unit_a.extra.extend([
        ("_origin".into(), json!("document_package")),
        ("unit_ordinal".into(), json!(1)),
    ]);
    let mut unit_b = node("unit_b", "report.docx", "b/report.docx");
    unit_b.file_type = "document".into();
    unit_b.extra.extend([
        ("_origin".into(), json!("document_package")),
        ("unit_ordinal".into(), json!(1)),
    ]);
    let mut part_a = node("part_a", "report.docx", "a/report.docx");
    part_a.file_type = "document".into();
    part_a.extra.extend([
        ("_origin".into(), json!("document_package")),
        ("internal_part".into(), json!("parts/report.docx")),
    ]);
    let mut part_b = node("part_b", "report.docx", "b/report.docx");
    part_b.file_type = "document".into();
    part_b.extra.extend([
        ("_origin".into(), json!("document_package")),
        ("internal_part".into(), json!("parts/report.docx")),
    ]);
    let mut nodes = vec![root_a, root_b, unit_a, unit_b, part_a, part_b];

    disambiguate_file_labels_in_nodes(&mut nodes);
    let labels = nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(labels["root_a"], "a/report.docx");
    assert_eq!(labels["root_b"], "b/report.docx");
    assert_eq!(labels["unit_a"], "report.docx");
    assert_eq!(labels["unit_b"], "report.docx");
    assert_eq!(labels["part_a"], "report.docx");
    assert_eq!(labels["part_b"], "report.docx");
}
