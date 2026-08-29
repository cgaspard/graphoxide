use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node, CONTAINER_SOURCE_ATTRIBUTE};
use graphoxide_export::{
    derive_topic_tree, export_wiki_with_options, render_structured_wiki,
    render_structured_wiki_with_catalog, Communities, GodNodeArticle, Topic, TopicTree,
    WikiOptions,
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

fn node(id: &str, label: &str, source_file: &str, community: Option<i64>) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: None,
        community,
        extra: BTreeMap::new(),
    }
}

fn edge(source: &str, target: &str, relation: &str, confidence: Confidence) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence,
        source_file: String::new(),
        extra: BTreeMap::from([("weight".into(), 1.0.into())]),
    }
}

fn graph() -> KnowledgeGraph {
    KnowledgeGraph {
        directed: false,
        multigraph: false,
        nodes: vec![
            node("n1", "parse", "parser.py", Some(0)),
            node("n2", "validate", "parser.py", Some(0)),
            node("n3", "render", "renderer.py", Some(1)),
            node("n4", "stream", "renderer.py", Some(1)),
        ],
        links: vec![
            edge("n1", "n2", "calls", Confidence::Extracted),
            edge("n1", "n3", "references", Confidence::Inferred),
            edge("n3", "n4", "calls", Confidence::Extracted),
        ],
        hyperedges: vec![],
        extra: BTreeMap::new(),
    }
}

fn communities() -> Communities {
    BTreeMap::from([
        (0, vec!["n1".into(), "n2".into()]),
        (1, vec!["n3".into(), "n4".into()]),
    ])
}

fn options() -> WikiOptions {
    WikiOptions {
        community_labels: BTreeMap::from([
            (0, "Parsing Layer".into()),
            (1, "Rendering Layer".into()),
        ]),
        cohesion: BTreeMap::from([(0, 0.85), (1, 0.72)]),
        god_nodes: vec![GodNodeArticle {
            id: "n1".into(),
            label: "parse".into(),
            degree: 2,
        }],
    }
}

fn export(
    graph: &KnowledgeGraph,
    communities: &Communities,
    options: &WikiOptions,
) -> (TempDir, graphoxide_export::WikiReport) {
    let temp = tempfile::tempdir().unwrap();
    let report = export_wiki_with_options(graph, communities, temp.path(), options).unwrap();
    (temp, report)
}

fn read(directory: &Path, name: &str) -> String {
    fs::read_to_string(directory.join(name)).unwrap()
}

#[test]
fn test_to_wiki_writes_index() {
    let (temp, _) = export(&graph(), &communities(), &options());
    assert!(temp.path().join("index.md").is_file());
}

#[test]
fn test_to_wiki_returns_article_count() {
    let (_, report) = export(&graph(), &communities(), &options());
    assert_eq!(report.article_count, 3);
}

#[test]
fn test_to_wiki_community_articles_created() {
    let (temp, _) = export(&graph(), &communities(), &options());
    assert!(temp.path().join("Parsing_Layer.md").is_file());
    assert!(temp.path().join("Rendering_Layer.md").is_file());
}

#[test]
fn test_to_wiki_god_node_article_created() {
    let (temp, _) = export(&graph(), &communities(), &options());
    assert!(temp.path().join("parse.md").is_file());
}

#[test]
fn test_index_links_all_communities() {
    let (temp, _) = export(&graph(), &communities(), &options());
    let index = read(temp.path(), "index.md");
    assert!(index.contains("[Parsing Layer](Parsing_Layer.md)"));
    assert!(index.contains("[Rendering Layer](Rendering_Layer.md)"));
}

#[test]
fn test_index_lists_god_nodes() {
    let (temp, _) = export(&graph(), &communities(), &options());
    let index = read(temp.path(), "index.md");
    assert!(index.contains("[parse](parse.md)"));
    assert!(index.contains("2 connections"));
}

#[test]
fn test_community_article_has_cross_links() {
    let (temp, _) = export(&graph(), &communities(), &options());
    assert!(read(temp.path(), "Parsing_Layer.md").contains("[Rendering Layer](Rendering_Layer.md)"));
}

#[test]
fn test_community_article_shows_cohesion() {
    let (temp, _) = export(&graph(), &communities(), &options());
    assert!(read(temp.path(), "Parsing_Layer.md").contains("cohesion 0.85"));
}

#[test]
fn test_community_article_has_audit_trail() {
    let (temp, _) = export(&graph(), &communities(), &options());
    let article = read(temp.path(), "Parsing_Layer.md");
    assert!(article.contains("EXTRACTED"));
    assert!(article.contains("INFERRED"));
}

#[test]
fn test_god_node_article_has_connections() {
    let (temp, _) = export(&graph(), &communities(), &options());
    let article = read(temp.path(), "parse.md");
    assert!(article.contains("validate") && article.contains("render"));
    assert!(!article.contains("[["));
    assert!(!article.contains("](validate.md)") && !article.contains("](render.md)"));
}

#[test]
fn test_god_node_article_links_community() {
    let (temp, _) = export(&graph(), &communities(), &options());
    assert!(read(temp.path(), "parse.md").contains("[Parsing Layer](Parsing_Layer.md)"));
}

#[test]
fn test_to_wiki_skips_missing_god_node_ids() {
    let mut options = options();
    options.god_nodes = vec![GodNodeArticle {
        id: "nonexistent".into(),
        label: "ghost".into(),
        degree: 99,
    }];
    let (_, report) = export(&graph(), &communities(), &options);
    assert_eq!(report.article_count, 2);
}

#[test]
fn test_to_wiki_no_labels_uses_fallback() {
    let (temp, _) = export(&graph(), &communities(), &WikiOptions::default());
    assert!(temp.path().join("Community_0.md").is_file());
    assert!(temp.path().join("Community_1.md").is_file());
    let index = read(temp.path(), "index.md");
    assert!(index.contains("Community_0.md") && index.contains("Community_1.md"));
}

#[test]
fn test_article_navigation_footer() {
    let (temp, _) = export(&graph(), &communities(), &options());
    assert!(read(temp.path(), "Parsing_Layer.md").contains("[← index](index.md)"));
}

#[test]
fn test_community_article_truncation_notice() {
    let ids = (0..30).map(|index| format!("n{index}")).collect::<Vec<_>>();
    let graph = KnowledgeGraph {
        nodes: ids
            .iter()
            .map(|id| node(id, &format!("concept_{id}"), "a.py", Some(0)))
            .collect(),
        links: ids
            .windows(2)
            .map(|pair| edge(&pair[0], &pair[1], "calls", Confidence::Extracted))
            .collect(),
        ..KnowledgeGraph::default()
    };
    let options = WikiOptions {
        community_labels: BTreeMap::from([(0, "Big Community".into())]),
        ..WikiOptions::default()
    };
    let (temp, _) = export(&graph, &BTreeMap::from([(0, ids)]), &options);
    assert!(read(temp.path(), "Big_Community.md").contains("and 5 more nodes"));
}

