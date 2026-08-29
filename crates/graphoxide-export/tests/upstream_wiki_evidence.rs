use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node, CONTAINER_SOURCE_ATTRIBUTE};
use graphoxide_export::project_wiki_evidence;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn annotation(source_id: &str, capture_id: &str, source_path: &str) -> Value {
    json!({
        "source_id": source_id,
        "capture_id": capture_id,
        "source_path": source_path,
        "sha256": SHA256,
        "captured_at": "2026-08-24T12:34:56Z",
        "accessed_at": "2026-08-24T12:35:56Z",
        "updated_at": "2026-08-24T12:34:56Z",
        "representation": "markdown",
        "source_system": "example",
        "url": "https://example.test/source",
        "location": "Library/Source",
    })
}

fn node(id: &str, label: &str, source_file: &str, source_id: &str, capture_id: &str) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "document".into(),
        source_file: source_file.into(),
        source_location: None,
        community: None,
        extra: BTreeMap::from([(
            "catalog".into(),
            json!({"source_id": source_id, "capture_id": capture_id, "sha256": SHA256}),
        )]),
    }
}

fn contains(source: &str, target: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "contains".into(),
        confidence: Confidence::Extracted,
        source_file: String::new(),
        extra: BTreeMap::new(),
    }
}

#[test]
fn projects_structured_json_value_and_path() {
    let mut value = node("value", "enabled", "config.json", "config", "capture-a");
    value.source_location = Some("L12".into());
    value.extra.extend([
        ("_origin".into(), "structured".into()),
        ("type".into(), "json_scalar".into()),
        ("structured_path".into(), "$.service.enabled".into()),
        ("structured_value".into(), json!({"enabled": true})),
        ("structured_value_redacted".into(), true.into()),
        ("structured_value_truncated".into(), true.into()),
    ]);
    let graph = KnowledgeGraph {
        nodes: vec![value],
        ..KnowledgeGraph::default()
    };

    let projection = project_wiki_evidence(&graph, None).unwrap();
    let source = &projection.sources[0];
    let block = &source.blocks[0];
    assert_eq!(source.citation, "config#capture-a");
    assert_eq!(block.kind, "document");
    assert_eq!(block.node_type.as_deref(), Some("json_scalar"));
    assert_eq!(block.structured_path.as_deref(), Some("$.service.enabled"));
    assert_eq!(block.value, Some(json!({"enabled": true})));
    assert_eq!(block.line, Some(12));
    assert_eq!(block.redacted_indicators, ["structured_value_redacted"]);
    assert_eq!(block.truncated_indicators, ["structured_value_truncated"]);
}

#[test]
fn redacted_structured_scalars_keep_the_selected_value_type() {
    let mut json_value = node(
        "json-value",
        "secret count",
        "config.json",
        "config",
        "capture-a",
    );
    json_value.extra.extend([
        ("structured_value".into(), "[REDACTED]".into()),
        ("structured_value_type".into(), "number".into()),
        ("structured_text_type".into(), "wrong-type".into()),
        ("structured_value_redacted".into(), true.into()),
    ]);
    let mut text_value = node(
        "text-value",
        "secret text",
        "config.json",
        "config",
        "capture-a",
    );
    text_value.extra.extend([
        ("structured_text".into(), "[REDACTED]".into()),
        ("structured_value_type".into(), "wrong-type".into()),
        ("structured_text_type".into(), "string".into()),
        ("structured_value_redacted".into(), true.into()),
    ]);

    let projection = project_wiki_evidence(
        &KnowledgeGraph {
            nodes: vec![text_value, json_value],
            ..KnowledgeGraph::default()
        },
        None,
    )
    .unwrap();
    let blocks = &projection.sources[0].blocks;
    let json_value = blocks
        .iter()
        .find(|block| block.node_id == "json-value")
        .unwrap();
    assert_eq!(json_value.value, Some(Value::String("[REDACTED]".into())));
    assert_eq!(json_value.value_type.as_deref(), Some("number"));
    let text_value = blocks
        .iter()
        .find(|block| block.node_id == "text-value")
        .unwrap();
    assert_eq!(text_value.value, Some(Value::String("[REDACTED]".into())));
    assert_eq!(text_value.value_type.as_deref(), Some("string"));
}

