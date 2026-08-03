//! Executable, case-by-case port of upstream `tests/test_export.py`.

use graphoxide_core::{Confidence, Edge, Extraction, KnowledgeGraph, Node};
use graphoxide_export::{
    backup_if_protected, existing_graph_node_count, export_graph_json, export_vault_with_options,
    node_filenames, obsidian_safe_stem, render_canvas, render_cypher, render_graphml, render_html,
    render_html_with_options, write_graphml, Communities, ExistingGraphNodeCount, HtmlOptions,
    VaultOptions,
};
use graphoxide_graph::build_graph;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path, sync::Mutex};
use tempfile::tempdir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn sample_graph() -> KnowledgeGraph {
    let extraction: Extraction = serde_json::from_str(include_str!(
        "../../../tests/fixtures/upstream/extraction.json"
    ))
    .unwrap();
    let mut graph = build_graph(&[extraction]).unwrap();
    for (index, node) in graph.nodes.iter_mut().enumerate() {
        node.community = Some(if index < 2 { 0 } else { 1 });
    }
    graph
}

fn node(id: &str, label: &str, community: Option<i64>) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: format!("{id}.py"),
        source_location: None,
        community,
        extra: BTreeMap::new(),
    }
}

fn edge(source: &str, target: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "calls".into(),
        confidence: Confidence::Extracted,
        source_file: String::new(),
        extra: BTreeMap::new(),
    }
}

fn communities(graph: &KnowledgeGraph) -> Communities {
    graphoxide_export::communities_from_graph(graph)
}

fn raw_nodes(html: &str) -> Vec<Value> {
    let prefix = "const RAW_NODES = ";
    let start = html.find(prefix).unwrap() + prefix.len();
    let end = html[start..].find(";\n").unwrap() + start;
    serde_json::from_str(&html[start..end].replace("<\\/", "</")).unwrap()
}

fn markdown_files(directory: &Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("md"))
        .collect()
}

#[test]
fn test_to_json_creates_file() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    assert!(export_graph_json(&sample_graph(), &path, false).unwrap());
    assert!(path.is_file());
}

#[test]
fn test_to_json_valid_json() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    export_graph_json(&sample_graph(), &path, false).unwrap();
    let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert!(value["nodes"].is_array());
    assert!(value["links"].is_array());
}

#[test]
fn test_to_json_nodes_have_community() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    export_graph_json(&sample_graph(), &path, false).unwrap();
    let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert!(value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|node| node.get("community").is_some()));
}

#[test]
fn test_to_cypher_creates_file() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("cypher.txt");
    graphoxide_core::write_text_atomic(&path, &render_cypher(&sample_graph())).unwrap();
    assert!(path.is_file());
}

#[test]
fn test_to_cypher_contains_merge_statements() {
    assert!(render_cypher(&sample_graph()).contains("MERGE"));
}

#[test]
fn test_to_graphml_creates_file() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.graphml");
    write_graphml(&sample_graph(), &path).unwrap();
    assert!(path.is_file());
}

#[test]
fn test_to_graphml_valid_xml() {
    let xml = render_graphml(&sample_graph());
    assert!(xml.contains("<graphml"));
    assert!(xml.contains("<node"));
    assert!(xml.ends_with("</graphml>\n"));
}

#[test]
fn test_to_graphml_has_community_attribute() {
    let xml = render_graphml(&sample_graph());
    assert!(xml.contains("attr.name=\"community\""));
    assert!(xml.contains(">0</data>") || xml.contains(">1</data>"));
}

#[test]
fn test_to_graphml_tolerates_none_attribute_values() {
    let mut graph = sample_graph();
    graph.nodes[0]
        .extra
        .insert("nullable_field".into(), Value::Null);
    graph.links[0]
        .extra
        .insert("nullable_field".into(), Value::Null);
    let xml = render_graphml(&graph);
    assert!(xml.contains("attr.name=\"nullable_field\""));
    assert!(xml.contains("></data>"));
}

