//! Port of upstream `tests/test_obsidian_filename_cap.py` (5 cases).

use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_export::{export_canvas, export_vault_with_options, Communities, VaultOptions};
use serde_json::Value;
use std::{collections::BTreeMap, fs};
use tempfile::tempdir;

fn graph(labels: &[String]) -> (KnowledgeGraph, Communities) {
    let nodes: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(index, label)| Node {
            id: format!("n{index}"),
            label: label.clone(),
            file_type: "code".into(),
            source_file: "x.py".into(),
            source_location: None,
            community: Some(0),
            extra: BTreeMap::new(),
        })
        .collect();
    let links = (0..nodes.len().saturating_sub(1))
        .map(|index| Edge {
            source: format!("n{index}"),
            target: format!("n{}", index + 1),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: String::new(),
            extra: BTreeMap::new(),
        })
        .collect();
    let communities = BTreeMap::from([(0, nodes.iter().map(|node| node.id.clone()).collect())]);
    (
        KnowledgeGraph {
            nodes,
            links,
            ..Default::default()
        },
        communities,
    )
}

fn markdown_names(directory: &std::path::Path) -> Vec<String> {
    fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".md"))
        .collect()
}

#[test]
fn test_obsidian_long_ascii_label_does_not_crash() {
    let (graph, communities) = graph(&["a".repeat(300), "short".into()]);
    let tmp = tempdir().unwrap();
    export_vault_with_options(&graph, &communities, tmp.path(), &VaultOptions::default()).unwrap();
    assert!(markdown_names(tmp.path())
        .iter()
        .all(|name| name.len() <= 255));
}

#[test]
fn test_obsidian_long_cjk_label_byte_cap() {
    let (graph, communities) = graph(&["中".repeat(300), "ok".into()]);
    let tmp = tempdir().unwrap();
    export_vault_with_options(&graph, &communities, tmp.path(), &VaultOptions::default()).unwrap();
    assert!(markdown_names(tmp.path())
        .iter()
        .all(|name| name.len() <= 255));
}

#[test]
fn test_obsidian_distinct_long_labels_sharing_prefix_do_not_collide() {
    let prefix = "z".repeat(250);
    let (graph, communities) = graph(&[format!("{prefix}_ALPHA"), format!("{prefix}_BETA")]);
    let tmp = tempdir().unwrap();
    export_vault_with_options(&graph, &communities, tmp.path(), &VaultOptions::default()).unwrap();
    let notes: Vec<_> = markdown_names(tmp.path())
        .into_iter()
        .filter(|name| !name.starts_with("_COMMUNITY_"))
        .collect();
    assert_eq!(notes.len(), 2);
    assert_ne!(notes[0], notes[1]);
    assert!(notes.iter().all(|name| name.len() <= 255));
}

#[test]
fn test_obsidian_wikilink_resolves_after_truncation() {
    let (graph, communities) = graph(&["w".repeat(300), "neighbor".into()]);
    let tmp = tempdir().unwrap();
    export_vault_with_options(&graph, &communities, tmp.path(), &VaultOptions::default()).unwrap();
    let body = fs::read_to_string(tmp.path().join("neighbor.md")).unwrap();
    let start = body.find("[[").expect("wikilink") + 2;
    let end = body[start..].find("]]").unwrap() + start;
    assert!(tmp
        .path()
        .join(format!("{}.md", &body[start..end]))
        .is_file());
}

#[test]
fn test_canvas_long_label_file_ref_capped() {
    let (graph, communities) = graph(&["c".repeat(300), "ok".into()]);
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.canvas");
    export_canvas(&graph, &communities, &path, &BTreeMap::new()).unwrap();
    let canvas: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert!(canvas["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "file")
        .all(|node| node["file"].as_str().unwrap().len() <= 255));
}
