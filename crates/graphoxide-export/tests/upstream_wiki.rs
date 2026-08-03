use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_export::{export_wiki_with_options, Communities, GodNodeArticle, WikiOptions};
use std::{collections::BTreeMap, fs, path::Path};
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
