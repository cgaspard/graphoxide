use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};
use tempfile::tempdir;

fn diagnostic_fixture() -> Value {
    json!({
        "nodes": [
            {"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"},
            {"id": "b", "label": "B", "file_type": "code", "source_file": "b.py"},
            {"id": "c", "label": "C", "file_type": "code", "source_file": "c.py"}
        ],
        "edges": [
            {"source": "a", "target": "b", "relation": "calls", "confidence": "EXTRACTED", "source_file": "a.py", "source_location": "L1", "context": "call"},
            {"source": "a", "target": "b", "relation": "imports", "confidence": "EXTRACTED", "source_file": "a.py", "source_location": "L2", "context": "import"},
            {"source": "a", "target": "b", "relation": "calls", "confidence": "INFERRED", "source_file": "a.py", "source_location": "L3", "context": "call"},
            {"source": "a", "target": "b", "relation": "calls", "confidence": "EXTRACTED", "source_file": "a.py", "source_location": "L1", "context": "call"},
            {"source": "a", "target": "missing", "relation": "calls", "confidence": "EXTRACTED", "source_file": "a.py"},
            {"source": "a", "relation": "calls", "confidence": "EXTRACTED", "source_file": "a.py"},
            {"source": "c", "target": "c", "relation": "references", "confidence": "EXTRACTED", "source_file": "c.py"}
        ]
    })
}

fn write_fixture(root: &Path, directed: Option<bool>) -> std::path::PathBuf {
    let path = root.join("graph.json");
    let mut fixture = diagnostic_fixture();
    if let Some(directed) = directed {
        fixture["directed"] = json!(directed);
    }
    fs::write(&path, fixture.to_string()).unwrap();
    path
}

fn run(root: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(arguments)
        .current_dir(root)
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT")
        .output()
        .unwrap()
}

fn stdout(result: &std::process::Output) -> String {
    String::from_utf8_lossy(&result.stdout).into_owned()
}

fn stderr(result: &std::process::Output) -> String {
    String::from_utf8_lossy(&result.stderr).into_owned()
}

#[test]
fn test_diagnose_multigraph_cli_human_output() {
    let temporary = tempdir().unwrap();
    let graph = write_fixture(temporary.path(), None);
    let result = run(
        temporary.path(),
        &["diagnose", "multigraph", "--graph", graph.to_str().unwrap()],
    );
    assert!(result.status.success(), "{}", stderr(&result));
    let output = stdout(&result);
    assert!(output.contains("[graphoxide] MultiDiGraph edge-collapse diagnostic"));
    assert!(output.contains("raw_edges: 7"));
    assert!(output.contains("effective_directed: True"));
    assert!(output.contains("directed_same_endpoint_collapsed_edges: 3"));
}

#[test]
fn test_diagnose_multigraph_cli_undirected_override() {
    let temporary = tempdir().unwrap();
    let graph = write_fixture(temporary.path(), Some(true));
    let result = run(
        temporary.path(),
        &[
            "diagnose",
            "multigraph",
            "--graph",
            graph.to_str().unwrap(),
            "--undirected",
        ],
    );
    assert!(result.status.success(), "{}", stderr(&result));
    let output = stdout(&result);
    assert!(output.contains("effective_directed: False"));
    assert!(output.contains("post_build_graph_type: Graph"));
}

#[test]
fn test_diagnose_multigraph_cli_max_examples_zero() {
    let temporary = tempdir().unwrap();
    let graph = write_fixture(temporary.path(), None);
    let result = run(
        temporary.path(),
        &[
            "diagnose",
            "multigraph",
            "--graph",
            graph.to_str().unwrap(),
            "--max-examples",
            "0",
        ],
    );
    assert!(result.status.success(), "{}", stderr(&result));
    assert!(!stdout(&result).contains("\nexamples:"));
}

#[test]
fn test_diagnose_multigraph_cli_json_output() {
    let temporary = tempdir().unwrap();
    let graph = write_fixture(temporary.path(), None);
    let result = run(
        temporary.path(),
        &[
            "diagnose",
            "multigraph",
            "--graph",
            graph.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(result.status.success(), "{}", stderr(&result));
    let payload: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(payload["schema_version"], 1);
    assert_eq!(
        payload["summary"]["directed_same_endpoint_collapsed_edges"],
        3
    );
}

fn assert_usage_error(arguments: &[&str], expected: &str) {
    let temporary = tempdir().unwrap();
    let result = run(temporary.path(), arguments);
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains(expected),
        "expected {expected:?} in {:?}",
        stderr(&result)
    );
}

#[test]
fn test_diagnose_multigraph_cli_usage_error_no_subcommand() {
    assert_usage_error(&["diagnose"], "Usage: graphoxide diagnose multigraph");
}

#[test]
fn test_diagnose_multigraph_cli_usage_error_wrong_subcommand() {
    assert_usage_error(
        &["diagnose", "wrong"],
        "Usage: graphoxide diagnose multigraph",
    );
}

#[test]
fn test_diagnose_multigraph_cli_usage_error_graph_requires_path() {
    assert_usage_error(
        &["diagnose", "multigraph", "--graph"],
        "error: --graph requires a path",
    );
}

#[test]
fn test_diagnose_multigraph_cli_usage_error_max_examples_requires_value() {
    assert_usage_error(
        &["diagnose", "multigraph", "--max-examples"],
        "error: --max-examples requires an integer",
    );
}

#[test]
fn test_diagnose_multigraph_cli_usage_error_max_examples_requires_integer() {
    assert_usage_error(
        &["diagnose", "multigraph", "--max-examples", "many"],
        "error: --max-examples requires an integer",
    );
}

#[test]
fn test_diagnose_multigraph_cli_usage_error_max_examples_nonnegative() {
    assert_usage_error(
        &["diagnose", "multigraph", "--max-examples", "-1"],
        "error: --max-examples must be >= 0",
    );
}

#[test]
fn test_diagnose_multigraph_cli_usage_error_unknown_option() {
    assert_usage_error(
        &["diagnose", "multigraph", "--unknown"],
        "error: unknown diagnose option --unknown",
    );
}

#[test]
fn test_diagnose_multigraph_cli_rejects_conflicting_direction_flags() {
    let temporary = tempdir().unwrap();
    let graph = write_fixture(temporary.path(), None);
    let result = run(
        temporary.path(),
        &[
            "diagnose",
            "multigraph",
            "--graph",
            graph.to_str().unwrap(),
            "--directed",
            "--undirected",
        ],
    );
    assert!(!result.status.success());
    assert!(stderr(&result).contains("--directed and --undirected are mutually exclusive"));
}
