use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_export::{
    derive_topic_tree, load_wiki_plan, render_canonical_wiki, render_structured_wiki_with_catalog,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn annotations() -> BTreeMap<String, Value> {
    BTreeMap::from([(
        "docs/guide.md".into(),
        json!({
            "source_id": "guide",
            "capture_id": "capture-current",
            "source_path": "docs/guide.md",
            "sha256": SHA256,
            "captured_at": "2026-08-24T12:34:56Z",
            "accessed_at": "2026-08-24T12:35:56Z",
            "updated_at": "2026-08-24T12:34:56Z",
            "representation": "markdown",
            "source_system": "example",
            "url": "https://example.test/guide",
            "location": "Library/Guide",
        }),
    )])
}

fn plan() -> graphoxide_export::WikiPlan {
    let citations = BTreeSet::from(["guide#capture-current".into(), "guide#capture-old".into()]);
    load_wiki_plan(
        br#"{
          "version": 1,
          "domains": [{"id":"getting-started","title":"Getting started","slug":"getting-started"}],
          "sources": [
            {"id":"guide#capture-current","title":"Installation guide","slug":"installation-guide","domain":"getting-started","coverage":"complete"},
            {"id":"guide#capture-old","title":"Installation guide archive","slug":"installation-guide-archive","domain":"getting-started","coverage":"inventory-only"}
          ],
          "articles": [{
            "id":"installation","title":"Installation","slug":"installation","domain":"getting-started",
            "article_type":"procedure","sources":["guide#capture-current"],"aliases":["Setup"],"related":[]
          }]
        }"#,
        &citations,
    )
    .unwrap()
}

fn graph() -> KnowledgeGraph {
    let catalog = json!({
        "source_id": "guide",
        "capture_id": "capture-current",
        "sha256": SHA256,
    });
    let mut heading = Node {
        id: "heading".into(),
        label: "Install".into(),
        file_type: "markdown".into(),
        source_file: "docs/guide.md".into(),
        source_location: Some("L1".into()),
        community: Some(7),
        extra: BTreeMap::from([
            ("catalog".into(), catalog.clone()),
            ("type".into(), "document_heading".into()),
            ("line_start".into(), 1.into()),
        ]),
    };
    heading.extra.insert("_origin".into(), "markdown".into());
    let body = Node {
        id: "paragraph".into(),
        label: "paragraph".into(),
        file_type: "markdown".into(),
        source_file: "docs/guide.md".into(),
        source_location: Some("L3".into()),
        community: Some(7),
        extra: BTreeMap::from([
            ("catalog".into(), catalog),
            ("type".into(), "document_paragraph".into()),
            (
                "structured_text".into(),
                "Run the installer with the approved command.".into(),
            ),
            ("structured_text_type".into(), "string".into()),
            ("line_start".into(), 3.into()),
        ]),
    };
    let endpoint = Node {
        id: "endpoint".into(),
        label: "/install".into(),
        file_type: "code".into(),
        source_file: "docs/guide.md".into(),
        source_location: Some("L4".into()),
        community: Some(7),
        extra: BTreeMap::from([
            (
                "catalog".into(),
                json!({
                    "source_id": "guide",
                    "capture_id": "capture-current",
                    "sha256": SHA256,
                }),
            ),
            ("type".into(), "endpoint".into()),
            ("line_start".into(), 4.into()),
        ]),
    };
    KnowledgeGraph {
        nodes: vec![heading, body, endpoint],
        links: vec![
            Edge {
                source: "heading".into(),
                target: "paragraph".into(),
                relation: "contains".into(),
                confidence: Confidence::Extracted,
                source_file: "docs/guide.md".into(),
                extra: BTreeMap::new(),
            },
            Edge {
                source: "heading".into(),
                target: "endpoint".into(),
                relation: "contains".into(),
                confidence: Confidence::Extracted,
                source_file: "docs/guide.md".into(),
                extra: BTreeMap::new(),
            },
        ],
        ..KnowledgeGraph::default()
    }
}