#[test]
fn test_to_graphml_tolerates_dict_and_list_attribute_values() {
    let mut graph = sample_graph();
    graph.nodes[0]
        .extra
        .insert("metadata".into(), json!({"kind": "file", "size": 12}));
    graph.nodes[0]
        .extra
        .insert("tags".into(), json!(["x", "y"]));
    graph.links[0].extra.insert("ctx".into(), json!({"k": "v"}));
    graph.hyperedges = vec![json!({"nodes": [graph.nodes[0].id.clone()], "label": "h"})];
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.graphml");
    write_graphml(&graph, &path).unwrap();
    let xml = fs::read_to_string(&path).unwrap();
    assert!(xml.contains("{&quot;kind&quot;:&quot;file&quot;,&quot;size&quot;:12}"));
    assert!(xml.contains("[&quot;x&quot;,&quot;y&quot;]"));
    assert!(
        xml.contains("&quot;hyperedges&quot;") || xml.contains("&quot;label&quot;:&quot;h&quot;")
    );
    assert!(!tmp.path().join("graph.graphml.tmp").exists());
}

#[test]
fn test_to_graphml_preserves_native_scalar_types() {
    let mut graph = KnowledgeGraph {
        nodes: vec![node("a", "x", Some(0)), node("b", "b", Some(0))],
        links: vec![edge("a", "b")],
        ..Default::default()
    };
    graph.nodes[0].extra.insert("count".into(), json!(3));
    graph.nodes[0].extra.insert("ratio".into(), json!(0.5));
    graph.nodes[0].extra.insert("flag".into(), json!(true));
    let xml = render_graphml(&graph);
    assert!(xml.contains("attr.name=\"count\" attr.type=\"long\""));
    assert!(xml.contains("attr.name=\"ratio\" attr.type=\"double\""));
    assert!(xml.contains("attr.name=\"flag\" attr.type=\"boolean\""));
}

#[test]
fn test_to_html_creates_file() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.html");
    graphoxide_core::write_text_atomic(&path, &render_html(&sample_graph()).unwrap()).unwrap();
    assert!(path.is_file());
}

#[test]
fn test_to_html_contains_visjs() {
    assert!(render_html(&sample_graph())
        .unwrap()
        .contains("vis-network"));
}

#[test]
fn test_to_html_neighbor_links_have_no_inline_onclick_xss() {
    let html = render_html(&sample_graph()).unwrap();
    assert!(!html.contains("onclick=\"focusNode("));
    assert!(!html.contains("JSON.stringify(nid)"));
    assert!(html.contains("data-nid=\"${esc(nid)}\""));
    assert!(html.contains("closest('.neighbor-link')"));
}

#[test]
fn test_to_html_pins_visjs_version_with_sri() {
    let html = render_html(&sample_graph()).unwrap();
    assert!(html.contains("vis-network@9.1.6/standalone/umd/vis-network.min.js"));
    assert!(!html.contains("https://unpkg.com/vis-network/standalone"));
    assert!(html.contains(
        "integrity=\"sha384-Ux6phic9PEHJ38YtrijhkzyJ8yQlH8i/+buBR8s3mAZOJrP1gwyvAcIYl3GWtpX1\""
    ));
    assert!(html.contains("crossorigin=\"anonymous\""));
}

#[test]
fn test_to_html_contains_search() {
    assert!(render_html(&sample_graph())
        .unwrap()
        .to_lowercase()
        .contains("search"));
}

#[test]
fn test_to_html_contains_legend_with_labels() {
    let options = HtmlOptions {
        community_labels: BTreeMap::from([(0, "Group 0".into()), (1, "Group 1".into())]),
        ..Default::default()
    };
    assert!(render_html_with_options(&sample_graph(), &options)
        .unwrap()
        .contains("Group 0"));
}

#[test]
fn test_to_html_contains_nodes_and_edges() {
    let html = render_html(&sample_graph()).unwrap();
    assert!(html.contains("RAW_NODES"));
    assert!(html.contains("RAW_EDGES"));
}

#[test]
fn test_to_html_member_counts_accepted() {
    let options = HtmlOptions {
        member_counts: BTreeMap::from([(0, 2), (1, 2)]),
        ..Default::default()
    };
    assert!(render_html_with_options(&sample_graph(), &options).is_ok());
}