fn two_node_graph(with_community: bool) -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: vec![
            node("n1", "parse", "parser.py", with_community.then_some(0)),
            node("n2", "render", "renderer.py", with_community.then_some(1)),
        ],
        links: vec![edge("n1", "n2", "references", Confidence::Inferred)],
        ..KnowledgeGraph::default()
    }
}

#[test]
fn test_cross_community_links_without_node_community_attrs() {
    let graph = two_node_graph(false);
    let communities = BTreeMap::from([(0, vec!["n1".into()]), (1, vec!["n2".into()])]);
    let options = WikiOptions {
        community_labels: BTreeMap::from([(0, "Parsing".into()), (1, "Rendering".into())]),
        ..WikiOptions::default()
    };
    let (temp, _) = export(&graph, &communities, &options);
    assert!(read(temp.path(), "Parsing.md").contains("[Rendering](Rendering.md)"));
}

#[test]
fn test_god_node_article_community_without_node_attr() {
    let graph = KnowledgeGraph {
        nodes: vec![
            node("n1", "parse", "parser.py", None),
            node("n2", "validate", "parser.py", None),
        ],
        links: vec![edge("n1", "n2", "calls", Confidence::Extracted)],
        ..KnowledgeGraph::default()
    };
    let communities = BTreeMap::from([(0, vec!["n1".into(), "n2".into()])]);
    let options = WikiOptions {
        community_labels: BTreeMap::from([(0, "Core Logic".into())]),
        god_nodes: vec![GodNodeArticle {
            id: "n1".into(),
            label: "parse".into(),
            degree: 1,
        }],
        ..WikiOptions::default()
    };
    let (temp, _) = export(&graph, &communities, &options);
    assert!(read(temp.path(), "parse.md").contains("[Core Logic](Core_Logic.md)"));
}

#[test]
fn test_to_wiki_drops_stale_community_nodes() {
    let communities = BTreeMap::from([
        (0, vec!["n1".into(), "n2".into(), "stale_ghost".into()]),
        (1, vec!["n3".into(), "n4".into()]),
    ]);
    let (temp, report) = export(&graph(), &communities, &options());
    assert_eq!(report.article_count, 3);
    let article = read(temp.path(), "Parsing_Layer.md");
    assert!(article.contains("parse"));
    assert!(!article.contains("stale_ghost"));
}

#[test]
fn test_to_wiki_all_stale_raises() {
    let temp = tempfile::tempdir().unwrap();
    let stale = BTreeMap::from([
        (0, vec!["ghost1".into(), "ghost2".into()]),
        (1, vec!["ghost3".into()]),
    ]);
    let error = export_wiki_with_options(&graph(), &stale, temp.path(), &options()).unwrap_err();
    assert!(error.to_string().contains("stale"));
}

#[test]
fn test_to_wiki_stale_nodes_prints_warning() {
    let communities = BTreeMap::from([
        (0, vec!["n1".into(), "stale1".into(), "stale2".into()]),
        (1, vec!["n3".into(), "n4".into()]),
    ]);
    let (_, report) = export(&graph(), &communities, &options());
    assert_eq!(report.stale_nodes_dropped, 2);
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains('2') && warning.contains("stale")));
}

#[test]
fn test_community_article_handles_null_source_file() {
    let graph = KnowledgeGraph {
        nodes: vec![
            node("n1", "parse", "", Some(0)),
            node("n2", "validate", "parser.py", Some(0)),
        ],
        links: vec![edge("n1", "n2", "calls", Confidence::Extracted)],
        ..KnowledgeGraph::default()
    };
    let (temp, _) = export(
        &graph,
        &BTreeMap::from([(0, vec!["n1".into(), "n2".into()])]),
        &WikiOptions {
            community_labels: BTreeMap::from([(0, "Parsing Layer".into())]),
            ..WikiOptions::default()
        },
    );
    assert!(temp.path().join("index.md").is_file());
}

fn collision_graph() -> (KnowledgeGraph, Communities) {
    (
        two_node_graph(true),
        BTreeMap::from([(0, vec!["n1".into()]), (1, vec!["n2".into()])]),
    )
}

#[test]
fn test_to_wiki_case_only_distinct_labels_dont_overwrite() {
    let (graph, communities) = collision_graph();
    let options = WikiOptions {
        community_labels: BTreeMap::from([(0, "Parser".into()), (1, "parser".into())]),
        ..WikiOptions::default()
    };
    let (temp, report) = export(&graph, &communities, &options);
    let mut articles = fs::read_dir(temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "index.md")
        .collect::<Vec<_>>();
    articles.sort();
    assert_eq!(articles.len(), report.article_count);
    let lowered = articles
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(lowered.len(), articles.len());
}

#[test]
fn test_to_wiki_god_node_label_case_collides_with_community() {
    let graph = KnowledgeGraph {
        nodes: vec![
            node("n1", "parse", "a.py", Some(0)),
            node("n2", "run", "b.py", Some(0)),
        ],
        links: vec![edge("n1", "n2", "calls", Confidence::Extracted)],
        ..KnowledgeGraph::default()
    };
    let options = WikiOptions {
        community_labels: BTreeMap::from([(0, "Parser".into())]),
        god_nodes: vec![GodNodeArticle {
            id: "n1".into(),
            label: "parser".into(),
            degree: 1,
        }],
        ..WikiOptions::default()
    };
    let (temp, report) = export(
        &graph,
        &BTreeMap::from([(0, vec!["n1".into(), "n2".into()])]),
        &options,
    );
    let names = fs::read_dir(temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_ascii_lowercase())
        .filter(|name| name != "index.md")
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), report.article_count);
}

#[test]
fn test_wiki_emits_no_obsidian_wikilinks() {
    let (temp, _) = export(&graph(), &communities(), &options());
    for entry in fs::read_dir(temp.path()).unwrap().filter_map(Result::ok) {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("md") {
            assert!(!fs::read_to_string(entry.path()).unwrap().contains("[["));
        }
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            output.push(u8::from_str_radix(&value[index + 1..index + 3], 16).unwrap());
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).unwrap()
}

fn markdown_targets(body: &str) -> Vec<String> {
    body.split("](")
        .skip(1)
        .filter_map(|tail| tail.split(')').next())
        .filter(|target| !target.contains("://"))
        .map(percent_decode)
        .collect()
}

#[test]
fn test_wiki_links_resolve_to_real_files() {
    let (temp, _) = export(&graph(), &communities(), &options());
    let mut seen = false;
    for entry in fs::read_dir(temp.path()).unwrap().filter_map(Result::ok) {
        if entry.path().extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        for target in markdown_targets(&fs::read_to_string(entry.path()).unwrap()) {
            seen = true;
            assert!(temp.path().join(&target).exists(), "{target}");
        }
    }
    assert!(seen);
}

