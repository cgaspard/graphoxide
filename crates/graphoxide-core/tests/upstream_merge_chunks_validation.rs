//! Executable port of upstream `test_merge_chunks_validation.py` (10 cases).

use graphoxide_core::{merge_semantic_chunk_files, validate_semantic_fragment};
use serde_json::{json, Value};
use std::{fs, path::PathBuf};
use tempfile::tempdir;

fn write(path: &std::path::Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

#[test]
fn merge_chunks_skips_chunk_with_path_escape_id() {
    let tmp = tempdir().unwrap();
    let good = tmp.path().join(".graphoxide_chunk_0.json");
    write(
        &good,
        &json!({"nodes": [{"id": "pkg.mod.good", "label": "G"}], "edges": [], "hyperedges": []}),
    );
    let bad = tmp.path().join(".graphoxide_chunk_1.json");
    write(
        &bad,
        &json!({"nodes": [{"id": "../../etc/passwd", "label": "B"}], "edges": [], "hyperedges": []}),
    );
    let output = tmp.path().join("merged.json");
    let report = merge_semantic_chunk_files(&[good, bad], &output).unwrap();
    let merged: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert_eq!(merged["nodes"][0]["id"], "pkg.mod.good");
    assert_eq!(report.valid_chunks, 1);
    assert_eq!(report.input_chunks, 2);
    assert_eq!(report.skipped_chunks.len(), 1);
}

#[test]
fn merge_chunks_fails_closed_when_every_chunk_is_invalid() {
    let tmp = tempdir().unwrap();
    let bad = tmp.path().join(".graphoxide_chunk_0.json");
    write(&bad, &json!({"nodes": "not-a-list", "edges": []}));
    let output = tmp.path().join("merged.json");
    write(&output, &json!({"previous": "semantic result"}));
    let error = merge_semantic_chunk_files(&[bad], &output).unwrap_err();
    assert!(error.to_string().contains("no valid chunks to merge"));
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(output).unwrap()).unwrap(),
        json!({"previous": "semantic result"})
    );
}

#[test]
fn merge_chunks_accepts_valid_empty_chunk() {
    let tmp = tempdir().unwrap();
    let empty = tmp.path().join(".graphoxide_chunk_0.json");
    write(&empty, &json!({"nodes": [], "edges": [], "hyperedges": []}));
    let output = tmp.path().join("merged.json");
    merge_semantic_chunk_files(&[empty], &output).unwrap();
    let merged: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert_eq!(merged["nodes"], json!([]));
    assert_eq!(merged["edges"], json!([]));
}

#[test]
fn merge_chunks_fails_closed_without_chunk_arguments() {
    let tmp = tempdir().unwrap();
    let output = tmp.path().join("merged.json");
    let paths: [PathBuf; 0] = [];
    assert!(merge_semantic_chunk_files(&paths, &output)
        .unwrap_err()
        .to_string()
        .contains("no valid chunks to merge"));
    assert!(!output.exists());
}

#[test]
fn merge_chunks_fails_closed_on_unmatched_glob() {
    let tmp = tempdir().unwrap();
    let output = tmp.path().join("merged.json");
    write(&output, &json!({"previous": true}));
    let unmatched = tmp.path().join(".graphoxide_chunk_*.json");
    assert!(merge_semantic_chunk_files(&[unmatched], &output).is_err());
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(output).unwrap()).unwrap(),
        json!({"previous": true})
    );
}

#[test]
fn merge_chunks_accepts_synonym_file_type() {
    let tmp = tempdir().unwrap();
    let chunk = tmp.path().join(".graphoxide_chunk_0.json");
    write(
        &chunk,
        &json!({"nodes": [
        {"id": "pkg.readme", "label": "Readme", "file_type": "markdown"},
        {"id": "pkg.tool", "label": "Tool", "file_type": "tool"}
    ], "edges": [], "hyperedges": []}),
    );
    let output = tmp.path().join("merged.json");
    merge_semantic_chunk_files(&[chunk], &output).unwrap();
    let merged: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    let ids: Vec<_> = merged["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["pkg.readme", "pkg.tool"]);
}

#[test]
fn merge_chunks_accepts_unicode_id() {
    let tmp = tempdir().unwrap();
    let chunk = tmp.path().join(".graphoxide_chunk_0.json");
    write(
        &chunk,
        &json!({"nodes": [{"id": "mod_处理数据", "label": "handler", "file_type": "code"}], "edges": [], "hyperedges": []}),
    );
    let output = tmp.path().join("merged.json");
    merge_semantic_chunk_files(&[chunk], &output).unwrap();
    let merged: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert_eq!(merged["nodes"][0]["id"], "mod_处理数据");
}

#[test]
fn validate_semantic_fragment_accepts_synonyms_and_unicode() {
    let fragment = json!({"nodes": [
        {"id": "mod_处理", "file_type": "markdown"},
        {"id": "a.b::C.d", "file_type": "tool"}
    ], "edges": [], "hyperedges": []});
    assert!(validate_semantic_fragment(&fragment).is_empty());
}

#[test]
fn validate_semantic_fragment_still_blocks_path_escape() {
    let errors = validate_semantic_fragment(
        &json!({"nodes": [{"id": "../../etc/passwd"}], "edges": [], "hyperedges": []}),
    );
    assert!(!errors.is_empty());
}

#[test]
fn merge_chunks_merges_valid_chunks() {
    let tmp = tempdir().unwrap();
    let first = tmp.path().join(".graphoxide_chunk_0.json");
    write(
        &first,
        &json!({"nodes": [{"id": "a", "label": "A"}], "edges": [], "hyperedges": [], "input_tokens": 10, "output_tokens": 5}),
    );
    let second = tmp.path().join(".graphoxide_chunk_1.json");
    write(
        &second,
        &json!({"nodes": [{"id": "b", "label": "B"}], "edges": [], "hyperedges": [], "input_tokens": 7, "output_tokens": 3}),
    );
    let output = tmp.path().join("merged.json");
    merge_semantic_chunk_files(&[first, second], &output).unwrap();
    let merged: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert_eq!(merged["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(merged["input_tokens"], 17);
    assert_eq!(merged["output_tokens"], 8);
}