#[test]
fn test_to_html_annotated_node_gets_learning_status_and_ring() {
    let options = HtmlOptions {
        learning_overlay: BTreeMap::from([(
            "n_transformer".into(),
            json!({"status": "preferred", "uses": 3, "score": 2.4, "stale": false, "neg": 0}),
        )]),
        ..Default::default()
    };
    let nodes = raw_nodes(&render_html_with_options(&sample_graph(), &options).unwrap());
    let annotated = nodes
        .iter()
        .find(|node| node["id"] == "n_transformer")
        .unwrap();
    assert_eq!(annotated["learning_status"], "preferred");
    assert_eq!(annotated["learning_stale"], false);
    assert_eq!(annotated["color"]["border"], "#22c55e");
    assert_eq!(annotated["borderWidth"], 3);
    assert!(annotated["title"]
        .as_str()
        .unwrap()
        .contains("Lesson: preferred source"));
    assert!(nodes
        .iter()
        .any(|node| node["id"] != "n_transformer" && node.get("learning_status").is_none()));
}

#[test]
fn test_to_html_contested_stale_node_gets_dashed_desaturated_ring() {
    let options = HtmlOptions {
        learning_overlay: BTreeMap::from([(
            "n_transformer".into(),
            json!({"status": "contested", "uses": 2, "neg": 1, "verdict": "dead end", "stale": true}),
        )]),
        ..Default::default()
    };
    let nodes = raw_nodes(&render_html_with_options(&sample_graph(), &options).unwrap());
    let annotated = nodes
        .iter()
        .find(|node| node["id"] == "n_transformer")
        .unwrap();
    assert_eq!(annotated["learning_status"], "contested");
    assert_eq!(annotated["learning_stale"], true);
    assert_eq!(annotated["color"]["border"], "#9ca3af");
    assert_eq!(annotated["shapeProperties"]["borderDashes"], json!([4, 4]));
    assert!(annotated["title"]
        .as_str()
        .unwrap()
        .contains("code changed"));
}

#[test]
fn test_to_html_unannotated_identical_to_pre_feature() {
    let graph = sample_graph();
    let default = render_html(&graph).unwrap();
    let empty = render_html_with_options(&graph, &HtmlOptions::default()).unwrap();
    assert_eq!(default, empty);
    assert!(!default.contains("learning_status"));
}

#[test]
fn test_to_canvas_file_paths_relative_to_vault() {
    let graph = sample_graph();
    let canvas = render_canvas(&graph, &communities(&graph), &BTreeMap::new());
    let files: Vec<_> = canvas["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "file")
        .collect();
    assert!(!files.is_empty());
    assert!(files.iter().all(|node| {
        let file = node["file"].as_str().unwrap();
        !file.contains('/') && file.ends_with(".md")
    }));
}

#[test]
fn test_to_canvas_no_communities_still_populates() {
    let graph = sample_graph();
    let canvas = render_canvas(&graph, &Communities::new(), &BTreeMap::new());
    assert!(canvas["nodes"].as_array().unwrap().len() >= graph.nodes.len());
    assert!(!canvas["edges"].as_array().unwrap().is_empty());
    assert!(serde_json::to_vec(&canvas).unwrap().len() > 32);
}

#[test]
fn test_to_canvas_node_grid_matches_box_columns() {
    for count in [10_usize, 25] {
        let graph = KnowledgeGraph {
            nodes: (0..count)
                .map(|index| node(&format!("n{index}"), &format!("sym_{index:02}"), Some(0)))
                .collect(),
            ..Default::default()
        };
        let canvas = render_canvas(&graph, &communities(&graph), &BTreeMap::new());
        let entries = canvas["nodes"].as_array().unwrap();
        let group = entries.iter().find(|node| node["type"] == "group").unwrap();
        let cards: Vec<_> = entries
            .iter()
            .filter(|node| node["type"] == "file")
            .collect();
        let expected_columns = (count as f64).sqrt().ceil() as usize;
        let expected_rows = count.div_ceil(expected_columns);
        let xs: std::collections::BTreeSet<_> = cards
            .iter()
            .map(|node| node["x"].as_u64().unwrap())
            .collect();
        let ys: std::collections::BTreeSet<_> = cards
            .iter()
            .map(|node| node["y"].as_u64().unwrap())
            .collect();
        assert_eq!(xs.len(), expected_columns);
        assert_eq!(ys.len(), expected_rows);
        let (gx, gy, gw, gh) = (
            group["x"].as_u64().unwrap(),
            group["y"].as_u64().unwrap(),
            group["width"].as_u64().unwrap(),
            group["height"].as_u64().unwrap(),
        );
        assert!(cards.iter().all(|card| {
            let (x, y, w, h) = (
                card["x"].as_u64().unwrap(),
                card["y"].as_u64().unwrap(),
                card["width"].as_u64().unwrap(),
                card["height"].as_u64().unwrap(),
            );
            gx <= x && x + w <= gx + gw && gy <= y && y + h <= gy + gh
        }));
    }
}

fn punctuation_graph(label: &str) -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: vec![
            node("n1", label, Some(0)),
            node("n2", "AuthHandler", Some(0)),
        ],
        ..Default::default()
    }
}

