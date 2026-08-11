use graphoxide_extract::cache::{
    body_content, cache_dir, cache_dir_with_ast_version, cached_files, check_semantic_cache,
    clear_cache, file_hash, load_cached_value, load_cached_value_with_version,
    prepare_structured_redaction_cache_schema, prompt_file_fingerprint, prompt_fingerprint,
    prune_semantic_cache, save_cached_value, save_cached_value_with_version, save_semantic_cache,
    SemanticCacheOptions,
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

fn write(root: &Path, relative: &str, content: &[u8]) -> std::path::PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

#[test]
fn test_file_hash_consistent() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "sample.txt", b"hello world");
    let first = file_hash(&file, temp.path()).unwrap();
    let second = file_hash(&file, temp.path()).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.len(), 64);
    assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

#[test]
fn test_file_hash_changes() {
    let temp = TempDir::new().unwrap();
    let first = write(temp.path(), "a.txt", b"content one");
    let second = write(temp.path(), "b.txt", b"content two");
    assert_ne!(
        file_hash(&first, temp.path()).unwrap(),
        file_hash(&second, temp.path()).unwrap()
    );
}

#[test]
fn test_cache_roundtrip() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "sample.txt", b"hello world");
    let result = json!({"nodes": [{"id": "n1", "label": "Node1"}], "edges": []});
    save_cached_value(&file, &result, temp.path(), "ast", None).unwrap();
    assert_eq!(
        load_cached_value(&file, temp.path(), "ast", None),
        Some(result)
    );
}

#[test]
fn test_cache_miss_on_change() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "sample.txt", b"hello world");
    let result = json!({"nodes": [], "edges": [{"source": "a", "target": "b"}]});
    save_cached_value(&file, &result, temp.path(), "ast", None).unwrap();
    fs::write(&file, b"completely different content").unwrap();
    assert_eq!(load_cached_value(&file, temp.path(), "ast", None), None);
}

#[test]
fn test_cached_files() {
    let temp = TempDir::new().unwrap();
    let first = write(temp.path(), "file1.py", b"alpha");
    let second = write(temp.path(), "file2.py", b"beta");
    let empty = json!({"nodes": [], "edges": []});
    save_cached_value(&first, &empty, temp.path(), "ast", None).unwrap();
    save_cached_value(&second, &empty, temp.path(), "ast", None).unwrap();
    let hashes = cached_files(temp.path());
    assert!(hashes.contains(&file_hash(&first, temp.path()).unwrap()));
    assert!(hashes.contains(&file_hash(&second, temp.path()).unwrap()));
}

#[test]
fn test_clear_cache() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "sample.txt", b"hello world");
    save_cached_value(
        &file,
        &json!({"nodes": [], "edges": []}),
        temp.path(),
        "ast",
        None,
    )
    .unwrap();
    assert!(!cached_files(temp.path()).is_empty());
    assert_eq!(clear_cache(temp.path()).unwrap(), 1);
    assert!(cached_files(temp.path()).is_empty());
}

#[test]
fn test_md_frontmatter_only_change_same_hash() {
    let temp = TempDir::new().unwrap();
    let file = write(
        temp.path(),
        "doc.md",
        b"---\nreviewed: 2026-01-01\n---\n\n# Title\n\nBody text.",
    );
    let first = file_hash(&file, temp.path()).unwrap();
    fs::write(
        &file,
        b"---\nreviewed: 2026-04-09\n---\n\n# Title\n\nBody text.",
    )
    .unwrap();
    assert_eq!(first, file_hash(&file, temp.path()).unwrap());
}

#[test]
fn test_md_body_change_different_hash() {
    let temp = TempDir::new().unwrap();
    let file = write(
        temp.path(),
        "doc.md",
        b"---\nreviewed: 2026-01-01\n---\n\n# Title\n\nOriginal body.",
    );
    let first = file_hash(&file, temp.path()).unwrap();
    fs::write(
        &file,
        b"---\nreviewed: 2026-01-01\n---\n\n# Title\n\nChanged body.",
    )
    .unwrap();
    assert_ne!(first, file_hash(&file, temp.path()).unwrap());
}

#[test]
fn test_md_no_frontmatter_hashed_normally() {
    let temp = TempDir::new().unwrap();
    let file = write(
        temp.path(),
        "doc.md",
        b"# Just a heading\n\nNo frontmatter here.",
    );
    let first = file_hash(&file, temp.path()).unwrap();
    fs::write(&file, b"# Just a heading\n\nDifferent content.").unwrap();
    assert_ne!(first, file_hash(&file, temp.path()).unwrap());
}

#[test]
fn test_non_md_file_hashed_fully() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "script.py", b"# comment\nx = 1");
    let first = file_hash(&file, temp.path()).unwrap();
    fs::write(&file, b"# changed comment\nx = 1").unwrap();
    assert_ne!(first, file_hash(&file, temp.path()).unwrap());
}

#[test]
fn test_body_content_strips_frontmatter() {
    assert_eq!(
        body_content(b"---\ntitle: Test\n---\n\nActual body."),
        b"\n\nActual body."
    );
}

#[test]
fn test_body_content_no_frontmatter() {
    let content = b"No frontmatter here.";
    assert_eq!(body_content(content), content);
}

#[test]
fn test_body_content_hr_start_is_not_frontmatter() {
    let content = b"----\nIntro paragraph that must be hashed.\n\n---\nbody";
    assert_eq!(body_content(content), content);
}

#[test]
fn test_body_content_dash_title_start_is_not_frontmatter() {
    let content = b"--- title\nIntro that must be hashed.\n\n---\nbody";
    assert_eq!(body_content(content), content);
}

