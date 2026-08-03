use graphoxide_extract::{
    cache::{
        group_has_partial_marker, load_cached_value, load_cached_value_allow_partial,
        prompt_fingerprint, save_semantic_cache, SemanticCacheOptions,
    },
    semantic_pipeline::{
        mark_partial_items, partial_source_files, stamped_manifest_files, strip_partial_markers,
        SemanticChunkResult,
    },
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

fn doc(root: &Path) -> PathBuf {
    let path = root.join("doc.md");
    fs::write(&path, "# Heading\nsome prose\n").unwrap();
    path
}

fn prompt_options() -> SemanticCacheOptions {
    SemanticCacheOptions {
        prompt: Some("P".into()),
        ..SemanticCacheOptions::default()
    }
}

fn load(root: &Path, path: &Path) -> Option<serde_json::Value> {
    load_cached_value(path, root, "semantic", Some(&prompt_fingerprint("P")))
}

fn peek(root: &Path, path: &Path) -> Option<serde_json::Value> {
    load_cached_value_allow_partial(path, root, root, "semantic", Some(&prompt_fingerprint("P")))
}

#[test]
fn test_intrinsic_partial_marker_makes_entry_a_cache_miss() {
    let temp = TempDir::new().unwrap();
    let path = doc(temp.path());
    let report = save_semantic_cache(
        &[json!({"id": "n1", "label": "Heading", "source_file": "doc.md", "_partial": true})],
        &[],
        &[],
        temp.path(),
        &prompt_options(),
    )
    .unwrap();
    assert_eq!(report.saved, 1);
    assert!(load(temp.path(), &path).is_none());
}

#[test]
fn test_partial_source_files_arg_stamps_entry() {
    let temp = TempDir::new().unwrap();
    let path = doc(temp.path());
    let mut options = prompt_options();
    options.partial_source_files = Some(BTreeSet::from([PathBuf::from("doc.md")]));
    save_semantic_cache(
        &[json!({"id": "n1", "label": "Heading", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    assert!(load(temp.path(), &path).is_none());
}

#[test]
fn test_non_partial_entry_loads_normally() {
    let temp = TempDir::new().unwrap();
    let path = doc(temp.path());
    save_semantic_cache(
        &[json!({"id": "n1", "label": "Heading", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &prompt_options(),
    )
    .unwrap();
    assert_eq!(
        load(temp.path(), &path).unwrap()["nodes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn test_partial_entry_self_heals_on_complete_reextraction() {
    let temp = TempDir::new().unwrap();
    let path = doc(temp.path());
    save_semantic_cache(
        &[json!({"id": "n1", "source_file": "doc.md", "_partial": true})],
        &[],
        &[],
        temp.path(),
        &prompt_options(),
    )
    .unwrap();
    assert!(load(temp.path(), &path).is_none());
    save_semantic_cache(
        &[
            json!({"id": "n1", "source_file": "doc.md"}),
            json!({"id": "n2", "source_file": "doc.md"}),
        ],
        &[],
        &[],
        temp.path(),
        &prompt_options(),
    )
    .unwrap();
    assert_eq!(
        load(temp.path(), &path).unwrap()["nodes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn test_merge_existing_accumulates_slices_and_stays_partial() {
    let temp = TempDir::new().unwrap();
    let path = doc(temp.path());
    save_semantic_cache(
        &[json!({"id": "n1", "source_file": "doc.md", "_partial": true})],
        &[],
        &[],
        temp.path(),
        &prompt_options(),
    )
    .unwrap();
    let mut merge = prompt_options();
    merge.merge_existing = true;
    save_semantic_cache(
        &[json!({"id": "n2", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &merge,
    )
    .unwrap();
    assert!(load(temp.path(), &path).is_none());
    let cached = peek(temp.path(), &path).unwrap();
    let ids = cached["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids, BTreeSet::from(["n1", "n2"]));
}

#[test]
fn test_save_stamps_partial_file_with_no_items() {
    let temp = TempDir::new().unwrap();
    let path = doc(temp.path());
    save_semantic_cache(
        &[json!({"id": "n1", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &prompt_options(),
    )
    .unwrap();
    assert!(load(temp.path(), &path).is_some());
    let mut partial = prompt_options();
    partial.merge_existing = true;
    partial.partial_source_files = Some(BTreeSet::from([PathBuf::from("doc.md")]));
    save_semantic_cache(&[], &[], &[], temp.path(), &partial).unwrap();
    assert!(load(temp.path(), &path).is_none());
    assert_eq!(peek(temp.path(), &path).unwrap()["nodes"][0]["id"], "n1");
}

#[test]
fn test_clean_slice_does_not_repromote_empty_parse_partial() {
    let temp = TempDir::new().unwrap();
    let path = doc(temp.path());
    let mut partial = prompt_options();
    partial.partial_source_files = Some(BTreeSet::from([PathBuf::from("doc.md")]));
    save_semantic_cache(&[], &[], &[], temp.path(), &partial).unwrap();
    assert!(load(temp.path(), &path).is_none());
    let mut merge = prompt_options();
    merge.merge_existing = true;
    save_semantic_cache(
        &[json!({"id": "n2", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &merge,
    )
    .unwrap();
    assert!(load(temp.path(), &path).is_none());
}

#[test]
fn test_partial_files_carries_empty_parse_truncation() {
    let empty = SemanticChunkResult {
        partial_files: BTreeSet::from([PathBuf::from("big.md")]),
        ..SemanticChunkResult::default()
    };
    assert_eq!(
        partial_source_files(&empty),
        BTreeSet::from([PathBuf::from("big.md")])
    );
    let marked = SemanticChunkResult {
        nodes: vec![json!({"id": "a", "source_file": "x.md", "_partial": true})],
        partial_files: BTreeSet::from([PathBuf::from("big.md")]),
        ..SemanticChunkResult::default()
    };
    assert_eq!(
        partial_source_files(&marked),
        BTreeSet::from([PathBuf::from("big.md"), PathBuf::from("x.md")])
    );
}

#[test]
fn test_stamped_manifest_excludes_partial_files() {
    let files = BTreeMap::from([
        (
            "document".into(),
            vec![PathBuf::from("a.md"), PathBuf::from("b.md")],
        ),
        ("code".into(), vec![PathBuf::from("x.py")]),
    ]);
    let result = SemanticChunkResult {
        nodes: vec![
            json!({"id": "1", "source_file": "a.md"}),
            json!({"id": "2", "source_file": "b.md"}),
        ],
        ..SemanticChunkResult::default()
    };
    let stamped = stamped_manifest_files(
        &files,
        &result,
        Path::new("."),
        &BTreeSet::from([PathBuf::from("b.md")]),
    );
    assert_eq!(stamped["document"], [PathBuf::from("a.md")]);
    assert_eq!(stamped["code"], [PathBuf::from("x.py")]);
}

#[test]
fn test_group_has_partial_marker() {
    assert!(group_has_partial_marker(
        &json!({"nodes": [{"_partial": true}]})
    ));
    assert!(group_has_partial_marker(
        &json!({"edges": [{"_partial": true}]})
    ));
    assert!(!group_has_partial_marker(
        &json!({"nodes": [{"id": "a"}], "edges": [], "hyperedges": []})
    ));
    assert!(!group_has_partial_marker(&json!({})));
}

#[test]
fn test_mark_partial_and_partial_source_files() {
    let mut result = SemanticChunkResult {
        nodes: vec![json!({"id": "a", "source_file": "x.md"})],
        edges: vec![json!({"source": "a", "target": "b", "source_file": "x.md"})],
        hyperedges: vec![json!({"id": "h", "source_file": "y.md"})],
        ..SemanticChunkResult::default()
    };
    mark_partial_items(&mut result);
    assert!(result.nodes[0]["_partial"].as_bool().unwrap());
    assert!(result.edges[0]["_partial"].as_bool().unwrap());
    assert!(result.hyperedges[0]["_partial"].as_bool().unwrap());
    assert_eq!(
        partial_source_files(&result),
        BTreeSet::from([PathBuf::from("x.md"), PathBuf::from("y.md")])
    );
}

#[test]
fn test_partial_source_files_empty_when_unmarked() {
    let result = SemanticChunkResult {
        nodes: vec![json!({"id": "a", "source_file": "x.md"})],
        ..SemanticChunkResult::default()
    };
    assert!(partial_source_files(&result).is_empty());
}

#[test]
fn test_strip_partial_markers_removes_internal_key() {
    let mut result = SemanticChunkResult {
        nodes: vec![json!({"id": "a", "_partial": true})],
        edges: vec![json!({"source": "a", "target": "b", "_partial": true})],
        hyperedges: vec![json!({"id": "h", "_partial": true})],
        ..SemanticChunkResult::default()
    };
    strip_partial_markers(&mut result);
    assert!(result
        .nodes
        .iter()
        .chain(&result.edges)
        .chain(&result.hyperedges)
        .all(|item| item.get("_partial").is_none()));
}
