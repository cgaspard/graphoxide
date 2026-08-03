use graphoxide_core::Extraction;
use graphoxide_extract::{
    cache::{ast_cache_get_from_output, load_manifest_from_output},
    extract_files, extract_files_with,
};
use std::{fs, path::Path};
use tempfile::TempDir;

fn write(root: &Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = root.join(name);
    fs::write(&path, content).unwrap();
    path
}

#[test]
fn test_zero_node_result_not_cached_then_self_heals() {
    let temp = TempDir::new().unwrap();
    let out = temp.path().join("out");
    let file = write(temp.path(), "thing.rb", "class Foo\n  def bar; end\nend\n");
    let first = extract_files_with(std::slice::from_ref(&file), Some(&out), false, |_, _| {
        Ok(Extraction::default())
    })
    .unwrap();
    assert!(first
        .warnings
        .iter()
        .any(|warning| warning.contains("zero nodes") && warning.contains("thing.rb")));
    let bytes = fs::read(&file).unwrap();
    assert!(ast_cache_get_from_output(&out.join("graphoxide-out"), "thing.rb", &bytes).is_none());
    assert!(
        load_manifest_from_output(&out.join("graphoxide-out"))["thing.rb"]
            .ast_hash
            .is_empty()
    );
    let healed = extract_files(&[file], Some(&out), false).unwrap();
    assert!(healed.extractions[0]
        .nodes
        .iter()
        .any(|node| node.source_file.ends_with("thing.rb")));
}

#[test]
fn test_normal_file_still_cached() {
    let temp = TempDir::new().unwrap();
    let out = temp.path().join("out");
    let file = write(temp.path(), "ok.rb", "class Bar\n  def baz; end\nend\n");
    let result = extract_files(std::slice::from_ref(&file), Some(&out), false).unwrap();
    assert!(!result.extractions[0].nodes.is_empty());
    assert!(ast_cache_get_from_output(
        &out.join("graphoxide-out"),
        "ok.rb",
        &fs::read(file).unwrap()
    )
    .is_some());
}

#[test]
fn test_no_warning_when_all_files_produce_nodes() {
    let temp = TempDir::new().unwrap();
    let out = temp.path().join("out");
    let file = write(
        temp.path(),
        "fine.rb",
        "module M\n  def self.go; end\nend\n",
    );
    let result = extract_files(&[file], Some(&out), false).unwrap();
    assert!(!result
        .warnings
        .iter()
        .any(|warning| warning.contains("zero nodes")));
}