#[test]
fn test_to_obsidian_never_emits_punctuation_only_filenames() {
    let graph = punctuation_graph("@/*");
    let names = node_filenames(&graph);
    assert!(names.values().all(|name| name
        .chars()
        .any(|character| character.is_alphanumeric() || character == '_')));
    assert!(names
        .values()
        .any(|name| name == "unnamed" || name.starts_with("unnamed_")));
}

#[test]
fn test_to_canvas_never_emits_punctuation_only_filenames() {
    let graph = punctuation_graph("@");
    let canvas = render_canvas(&graph, &communities(&graph), &BTreeMap::new());
    assert!(canvas["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "file")
        .all(|node| Path::new(node["file"].as_str().unwrap())
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .chars()
            .any(|character| character.is_alphanumeric() || character == '_')));
}

#[test]
fn test_to_obsidian_leading_dot_labels_are_not_hidden_filenames() {
    let graph = KnowledgeGraph {
        nodes: vec![
            node("env", ".env", Some(0)),
            node("gi", ".gitignore", Some(0)),
            node("readme", "README", Some(0)),
        ],
        links: vec![edge("readme", "env")],
        ..Default::default()
    };
    let names = node_filenames(&graph);
    assert!(names.values().any(|name| name == "dot-env"));
    assert!(names.values().any(|name| name == "dot-gitignore"));
    assert!(names.values().all(|name| !name.starts_with('.')));
    let canvas = render_canvas(&graph, &communities(&graph), &BTreeMap::new());
    assert!(canvas["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "file")
        .all(|node| !node["file"].as_str().unwrap().starts_with('.')));
}

#[test]
fn test_obsidian_safe_stem_all_dots_label_falls_back_to_unnamed() {
    assert_eq!(obsidian_safe_stem(".env"), "dot-env");
    assert_eq!(obsidian_safe_stem("..."), "unnamed");
    assert_eq!(obsidian_safe_stem("Database"), "Database");
}

fn two_node_graph() -> (KnowledgeGraph, Communities, VaultOptions) {
    let graph = KnowledgeGraph {
        nodes: vec![
            node("n1", "Database", Some(0)),
            node("n2", "Server", Some(0)),
        ],
        links: vec![edge("n1", "n2")],
        ..Default::default()
    };
    let communities = communities(&graph);
    let options = VaultOptions {
        community_labels: BTreeMap::from([(0, "Backend".into())]),
        ..Default::default()
    };
    (graph, communities, options)
}

#[test]
fn test_to_obsidian_preserves_existing_user_notes_and_obsidian_config() {
    let (graph, communities, options) = two_node_graph();
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("Database.md"), "# MY NOTES\nkeep me\n").unwrap();
    fs::create_dir(tmp.path().join(".obsidian")).unwrap();
    fs::write(
        tmp.path().join(".obsidian/graph.json"),
        r#"{"USER":"settings"}"#,
    )
    .unwrap();
    export_vault_with_options(&graph, &communities, tmp.path(), &options).unwrap();
    assert!(fs::read_to_string(tmp.path().join("Database.md"))
        .unwrap()
        .contains("MY NOTES"));
    assert_eq!(
        serde_json::from_slice::<Value>(
            &fs::read(tmp.path().join(".obsidian/graph.json")).unwrap()
        )
        .unwrap(),
        json!({"USER":"settings"})
    );
    assert!(tmp.path().join("Server.md").is_file());
}

#[test]
fn test_to_obsidian_empty_dir_writes_full_vault() {
    let (graph, communities, options) = two_node_graph();
    let tmp = tempdir().unwrap();
    let count = export_vault_with_options(&graph, &communities, tmp.path(), &options).unwrap();
    assert!(tmp.path().join("Database.md").is_file());
    assert!(tmp.path().join("Server.md").is_file());
    assert!(tmp.path().join(".obsidian/graph.json").is_file());
    assert_eq!(count, 3);
}

#[test]
fn test_to_obsidian_rerun_updates_own_notes_but_not_user_files() {
    let (graph, communities, mut options) = two_node_graph();
    let tmp = tempdir().unwrap();
    export_vault_with_options(&graph, &communities, tmp.path(), &options).unwrap();
    fs::write(tmp.path().join("UserNote.md"), "mine\n").unwrap();
    options.community_labels.insert(0, "Backend2".into());
    export_vault_with_options(&graph, &communities, tmp.path(), &options).unwrap();
    assert!(tmp.path().join("Database.md").is_file());
    assert_eq!(
        fs::read_to_string(tmp.path().join("UserNote.md"))
            .unwrap()
            .trim(),
        "mine"
    );
}

#[test]
fn test_to_obsidian_rerun_prunes_removed_nodes() {
    let graph4 = KnowledgeGraph {
        nodes: vec![
            node("n1", "Database", Some(0)),
            node("n2", "Server", Some(0)),
            node("n3", "Cache", Some(1)),
            node("n4", "Queue", Some(1)),
        ],
        links: vec![edge("n1", "n2"), edge("n3", "n4")],
        ..Default::default()
    };
    let options4 = VaultOptions {
        community_labels: BTreeMap::from([(0, "Backend".into()), (1, "Infra".into())]),
        ..Default::default()
    };
    let (graph2, communities2, options2) = two_node_graph();
    let tmp = tempdir().unwrap();
    export_vault_with_options(&graph4, &communities(&graph4), tmp.path(), &options4).unwrap();
    fs::write(tmp.path().join("MyOwnNote.md"), "mine\n").unwrap();
    export_vault_with_options(&graph2, &communities2, tmp.path(), &options2).unwrap();
    assert!(!tmp.path().join("Cache.md").exists());
    assert!(!tmp.path().join("Queue.md").exists());
    assert!(!tmp.path().join("_COMMUNITY_Infra.md").exists());
    assert!(tmp.path().join("Database.md").is_file());
    assert_eq!(
        fs::read_to_string(tmp.path().join("MyOwnNote.md"))
            .unwrap()
            .trim(),
        "mine"
    );
}

#[test]
fn test_to_obsidian_removed_node_returning_is_writable_again() {
    let (graph_a, communities_a, options) = two_node_graph();
    let graph_b = KnowledgeGraph {
        nodes: vec![node("n1", "Database", Some(0))],
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    export_vault_with_options(&graph_a, &communities_a, tmp.path(), &options).unwrap();
    export_vault_with_options(&graph_b, &communities(&graph_b), tmp.path(), &options).unwrap();
    assert!(!tmp.path().join("Server.md").exists());
    export_vault_with_options(&graph_a, &communities_a, tmp.path(), &options).unwrap();
    assert!(fs::read_to_string(tmp.path().join("Server.md"))
        .unwrap()
        .contains("# Server"));
}

fn case_collision_graph() -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: vec![
            node("n1", "References", Some(0)),
            node("n2", "references", Some(0)),
        ],
        ..Default::default()
    }
}

