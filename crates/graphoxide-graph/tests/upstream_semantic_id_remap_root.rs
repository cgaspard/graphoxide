use graphoxide_core::Extraction;
use graphoxide_graph::{build_graph_with_root, semantic_id_remap, source_file_stem};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

fn extraction(value: serde_json::Value) -> Extraction {
    serde_json::from_value(value).unwrap()
}

#[test]
fn test_file_stem_handles_dot_path() {
    assert_eq!(source_file_stem(Path::new(".")), "");
    assert_eq!(source_file_stem(Path::new("src/foo.py")), "src/foo");
}

#[test]
fn test_semantic_id_remap_root_equal_source_file_no_crash() {
    let node = extraction(json!({
        "nodes": [{"id": "some_concept", "source_file": "/some/project/root", "_origin": "semantic"}],
        "edges": []
    }));
    assert!(!semantic_id_remap(&[node]).contains_key("some_concept"));
}

#[test]
fn test_build_from_json_with_root_level_concept_node() {
    let combined = extraction(json!({
        "nodes": [
            {"id": "proj_concept", "label": "Project", "file_type": "concept", "source_file": "/proj", "_origin": "semantic"},
            {"id": "src_foo", "label": "foo", "file_type": "code", "source_file": "src/foo.py", "_origin": "ast"}
        ],
        "edges": []
    }));
    assert_eq!(
        build_graph_with_root(&[combined], "/proj")
            .unwrap()
            .nodes
            .len(),
        2
    );
}

#[test]
fn test_normal_semantic_remap_still_works() {
    let node = extraction(
        json!({"nodes": [{"id": "foo", "source_file": "src/foo.py", "_origin": "semantic"}], "edges": []}),
    );
    let _: BTreeMap<String, String> = semantic_id_remap(&[node]);
}

#[test]
fn test_semantic_id_remap_is_idempotent_when_stem_contains_legacy_stem() {
    let original = extraction(
        json!({"nodes": [{"id": "claude_graphify_trigger", "source_file": ".claude/CLAUDE.md", "_origin": "semantic"}], "edges": []}),
    );
    let first = semantic_id_remap(&[original]);
    assert_eq!(
        first,
        BTreeMap::from([(
            "claude_graphify_trigger".to_owned(),
            "claude_claude_graphify_trigger".to_owned()
        )])
    );
    let migrated = extraction(
        json!({"nodes": [{"id": "claude_claude_graphify_trigger", "source_file": ".claude/CLAUDE.md", "_origin": "semantic"}], "edges": []}),
    );
    assert!(semantic_id_remap(&[migrated]).is_empty());
}

#[test]
fn test_semantic_id_remap_bare_file_node_is_idempotent() {
    let original = extraction(
        json!({"nodes": [{"id": "claude", "source_file": ".claude/CLAUDE.md", "_origin": "semantic"}], "edges": []}),
    );
    assert_eq!(
        semantic_id_remap(&[original]),
        BTreeMap::from([("claude".to_owned(), "claude_claude".to_owned())])
    );
    let migrated = extraction(
        json!({"nodes": [{"id": "claude_claude", "source_file": ".claude/CLAUDE.md", "_origin": "semantic"}], "edges": []}),
    );
    assert!(semantic_id_remap(&[migrated]).is_empty());
}

#[test]
fn test_semantic_id_remap_still_migrates_genuine_legacy_id() {
    let original = extraction(
        json!({"nodes": [{"id": "readme_booking", "source_file": "api/README.md", "_origin": "semantic"}], "edges": []}),
    );
    assert_eq!(
        semantic_id_remap(&[original]),
        BTreeMap::from([("readme_booking".to_owned(), "api_readme_booking".to_owned())])
    );
}