#[test]
fn test_body_content_dash_text_line_is_not_close_delimiter() {
    let content = b"---\ntitle: Test\nbody starts here\n--- not a delimiter\n----\nreal content";
    assert_eq!(body_content(content), content);
}

#[test]
fn test_body_content_later_proper_close_skips_dash_text_lines() {
    assert_eq!(
        body_content(b"---\ntitle: Test\nnote: --- inline\n---\nreal body"),
        b"\nreal body"
    );
}

#[test]
fn test_body_content_well_formed_output_byte_identical() {
    let cases: &[(&[u8], &[u8])] = &[
        (
            b"---\ntitle: Test\n---\n\nActual body.",
            b"\n\nActual body.",
        ),
        (
            b"---\nreviewed: 2026-01-01\n---\n\n# Title\n\nBody text.",
            b"\n\n# Title\n\nBody text.",
        ),
        (b"---\ntitle: Test\n---  \nbody", b"  \nbody"),
        (b"---\r\ntitle: Test\r\n---\r\nbody", b"\r\nbody"),
        (b"---\n---\nbody", b"\nbody"),
        (b"---\ntitle: Test\n---", b""),
    ];
    for (content, expected) in cases {
        assert_eq!(body_content(content), *expected, "{content:?}");
    }
}

#[test]
fn test_md_edit_above_hr_changes_hash() {
    let temp = TempDir::new().unwrap();
    let file = write(
        temp.path(),
        "doc.md",
        b"----\nIntro paragraph.\n\n---\nbody",
    );
    let first = file_hash(&file, temp.path()).unwrap();
    fs::write(&file, b"----\nEdited intro paragraph.\n\n---\nbody").unwrap();
    assert_ne!(first, file_hash(&file, temp.path()).unwrap());
}