#[test]
fn test_to_obsidian_case_only_distinct_labels_dont_overwrite() {
    let graph = case_collision_graph();
    let names = node_filenames(&graph);
    let values: std::collections::BTreeSet<_> = names.values().cloned().collect();
    assert_eq!(
        values,
        std::collections::BTreeSet::from(["References".into(), "references_1".into()])
    );
    assert_eq!(
        names
            .values()
            .map(|name| name.to_lowercase())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2
    );
}

#[test]
fn test_to_obsidian_generated_suffix_doesnt_overwrite_literal() {
    let graph = KnowledgeGraph {
        nodes: vec![
            node("a", "dup", Some(0)),
            node("b", "dup", Some(0)),
            node("c", "dup_1", Some(0)),
        ],
        ..Default::default()
    };
    let names = node_filenames(&graph);
    assert_eq!(names.len(), 3);
    assert_eq!(
        names
            .values()
            .map(|name| name.to_lowercase())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
}

#[test]
fn test_to_canvas_case_only_distinct_labels_get_distinct_files() {
    let graph = case_collision_graph();
    let canvas = render_canvas(&graph, &communities(&graph), &BTreeMap::new());
    let files: Vec<_> = canvas["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "file")
        .map(|node| node["file"].as_str().unwrap().to_lowercase())
        .collect();
    assert_eq!(
        files
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        files.len()
    );
}

#[test]
fn test_obsidian_canvas_filenames_agree() {
    let graph = case_collision_graph();
    let expected: std::collections::BTreeSet<_> = node_filenames(&graph).into_values().collect();
    let canvas = render_canvas(&graph, &communities(&graph), &BTreeMap::new());
    let actual: std::collections::BTreeSet<_> = canvas["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "file")
        .map(|node| {
            Path::new(node["file"].as_str().unwrap())
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(actual.is_subset(&expected));
}

#[test]
fn test_to_obsidian_community_notes_case_collision() {
    let graph = KnowledgeGraph {
        nodes: vec![node("n1", "alpha", Some(0)), node("n2", "beta", Some(1))],
        ..Default::default()
    };
    let options = VaultOptions {
        community_labels: BTreeMap::from([(0, "API".into()), (1, "Api".into())]),
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    export_vault_with_options(&graph, &communities(&graph), tmp.path(), &options).unwrap();
    let notes: Vec<_> = markdown_files(tmp.path())
        .into_iter()
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("_COMMUNITY_")
        })
        .collect();
    assert_eq!(notes.len(), 2);
    assert_eq!(
        notes
            .iter()
            .map(|path| path.file_stem().unwrap().to_string_lossy().to_lowercase())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2
    );
}

#[test]
fn test_backup_no_graph_json() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempdir().unwrap();
    assert!(backup_if_protected(tmp.path()).is_none());
}

#[test]
fn test_backup_no_markers() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("graph.json"), r#"{"nodes":[],"links":[]}"#).unwrap();
    assert!(backup_if_protected(tmp.path()).is_none());
}

#[test]
fn test_backup_semantic_marker() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("graph.json"), r#"{"nodes":[],"links":[]}"#).unwrap();
    fs::write(tmp.path().join("GRAPH_REPORT.md"), "# Report").unwrap();
    fs::write(tmp.path().join(".graphify_semantic_marker"), "{}").unwrap();
    let backup = backup_if_protected(tmp.path()).unwrap();
    assert!(backup.join("graph.json").is_file());
    assert!(backup.join("GRAPH_REPORT.md").is_file());
    assert!(backup.join(".graphify_semantic_marker").is_file());
}

#[test]
fn test_backup_curated_labels() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("graph.json"), r#"{"nodes":[],"links":[]}"#).unwrap();
    fs::write(
        tmp.path().join(".graphify_labels.json"),
        r#"{"0":"Auth Pipeline","1":"Community 1"}"#,
    )
    .unwrap();
    assert!(backup_if_protected(tmp.path()).is_some());
}

