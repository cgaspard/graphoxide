use graphoxide_extract::{
    detect::{detect, DetectOptions, DetectResult},
    stale::stale_graph_sources,
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use unicode_normalization::UnicodeNormalization;

fn nfc_name() -> String {
    "café.md".nfc().collect()
}

fn nfd_name() -> String {
    "café.md".nfd().collect()
}

fn write_graph(root: &Path, sources: &[&str]) -> PathBuf {
    let out = root.join("graphoxide-out");
    fs::create_dir_all(&out).unwrap();
    let path = out.join("graph.json");
    let nodes = sources
        .iter()
        .enumerate()
        .map(|(index, source)| json!({"id": format!("n{index}"), "label": format!("node {index}"), "source_file": source}))
        .collect::<Vec<_>>();
    fs::write(
        &path,
        serde_json::to_vec(&json!({"nodes": nodes, "links": []})).unwrap(),
    )
    .unwrap();
    path
}

fn scan(root: &Path) -> (DetectResult, BTreeSet<PathBuf>) {
    let detection = detect(root, &DetectOptions::default()).unwrap();
    let seen = detection
        .files
        .values()
        .flatten()
        .chain(&detection.unclassified)
        .map(PathBuf::from)
        .collect();
    (detection, seen)
}

#[test]
fn test_nfd_disk_nfc_graph_source_not_pruned() {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("docs")).unwrap();
    fs::write(
        temp.path().join("docs").join(nfd_name()),
        "# cafe notes\n\nhello\n",
    )
    .unwrap();
    let source = format!("docs/{}", nfc_name());
    let graph = write_graph(temp.path(), &[&source]);
    let (detection, seen) = scan(temp.path());
    assert!(
        stale_graph_sources(&graph, temp.path(), &seen, Some(&detection))
            .stale
            .is_empty()
    );
}

#[test]
fn test_bare_basename_alive_elsewhere_not_pruned() {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("docs")).unwrap();
    fs::write(
        temp.path().join("docs").join(nfd_name()),
        "# cafe notes\n\nhello\n",
    )
    .unwrap();
    let graph = write_graph(temp.path(), &[&nfc_name()]);
    let (detection, seen) = scan(temp.path());
    assert!(
        stale_graph_sources(&graph, temp.path(), &seen, Some(&detection))
            .stale
            .is_empty()
    );
}

#[test]
fn test_genuinely_deleted_source_still_pruned() {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("docs")).unwrap();
    fs::write(temp.path().join("docs/keep.md"), "# keep\n").unwrap();
    let graph = write_graph(temp.path(), &["docs/keep.md", "docs/gone.md"]);
    let (detection, seen) = scan(temp.path());
    assert_eq!(
        stale_graph_sources(&graph, temp.path(), &seen, Some(&detection)).stale,
        ["docs/gone.md"]
    );
}

#[test]
fn test_alive_but_ignored_source_is_pruned() {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("docs")).unwrap();
    fs::write(temp.path().join("docs/keep.md"), "# keep\n").unwrap();
    fs::write(temp.path().join("docs/secret.md"), "# secret\n").unwrap();
    fs::write(temp.path().join(".graphifyignore"), "docs/secret.md\n").unwrap();
    let graph = write_graph(temp.path(), &["docs/keep.md", "docs/secret.md"]);
    let (detection, seen) = scan(temp.path());
    assert_eq!(
        stale_graph_sources(&graph, temp.path(), &seen, Some(&detection)).stale,
        ["docs/secret.md"]
    );
}

#[test]
fn test_alive_unproven_exclusion_kept_with_warning() {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("docs")).unwrap();
    fs::write(temp.path().join("docs/keep.md"), "# keep\n").unwrap();
    fs::write(temp.path().join("docs/other.md"), "# other\n").unwrap();
    let graph = write_graph(temp.path(), &["docs/keep.md", "docs/other.md"]);
    let (mut detection, mut seen) = scan(temp.path());
    seen.retain(|path| !path.to_string_lossy().ends_with("other.md"));
    detection.ignored.clear();
    detection.pruned_noise_dirs.clear();
    detection.skipped_sensitive.clear();
    let report = stale_graph_sources(&graph, temp.path(), &seen, Some(&detection));
    assert!(report.stale.is_empty());
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("fail-closed")));
}