#[test]
fn test_wiki_link_display_keeps_label_but_target_is_filename() {
    let (temp, _) = export(&graph(), &communities(), &options());
    let index = read(temp.path(), "index.md");
    assert!(index.contains("[Parsing Layer](Parsing_Layer.md)"));
    assert!(!index.contains("Parsing Layer.md"));
}

#[test]
fn test_wiki_special_characters_in_label_resolve() {
    let (graph, communities) = collision_graph();
    let options = WikiOptions {
        community_labels: BTreeMap::from([(0, "C# & Auth (v2)".into()), (1, "Other".into())]),
        ..WikiOptions::default()
    };
    let (temp, _) = export(&graph, &communities, &options);
    let article = read(temp.path(), "Other.md");
    assert!(markdown_targets(&article).contains(&"C#_&_Auth_(v2).md".into()));
    assert!(temp.path().join("C#_&_Auth_(v2).md").is_file());
    assert!(article.contains("C%23_%26_Auth_%28v2%29.md"));
}

#[test]
fn test_wiki_link_with_bracketed_label_resolves() {
    let (graph, communities) = collision_graph();
    let options = WikiOptions {
        community_labels: BTreeMap::from([(0, "Array[T] Models".into()), (1, "Other".into())]),
        ..WikiOptions::default()
    };
    let (temp, _) = export(&graph, &communities, &options);
    let article = read(temp.path(), "Other.md");
    assert!(article.contains(r"[Array\[T\] Models](Array%5BT%5D_Models.md)"));
    assert!(temp.path().join("Array[T]_Models.md").is_file());
}

#[test]
fn test_wiki_links_to_nodes_without_articles_are_plain_text() {
    let (temp, _) = export(&graph(), &communities(), &options());
    let article = read(temp.path(), "parse.md");
    assert!(article.contains("- validate") && article.contains("- render"));
    assert!(!article.contains("[[validate]]") && !article.contains("[[render]]"));
    assert!(!markdown_targets(&article)
        .iter()
        .any(|target| matches!(target.as_str(), "validate.md" | "render.md")));
}

#[test]
fn test_wiki_links_use_collision_suffixed_slug() {
    let (graph, communities) = collision_graph();
    let options = WikiOptions {
        community_labels: BTreeMap::from([(0, "Parser".into()), (1, "parser".into())]),
        ..WikiOptions::default()
    };
    let (temp, _) = export(&graph, &communities, &options);
    let targets = markdown_targets(&read(temp.path(), "index.md"));
    assert!(targets.contains(&"parser_2.md".into()));
    assert!(targets
        .iter()
        .all(|target| temp.path().join(target).exists()));
}

fn catalog_node(
    id: &str,
    label: &str,
    source_file: &str,
    community: Option<i64>,
    source_id: &str,
    capture_id: &str,
) -> Node {
    let mut node = node(id, label, source_file, community);
    node.extra.insert(
        "catalog".into(),
        serde_json::json!({
            "source_id": source_id,
            "capture_id": capture_id,
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }),
    );
    node
}

fn structured_graph() -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: vec![
            catalog_node(
                "n1",
                "Parser",
                "docs/parser.md",
                Some(1),
                "parser",
                "capture-a",
            ),
            catalog_node(
                "n2",
                "Grammar",
                "docs/parser.md",
                Some(1),
                "parser",
                "capture-a",
            ),
            catalog_node(
                "n3",
                "Shared",
                "docs/shared.md",
                Some(2),
                "shared",
                "capture-b",
            ),
            catalog_node(
                "n4",
                "Client",
                "docs/client.md",
                Some(2),
                "client",
                "capture-c",
            ),
            catalog_node("n5", "Loose", "docs/loose.md", None, "loose", "capture-d"),
        ],
        ..KnowledgeGraph::default()
    }
}

fn structured_tree() -> TopicTree {
    TopicTree {
        topics: vec![Topic {
            id: "topic-0".into(),
            label: "Language".into(),
            communities: vec![1, 2],
        }],
        community_paths: BTreeMap::from([(1, vec!["topic-0".into()]), (2, vec!["topic-0".into()])]),
    }
}

fn chunking_fixture(count: usize) -> (KnowledgeGraph, TopicTree) {
    let graph = KnowledgeGraph {
        nodes: (0..count)
            .rev()
            .map(|index| {
                catalog_node(
                    &format!("node-{index}"),
                    &format!("Node {index:03}"),
                    &format!("docs/{index}.md"),
                    Some(index as i64),
                    &format!("source-{index}"),
                    "capture",
                )
            })
            .collect(),
        ..KnowledgeGraph::default()
    };
    let topics = (0..count)
        .rev()
        .map(|index| Topic {
            id: format!("topic-{index}"),
            label: format!("Label {index:03}"),
            communities: vec![index as i64],
        })
        .collect();
    let community_paths = (0..count)
        .map(|index| (index as i64, vec![format!("topic-{index}")]))
        .collect();
    (
        graph,
        TopicTree {
            topics,
            community_paths,
        },
    )
}

fn active_catalog_annotation(
    source_id: &str,
    capture_id: &str,
    source_path: &str,
) -> serde_json::Value {
    serde_json::json!({
        "source_id": source_id,
        "capture_id": capture_id,
        "source_path": source_path,
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "captured_at": "2026-08-24T12:34:56Z",
        "accessed_at": "2026-08-24T12:35:56Z",
        "updated_at": "2026-08-24T12:34:56Z",
        "representation": "markdown",
        "source_system": "example",
        "url": "https://example.test/source",
        "location": "Library/Source",
    })
}

#[test]
fn structured_wiki_materializes_every_active_catalog_source_and_unrepresented_inventory() {
    let graph = KnowledgeGraph {
        nodes: vec![catalog_node(
            "extracted-node",
            "Extracted",
            "docs/extracted.md",
            Some(1),
            "extracted",
            "capture-extracted",
        )],
        ..KnowledgeGraph::default()
    };
    let tree = TopicTree {
        topics: vec![Topic {
            id: "topic-0".into(),
            label: "Topic".into(),
            communities: vec![1],
        }],
        community_paths: BTreeMap::from([(1, vec!["topic-0".into()])]),
    };
    let annotations = BTreeMap::from([
        (
            "docs/extracted.md".into(),
            active_catalog_annotation("extracted", "capture-extracted", "docs/extracted.md"),
        ),
        (
            "docs/unrepresented.md".into(),
            active_catalog_annotation(
                "unrepresented",
                "capture-unrepresented",
                "docs/unrepresented.md",
            ),
        ),
    ]);

    let plan = render_structured_wiki_with_catalog(&graph, &tree, &annotations).unwrap();

    assert!(plan.page("sources/extracted.md").is_some());
    assert!(plan.page("sources/unrepresented.md").is_some());
    assert!(plan.page("inventory/unrepresented.md").is_some());
    assert!(plan.page("inventory/extracted.md").is_none());
    assert!(plan
        .page("sources/unrepresented.md")
        .unwrap()
        .markdown
        .contains("[Inventory](../inventory/unrepresented.md)"));
    assert!(plan
        .page("inventory/unrepresented.md")
        .unwrap()
        .markdown
        .contains("parent: \"sources/unrepresented.md\""));
}