#[test]
fn test_backup_default_labels_only() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("graph.json"), r#"{"nodes":[],"links":[]}"#).unwrap();
    fs::write(
        tmp.path().join(".graphify_labels.json"),
        r#"{"0":"Community 0","1":"Community 1"}"#,
    )
    .unwrap();
    assert!(backup_if_protected(tmp.path()).is_none());
}

#[test]
fn test_backup_same_day_no_accumulation() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("graph.json"), r#"{"nodes":[],"links":[]}"#).unwrap();
    fs::write(tmp.path().join(".graphify_semantic_marker"), "{}").unwrap();
    let first = backup_if_protected(tmp.path()).unwrap();
    let second = backup_if_protected(tmp.path()).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.file_name().unwrap().to_string_lossy().len(), 10);
}

#[test]
fn test_backup_same_day_changed_content() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("graph.json"), r#"{"nodes":[],"links":[]}"#).unwrap();
    fs::write(tmp.path().join(".graphify_semantic_marker"), "{}").unwrap();
    let first = backup_if_protected(tmp.path()).unwrap();
    fs::write(
        tmp.path().join("graph.json"),
        r#"{"nodes":[{"id":"x"}],"links":[]}"#,
    )
    .unwrap();
    let second = backup_if_protected(tmp.path()).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        fs::read_to_string(second.join("graph.json")).unwrap(),
        r#"{"nodes":[{"id":"x"}],"links":[]}"#
    );
}