#[test]
fn test_save_cached_relativizes_source_file() {
    let temp = TempDir::new().unwrap();
    let source = write(temp.path(), "src/foo.py", b"def x(): pass\n");
    let absolute = source.to_string_lossy().into_owned();
    save_cached_value(
        &source,
        &json!({
            "nodes": [{"id": "n1", "label": "foo", "source_file": absolute}],
            "edges": [{"source": "n1", "target": "n1", "source_file": absolute}],
        }),
        temp.path(),
        "ast",
        None,
    )
    .unwrap();
    let hash = file_hash(&source, temp.path()).unwrap();
    let on_disk: serde_json::Value = serde_json::from_slice(
        &fs::read(
            cache_dir(temp.path(), "ast", None)
                .unwrap()
                .join(format!("{hash}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(on_disk["nodes"][0]["source_file"], "src/foo.py");
    assert_eq!(on_disk["edges"][0]["source_file"], "src/foo.py");
}

#[test]
fn test_load_cached_absolutizes_source_file() {
    let temp = TempDir::new().unwrap();
    let source = write(temp.path(), "src/foo.py", b"def x(): pass\n");
    let absolute = source.to_string_lossy().into_owned();
    save_cached_value(
        &source,
        &json!({
            "nodes": [{"id": "n1", "source_file": absolute}],
            "edges": [{"source": "n1", "target": "n1", "source_file": absolute}],
        }),
        temp.path(),
        "ast",
        None,
    )
    .unwrap();
    let loaded = load_cached_value(&source, temp.path(), "ast", None).unwrap();
    assert_eq!(loaded["nodes"][0]["source_file"], absolute);
    assert_eq!(loaded["edges"][0]["source_file"], absolute);
}

#[test]
fn test_load_cached_passes_through_legacy_absolute_source_file() {
    let temp = TempDir::new().unwrap();
    let source = write(temp.path(), "src/foo.py", b"pass\n");
    let absolute = source.to_string_lossy().into_owned();
    let hash = file_hash(&source, temp.path()).unwrap();
    let entry = cache_dir(temp.path(), "ast", None)
        .unwrap()
        .join(format!("{hash}.json"));
    fs::write(
        entry,
        serde_json::to_vec(&json!({
            "nodes": [{"id": "n1", "source_file": absolute}],
            "edges": [],
        }))
        .unwrap(),
    )
    .unwrap();
    let loaded = load_cached_value(&source, temp.path(), "ast", None).unwrap();
    assert_eq!(loaded["nodes"][0]["source_file"], absolute);
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn test_cache_portable_across_roots() {
    let temp = TempDir::new().unwrap();
    let first_root = temp.path().join("repo_a");
    let first_source = write(&first_root, "src/foo.py", b"def x(): pass\n");
    save_cached_value(
        &first_source,
        &json!({
            "nodes": [{"id": "n1", "source_file": first_source}],
            "edges": [],
        }),
        &first_root,
        "ast",
        None,
    )
    .unwrap();

    let second_root = temp.path().join("repo_b");
    copy_tree(&first_root, &second_root);
    let second_source = second_root.join("src/foo.py");
    let loaded = load_cached_value(&second_source, &second_root, "ast", None).unwrap();
    assert_eq!(
        loaded["nodes"][0]["source_file"],
        second_source.to_string_lossy().as_ref()
    );
    assert!(!loaded["nodes"][0]["source_file"]
        .as_str()
        .unwrap()
        .contains("repo_a"));
}

#[test]
fn test_warm_cache_from_another_root_does_not_leak_that_root() {
    let temp = TempDir::new().unwrap();
    let first_root = temp.path().join("aaa_root_marker");
    let first_source = write(&first_root, "pkg/mod.py", b"class Base: pass\n");
    let first_id = graphoxide_core::normalize_id(&first_source.to_string_lossy());
    let result = json!({
        "nodes": [{"id": first_id, "source_file": first_source}],
        "edges": [{
            "source": first_id,
            "target": format!("{first_id}_base"),
            "source_file": first_source,
            "target_file": first_root.join("pkg/other.py"),
        }],
        "raw_calls": [{"caller_nid": format!("{first_id}_base"), "source_file": first_source}],
        "bash_sources": [{"source_file": first_root.join("lib/common.sh")}],
    });
    save_cached_value(&first_source, &result, &first_root, "ast", None).unwrap();
    assert_eq!(result["nodes"][0]["id"], first_id);
    let hash = file_hash(&first_source, &first_root).unwrap();
    let blob = fs::read_to_string(
        cache_dir(&first_root, "ast", None)
            .unwrap()
            .join(format!("{hash}.json")),
    )
    .unwrap();
    assert!(!blob.to_lowercase().contains("aaa_root_marker"));
    assert!(!blob.contains(first_root.to_string_lossy().as_ref()));

    let second_root = temp.path().join("bbb_root_marker");
    copy_tree(&first_root, &second_root);
    let second_source = second_root.join("pkg/mod.py");
    let warm = load_cached_value(&second_source, &second_root, "ast", None).unwrap();
    let second_id = graphoxide_core::normalize_id(&second_source.to_string_lossy());
    assert_eq!(warm["nodes"][0]["id"], second_id);
    assert_eq!(warm["edges"][0]["source"], second_id);
    assert_eq!(warm["edges"][0]["target"], format!("{second_id}_base"));
    let warm_blob = serde_json::to_string(&warm).unwrap();
    assert!(!warm_blob.contains("aaa_root_marker"));
    assert!(!warm_blob.contains('$'));
}

#[test]
fn test_cached_ids_round_trip_under_the_same_root() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let source = write(&root, "src/foo.py", b"def x(): pass\n");
    let minted = graphoxide_core::normalize_id(&source.to_string_lossy());
    let result = json!({
        "nodes": [{"id": minted, "source_file": source}],
        "edges": [{"source": minted, "target": format!("{minted}_x"), "source_file": source}],
        "raw_calls": [{"caller_nid": format!("{minted}_x"), "source_file": source}],
    });
    save_cached_value(&source, &result, &root, "ast", None).unwrap();
    assert_eq!(result["nodes"][0]["id"], minted);
    let loaded = load_cached_value(&source, &root, "ast", None).unwrap();
    assert_eq!(loaded["nodes"][0]["id"], minted);
    assert_eq!(loaded["edges"][0]["source"], minted);
    assert_eq!(loaded["edges"][0]["target"], format!("{minted}_x"));
    assert_eq!(loaded["raw_calls"][0]["caller_nid"], format!("{minted}_x"));
}

#[test]
fn test_relative_root_does_not_reanchor_an_already_canonical_id() {
    let current = std::env::current_dir().unwrap();
    let temp = tempfile::Builder::new()
        .prefix(".graphoxide-cache-")
        .tempdir_in(&current)
        .unwrap();
    let relative_base = temp.path().strip_prefix(&current).unwrap();
    let relative_root = relative_base.join("src");
    let source = write(temp.path(), "src/utils/foo.py", b"x = 1\n");
    let canonical = json!({
        "nodes": [{"id": "src_utils_foo", "source_file": source}],
        "edges": [],
    });
    save_cached_value(&source, &canonical, &relative_root, "semantic", None).unwrap();
    let loaded = load_cached_value(&source, &relative_root, "semantic", None).unwrap();
    assert_eq!(loaded["nodes"][0]["id"], "src_utils_foo");
}

#[test]
fn test_ast_cache_invalidated_on_version_bump() {
    let temp = TempDir::new().unwrap();
    let source = write(temp.path(), "mod.py", b"def f(): pass\n");
    let value = json!({"nodes": [{"id": "n1"}], "edges": []});
    save_cached_value_with_version(&source, &value, temp.path(), "ast", None, 800).unwrap();
    assert!(load_cached_value_with_version(&source, temp.path(), "ast", None, 800).is_some());
    assert!(load_cached_value_with_version(&source, temp.path(), "ast", None, 801).is_none());
}

#[test]
fn test_ast_cache_schema_preparation_retires_only_pre_redaction_versions() {
    let temp = TempDir::new().unwrap();
    let source = write(temp.path(), "mod.py", b"def f(): pass\n");
    let value = json!({"nodes": [{"id": "n1"}], "edges": []});
    save_cached_value_with_version(&source, &value, temp.path(), "ast", None, 29).unwrap();
    save_cached_value_with_version(&source, &value, temp.path(), "ast", None, 32).unwrap();
    let retired = temp.path().join("graphoxide-out/cache/ast/v29");
    let future = temp.path().join("graphoxide-out/cache/ast/v32");
    assert!(retired.read_dir().unwrap().next().is_some());
    assert!(future.read_dir().unwrap().next().is_some());

    let current = cache_dir(temp.path(), "ast", None).unwrap();
    assert!(retired.exists(), "opening a schema must not erase another");
    assert!(future.exists(), "opening v31 must preserve a future schema");

    prepare_structured_redaction_cache_schema(&temp.path().join("graphoxide-out")).unwrap();
    assert!(!retired.exists());
    assert!(current.exists());
    assert!(future.exists(), "schema preparation only retires v0..v29");
}

#[test]
fn test_legacy_unversioned_ast_entries_not_served() {
    let temp = TempDir::new().unwrap();
    let source = write(temp.path(), "mod.py", b"def f(): pass\n");
    let hash = file_hash(&source, temp.path()).unwrap();
    let ast = temp.path().join("graphoxide-out/cache/ast");
    fs::create_dir_all(&ast).unwrap();
    let payload = serde_json::to_vec(&json!({"nodes": [{"id": "stale"}], "edges": []})).unwrap();
    fs::write(ast.join(format!("{hash}.json")), &payload).unwrap();
    fs::write(ast.parent().unwrap().join(format!("{hash}.json")), payload).unwrap();
    assert!(load_cached_value(&source, temp.path(), "ast", None).is_none());
}

#[test]
fn test_semantic_cache_survives_version_bump() {
    let temp = TempDir::new().unwrap();
    let source = write(temp.path(), "doc.md", b"# Title\n\nBody.\n");
    save_cached_value(
        &source,
        &json!({"nodes": [{"id": "n1"}], "edges": []}),
        temp.path(),
        "semantic",
        None,
    )
    .unwrap();
    cache_dir_with_ast_version(temp.path(), "ast", None, 800).unwrap();
    cache_dir_with_ast_version(temp.path(), "ast", None, 801).unwrap();
    assert!(load_cached_value(&source, temp.path(), "semantic", None).is_some());
    assert!(cache_dir(temp.path(), "semantic", None)
        .unwrap()
        .read_dir()
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|ext| ext == "json")));
}

#[test]
fn test_save_cached_in_root_symlink_keeps_symlink_name() {
    let temp = TempDir::new().unwrap();
    let target = write(temp.path(), "sub/target.py", b"pass\n");
    let alias = temp.path().join("alias.py");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &alias).unwrap();
    save_cached_value(
        &alias,
        &json!({"nodes": [{"id": "n1", "source_file": alias}], "edges": []}),
        temp.path(),
        "ast",
        None,
    )
    .unwrap();
    let hash = file_hash(&alias, temp.path()).unwrap();
    let value: serde_json::Value = serde_json::from_slice(
        &fs::read(
            cache_dir(temp.path(), "ast", None)
                .unwrap()
                .join(format!("{hash}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(value["nodes"][0]["source_file"], "alias.py");
}

#[test]
fn test_semantic_prune_removes_orphan_entries() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# A\n\nContent A.\n");
    let old_hash = file_hash(&file, temp.path()).unwrap();
    save_cached_value(
        &file,
        &json!({"nodes": [{"id": "a"}], "edges": []}),
        temp.path(),
        "semantic",
        None,
    )
    .unwrap();
    fs::write(&file, b"# B\n\nContent B.\n").unwrap();
    let live_hash = file_hash(&file, temp.path()).unwrap();
    save_cached_value(
        &file,
        &json!({"nodes": [{"id": "b"}], "edges": []}),
        temp.path(),
        "semantic",
        None,
    )
    .unwrap();
    let directory = cache_dir(temp.path(), "semantic", None).unwrap();
    assert!(directory.join(format!("{old_hash}.json")).is_file());
    assert!(directory.join(format!("{live_hash}.json")).is_file());
    assert_eq!(
        prune_semantic_cache(temp.path(), &BTreeSet::from([live_hash.clone()])),
        1
    );
    assert!(!directory.join(format!("{old_hash}.json")).exists());
    assert!(directory.join(format!("{live_hash}.json")).is_file());
}

#[test]
fn test_semantic_prune_keeps_live_unchanged_entries() {
    let temp = TempDir::new().unwrap();
    let mut live = BTreeSet::new();
    for index in 0..5 {
        let file = write(
            temp.path(),
            &format!("doc{index}.md"),
            format!("# Doc {index}\n\nBody {index}.\n").as_bytes(),
        );
        save_cached_value(
            &file,
            &json!({"nodes": [{"id": index.to_string()}], "edges": []}),
            temp.path(),
            "semantic",
            None,
        )
        .unwrap();
        live.insert(file_hash(&file, temp.path()).unwrap());
    }
    assert_eq!(prune_semantic_cache(temp.path(), &live), 0);
    assert_eq!(cached_files(temp.path()).intersection(&live).count(), 5);
}

#[test]
fn test_semantic_prune_handles_deleted_file() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "gone.md", b"# Gone\n\nWill be deleted.\n");
    let hash = file_hash(&file, temp.path()).unwrap();
    save_cached_value(
        &file,
        &json!({"nodes": [{"id": "g"}], "edges": []}),
        temp.path(),
        "semantic",
        None,
    )
    .unwrap();
    fs::remove_file(&file).unwrap();
    assert_eq!(prune_semantic_cache(temp.path(), &BTreeSet::new()), 1);
    assert!(!cache_dir(temp.path(), "semantic", None)
        .unwrap()
        .join(format!("{hash}.json"))
        .exists());
}

#[test]
fn test_semantic_prune_ignores_ast_and_tmp() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n\nBody.\n");
    save_cached_value(
        &file,
        &json!({"nodes": [{"id": "ast"}], "edges": []}),
        temp.path(),
        "ast",
        None,
    )
    .unwrap();
    let ast = cache_dir(temp.path(), "ast", None).unwrap();
    let semantic = cache_dir(temp.path(), "semantic", None).unwrap();
    fs::write(semantic.join("deadbeef.json"), b"{}").unwrap();
    fs::write(semantic.join("deadbeef.tmp"), b"partial").unwrap();
    assert_eq!(prune_semantic_cache(temp.path(), &BTreeSet::new()), 1);
    assert!(semantic.join("deadbeef.tmp").is_file());
    assert_eq!(ast.read_dir().unwrap().count(), 1);
}

#[test]
fn test_save_semantic_cache_overwrites_by_default() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n");
    let options = SemanticCacheOptions::default();
    save_semantic_cache(
        &[json!({"id": "a", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    save_semantic_cache(
        &[json!({"id": "b", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    let cached = load_cached_value(&file, temp.path(), "semantic", None).unwrap();
    assert_eq!(cached["nodes"], json!([{"id": "b", "source_file": file}]));
}

#[test]
fn test_save_semantic_cache_rejects_out_of_scope_source_file() {
    let temp = TempDir::new().unwrap();
    let intended = write(temp.path(), "intended.md", b"# Intended\n");
    let protected = write(temp.path(), "protected.md", b"# Protected\n");
    save_semantic_cache(
        &[json!({"id": "original", "source_file": "protected.md"})],
        &[],
        &[],
        temp.path(),
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    let options = SemanticCacheOptions {
        allowed_source_files: Some(BTreeSet::from([PathBuf::from("intended.md")])),
        ..SemanticCacheOptions::default()
    };
    let report = save_semantic_cache(
        &[
            json!({"id": "expected", "source_file": intended}),
            json!({"id": "stray", "source_file": "protected.md"}),
        ],
        &[json!({"source": "stray", "target": "expected", "source_file": "protected.md"})],
        &[json!({"id": "stray_hyperedge", "nodes": ["stray"], "source_file": "protected.md"})],
        temp.path(),
        &options,
    )
    .unwrap();
    assert_eq!(report.saved, 1);
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("out-of-scope source_file 'protected.md'")));
    let intended_cache = load_cached_value(&intended, temp.path(), "semantic", None).unwrap();
    assert_eq!(intended_cache["nodes"][0]["id"], "expected");
    let protected_cache = load_cached_value(&protected, temp.path(), "semantic", None).unwrap();
    assert_eq!(protected_cache["nodes"][0]["id"], "original");
    assert_eq!(protected_cache["edges"], json!([]));
    assert_eq!(protected_cache["hyperedges"], json!([]));
}

fn options_with_mode(mode: &str) -> SemanticCacheOptions {
    SemanticCacheOptions {
        mode: Some(mode.into()),
        ..SemanticCacheOptions::default()
    }
}

#[test]
fn test_semantic_cache_deep_mode_roundtrip_under_deep_namespace() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n\nBody.\n");
    let options = options_with_mode("deep");
    let report = save_semantic_cache(
        &[json!({"id": "deep_n", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    assert_eq!(report.saved, 1);
    let hash = file_hash(&file, temp.path()).unwrap();
    assert!(temp
        .path()
        .join(format!("graphoxide-out/cache/semantic-deep/{hash}.json"))
        .is_file());
    assert!(!temp
        .path()
        .join(format!("graphoxide-out/cache/semantic/{hash}.json"))
        .exists());
    let checked = check_semantic_cache(&[file], temp.path(), &options);
    assert_eq!(checked.nodes[0]["id"], "deep_n");
    assert!(checked.uncached.is_empty());
}

#[test]
fn test_semantic_cache_deep_invisible_to_plain_reads_and_vice_versa() {
    let temp = TempDir::new().unwrap();
    let deep = write(temp.path(), "deep.md", b"# Deep\n");
    let plain = write(temp.path(), "plain.md", b"# Plain\n");
    save_semantic_cache(
        &[json!({"id": "d", "source_file": "deep.md"})],
        &[],
        &[],
        temp.path(),
        &options_with_mode("deep"),
    )
    .unwrap();
    save_semantic_cache(
        &[json!({"id": "p", "source_file": "plain.md"})],
        &[],
        &[],
        temp.path(),
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    let plain_check = check_semantic_cache(
        &[deep.clone(), plain.clone()],
        temp.path(),
        &SemanticCacheOptions::default(),
    );
    assert_eq!(plain_check.nodes[0]["id"], "p");
    assert_eq!(plain_check.uncached, vec![deep.clone()]);
    let deep_check = check_semantic_cache(
        &[deep, plain.clone()],
        temp.path(),
        &options_with_mode("deep"),
    );
    assert_eq!(deep_check.nodes[0]["id"], "d");
    assert_eq!(deep_check.uncached, vec![plain]);
}

#[test]
fn test_semantic_cache_mode_none_layout_unchanged() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n");
    let options = SemanticCacheOptions::default();
    save_semantic_cache(
        &[json!({"id": "n", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    let hash = file_hash(&file, temp.path()).unwrap();
    assert!(temp
        .path()
        .join(format!("graphoxide-out/cache/semantic/{hash}.json"))
        .is_file());
    assert!(!temp
        .path()
        .join("graphoxide-out/cache/semantic-deep")
        .exists());
    let checked = check_semantic_cache(&[file], temp.path(), &options);
    assert_eq!(checked.nodes[0]["id"], "n");
    assert!(checked.uncached.is_empty());
}

#[test]
fn test_clear_cache_removes_deep_namespace() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "doc.md", b"# Doc\n");
    save_semantic_cache(
        &[json!({"id": "p", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    save_semantic_cache(
        &[json!({"id": "d", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options_with_mode("deep"),
    )
    .unwrap();
    assert_eq!(clear_cache(temp.path()).unwrap(), 2);
    assert!(cached_files(temp.path()).is_empty());
}

#[test]
fn test_cached_files_includes_deep_namespace() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n");
    save_semantic_cache(
        &[json!({"id": "d", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options_with_mode("deep"),
    )
    .unwrap();
    assert!(cached_files(temp.path()).contains(&file_hash(&file, temp.path()).unwrap()));
}

#[test]
fn test_semantic_prune_sweeps_both_namespaces_against_same_live_set() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# A\n\nContent A.\n");
    let old = file_hash(&file, temp.path()).unwrap();
    for (id, options) in [
        ("pa", SemanticCacheOptions::default()),
        ("da", options_with_mode("deep")),
    ] {
        save_semantic_cache(
            &[json!({"id": id, "source_file": "doc.md"})],
            &[],
            &[],
            temp.path(),
            &options,
        )
        .unwrap();
    }
    fs::write(&file, b"# B\n\nContent B.\n").unwrap();
    let live = file_hash(&file, temp.path()).unwrap();
    for (id, options) in [
        ("pb", SemanticCacheOptions::default()),
        ("db", options_with_mode("deep")),
    ] {
        save_semantic_cache(
            &[json!({"id": id, "source_file": "doc.md"})],
            &[],
            &[],
            temp.path(),
            &options,
        )
        .unwrap();
    }
    assert_eq!(
        prune_semantic_cache(temp.path(), &BTreeSet::from([live.clone()])),
        2
    );
    for kind in ["semantic", "semantic-deep"] {
        let directory = temp.path().join(format!("graphoxide-out/cache/{kind}"));
        assert!(!directory.join(format!("{old}.json")).exists());
        assert!(directory.join(format!("{live}.json")).is_file());
    }
}

#[test]
fn test_save_semantic_cache_merge_existing_unions() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "big.md", b"# Big\n");
    let options = SemanticCacheOptions {
        merge_existing: true,
        ..SemanticCacheOptions::default()
    };
    save_semantic_cache(
        &[json!({"id": "a", "source_file": "big.md"})],
        &[json!({"source": "a", "target": "x", "source_file": "big.md"})],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    save_semantic_cache(
        &[json!({"id": "b", "source_file": "big.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    let cached = load_cached_value(&file, temp.path(), "semantic", None).unwrap();
    let ids = cached["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids, BTreeSet::from(["a", "b"]));
    assert_eq!(cached["edges"].as_array().unwrap().len(), 1);
}

fn scoped_options(allowed: &str, merge_existing: bool) -> SemanticCacheOptions {
    SemanticCacheOptions {
        merge_existing,
        allowed_source_files: Some(BTreeSet::from([PathBuf::from(allowed)])),
        ..SemanticCacheOptions::default()
    }
}

#[test]
fn test_save_semantic_cache_drops_edges_to_out_of_scope_nodes() {
    let temp = TempDir::new().unwrap();
    let allowed = write(temp.path(), "allowed.md", b"# Allowed\n");
    write(temp.path(), "outside.md", b"# Outside\n");
    let report = save_semantic_cache(
        &[
            json!({"id": "kept", "source_file": "allowed.md"}),
            json!({"id": "stray", "source_file": "outside.md"}),
            json!({"id": "dup", "source_file": "allowed.md"}),
            json!({"id": "dup", "source_file": "outside.md"}),
        ],
        &[
            json!({"source": "kept", "target": "stray", "source_file": "allowed.md"}),
            json!({"source": "stray", "target": "kept", "source_file": "allowed.md"}),
            json!({"source": "kept", "target": "dup", "source_file": "allowed.md"}),
        ],
        &[],
        temp.path(),
        &scoped_options("allowed.md", false),
    )
    .unwrap();
    assert_eq!(report.saved, 1);
    let checked = check_semantic_cache(&[allowed], temp.path(), &SemanticCacheOptions::default());
    assert_eq!(
        checked
            .nodes
            .iter()
            .map(|node| node["id"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["dup", "kept"])
    );
    assert_eq!(
        checked.edges,
        vec![
            json!({"source": "kept", "target": "dup", "source_file": temp.path().join("allowed.md")})
        ]
    );
}

#[test]
fn test_save_semantic_cache_drops_edges_to_ghost_file_nodes() {
    let temp = TempDir::new().unwrap();
    let real = write(temp.path(), "real.md", b"# Real\n");
    save_semantic_cache(
        &[json!({"id": "kept", "source_file": "real.md"}), json!({"id": "phantom", "source_file": "ghost.md"})],
        &[json!({"source": "kept", "target": "phantom", "source_file": "real.md"}), json!({"source": "kept", "target": "kept", "relation": "self", "source_file": "real.md"})],
        &[], temp.path(), &scoped_options("real.md", false),
    ).unwrap();
    let checked = check_semantic_cache(&[real], temp.path(), &SemanticCacheOptions::default());
    assert_eq!(checked.nodes[0]["id"], "kept");
    assert_eq!(checked.edges.len(), 1);
    assert_eq!(checked.edges[0]["target"], "kept");
}

#[test]
fn test_save_semantic_cache_drops_hyperedges_touching_skipped_nodes() {
    let temp = TempDir::new().unwrap();
    let allowed = write(temp.path(), "allowed.md", b"# Allowed\n");
    write(temp.path(), "outside.md", b"# Outside\n");
    save_semantic_cache(
        &[
            json!({"id": "kept", "source_file": "allowed.md"}),
            json!({"id": "kept2", "source_file": "allowed.md"}),
            json!({"id": "stray", "source_file": "outside.md"}),
        ],
        &[],
        &[
            json!({"id": "he_bad", "nodes": ["kept", "stray"], "source_file": "allowed.md"}),
            json!({"id": "he_ok", "nodes": ["kept", "kept2"], "source_file": "allowed.md"}),
        ],
        temp.path(),
        &scoped_options("allowed.md", false),
    )
    .unwrap();
    let checked = check_semantic_cache(&[allowed], temp.path(), &SemanticCacheOptions::default());
    assert_eq!(checked.hyperedges.len(), 1);
    assert_eq!(checked.hyperedges[0]["id"], "he_ok");
}

#[test]
fn test_save_semantic_cache_unscoped_preserves_dangling_refs_verbatim() {
    let temp = TempDir::new().unwrap();
    let doc = write(temp.path(), "doc.md", b"# Doc\n");
    let edges = vec![json!({"source": "a", "target": "ghost_n", "source_file": "doc.md"})];
    let hyperedges = vec![json!({"id": "he", "nodes": ["a", "ghost_n"], "source_file": "doc.md"})];
    let report = save_semantic_cache(
        &[
            json!({"id": "a", "source_file": "doc.md"}),
            json!({"id": "ghost_n", "source_file": "ghost.md"}),
        ],
        &edges,
        &hyperedges,
        temp.path(),
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    assert_eq!(report.saved, 1);
    let hash = file_hash(&doc, temp.path()).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(
        &fs::read(
            cache_dir(temp.path(), "semantic", None)
                .unwrap()
                .join(format!("{hash}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(raw["edges"], json!(edges));
    assert_eq!(raw["hyperedges"], json!(hyperedges));
}

#[test]
fn test_save_semantic_cache_merge_existing_prunes_only_incoming() {
    let temp = TempDir::new().unwrap();
    let big = write(temp.path(), "big.md", b"# Big\n");
    write(temp.path(), "other.md", b"# Other\n");
    let options = scoped_options("big.md", true);
    save_semantic_cache(
        &[json!({"id": "a", "source_file": "big.md"})],
        &[json!({"source": "a", "target": "a", "relation": "self", "source_file": "big.md"})],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    save_semantic_cache(
        &[
            json!({"id": "b", "source_file": "big.md"}),
            json!({"id": "stray", "source_file": "other.md"}),
        ],
        &[
            json!({"source": "b", "target": "stray", "source_file": "big.md"}),
            json!({"source": "a", "target": "b", "source_file": "big.md"}),
        ],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    let cached = load_cached_value(&big, temp.path(), "semantic", None).unwrap();
    let pairs = cached["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            (
                edge["source"].as_str().unwrap(),
                edge["target"].as_str().unwrap(),
            )
        })
        .collect::<BTreeSet<_>>();
    assert!(pairs.contains(&("a", "a")));
    assert!(pairs.contains(&("a", "b")));
    assert!(pairs
        .iter()
        .all(|(source, target)| *source != "stray" && *target != "stray"));
}

#[test]
fn test_prompt_fingerprint_stable_and_prompt_sensitive() {
    assert_eq!(
        prompt_fingerprint("extract a graph"),
        prompt_fingerprint("extract a graph")
    );
    assert_ne!(
        prompt_fingerprint("extract a graph"),
        prompt_fingerprint("extract a graph v2")
    );
    let temp = TempDir::new().unwrap();
    let spec = write(temp.path(), "extraction-spec.md", b"extract a graph");
    assert_eq!(
        prompt_file_fingerprint(&spec).unwrap(),
        prompt_fingerprint("extract a graph")
    );
}

#[test]
fn test_prompt_fingerprint_ignores_line_endings() {
    assert_eq!(
        prompt_fingerprint("a\r\nb\r\n"),
        prompt_fingerprint("a\nb\n")
    );
    assert_eq!(prompt_fingerprint("a  \nb\n"), prompt_fingerprint("a\nb\n"));
}

fn prompt_options(prompt: &str) -> SemanticCacheOptions {
    SemanticCacheOptions {
        prompt: Some(prompt.into()),
        ..SemanticCacheOptions::default()
    }
}

#[test]
fn test_semantic_cache_prompt_change_invalidates() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n\nBody.\n");
    save_semantic_cache(
        &[json!({"id": "old_vintage", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &prompt_options("PROMPT V1"),
    )
    .unwrap();
    let v1 = check_semantic_cache(
        std::slice::from_ref(&file),
        temp.path(),
        &prompt_options("PROMPT V1"),
    );
    assert_eq!(v1.nodes[0]["id"], "old_vintage");
    let miss = check_semantic_cache(
        std::slice::from_ref(&file),
        temp.path(),
        &prompt_options("PROMPT V2"),
    );
    assert!(miss.nodes.is_empty());
    assert_eq!(miss.uncached, vec![file.clone()]);
    save_semantic_cache(
        &[json!({"id": "new_vintage", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &prompt_options("PROMPT V2"),
    )
    .unwrap();
    assert_eq!(
        check_semantic_cache(
            std::slice::from_ref(&file),
            temp.path(),
            &prompt_options("PROMPT V2"),
        )
        .nodes[0]["id"],
        "new_vintage"
    );
    assert_eq!(
        check_semantic_cache(&[file], temp.path(), &prompt_options("PROMPT V1")).nodes[0]["id"],
        "old_vintage"
    );
}

#[test]
fn test_semantic_cache_prompt_namespaced_layout() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n");
    save_semantic_cache(
        &[json!({"id": "n", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &prompt_options("PROMPT V1"),
    )
    .unwrap();
    let hash = file_hash(&file, temp.path()).unwrap();
    let semantic = temp.path().join("graphoxide-out/cache/semantic");
    assert!(semantic
        .join(format!("p{}/{hash}.json", prompt_fingerprint("PROMPT V1")))
        .is_file());
    assert!(!semantic.join(format!("{hash}.json")).exists());
}

#[test]
fn test_semantic_cache_prompt_and_mode_compose() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n");
    let options = SemanticCacheOptions {
        mode: Some("deep".into()),
        prompt: Some("PROMPT V1".into()),
        ..SemanticCacheOptions::default()
    };
    save_semantic_cache(
        &[json!({"id": "d", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    assert!(temp
        .path()
        .join("graphoxide-out/cache/semantic-deep")
        .read_dir()
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('p')));
    let wrong_prompt = SemanticCacheOptions {
        mode: Some("deep".into()),
        prompt: Some("PROMPT V2".into()),
        ..SemanticCacheOptions::default()
    };
    assert_eq!(
        check_semantic_cache(std::slice::from_ref(&file), temp.path(), &wrong_prompt).uncached,
        vec![file.clone()]
    );
    assert_eq!(
        check_semantic_cache(
            std::slice::from_ref(&file),
            temp.path(),
            &prompt_options("PROMPT V1"),
        )
        .uncached,
        vec![file.clone()]
    );
    assert_eq!(
        check_semantic_cache(&[file], temp.path(), &options).nodes[0]["id"],
        "d"
    );
}

#[test]
fn test_semantic_cache_legacy_entries_served_with_warning() {
    let temp = TempDir::new().unwrap();
    let first = write(temp.path(), "a.md", b"# A\n");
    let second = write(temp.path(), "b.md", b"# B\n");
    save_semantic_cache(
        &[
            json!({"id": "a_old", "source_file": "a.md"}),
            json!({"id": "b_old", "source_file": "b.md"}),
        ],
        &[],
        &[],
        temp.path(),
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    let checked = check_semantic_cache(&[first, second], temp.path(), &prompt_options("PROMPT V1"));
    assert_eq!(checked.nodes.len(), 2);
    assert!(checked.uncached.is_empty());
    assert!(checked
        .warnings
        .iter()
        .any(|warning| warning.contains("2 semantic cache entries predate")));
}

#[test]
fn test_semantic_cache_fingerprinted_entry_beats_legacy() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n");
    save_semantic_cache(
        &[json!({"id": "unknown_vintage", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    save_semantic_cache(
        &[json!({"id": "current", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &prompt_options("PROMPT V1"),
    )
    .unwrap();
    let checked = check_semantic_cache(&[file], temp.path(), &prompt_options("PROMPT V1"));
    assert_eq!(checked.nodes[0]["id"], "current");
    assert!(checked.warnings.is_empty());
}

#[test]
fn test_semantic_cache_merge_existing_never_fuses_legacy_vintage() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n");
    save_semantic_cache(
        &[json!({"id": "unknown_vintage", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    let options = SemanticCacheOptions {
        prompt: Some("PROMPT V1".into()),
        merge_existing: true,
        ..SemanticCacheOptions::default()
    };
    save_semantic_cache(
        &[json!({"id": "current", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    let fp = prompt_fingerprint("PROMPT V1");
    let first = load_cached_value(&file, temp.path(), "semantic", Some(&fp)).unwrap();
    assert_eq!(first["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(first["nodes"][0]["id"], "current");
    save_semantic_cache(
        &[json!({"id": "second_chunk", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    let second = load_cached_value(&file, temp.path(), "semantic", Some(&fp)).unwrap();
    assert_eq!(
        second["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| node["id"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["current", "second_chunk"])
    );
}

#[test]
fn test_semantic_prune_and_clear_reach_fingerprint_subdirs() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n");
    let options = prompt_options("PROMPT V1");
    save_semantic_cache(
        &[json!({"id": "n", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    let hash = file_hash(&file, temp.path()).unwrap();
    assert!(cached_files(temp.path()).contains(&hash));
    assert_eq!(
        prune_semantic_cache(temp.path(), &BTreeSet::from([hash])),
        0
    );
    assert_eq!(prune_semantic_cache(temp.path(), &BTreeSet::new()), 1);
    save_semantic_cache(
        &[json!({"id": "n", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    clear_cache(temp.path()).unwrap();
    assert!(cached_files(temp.path()).is_empty());
}

#[test]
fn test_semantic_cache_unreadable_prompt_file_warns_and_falls_back() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "doc.md", b"# Doc\n");
    save_semantic_cache(
        &[json!({"id": "n", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    let options = SemanticCacheOptions {
        prompt_file: Some(temp.path().join("nope.md")),
        ..SemanticCacheOptions::default()
    };
    let checked = check_semantic_cache(&[file], temp.path(), &options);
    assert_eq!(checked.nodes[0]["id"], "n");
    assert!(checked.uncached.is_empty());
    assert!(checked
        .warnings
        .iter()
        .any(|warning| warning.contains("could not read extraction prompt")));
}

#[test]
fn test_prompt_file_reflects_edited_spec() {
    let temp = TempDir::new().unwrap();
    let spec = write(temp.path(), "extraction-spec.md", b"prompt one");
    let file = write(temp.path(), "doc.md", b"# Doc\n");
    let options = SemanticCacheOptions {
        prompt_file: Some(spec.clone()),
        ..SemanticCacheOptions::default()
    };
    save_semantic_cache(
        &[json!({"id": "v1", "source_file": "doc.md"})],
        &[],
        &[],
        temp.path(),
        &options,
    )
    .unwrap();
    assert_eq!(
        check_semantic_cache(std::slice::from_ref(&file), temp.path(), &options).nodes[0]["id"],
        "v1"
    );
    fs::write(&spec, "prompt two — rewritten by an upgrade").unwrap();
    let checked = check_semantic_cache(std::slice::from_ref(&file), temp.path(), &options);
    assert_eq!(checked.uncached, vec![file]);
}