#[test]
fn projects_pdf_page_text_and_page_context() {
    let mut page = node("page-2", "Page 2", "manual.pdf", "manual", "capture-a");
    page.file_type = "paper".into();
    page.extra.extend([
        ("_origin".into(), "pdf".into()),
        ("type".into(), "pdf_page".into()),
        ("page_number".into(), 2.into()),
        ("text".into(), "Second page evidence".into()),
    ]);

    let projection = project_wiki_evidence(
        &KnowledgeGraph {
            nodes: vec![page],
            ..KnowledgeGraph::default()
        },
        None,
    )
    .unwrap();
    let block = &projection.sources[0].blocks[0];
    assert_eq!(block.page_number, Some(2));
    assert_eq!(
        block.value,
        Some(Value::String("Second page evidence".into()))
    );
}

#[test]
fn keeps_catalog_container_owner_and_virtual_member_path() {
    let outer = "bundles/reference.zip";
    let member = "bundles/reference.zip!/docs/guide.md";
    let mut block = node("guide", "Guide", member, "reference", "capture-a");
    block
        .extra
        .insert(CONTAINER_SOURCE_ATTRIBUTE.into(), outer.into());
    let annotations = BTreeMap::from([(outer.into(), annotation("reference", "capture-a", outer))]);

    let projection = project_wiki_evidence(
        &KnowledgeGraph {
            nodes: vec![block],
            ..KnowledgeGraph::default()
        },
        Some(&annotations),
    )
    .unwrap();
    let source = &projection.sources[0];
    assert_eq!(source.source_path.as_deref(), Some(outer));
    assert_eq!(source.blocks[0].source_file, member);
}

#[test]
fn heading_ancestry_uses_only_same_source_contains_edges() {
    let mut h1 = node("h1", "Install", "guide.md", "guide", "capture-a");
    h1.extra.insert("type".into(), "document_heading".into());
    let mut h2 = node("h2", "Linux", "guide.md", "guide", "capture-a");
    h2.extra.insert("type".into(), "document_heading".into());
    let mut body = node("body", "Commands", "guide.md", "guide", "capture-a");
    body.extra.insert("text".into(), "Run the installer".into());

    let projection = project_wiki_evidence(
        &KnowledgeGraph {
            nodes: vec![body, h2, h1],
            links: vec![contains("h1", "h2"), contains("h2", "body")],
            ..KnowledgeGraph::default()
        },
        None,
    )
    .unwrap();
    let body = projection.sources[0]
        .blocks
        .iter()
        .find(|block| block.node_id == "body")
        .unwrap();
    assert_eq!(body.heading_ancestry, ["Install", "Linux"]);
}

#[test]
fn shuffled_nodes_and_edges_project_identically() {
    let mut heading = node("heading", "Overview", "guide.md", "guide", "capture-a");
    heading
        .extra
        .insert("type".into(), "document_heading".into());
    let mut body = node("body", "Body", "guide.md", "guide", "capture-a");
    body.source_location = Some("L20".into());
    body.extra.insert("text".into(), "Evidence".into());
    let mut graph = KnowledgeGraph {
        nodes: vec![heading, body],
        links: vec![contains("heading", "body")],
        ..KnowledgeGraph::default()
    };
    let expected = project_wiki_evidence(&graph, None).unwrap();
    graph.nodes.reverse();
    graph.links.reverse();
    assert_eq!(project_wiki_evidence(&graph, None).unwrap(), expected);
}