#[test]
fn test_backup_env_disable() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("graph.json"), r#"{"nodes":[],"links":[]}"#).unwrap();
    fs::write(tmp.path().join(".graphify_semantic_marker"), "{}").unwrap();
    unsafe { std::env::set_var("GRAPHIFY_NO_BACKUP", "1") };
    assert!(backup_if_protected(tmp.path()).is_none());
    unsafe { std::env::remove_var("GRAPHIFY_NO_BACKUP") };
}

fn sized_graph(count: usize) -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: (0..count)
            .map(|index| node(&format!("n{index}"), &format!("n{index}"), Some(0)))
            .collect(),
        ..Default::default()
    }
}

#[test]
fn test_to_json_refuses_shrink() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    export_graph_json(&sized_graph(5), &path, true).unwrap();
    assert!(!export_graph_json(&sized_graph(2), &path, false).unwrap());
    assert!(export_graph_json(&sized_graph(2), &path, true).unwrap());
}

#[test]
fn test_to_json_fails_safe_on_corrupt_existing() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, "{ this has content but is not valid json").unwrap();
    assert!(!export_graph_json(&sized_graph(10), &path, false).unwrap());
    assert!(export_graph_json(&sized_graph(10), &path, true).unwrap());
}

#[test]
fn test_to_json_proceeds_on_empty_existing() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, "").unwrap();
    assert!(export_graph_json(&sized_graph(3), &path, false).unwrap());
    let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(value["nodes"].as_array().unwrap().len(), 3);
}

#[test]
fn test_to_html_handles_null_source_file_and_label() {
    let graph: KnowledgeGraph = serde_json::from_value(json!({
        "nodes": [
            {"id":"n1","label":"Foo","source_file":null,"community":0},
            {"id":"n2","label":null,"source_file":"a.py","community":0},
            {"id":"n3","label":null,"source_file":null,"community":0}
        ], "links": []
    }))
    .unwrap();
    let html = render_html(&graph).unwrap();
    assert!(!html.is_empty());
}

#[test]
fn test_existing_graph_node_count() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    assert_eq!(
        existing_graph_node_count(&path),
        ExistingGraphNodeCount::NothingToProtect
    );
    fs::write(&path, "").unwrap();
    assert_eq!(
        existing_graph_node_count(&path),
        ExistingGraphNodeCount::NothingToProtect
    );
    fs::write(&path, "{not json").unwrap();
    assert_eq!(
        existing_graph_node_count(&path),
        ExistingGraphNodeCount::Malformed
    );
    fs::write(&path, r#"{"nodes":"notalist"}"#).unwrap();
    assert_eq!(
        existing_graph_node_count(&path),
        ExistingGraphNodeCount::Malformed
    );
    fs::write(&path, r#"{"nodes":[{"id":"a"},{"id":"b"}],"links":[]}"#).unwrap();
    assert_eq!(
        existing_graph_node_count(&path),
        ExistingGraphNodeCount::Count(2)
    );
}
