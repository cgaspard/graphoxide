//! Port of upstream `tests/test_obsidian_dangling_member.py` (3 cases).

use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_export::{export_canvas, export_vault_with_options, Communities, VaultOptions};
use serde_json::Value;
use std::{collections::BTreeMap, fs};
use tempfile::tempdir;

fn graph_with_dangling_member() -> (KnowledgeGraph, Communities) {
    let make_node = |id: &str, label: &str, source: &str| Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source.into(),
        source_location: None,
        community: Some(0),
        extra: BTreeMap::new(),
    };
    let graph = KnowledgeGraph {
        nodes: vec![
            make_node("n0", "Alpha", "a.py"),
            make_node("n1", "Beta", "b.py"),
        ],
        links: vec![Edge {
            source: "n0".into(),
            target: "n1".into(),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: String::new(),
            extra: BTreeMap::new(),
        }],
        ..Default::default()
    };
    (
        graph,
        BTreeMap::from([(0, vec!["n0".into(), "n1".into(), "agents_doc".into()])]),
    )
}

#[test]
fn test_obsidian_dangling_community_member_does_not_crash() {
    let (graph, communities) = graph_with_dangling_member();
    let tmp = tempdir().unwrap();
    let count =
        export_vault_with_options(&graph, &communities, tmp.path(), &VaultOptions::default())
            .unwrap();
    assert!(count > 0);
    let notes: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("_COMMUNITY_")
        })
        .collect();
    assert_eq!(notes.len(), 1);
    let body = fs::read_to_string(&notes[0]).unwrap();
    assert!(body.contains("[[Alpha]]"));
    assert!(body.contains("[[Beta]]"));
    assert!(!body.contains("agents_doc"));
    assert!(body.contains("**Members:** 2 nodes"));
}

#[test]
fn test_obsidian_community_of_only_dangling_members() {
    let graph = KnowledgeGraph {
        nodes: vec![Node {
            id: "n0".into(),
            label: "Alpha".into(),
            file_type: "code".into(),
            source_file: "a.py".into(),
            source_location: None,
            community: Some(0),
            extra: BTreeMap::new(),
        }],
        ..Default::default()
    };
    let communities = BTreeMap::from([
        (0, vec!["n0".into()]),
        (1, vec!["ghost_a".into(), "ghost_b".into()]),
    ]);
    let tmp = tempdir().unwrap();
    let count =
        export_vault_with_options(&graph, &communities, tmp.path(), &VaultOptions::default())
            .unwrap();
    assert!(count > 0);
    let ghost = tmp.path().join("_COMMUNITY_Community 1.md");
    assert!(ghost.is_file());
    assert!(fs::read_to_string(ghost)
        .unwrap()
        .contains("**Members:** 0 nodes"));
}

#[test]
fn test_canvas_dangling_community_member_does_not_crash() {
    let (graph, communities) = graph_with_dangling_member();
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.canvas");
    export_canvas(&graph, &communities, &path, &BTreeMap::new()).unwrap();
    let canvas: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let ids: std::collections::BTreeSet<_> = canvas["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["id"].as_str())
        .collect();
    assert!(ids.contains("n_n0"));
    assert!(ids.contains("n_n1"));
    assert!(!ids.contains("n_agents_doc"));
}
