use graphoxide_cli::build_guard::{commit_build, BuildArtifact, BuildCommitOutcome, BuildProgress};
use graphoxide_core::{Extraction, KnowledgeGraph, Node};
use graphoxide_graph::merge_raw_extraction;
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path, process::Command};
use tempfile::TempDir;

fn node(id: &str, source_file: &str) -> Node {
    Node {
        id: id.into(),
        label: id.into(),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: Some("L1".into()),
        community: None,
        extra: BTreeMap::from([("_origin".into(), "ast".into())]),
    }
}

fn graph(count: usize) -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: (0..count)
            .map(|index| node(&format!("keep{index}"), &format!("keep{index}.py")))
            .collect(),
        ..KnowledgeGraph::default()
    }
}

fn raw(id: &str, source_file: &str) -> Vec<Extraction> {
    vec![Extraction {
        nodes: vec![node(id, source_file)],
        ..Extraction::default()
    }]
}

fn seed(path: &Path, count: usize) {
    graphoxide_core::write_graph_atomic(path, &graph(count), true).unwrap();
}

fn write_manifest(path: &Path) -> anyhow::Result<()> {
    graphoxide_core::write_json_atomic(path, &json!({"completed": true}), true)
}

fn graph_node_count(path: &Path) -> usize {
    graphoxide_core::read_graph(path).unwrap().nodes.len()
}

