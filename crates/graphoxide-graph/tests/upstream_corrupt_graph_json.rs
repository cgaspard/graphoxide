//! Executable port of upstream `test_corrupt_graph_json.py` (4 cases).

use graphoxide_core::{read_graph, read_json_object};
use graphoxide_graph::build_merge;
use std::fs;
use tempfile::tempdir;

const CORRUPT: &str = r#"{"nodes": [{"id": "a", "labe"#;

#[test]
fn build_merge_corrupt_graph_raises_actionable_error() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, CORRUPT).unwrap();
    let message = build_merge(&[], &path, &[], None).unwrap_err().to_string();
    assert!(message.contains("incremental merge"));
    assert!(message.contains("rebuild"));
}

#[test]
fn affected_load_graph_corrupt_raises_actionable_error() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, CORRUPT).unwrap();
    let message = read_graph(&path).unwrap_err().to_string();
    assert!(message.contains("Cannot read graph file"));
    assert!(message.contains("regenerate") || message.contains("rebuild"));
}

#[test]
fn diagnostics_read_corrupt_raises_actionable_error() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, CORRUPT).unwrap();
    let message = read_json_object(&path).unwrap_err().to_string();
    assert!(message.contains("Cannot parse"));
    assert!(message.contains("corrupted"));
}

#[test]
fn valid_graph_still_loads() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(
        &path,
        r#"{"nodes": [{"id": "a", "label": "a", "file_type": "code"}], "edges": []}"#,
    )
    .unwrap();
    read_graph(&path).unwrap();
    read_json_object(&path).unwrap();
    build_merge(&[], &path, &[], None).unwrap();
}