#[test]
fn canonical_render_materializes_reviewed_navigation_and_source_grounded_reference() {
    let rendered = render_canonical_wiki(&graph(), &plan(), &annotations()).unwrap();
    let paths = rendered
        .pages
        .iter()
        .map(|page| page.path.as_str())
        .collect::<BTreeSet<_>>();
    assert!(paths.contains("index.md"));
    assert!(paths.contains("AGENTS.md"));
    let topic_path = paths
        .iter()
        .find(|path| path.starts_with("topics/install-"))
        .expect("readable topic path");
    let community_path = paths
        .iter()
        .find(|path| path.starts_with("communities/installation-guide-"))
        .expect("readable community path");
    assert!(!paths.contains("topics/topic-0.md"));
    assert!(!paths.contains("communities/7.md"));
    assert!(paths.contains("getting-started/index.md"));
    assert!(paths.contains("getting-started/installation--installation.md"));
    assert!(paths.contains("sources/installation-guide.md"));
    assert!(paths.contains("sources/installation-guide-archive.md"));
    assert!(paths.contains("inventory/installation-guide-archive.md"));
    let reference_path = paths
        .iter()
        .find(|path| path.starts_with("references/installation-guide--"))
        .expect("canonical reference path");
    assert!(!reference_path.contains("capture-current"));

    let article = rendered
        .page("getting-started/installation--installation.md")
        .unwrap()
        .markdown
        .as_str();
    assert!(article.contains("# Installation\n"));
    assert!(article.contains("[Installation guide](../sources/installation-guide.md)"));
    assert!(article.contains("guide#capture-current"));

    let root = rendered.page("index.md").unwrap().markdown.as_str();
    assert!(root.contains("## Graph topics"));
    assert!(root.contains(&format!("[Install]({topic_path})")));

    let community = rendered.page(community_path).unwrap().markdown.as_str();
    assert!(community.contains("# Installation guide"));
    assert!(community.contains("[Installation guide](../sources/installation-guide.md)"));

    let source = rendered
        .page("sources/installation-guide.md")
        .unwrap()
        .markdown
        .as_str();
    assert!(source.contains("# Installation guide\n"));
    assert!(source.contains("## Extraction coverage"));
    assert!(source.contains("## Technical reference"));
    assert!(source.contains("Library/Guide"));

    let reference = rendered
        .pages
        .iter()
        .find(|page| page.path.starts_with("references/installation-guide--"))
        .unwrap()
        .markdown
        .as_str();
    assert!(reference.contains("# Installation guide — Install\n"));
    assert!(reference.contains("Run the installer with the approved command."));
    assert!(reference.contains("### /install"));
    assert!(reference.contains("- Node type: endpoint"));
    assert!(reference.contains("guide#capture-current"));

    let historical = rendered
        .page("sources/installation-guide-archive.md")
        .unwrap()
        .markdown
        .as_str();
    assert!(historical.contains("coverage: \"inventory-only\""));
    assert!(historical.contains("No extracted graph evidence is available"));
    assert!(historical.contains("[Inventory](../inventory/installation-guide-archive.md)"));
}

#[test]
fn structured_render_uses_readable_navigation_routes() {
    let graph = graph();
    let topics = derive_topic_tree(&graph).expect("derive graph topics");
    let rendered = render_structured_wiki_with_catalog(&graph, &topics, &annotations())
        .expect("render catalog-backed structural wiki");
    let paths = rendered
        .pages
        .iter()
        .map(|page| page.path.as_str())
        .collect::<BTreeSet<_>>();

    assert!(paths.iter().any(|path| path.starts_with("topics/guide-")));
    assert!(paths
        .iter()
        .any(|path| path.starts_with("communities/guide-")));
    assert!(!paths.contains("topics/topic-0.md"));
    assert!(!paths.contains("communities/7.md"));
}

#[test]
fn canonical_render_rejects_false_complete_coverage() {
    let mut graph = graph();
    graph.nodes.clear();
    graph.links.clear();
    let error = render_canonical_wiki(&graph, &plan(), &annotations()).unwrap_err();
    assert!(error.to_string().contains("complete coverage"));
}

#[test]
fn canonical_render_rejects_complete_coverage_after_partial_extraction() {
    let mut graph = graph();
    graph.nodes[1]
        .extra
        .insert("parse_status".into(), "unsupported-partial".into());
    let error = render_canonical_wiki(&graph, &plan(), &annotations()).unwrap_err();
    assert!(error.to_string().contains("complete coverage"));
}

#[test]
fn canonical_render_is_deterministic_after_graph_shuffle() {
    let plan = plan();
    let annotations = annotations();
    let mut shuffled = graph();
    let expected = render_canonical_wiki(&shuffled, &plan, &annotations).unwrap();
    shuffled.nodes.reverse();
    shuffled.links.reverse();
    assert_eq!(
        render_canonical_wiki(&shuffled, &plan, &annotations).unwrap(),
        expected
    );
}