#[test]
fn structured_wiki_graph_only_wrapper_retains_the_catalog_free_render_plan() {
    let graph = structured_graph();
    assert_eq!(
        render_structured_wiki(&graph, &structured_tree()).unwrap(),
        render_structured_wiki_with_catalog(&graph, &structured_tree(), &BTreeMap::new()).unwrap()
    );
}

#[test]
fn structured_wiki_rejects_catalog_graph_active_capture_mismatches() {
    let graph = KnowledgeGraph {
        nodes: vec![catalog_node(
            "extracted-node",
            "Extracted",
            "docs/extracted.md",
            Some(1),
            "extracted",
            "capture-from-graph",
        )],
        ..KnowledgeGraph::default()
    };
    let tree = TopicTree {
        topics: vec![Topic {
            id: "topic-0".into(),
            label: "Topic".into(),
            communities: vec![1],
        }],
        community_paths: BTreeMap::from([(1, vec!["topic-0".into()])]),
    };
    let annotations = BTreeMap::from([(
        "docs/extracted.md".into(),
        active_catalog_annotation("extracted", "capture-from-catalog", "docs/extracted.md"),
    )]);

    assert!(render_structured_wiki_with_catalog(&graph, &tree, &annotations).is_err());
}

#[test]
fn structured_wiki_rejects_catalog_annotations_bound_to_another_source_path() {
    let graph = KnowledgeGraph {
        nodes: vec![catalog_node(
            "misbound-node",
            "Misbound",
            "docs/other.md",
            Some(1),
            "source-a",
            "capture-a",
        )],
        ..KnowledgeGraph::default()
    };
    let annotations = BTreeMap::from([(
        "docs/active.md".into(),
        active_catalog_annotation("source-a", "capture-a", "docs/active.md"),
    )]);

    assert!(render_structured_wiki_with_catalog(&graph, &structured_tree(), &annotations).is_err());
}

#[test]
fn structured_wiki_rejects_an_unannotated_node_from_an_active_catalog_source() {
    let graph = KnowledgeGraph {
        nodes: vec![node("missing-node", "Missing", "docs/active.md", Some(1))],
        ..KnowledgeGraph::default()
    };
    let annotations = BTreeMap::from([(
        "docs/active.md".into(),
        active_catalog_annotation("source-a", "capture-a", "docs/active.md"),
    )]);

    assert!(render_structured_wiki_with_catalog(&graph, &structured_tree(), &annotations).is_err());
}

#[test]
fn structured_wiki_binds_container_members_to_their_active_container_source() {
    let mut member = catalog_node(
        "member-node",
        "Member",
        "raw/archive.zip!/member.md",
        Some(1),
        "archive-source",
        "capture-a",
    );
    member.extra.insert(
        CONTAINER_SOURCE_ATTRIBUTE.into(),
        serde_json::json!("raw/archive.zip"),
    );
    let graph = KnowledgeGraph {
        nodes: vec![member],
        ..KnowledgeGraph::default()
    };
    let annotations = BTreeMap::from([(
        "raw/archive.zip".into(),
        active_catalog_annotation("archive-source", "capture-a", "raw/archive.zip"),
    )]);

    let plan = render_structured_wiki_with_catalog(&graph, &structured_tree(), &annotations)
        .expect("container source annotation binds");
    assert!(plan.page("sources/archive-source.md").is_some());
    assert!(plan.page("inventory/archive-source.md").is_none());
}

