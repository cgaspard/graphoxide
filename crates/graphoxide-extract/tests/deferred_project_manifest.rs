use graphoxide_extract::{
    detect::DetectOptions, extract_project_with_scan_options_deferred_manifest,
};
use serde_json::Value;
use std::fs;

fn write(path: &std::path::Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

#[test]
fn deferred_manifest_stays_unchanged_until_graph_commit_point() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let output = fixture.path().join("managed/graphoxide-out");
    write(&project.join("app.py"), "def app():\n    return 1\n");
    let manifest_path = output.join("manifest.json");
    let previous = br#"{"old.py":{"mtime":1.0,"ast_hash":"old","semantic_hash":""}}"#;
    write(&manifest_path, std::str::from_utf8(previous).unwrap());

    let prepared = extract_project_with_scan_options_deferred_manifest(
        &project,
        false,
        &output,
        true,
        &DetectOptions {
            output_dir: Some(output.clone()),
            ..DetectOptions::default()
        },
    )
    .unwrap();

    assert!(prepared.progress.is_complete());
    assert_eq!(prepared.progress.total, 1);
    assert_eq!(prepared.progress.succeeded, 1);
    assert_eq!(prepared.pending_manifest.path(), manifest_path);
    assert_eq!(fs::read(&manifest_path).unwrap(), previous);

    prepared.pending_manifest.commit().unwrap();
    let committed: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert!(committed.get("app.py").is_some());
    assert!(committed.get("old.py").is_none());
}

#[test]
fn dropping_deferred_manifest_does_not_publish_it() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let output = fixture.path().join("managed/graphoxide-out");
    write(&project.join("app.py"), "def app():\n    return 1\n");

    let prepared = extract_project_with_scan_options_deferred_manifest(
        &project,
        false,
        &output,
        true,
        &DetectOptions {
            output_dir: Some(output.clone()),
            ..DetectOptions::default()
        },
    )
    .unwrap();
    drop(prepared);

    assert!(!output.join("manifest.json").exists());
}

#[test]
fn code_only_manifest_preserves_live_document_semantic_state() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let output = fixture.path().join("managed/graphoxide-out");
    write(&project.join("app.py"), "def app():\n    return 1\n");
    write(&project.join("README.md"), "# Live document\n");
    write(
        &output.join("manifest.json"),
        r#"{"README.md":{"mtime":1.0,"ast_hash":"doc-ast","semantic_hash":"doc-semantic"}}"#,
    );

    let prepared = extract_project_with_scan_options_deferred_manifest(
        &project,
        false,
        &output,
        true,
        &DetectOptions::default(),
    )
    .unwrap();
    prepared.pending_manifest.commit().unwrap();

    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["README.md"]["ast_hash"], "doc-ast");
    assert_eq!(manifest["README.md"]["semantic_hash"], "doc-semantic");
    assert!(manifest.get("app.py").is_some());
}

#[test]
fn manifest_keys_are_nfc_and_legacy_absolute_rows_reanchor() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let output = fixture.path().join("managed/graphoxide-out");
    write(&project.join("app.py"), "def app():\n    return 1\n");
    let composed_name = "caf\u{e9}.md";
    let decomposed_name = "cafe\u{301}.md";
    let document = project.join(composed_name);
    write(&document, "# Unicode document\n");
    let legacy_document = project.join("legacy.md");
    write(&legacy_document, "# Legacy absolute key\n");
    let seeded = serde_json::json!({
        (decomposed_name): {
            "mtime": 1.0,
            "ast_hash": "unicode-ast",
            "semantic_hash": "unicode-semantic"
        },
        (legacy_document.to_string_lossy().into_owned()): {
            "mtime": 2.0,
            "ast_hash": "legacy-absolute-ast",
            "semantic_hash": "legacy-absolute-semantic"
        }
    });
    write(
        &output.join("manifest.json"),
        &serde_json::to_string(&seeded).unwrap(),
    );

    let prepared = extract_project_with_scan_options_deferred_manifest(
        &project,
        false,
        &output,
        true,
        &DetectOptions::default(),
    )
    .unwrap();
    prepared.pending_manifest.commit().unwrap();

    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest.get(decomposed_name).is_none());
    assert_eq!(manifest[composed_name]["semantic_hash"], "unicode-semantic");
    assert_eq!(
        manifest["legacy.md"]["semantic_hash"],
        "legacy-absolute-semantic"
    );
    assert!(manifest
        .as_object()
        .unwrap()
        .keys()
        .all(|key| !key.starts_with('/')));
}

#[test]
fn managed_output_ownership_overrides_a_mismatched_detect_option() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let output = project.join("artifacts");
    write(&project.join("app.py"), "def app():\n    return 1\n");
    write(
        &output.join("must_not_be_ingested.py"),
        "def generated():\n    return 2\n",
    );

    let prepared = extract_project_with_scan_options_deferred_manifest(
        &project,
        false,
        &output,
        true,
        &DetectOptions {
            output_dir: Some(project.join("wrong-output")),
            ..DetectOptions::default()
        },
    )
    .unwrap();

    let sources = prepared
        .extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .map(|node| node.source_file.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(sources.contains("app.py"));
    assert!(sources
        .iter()
        .all(|source| !source.contains("must_not_be_ingested")));
    assert_eq!(
        prepared.pending_manifest.path(),
        output.join("manifest.json")
    );
}

#[test]
#[cfg(unix)]
fn walk_errors_mark_the_prepared_extraction_incomplete() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let output = fixture.path().join("managed/graphoxide-out");
    write(&project.join("app.py"), "def app():\n    return 1\n");
    write(
        &project.join("locked/hidden.py"),
        "def hidden():\n    pass\n",
    );
    let locked = project.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o0)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let prepared = extract_project_with_scan_options_deferred_manifest(
        &project,
        false,
        &output,
        true,
        &DetectOptions {
            output_dir: Some(output.clone()),
            ..DetectOptions::default()
        },
    )
    .unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!prepared.detection.walk_errors.is_empty());
    assert!(!prepared.progress.is_complete());
    assert_eq!(
        prepared.progress.total,
        prepared.progress.succeeded + prepared.detection.walk_errors.len()
    );
    assert!(!output.join("manifest.json").exists());
}
