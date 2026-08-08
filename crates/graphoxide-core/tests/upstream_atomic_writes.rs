//! Executable port of upstream `test_atomic_writes.py` (10 cases).

use graphoxide_core::{
    permission_fallback, write_graph_atomic, write_graph_atomic_strict,
    write_graph_atomic_strict_with_replacer, write_json_atomic, write_json_atomic_strict,
    write_json_atomic_strict_with_replacer, write_text_atomic, write_text_atomic_with_replacer,
    KnowledgeGraph, Node,
};
use serde_json::json;
use std::{collections::BTreeMap, fs};
use tempfile::tempdir;

#[test]
fn write_text_atomic_writes_and_leaves_no_tmp() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("out/graph.json");
    write_text_atomic(&path, r#"{"a": 1}"#).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(value, json!({"a": 1}));
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn write_text_atomic_preserves_existing_on_failure() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, "original").unwrap();
    let error = write_text_atomic_with_replacer(&path, "content-that-must-not-land", |_, _| {
        Err(std::io::Error::other("simulated disk full"))
    });
    assert!(error.is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn write_text_atomic_preserves_existing_mode() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, "{}").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    write_text_atomic(&path, r#"{"x": 1}"#).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[cfg(not(unix))]
#[test]
fn write_text_atomic_preserves_existing_mode() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, "{}").unwrap();
    let readonly = fs::metadata(&path).unwrap().permissions().readonly();
    write_text_atomic(&path, r#"{"x": 1}"#).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().readonly(),
        readonly
    );
}

#[cfg(unix)]
#[test]
fn write_text_atomic_new_file_respects_umask() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("new.json");
    // SAFETY: passing zero reads the process umask; it is restored immediately,
    // exactly as the upstream test does via `os.umask`.
    let mask = unsafe { libc::umask(0) };
    // SAFETY: restore the value returned by the preceding call.
    unsafe { libc::umask(mask) };
    write_text_atomic(&path, "{}").unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o666 & !(mask as u32)
    );
}

#[cfg(not(unix))]
#[test]
fn write_text_atomic_new_file_respects_umask() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("new.json");
    write_text_atomic(&path, "{}").unwrap();
    assert!(!fs::metadata(&path).unwrap().permissions().readonly());
}

#[cfg(unix)]
#[test]
fn write_text_atomic_writes_through_symlink() {
    use std::os::unix::fs::symlink;
    let tmp = tempdir().unwrap();
    let target = tmp.path().join("real.json");
    fs::write(&target, "old").unwrap();
    let link = tmp.path().join("link.json");
    symlink(&target, &link).unwrap();
    write_text_atomic(&link, "new").unwrap();
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(target).unwrap(), "new");
}

#[cfg(not(unix))]
#[test]
fn write_text_atomic_writes_through_symlink() {
    // Windows symlink creation needs a privilege unavailable on many CI hosts;
    // the destination-resolution branch is exercised on Unix CI.
}

#[test]
fn write_json_atomic_roundtrip() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("g.json");
    write_json_atomic(&path, &json!({"nodes": [1, 2], "x": "é"}), true).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap(),
        json!({"nodes": [1, 2], "x": "é"})
    );
    assert!(!fs::read_dir(tmp.path()).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}

#[test]
fn to_json_writes_atomically_no_tmp_leftover() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    let graph = KnowledgeGraph {
        nodes: vec![node("a"), node("b")],
        ..KnowledgeGraph::default()
    };
    assert!(write_graph_atomic(&path, &graph, true).unwrap());
    serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap();
    assert!(!fs::read_dir(tmp.path()).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}

#[test]
fn save_manifest_writes_atomically() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graphoxide-out/manifest.json");
    write_json_atomic(&path, &json!({"a.py": {"hash": "abc"}}), false).unwrap();
    assert!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap())
            .unwrap()
            .is_object()
    );
    assert!(!fs::read_dir(path.parent().unwrap())
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
}

#[test]
fn write_text_atomic_windows_permission_fallback() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, "original").unwrap();
    let temporary = tmp.path().join(".graph.json.tmp");
    fs::write(&temporary, "new-content").unwrap();
    permission_fallback(&temporary, &path).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "new-content");
    assert!(!temporary.exists());
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
}