#[test]
fn structured_wiki_covers_each_annotated_source_once_and_links_community_sources() {
    let plan = render_structured_wiki(&structured_graph(), &structured_tree()).unwrap();
    let paths = plan
        .pages
        .iter()
        .map(|page| page.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(paths.contains("index.md"));
    assert!(plan.page("topics/topic-0.md").is_some());
    assert!(plan.page("communities/1.md").is_some());
    assert!(plan.page("communities/2.md").is_some());
    assert!(paths.iter().any(|path| path.starts_with("topics/")));
    assert_eq!(
        paths
            .iter()
            .filter(|path| path.starts_with("communities/"))
            .count(),
        2
    );
    assert!(!paths.contains("topics/topic-0.md"));
    assert!(!paths.contains("communities/1.md"));
    assert!(paths.contains("sources/parser.md"));
    assert!(paths.contains("sources/shared.md"));
    assert!(paths.contains("sources/client.md"));
    assert!(paths.contains("sources/loose.md"));
    assert_eq!(
        paths
            .iter()
            .filter(|path| path.starts_with("sources/"))
            .count(),
        4
    );

    let parser_community = plan.page("communities/1.md").unwrap();
    assert!(parser_community
        .markdown
        .contains("[parser](../sources/parser.md)"));
    assert!(parser_community.markdown.contains("  - parser#capture-a\n"));
    assert!(!parser_community.markdown.contains("  - shared#capture-b\n"));
}

#[test]
fn structured_wiki_assigns_shared_source_one_primary_community() {
    let mut graph = structured_graph();
    graph.nodes.push(catalog_node(
        "n6",
        "Shared parser detail",
        "docs/shared.md",
        Some(1),
        "shared",
        "capture-b",
    ));
    let plan = render_structured_wiki(&graph, &structured_tree()).unwrap();
    let source = plan.page("sources/shared.md").unwrap();
    let primary = plan.page("communities/1.md").unwrap().path.as_str();
    let secondary = plan.page("communities/2.md").unwrap().path.as_str();
    assert!(source.markdown.contains(&format!("parent: \"{primary}\"")));
    assert!(source.markdown.contains(&format!("](../{primary})")));
    assert!(!source.markdown.contains(&format!("../{secondary}")));
    assert!(plan
        .page("communities/2.md")
        .unwrap()
        .markdown
        .contains("[shared](../sources/shared.md)"));
}

#[test]
fn structured_wiki_is_identical_after_shuffling_graph_input() {
    let graph = structured_graph();
    let expected = render_structured_wiki(&graph, &structured_tree()).unwrap();
    let mut shuffled = graph;
    shuffled.nodes.reverse();
    shuffled.links.reverse();
    let actual = render_structured_wiki(&shuffled, &structured_tree()).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn structured_wiki_orders_root_links_by_label_and_stable_topic_id() {
    let graph = KnowledgeGraph {
        nodes: vec![
            catalog_node("one", "One", "one.md", Some(1), "one", "capture-one"),
            catalog_node("two", "Two", "two.md", Some(2), "two", "capture-two"),
            catalog_node(
                "three",
                "Three",
                "three.md",
                Some(3),
                "three",
                "capture-three",
            ),
        ],
        ..KnowledgeGraph::default()
    };
    let tree = TopicTree {
        topics: vec![
            Topic {
                id: "topic-0".into(),
                label: "Zulu".into(),
                communities: vec![1],
            },
            Topic {
                id: "topic-2".into(),
                label: "Alpha".into(),
                communities: vec![3],
            },
            Topic {
                id: "topic-1".into(),
                label: "Alpha".into(),
                communities: vec![2],
            },
        ],
        community_paths: BTreeMap::from([
            (1, vec!["topic-0".into()]),
            (2, vec!["topic-1".into()]),
            (3, vec!["topic-2".into()]),
        ]),
    };

    let plan = render_structured_wiki(&graph, &tree).unwrap();
    let index = &plan.page("index.md").unwrap().markdown;
    assert_eq!(index.matches("### Topics 1–3").count(), 1);
    let first_path = &plan.page("topics/topic-1.md").unwrap().path;
    let second_path = &plan.page("topics/topic-2.md").unwrap().path;
    let third_path = &plan.page("topics/topic-0.md").unwrap().path;
    let first = index.find(&format!("[Alpha]({first_path})")).unwrap();
    let second = index.find(&format!("[Alpha]({second_path})")).unwrap();
    let third = index.find(&format!("[Zulu]({third_path})")).unwrap();
    assert!(first < second && second < third);

    let mut relabeled = tree;
    for topic in &mut relabeled.topics {
        topic.label = format!("Renamed {}", topic.id);
    }
    assert_eq!(
        plan.pages.iter().map(|page| &page.path).collect::<Vec<_>>(),
        render_structured_wiki(&graph, &relabeled)
            .unwrap()
            .pages
            .iter()
            .map(|page| &page.path)
            .collect::<Vec<_>>()
    );
}

#[test]
fn structured_wiki_propagates_explicit_fallbacks_without_path_or_citation_changes() {
    let mut graph = KnowledgeGraph {
        nodes: vec![
            catalog_node(
                "capture",
                "capture-20260510t093300z.txt",
                "raw/capture-20260510t093300z.txt",
                Some(7),
                "capture-source",
                "capture-a",
            ),
            catalog_node(
                "fragment",
                "#fragment",
                "raw/capture-20260510t093300z.txt",
                Some(7),
                "capture-source",
                "capture-a",
            ),
            catalog_node(
                "relative",
                "../path",
                "raw/capture-20260510t093300z.txt",
                Some(7),
                "capture-source",
                "capture-a",
            ),
        ],
        links: vec![
            edge("fragment", "capture", "references", Confidence::Extracted),
            edge("fragment", "relative", "references", Confidence::Extracted),
        ],
        ..KnowledgeGraph::default()
    };
    for node in &mut graph.nodes[1..] {
        node.extra.insert("type".into(), "html_link".into());
    }
    let mut semantic = graph.clone();
    semantic.nodes[1].label = "Semantic fragment".into();
    semantic.nodes[2].label = "Semantic path".into();
    let baseline =
        render_structured_wiki(&semantic, &derive_topic_tree(&semantic).unwrap()).unwrap();

    let plan = render_structured_wiki(&graph, &derive_topic_tree(&graph).unwrap()).unwrap();
    assert_eq!(
        plan.pages.iter().map(|page| &page.path).collect::<Vec<_>>(),
        baseline
            .pages
            .iter()
            .map(|page| &page.path)
            .collect::<Vec<_>>()
    );
    let topic = plan.page("topics/topic-0.md").unwrap();
    assert!(!topic.markdown.contains("title: \"Topic 0\""));
    assert!(!topic.markdown.contains("# Topic 0"));
    let community = plan.page("communities/7.md").unwrap();
    assert!(!community.markdown.contains("title: \"Community 7\""));
    assert!(!community.markdown.contains("# Community 7"));

    let citations = |plan: &graphoxide_export::StructuredWikiPlan| {
        plan.pages
            .iter()
            .flat_map(|page| {
                page.markdown
                    .lines()
                    .filter(|line| line.starts_with("  - ") && line.contains('#'))
                    .map(|line| (page.path.clone(), line.to_owned()))
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(citations(&plan), citations(&baseline));
}

#[test]
fn structured_wiki_chunks_the_root_without_adding_pages() {
    let (graph, tree) = chunking_fixture(101);
    let plan = render_structured_wiki(&graph, &tree).unwrap();
    let index = &plan.page("index.md").unwrap().markdown;

    let first = index.find("### Topics 1–100").unwrap();
    let second = index.find("### Topics 101–101").unwrap();
    assert!(first < second);
    assert_eq!(index.matches("### Topics 1–100").count(), 1);
    assert_eq!(index.matches("### Topics 101–101").count(), 1);
    assert_eq!(index.matches("](topics/").count(), 101);
    for topic in 0..101 {
        assert!(plan.page(&format!("topics/topic-{topic}.md")).is_some());
    }
    assert!(!plan
        .pages
        .iter()
        .any(|page| page.path.starts_with("topic-index/")));
}

#[test]
fn structured_wiki_root_chunking_is_deterministic_after_shuffling() {
    let (graph, tree) = chunking_fixture(101);
    let expected = render_structured_wiki(&graph, &tree).unwrap();
    let mut shuffled_graph = graph;
    shuffled_graph.nodes.reverse();
    shuffled_graph.links.reverse();
    let mut shuffled_tree = tree;
    shuffled_tree.topics.reverse();

    assert_eq!(
        render_structured_wiki(&shuffled_graph, &shuffled_tree).unwrap(),
        expected
    );
}

#[test]
fn structured_wiki_keeps_normal_sized_community_citations_and_links_every_source_page() {
    let graph = KnowledgeGraph {
        nodes: (0..13)
            .map(|index| {
                catalog_node(
                    &format!("n{index}"),
                    &format!("Source {index}"),
                    &format!("docs/{index}.md"),
                    Some(1),
                    &format!("source-{index}"),
                    "capture",
                )
            })
            .collect(),
        ..KnowledgeGraph::default()
    };
    let tree = TopicTree {
        topics: vec![Topic {
            id: "topic-0".into(),
            label: "Topic".into(),
            communities: vec![1],
        }],
        community_paths: BTreeMap::from([(1, vec!["topic-0".into()])]),
    };
    let page = render_structured_wiki(&graph, &tree)
        .unwrap()
        .page("communities/1.md")
        .unwrap()
        .markdown
        .clone();
    assert_eq!(page.matches("  - source-").count(), 13);
    assert_eq!(page.matches("../sources/source-").count(), 13);
}

fn page_digest(page: &graphoxide_export::StructuredWikiPage) -> String {
    page.markdown
        .lines()
        .find_map(|line| line.strip_prefix("input_sha256: "))
        .unwrap()
        .trim_matches('"')
        .into()
}

fn serialized_frontmatter_bytes(page: &graphoxide_export::StructuredWikiPage) -> usize {
    page.markdown.find("\n---\n\n").unwrap() + "\n---\n\n".len()
}

#[test]
fn structured_wiki_bounds_catalog_id_filenames_and_serialized_frontmatter() {
    let max_source_id = |index: usize| format!("s{index}{}", "a".repeat(4_094));
    let max_capture_id = format!("c{}", "b".repeat(4_095));
    let graph = KnowledgeGraph {
        nodes: (0..8)
            .map(|index| {
                catalog_node(
                    &format!("n{index}"),
                    "Source",
                    "docs/source.md",
                    Some(1),
                    &max_source_id(index),
                    &max_capture_id,
                )
            })
            .collect(),
        ..KnowledgeGraph::default()
    };
    let plan = render_structured_wiki(
        &graph,
        &TopicTree {
            topics: vec![Topic {
                id: "topic-0".into(),
                label: "Topic".into(),
                communities: vec![1],
            }],
            community_paths: BTreeMap::from([(1, vec!["topic-0".into()])]),
        },
    )
    .unwrap();
    for page in plan
        .pages
        .iter()
        .filter(|page| page.path.starts_with("sources/"))
    {
        assert!(PathBuf::from(&page.path).file_name().unwrap().len() <= 255);
    }
    let community = plan.page("communities/1.md").unwrap();
    assert!(serialized_frontmatter_bytes(community) <= 64 * 1024);
    assert_eq!(community.markdown.matches("  - s").count(), 7);

    let invalid = KnowledgeGraph {
        nodes: vec![catalog_node(
            "too-long",
            "Too long",
            "docs/too-long.md",
            Some(1),
            &format!("s{}", "a".repeat(4_096)),
            "capture",
        )],
        ..KnowledgeGraph::default()
    };
    assert!(render_structured_wiki(&invalid, &structured_tree()).is_err());
}

#[test]
fn structured_wiki_allows_an_exact_full_frontmatter() {
    let tree = TopicTree {
        topics: vec![Topic {
            id: "topic-0".into(),
            label: "Topic".into(),
            communities: vec![1],
        }],
        community_paths: BTreeMap::from([(1, vec!["topic-0".into()])]),
    };
    let probe = KnowledgeGraph {
        nodes: vec![catalog_node(
            "probe",
            "Source",
            "docs/source.md",
            Some(1),
            "p",
            "c",
        )],
        ..KnowledgeGraph::default()
    };
    let base = serialized_frontmatter_bytes(
        render_structured_wiki(&probe, &tree)
            .unwrap()
            .page("communities/1.md")
            .unwrap(),
    ) - "  - p#c\n".len();
    let max_citation = 4_096 + 1 + 4_096;
    let final_citation = (64 * 1024 - base) - 7 * ("  - \n".len() + max_citation) - "  - \n".len();
    assert!((3..max_citation).contains(&final_citation));
    let graph_for = |last_citation| KnowledgeGraph {
        nodes: (0..8)
            .map(|index| {
                let citation_length = if index == 7 {
                    last_citation
                } else {
                    max_citation
                };
                let source_length = (citation_length - 1).saturating_sub(4_096).max(1);
                let capture_length = citation_length - source_length - 1;
                catalog_node(
                    &format!("n{index}"),
                    "Source",
                    "docs/source.md",
                    Some(1),
                    &format!("s{index}{}", "a".repeat(source_length - 2)),
                    &format!("c{}", "b".repeat(capture_length - 1)),
                )
            })
            .collect(),
        ..KnowledgeGraph::default()
    };
    let page = render_structured_wiki(&graph_for(final_citation), &tree)
        .unwrap()
        .page("communities/1.md")
        .unwrap()
        .clone();
    assert_eq!(serialized_frontmatter_bytes(&page), 64 * 1024);
    assert_eq!(page.markdown.matches("  - ").count(), 8);

    let overfull = render_structured_wiki(&graph_for(final_citation + 1), &tree)
        .unwrap()
        .page("communities/1.md")
        .unwrap()
        .clone();
    assert!(serialized_frontmatter_bytes(&overfull) <= 64 * 1024);
}

#[test]
fn structured_wiki_digests_track_capture_hashes_and_placement() {
    let graph = structured_graph();
    let baseline = render_structured_wiki(&graph, &structured_tree()).unwrap();

    let mut changed_hash = graph.clone();
    changed_hash.nodes[0].extra.insert(
        "catalog".into(),
        serde_json::json!({
            "source_id": "parser",
            "capture_id": "capture-a",
            "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        }),
    );
    changed_hash.nodes[1].extra.insert(
        "catalog".into(),
        serde_json::json!({
            "source_id": "parser",
            "capture_id": "capture-a",
            "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        }),
    );
    let hashed = render_structured_wiki(&changed_hash, &structured_tree()).unwrap();
    for path in [
        "index.md",
        "topics/topic-0.md",
        "communities/1.md",
        "sources/parser.md",
    ] {
        assert_ne!(
            page_digest(baseline.page(path).unwrap()),
            page_digest(hashed.page(path).unwrap())
        );
    }
    for path in [
        "communities/2.md",
        "sources/client.md",
        "sources/shared.md",
        "sources/loose.md",
    ] {
        assert_eq!(
            page_digest(baseline.page(path).unwrap()),
            page_digest(hashed.page(path).unwrap()),
            "unaffected page {path} changed after an active capture update"
        );
    }

    let mut moved = graph;
    moved.nodes[2].community = Some(1);
    let placed = render_structured_wiki(&moved, &structured_tree()).unwrap();
    assert_ne!(
        page_digest(baseline.page("sources/shared.md").unwrap()),
        page_digest(placed.page("sources/shared.md").unwrap())
    );
    assert_ne!(
        page_digest(baseline.page("communities/1.md").unwrap()),
        page_digest(placed.page("communities/1.md").unwrap())
    );
    assert_eq!(
        page_digest(baseline.page("sources/loose.md").unwrap()),
        page_digest(placed.page("sources/loose.md").unwrap())
    );

    let split_tree = TopicTree {
        topics: vec![
            Topic {
                id: "topic-0".into(),
                label: "Language".into(),
                communities: vec![1],
            },
            Topic {
                id: "topic-1".into(),
                label: "Clients".into(),
                communities: vec![2],
            },
        ],
        community_paths: BTreeMap::from([(1, vec!["topic-0".into()]), (2, vec!["topic-1".into()])]),
    };
    let repartitioned = render_structured_wiki(&structured_graph(), &split_tree).unwrap();
    for path in [
        "index.md",
        "topics/topic-0.md",
        "communities/2.md",
        "sources/shared.md",
    ] {
        assert_ne!(
            page_digest(baseline.page(path).unwrap()),
            page_digest(repartitioned.page(path).unwrap())
        );
    }
}

#[test]
fn structured_wiki_escapes_backslashes_before_brackets_in_links() {
    let mut graph = structured_graph();
    graph.nodes[0].label = "A\\[B]".into();
    graph.nodes[1].label = "A\\[B]".into();
    graph.nodes[0]
        .extra
        .insert("community_name".into(), "C\\[D]".into());
    let mut tree = structured_tree();
    tree.topics[0].label = "T\\[E]".into();
    let plan = render_structured_wiki(&graph, &tree).unwrap();
    let topic_path = &plan.page("topics/topic-0.md").unwrap().path;
    let community_path = &plan.page("communities/1.md").unwrap().path;
    assert!(plan
        .page("index.md")
        .unwrap()
        .markdown
        .contains(&format!(r"[T\\\[E\]]({topic_path})")));
    assert!(plan
        .page("topics/topic-0.md")
        .unwrap()
        .markdown
        .contains(&format!(r"[parser](../{community_path})")));
    assert!(plan
        .page("communities/1.md")
        .unwrap()
        .markdown
        .contains(r"[parser](../sources/parser.md)"));
    assert!(plan
        .page("communities/1.md")
        .unwrap()
        .markdown
        .contains(&format!(r"[← T\\\[E\]](../{topic_path})")));
    assert!(plan
        .page("sources/parser.md")
        .unwrap()
        .markdown
        .contains(&format!(r"[← parser](../{community_path})")));
}

#[test]
fn structured_wiki_recomputes_semantic_community_name_after_shuffling() {
    let mut graph = structured_graph();
    graph.nodes[0].label = "capture-20260825.md".into();
    graph.nodes[0].source_file = "docs/capture-20260825.md".into();
    for node in &mut graph.nodes[..2] {
        node.extra
            .insert("community_name".into(), "Stale capture label".into());
    }
    graph.links = vec![
        edge("n1", "n2", "contains", Confidence::Extracted),
        edge("n1", "n5", "contains", Confidence::Extracted),
    ];
    let expected = render_structured_wiki(&graph, &structured_tree()).unwrap();
    graph.nodes.reverse();
    graph.links.reverse();
    let actual = render_structured_wiki(&graph, &structured_tree()).unwrap();
    assert_eq!(actual, expected);
    assert!(expected
        .page("communities/1.md")
        .unwrap()
        .markdown
        .contains("# capture-20260825"));
}

#[test]
fn structured_wiki_renders_related_communities_only_from_graph_links() {
    let mut graph = KnowledgeGraph {
        nodes: vec![
            catalog_node("one", "One", "docs/one.md", Some(1), "one", "capture-one"),
            catalog_node("two", "Two", "docs/two.md", Some(2), "two", "capture-two"),
            catalog_node(
                "three",
                "Three",
                "docs/three.md",
                Some(3),
                "three",
                "capture-three",
            ),
        ],
        links: vec![
            edge("one", "two", "references", Confidence::Extracted),
            Edge {
                extra: BTreeMap::from([("weight".into(), 0.0.into())]),
                ..edge("one", "three", "references", Confidence::Extracted)
            },
        ],
        ..KnowledgeGraph::default()
    };
    let tree = TopicTree {
        topics: vec![
            Topic {
                id: "topic-0".into(),
                label: "One".into(),
                communities: vec![1],
            },
            Topic {
                id: "topic-1".into(),
                label: "Two".into(),
                communities: vec![2],
            },
            Topic {
                id: "topic-2".into(),
                label: "Three".into(),
                communities: vec![3],
            },
        ],
        community_paths: BTreeMap::from([
            (1, vec!["topic-0".into()]),
            (2, vec!["topic-1".into()]),
            (3, vec!["topic-2".into()]),
        ]),
    };

    let expected = render_structured_wiki(&graph, &tree).unwrap();
    let first = expected.page("communities/1.md").unwrap();
    let second_path = &expected.page("communities/2.md").unwrap().path;
    assert!(first.markdown.contains("## Related communities"));
    assert!(first.markdown.contains(&format!("[two](../{second_path})")));
    assert!(!first.markdown.contains("Three"));
    assert!(!expected
        .page("communities/3.md")
        .unwrap()
        .markdown
        .contains("## Related communities"));

    let mut reweighted_graph = graph.clone();
    reweighted_graph.links[0]
        .extra
        .insert("weight".into(), serde_json::json!(2.0));
    let reweighted = render_structured_wiki(&reweighted_graph, &tree).unwrap();
    assert_ne!(
        page_digest(first),
        page_digest(reweighted.page("communities/1.md").unwrap())
    );

    graph.nodes.reverse();
    graph.links.reverse();
    assert_eq!(render_structured_wiki(&graph, &tree).unwrap(), expected);
}

#[test]
fn structured_wiki_keeps_repeated_v1_sources_distinct_and_renders_provenance() {
    let graph = KnowledgeGraph {
        nodes: vec![
            catalog_node(
                "shared-one",
                "Shared one",
                "docs/shared-one.md",
                Some(1),
                "shared",
                "capture-one",
            ),
            catalog_node(
                "shared-two",
                "Shared two",
                "docs/shared-two.md",
                Some(2),
                "shared",
                "capture-two",
            ),
            catalog_node(
                "single",
                "Single",
                "docs/single.md",
                Some(1),
                "single",
                "capture-one",
            ),
        ],
        ..KnowledgeGraph::default()
    };
    let tree = TopicTree {
        topics: vec![
            Topic {
                id: "topic-0".into(),
                label: "One".into(),
                communities: vec![1],
            },
            Topic {
                id: "topic-1".into(),
                label: "Two".into(),
                communities: vec![2],
            },
        ],
        community_paths: BTreeMap::from([(1, vec!["topic-0".into()]), (2, vec!["topic-1".into()])]),
    };
    let mut one = active_catalog_annotation("shared", "capture-one", "docs/shared-one.md");
    one["source_system"] = "Share[Point]".into();
    one["url"] = "https://example.test/shared-one".into();
    one["location"] = "Library/Shared [one]".into();
    one["representation"] = "PDF".into();
    let mut two = active_catalog_annotation("shared", "capture-two", "docs/shared-two.md");
    two["location"] = "Library/Shared two".into();
    let annotations = BTreeMap::from([
        ("docs/shared-one.md".into(), one.clone()),
        ("docs/shared-two.md".into(), two),
        (
            "docs/single.md".into(),
            active_catalog_annotation("single", "capture-one", "docs/single.md"),
        ),
        (
            "docs/unrepresented.md".into(),
            active_catalog_annotation("unrepresented", "capture-one", "docs/unrepresented.md"),
        ),
    ]);

    let plan = render_structured_wiki_with_catalog(&graph, &tree, &annotations).unwrap();
    let source = plan.page("sources/shared--capture-one.md").unwrap();
    assert!(plan.page("sources/shared--capture-two.md").is_some());
    assert!(plan.page("sources/single.md").is_some());
    assert!(source
        .markdown
        .contains("graph_ref: \"shared#capture-one\""));
    assert!(plan
        .page("sources/single.md")
        .unwrap()
        .markdown
        .contains("graph_ref: \"single\""));
    assert!(source.markdown.contains("## Provenance"));
    assert!(source.markdown.contains("Share\\[Point\\]"));
    assert!(source.markdown.contains("https://example.test/shared-one"));
    assert!(source.markdown.contains("Library/Shared \\[one\\]"));
    assert!(source.markdown.contains("docs/shared-one.md"));
    assert!(source.markdown.contains("PDF"));
    assert!(source.markdown.contains("2026-08-24T12:34:56Z"));
    assert!(source
        .markdown
        .contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    let inventory = plan.page("inventory/unrepresented.md").unwrap();
    assert!(inventory.markdown.contains("## Provenance"));
    assert!(inventory.markdown.contains("docs/unrepresented.md"));

    let mut changed_annotations = annotations;
    changed_annotations.get_mut("docs/shared-one.md").unwrap()["location"] =
        "Library/Changed".into();
    let changed = render_structured_wiki_with_catalog(&graph, &tree, &changed_annotations).unwrap();
    assert_ne!(
        page_digest(source),
        page_digest(changed.page("sources/shared--capture-one.md").unwrap())
    );
    assert_eq!(
        page_digest(inventory),
        page_digest(changed.page("inventory/unrepresented.md").unwrap())
    );
    let mut inventory_annotations = changed_annotations;
    inventory_annotations
        .get_mut("docs/unrepresented.md")
        .unwrap()["location"] = "Library/Changed inventory".into();
    let inventory_changed =
        render_structured_wiki_with_catalog(&graph, &tree, &inventory_annotations).unwrap();
    assert_ne!(
        page_digest(inventory),
        page_digest(
            inventory_changed
                .page("inventory/unrepresented.md")
                .unwrap()
        )
    );
}

#[test]
fn structured_wiki_tolerates_partial_graph_only_provenance() {
    let mut node = catalog_node(
        "legacy",
        "Legacy PDF",
        "docs/legacy.pdf",
        Some(1),
        "legacy",
        "capture-one",
    );
    node.extra.get_mut("catalog").unwrap()["representation"] = "pdf".into();
    let graph = KnowledgeGraph {
        nodes: vec![node],
        ..KnowledgeGraph::default()
    };
    let tree = TopicTree {
        topics: vec![Topic {
            id: "topic-0".into(),
            label: "Legacy".into(),
            communities: vec![1],
        }],
        community_paths: BTreeMap::from([(1, vec!["topic-0".into()])]),
    };

    let page = render_structured_wiki(&graph, &tree)
        .expect("legacy graph-only annotation renders")
        .page("sources/legacy.md")
        .unwrap()
        .markdown
        .clone();
    assert!(page.contains("## Provenance"));
    assert!(page.contains("- Representation: pdf"));
    assert!(!page.contains("- Source system:"));

    let active_annotations = BTreeMap::from([(
        "docs/legacy.pdf".into(),
        graph.nodes[0].extra["catalog"].clone(),
    )]);
    assert!(render_structured_wiki_with_catalog(&graph, &tree, &active_annotations).is_err());
}

#[test]
fn structured_wiki_merges_partial_graph_only_provenance_independently_of_node_order() {
    let mut first = catalog_node(
        "one",
        "Shared one",
        "docs/shared.pdf",
        Some(1),
        "shared",
        "capture-one",
    );
    first.extra.get_mut("catalog").unwrap()["representation"] = "pdf".into();
    first.extra.get_mut("catalog").unwrap()["source_system"] = "Zulu archive".into();
    let mut second = catalog_node(
        "two",
        "Shared two",
        "docs/shared.pdf",
        Some(1),
        "shared",
        "capture-one",
    );
    second.extra.get_mut("catalog").unwrap()["source_system"] = "Archive".into();
    let mut graph = KnowledgeGraph {
        nodes: vec![first, second],
        ..KnowledgeGraph::default()
    };
    let tree = TopicTree {
        topics: vec![Topic {
            id: "topic-0".into(),
            label: "Shared".into(),
            communities: vec![1],
        }],
        community_paths: BTreeMap::from([(1, vec!["topic-0".into()])]),
    };

    let expected = render_structured_wiki(&graph, &tree).unwrap();
    let page = expected.page("sources/shared.md").unwrap();
    assert!(page.markdown.contains("- Source system: Archive"));
    assert!(!page.markdown.contains("Zulu archive"));
    assert!(page.markdown.contains("- Representation: pdf"));
    graph.nodes.reverse();
    assert_eq!(render_structured_wiki(&graph, &tree).unwrap(), expected);
}

#[test]
fn structured_wiki_materializes_each_unrepresented_repeated_v1_capture() {
    let first = active_catalog_annotation("shared", "capture-one", "docs/shared-one.pdf");
    let second = active_catalog_annotation("shared", "capture-two", "docs/shared-two.pdf");
    let annotations = BTreeMap::from([
        ("docs/shared-one.pdf".into(), first.clone()),
        ("docs/shared-two.pdf".into(), second.clone()),
    ]);
    let graph = KnowledgeGraph::default();
    let tree = TopicTree::default();

    let expected = render_structured_wiki_with_catalog(&graph, &tree, &annotations).unwrap();
    for (capture, path) in [
        ("capture-one", "sources/shared--capture-one.md"),
        ("capture-two", "sources/shared--capture-two.md"),
    ] {
        let reference = format!("shared#{capture}");
        let source = expected.page(path).expect("capture source page");
        let inventory_path = path.replacen("sources/", "inventory/", 1);
        let inventory = expected
            .page(&inventory_path)
            .expect("capture inventory page");
        assert!(source
            .markdown
            .contains(&format!("graph_ref: \"{reference}\"")));
        assert!(inventory
            .markdown
            .contains(&format!("graph_ref: \"{reference}\"")));
        assert!(inventory.markdown.contains(&format!("parent: \"{path}\"")));
    }

    let reversed = BTreeMap::from([
        ("docs/shared-two.pdf".into(), second),
        ("docs/shared-one.pdf".into(), first),
    ]);
    assert_eq!(
        render_structured_wiki_with_catalog(&graph, &tree, &reversed).unwrap(),
        expected
    );
}