fn output_text(output: &std::process::Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn test_partial_extraction_refuses_to_shrink_existing_graph() {
    let fixture = TempDir::new().unwrap();
    let graph_path = fixture.path().join("out/graph.json");
    let manifest_path = fixture.path().join("out/manifest.json");
    seed(&graph_path, 5);
    let candidate = graph(1);
    let outcome = commit_build(
        &graph_path,
        BuildArtifact::Graph(&candidate),
        BuildProgress::new(3, 1).unwrap(),
        false,
        || write_manifest(&manifest_path),
    )
    .unwrap();
    assert_eq!(outcome, BuildCommitOutcome::RefusedShrink);
    assert!(outcome.to_string().contains("Refusing to overwrite"));
    assert_eq!(graph_node_count(&graph_path), 5);
    assert!(!manifest_path.exists());
}

#[test]
fn test_partial_extraction_writes_when_not_shrinking() {
    let fixture = TempDir::new().unwrap();
    let graph_path = fixture.path().join("out/graph.json");
    let manifest_path = fixture.path().join("out/manifest.json");
    seed(&graph_path, 1);
    let candidate = graph(2);
    let outcome = commit_build(
        &graph_path,
        BuildArtifact::Graph(&candidate),
        BuildProgress::new(3, 1).unwrap(),
        false,
        || write_manifest(&manifest_path),
    )
    .unwrap();
    assert_eq!(outcome, BuildCommitOutcome::Written);
    assert_eq!(graph_node_count(&graph_path), 2);
    assert!(manifest_path.is_file());
}

#[test]
fn test_allow_partial_forces_write_despite_incomplete() {
    let fixture = TempDir::new().unwrap();
    let graph_path = fixture.path().join("out/graph.json");
    seed(&graph_path, 5);
    let candidate = graph(1);
    let outcome = commit_build(
        &graph_path,
        BuildArtifact::Graph(&candidate),
        BuildProgress::new(3, 1).unwrap(),
        true,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(outcome, BuildCommitOutcome::Written);
    assert_eq!(graph_node_count(&graph_path), 1);
}

#[test]
fn test_complete_extraction_keeps_force_write() {
    let fixture = TempDir::new().unwrap();
    let graph_path = fixture.path().join("out/graph.json");
    seed(&graph_path, 5);
    let candidate = graph(1);
    let outcome = commit_build(
        &graph_path,
        BuildArtifact::Graph(&candidate),
        BuildProgress::new(1, 1).unwrap(),
        false,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(outcome, BuildCommitOutcome::Written);
    assert_eq!(graph_node_count(&graph_path), 1);
}

#[test]
fn test_no_cluster_incomplete_build_refuses_to_shrink() {
    let fixture = TempDir::new().unwrap();
    let graph_path = fixture.path().join("out/graph.json");
    seed(&graph_path, 5);
    let candidate = raw("s1", "README.md");
    let outcome = commit_build(
        &graph_path,
        BuildArtifact::Raw(&candidate),
        BuildProgress::new(3, 1).unwrap(),
        false,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(outcome, BuildCommitOutcome::RefusedShrink);
    assert_eq!(graph_node_count(&graph_path), 5);
}

#[test]
fn test_no_cluster_incremental_incomplete_build_carries_existing_nodes() {
    let fixture = TempDir::new().unwrap();
    let graph_path = fixture.path().join("out/graph.json");
    seed(&graph_path, 5);
    let fresh = raw("s1", "README.md").pop().unwrap();
    let merged = merge_raw_extraction(&fresh, &graph_path, &[], Some(fixture.path())).unwrap();
    let candidate = [merged];
    let outcome = commit_build(
        &graph_path,
        BuildArtifact::Raw(&candidate),
        BuildProgress::new(3, 1).unwrap(),
        false,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(outcome, BuildCommitOutcome::Written);
    let ids = graphoxide_core::read_graph(&graph_path)
        .unwrap()
        .nodes
        .into_iter()
        .map(|node| node.id)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(ids.contains("s1"));
    for index in 0..5 {
        assert!(ids.contains(&format!("keep{index}")));
    }
}

#[test]
fn test_no_cluster_allow_partial_overwrites() {
    let fixture = TempDir::new().unwrap();
    let graph_path = fixture.path().join("out/graph.json");
    seed(&graph_path, 5);
    let candidate = raw("s1", "README.md");
    let outcome = commit_build(
        &graph_path,
        BuildArtifact::Raw(&candidate),
        BuildProgress::new(3, 1).unwrap(),
        true,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(outcome, BuildCommitOutcome::Written);
    assert_eq!(graph_node_count(&graph_path), 1);
}

#[test]
fn test_no_cluster_incomplete_build_fails_closed_on_malformed_existing_graph() {
    let fixture = TempDir::new().unwrap();
    let graph_path = fixture.path().join("out/graph.json");
    let manifest_path = fixture.path().join("out/manifest.json");
    fs::create_dir_all(graph_path.parent().unwrap()).unwrap();
    fs::write(&graph_path, "{corrupt json").unwrap();
    let candidate = raw("s1", "README.md");
    let error = commit_build(
        &graph_path,
        BuildArtifact::Raw(&candidate),
        BuildProgress::new(3, 1).unwrap(),
        false,
        || write_manifest(&manifest_path),
    )
    .unwrap_err();
    assert!(error.to_string().contains("unreadable"));
    assert_eq!(fs::read_to_string(&graph_path).unwrap(), "{corrupt json");
    assert!(!manifest_path.exists());
}

#[test]
fn test_no_cluster_incremental_malformed_existing_graph_refuses_merge() {
    let fixture = TempDir::new().unwrap();
    let graph_path = fixture.path().join("out/graph.json");
    fs::create_dir_all(graph_path.parent().unwrap()).unwrap();
    fs::write(&graph_path, "{corrupt json").unwrap();
    let fresh = raw("s1", "README.md").pop().unwrap();
    let error = merge_raw_extraction(&fresh, &graph_path, &[], Some(fixture.path())).unwrap_err();
    assert!(error.to_string().contains("Cannot read"));
    assert_eq!(fs::read_to_string(&graph_path).unwrap(), "{corrupt json");
}

#[test]
fn build_progress_rejects_impossible_success_counts() {
    assert!(BuildProgress::new(1, 2).is_err());
}

#[test]
#[cfg(unix)]
fn cli_walk_error_activates_partial_shrink_guard_even_with_force_rescan() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("project");
    let locked = project.join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(project.join("app.py"), "def app():\n    return 1\n").unwrap();
    fs::write(locked.join("hidden.py"), "def hidden():\n    return 2\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o0)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let output_directory = project.join("graphoxide-out");
    let graph_path = output_directory.join("graph.json");
    let manifest_path = output_directory.join("manifest.json");
    seed(&graph_path, 5);
    let previous_manifest = br#"{"old.py":{"mtime":1.0,"ast_hash":"old","semantic_hash":""}}"#;
    fs::write(&manifest_path, previous_manifest).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args([
            "extract",
            project.to_str().unwrap(),
            "--code-only",
            "--no-cluster",
            "--force",
        ])
        .output()
        .unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("Refusing to overwrite"),
        "{}",
        output_text(&output)
    );
    assert_eq!(graph_node_count(&graph_path), 5);
    assert_eq!(fs::read(&manifest_path).unwrap(), previous_manifest);
}

#[test]
#[cfg(unix)]
fn cli_allow_partial_overrides_real_walk_error_and_commits_manifest() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("project");
    let locked = project.join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(project.join("app.py"), "def app():\n    return 1\n").unwrap();
    fs::write(locked.join("hidden.py"), "def hidden():\n    return 2\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o0)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let output_directory = project.join("graphoxide-out");
    let graph_path = output_directory.join("graph.json");
    let manifest_path = output_directory.join("manifest.json");
    seed(&graph_path, 5);
    fs::write(
        &manifest_path,
        br#"{"old.py":{"mtime":1.0,"ast_hash":"old","semantic_hash":""}}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args([
            "extract",
            project.to_str().unwrap(),
            "--code-only",
            "--no-cluster",
            "--force",
            "--allow-partial",
        ])
        .output()
        .unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(output.status.success(), "{}", output_text(&output));
    assert!(graph_node_count(&graph_path) < 5);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert!(manifest.get("app.py").is_some());
    assert!(manifest.get("old.py").is_none());
}