#[test]
fn write_json_atomic_ensure_ascii_false_preserves_utf8() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("g.json");
    write_json_atomic(&path, &json!({"label": "Wörker 数据"}), false).unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    assert!(raw.contains("Wörker 数据"));
    assert!(!raw.contains("\\u"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
        json!({"label": "Wörker 数据"})
    );
}

#[test]
fn strict_json_replaces_an_existing_destination_without_a_copy_fallback() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("manifest.json");
    write_json_atomic_strict(&path, &json!({"generation": 1}), false).unwrap();
    write_json_atomic_strict(&path, &json!({"generation": 2}), false).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap(),
        json!({"generation": 2})
    );
    assert!(!fs::read_dir(tmp.path()).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}

#[test]
fn strict_json_replace_failure_preserves_existing_bytes() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("manifest.json");
    fs::write(&path, b"previous manifest\n").unwrap();

    let error =
        write_json_atomic_strict_with_replacer(&path, &json!({"generation": 2}), false, |_, _| {
            Err(std::io::Error::other("injected strict replace failure"))
        })
        .expect_err("replace failure");
    assert!(error
        .to_string()
        .contains("injected strict replace failure"));
    assert_eq!(fs::read(&path).unwrap(), b"previous manifest\n");
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
}

#[test]
fn strict_graph_replace_failure_preserves_existing_bytes() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    let previous = KnowledgeGraph {
        nodes: vec![node("previous")],
        ..KnowledgeGraph::default()
    };
    write_graph_atomic_strict(&path, &previous, true).unwrap();
    let before = fs::read(&path).unwrap();
    let replacement = KnowledgeGraph {
        nodes: vec![node("replacement"), node("second")],
        ..KnowledgeGraph::default()
    };

    let error = write_graph_atomic_strict_with_replacer(&path, &replacement, true, |_, _| {
        Err(std::io::Error::other("injected graph replace failure"))
    })
    .expect_err("replace failure");
    assert!(error.to_string().contains("injected graph replace failure"));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn strict_graph_rejects_a_symlink_without_touching_its_external_target() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let managed = tmp.path().join("managed");
    fs::create_dir(&managed).unwrap();
    let external = tmp.path().join("external-graph.json");
    fs::write(&external, b"external graph\n").unwrap();
    let link = managed.join("graph.json");
    symlink(&external, &link).unwrap();

    let error = write_graph_atomic_strict(&link, &KnowledgeGraph::default(), true)
        .expect_err("symlink must fail closed");
    assert!(error
        .to_string()
        .contains("symlinked publication destination"));
    assert_eq!(fs::read(&external).unwrap(), b"external graph\n");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(fs::read_dir(&managed).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn strict_json_rejects_a_symlink_without_touching_its_external_target() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let managed = tmp.path().join("managed");
    fs::create_dir(&managed).unwrap();
    let external = tmp.path().join("external-manifest.json");
    fs::write(&external, b"external manifest\n").unwrap();
    let link = managed.join("manifest.json");
    symlink(&external, &link).unwrap();

    let error = write_json_atomic_strict(&link, &json!({"new": true}), false)
        .expect_err("symlink must fail closed");
    assert!(error
        .to_string()
        .contains("symlinked publication destination"));
    assert_eq!(fs::read(&external).unwrap(), b"external manifest\n");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(fs::read_dir(&managed).unwrap().count(), 1);
}

#[test]
fn strict_json_rejects_a_non_file_destination() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("manifest.json");
    fs::create_dir(&path).unwrap();

    let error = write_json_atomic_strict(&path, &json!({"new": true}), false)
        .expect_err("directory must fail closed");
    assert!(error
        .to_string()
        .contains("non-file publication destination"));
    assert!(path.is_dir());
}

fn node(id: &str) -> Node {
    Node {
        id: id.into(),
        label: id.into(),
        file_type: "code".into(),
        source_file: format!("{id}.py"),
        source_location: None,
        community: None,
        extra: BTreeMap::new(),
    }
}