#[test]
fn block_id_stays_stable_when_another_capture_of_the_source_is_added() {
    let first = node("first", "First", "guide.md", "guide", "capture-a");
    let baseline = project_wiki_evidence(
        &KnowledgeGraph {
            nodes: vec![first.clone()],
            ..KnowledgeGraph::default()
        },
        None,
    )
    .unwrap()
    .sources[0]
        .blocks[0]
        .id
        .clone();
    let second = node("second", "Second", "guide-old.md", "guide", "capture-b");
    let expanded = project_wiki_evidence(
        &KnowledgeGraph {
            nodes: vec![second, first],
            ..KnowledgeGraph::default()
        },
        None,
    )
    .unwrap();
    let first = expanded
        .sources
        .iter()
        .find(|source| source.capture_id == "capture-a")
        .unwrap();
    assert_eq!(first.blocks[0].id, baseline);
}

#[test]
fn invalid_containment_stays_root_level_and_reports_diagnostics() {
    let a = node("a", "A", "one.md", "one", "capture-a");
    let b = node("b", "B", "one.md", "one", "capture-a");
    let foreign = node("foreign", "Foreign", "two.md", "two", "capture-b");
    let missing_child = node("missing-child", "Missing", "one.md", "one", "capture-a");
    let foreign_child = node(
        "foreign-child",
        "Foreign child",
        "one.md",
        "one",
        "capture-a",
    );
    let projection = project_wiki_evidence(
        &KnowledgeGraph {
            nodes: vec![a, b, foreign, missing_child, foreign_child],
            links: vec![
                contains("a", "b"),
                contains("b", "a"),
                contains("missing-parent", "missing-child"),
                contains("foreign", "foreign-child"),
            ],
            ..KnowledgeGraph::default()
        },
        None,
    )
    .unwrap();
    let source = projection
        .sources
        .iter()
        .find(|source| source.source_id == "one")
        .unwrap();
    for node_id in ["a", "b", "missing-child", "foreign-child"] {
        assert!(source
            .blocks
            .iter()
            .find(|block| block.node_id == node_id)
            .unwrap()
            .heading_ancestry
            .is_empty());
    }
    let codes = source
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_str())
        .collect::<Vec<_>>();
    assert!(codes.contains(&"containment_cycle"));
    assert!(codes.contains(&"containment_missing_parent"));
    assert!(codes.contains(&"containment_foreign_parent"));
}

#[test]
fn preserves_partial_and_rejected_source_status_and_diagnostics() {
    let mut partial = node("partial", "Partial", "partial.dot", "partial", "capture-a");
    partial.extra.extend([
        ("format_capability".into(), "semantic_full".into()),
        ("parse_status".into(), "partial".into()),
        ("dot_diagnostics".into(), json!([{"code": "dot_limit"}])),
    ]);
    let mut rejected = node(
        "rejected",
        "Rejected",
        "rejected.pdf",
        "rejected",
        "capture-b",
    );
    rejected.extra.extend([
        ("format_capability".into(), "structural_partial".into()),
        ("parse_status".into(), "rejected".into()),
        ("diagnostic".into(), "pdf_parse_failed".into()),
    ]);

    let projection = project_wiki_evidence(
        &KnowledgeGraph {
            nodes: vec![rejected, partial],
            ..KnowledgeGraph::default()
        },
        None,
    )
    .unwrap();
    let partial = projection
        .sources
        .iter()
        .find(|source| source.source_id == "partial")
        .unwrap();
    assert_eq!(partial.capabilities, ["semantic_full"]);
    assert_eq!(partial.statuses, ["partial"]);
    assert_eq!(partial.diagnostics[0].code, "dot_diagnostics");
    let rejected = projection
        .sources
        .iter()
        .find(|source| source.source_id == "rejected")
        .unwrap();
    assert_eq!(rejected.statuses, ["rejected"]);
    assert_eq!(rejected.diagnostics[0].code, "diagnostic");
}
