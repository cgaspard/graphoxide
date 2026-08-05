//! Regression coverage for #4: an `mcp.json` that is not a Claude-shaped MCP
//! configuration must not abort the repository scan, and the VS Code layout
//! must be recognised as MCP rather than treated as malformed.

use graphoxide_extract::{
    detect::DetectOptions, extract_project_with_scan_options_deferred_manifest,
};
use std::fs;

fn write(path: &std::path::Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn extract_named(name: &str, contents: &str) -> anyhow::Result<graphoxide_core::Extraction> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(name);
    write(&path, contents);
    graphoxide_extract::extract(&path)
}

fn labels(extraction: &graphoxide_core::Extraction, kind: &str) -> Vec<String> {
    extraction
        .nodes
        .iter()
        .filter(|node| {
            node.extra
                .get("metadata")
                .and_then(|metadata| metadata.get("mcp_kind"))
                .and_then(serde_json::Value::as_str)
                == Some(kind)
        })
        .map(|node| node.label.clone())
        .collect()
}

#[test]
fn mcp_json_without_a_server_map_is_indexed_instead_of_failing() {
    let extraction = extract_named("mcp.json", r#"{"someOtherKey": "value"}"#)
        .expect("a non-MCP mcp.json must not error");
    assert!(labels(&extraction, "mcp_server").is_empty());
}

#[test]
fn empty_mcp_json_is_indexed_instead_of_failing() {
    extract_named("mcp.json", "{}").expect("an empty mcp.json must not error");
}

#[test]
fn mcp_json_with_a_non_object_root_is_indexed_instead_of_failing() {
    extract_named("mcp.json", "[1, 2, 3]").expect("an array-rooted mcp.json must not error");
}

#[test]
fn vscode_mcp_json_layout_is_recognised_as_mcp() {
    let extraction = extract_named(
        "mcp.json",
        r#"{
  "inputs": [],
  "servers": {
    "graphoxide": { "type": "stdio", "command": "graphoxide", "args": ["serve"] }
  }
}"#,
    )
    .expect("the VS Code layout must extract");
    assert!(labels(&extraction, "mcp_server").contains(&"graphoxide".to_string()));
    assert!(labels(&extraction, "mcp_command").contains(&"graphoxide".to_string()));
}

#[test]
fn claude_mcp_servers_layout_still_extracts() {
    let extraction = extract_named(
        ".mcp.json",
        r#"{"mcpServers": {"legacy": {"command": "node", "args": ["dist/index.js"]}}}"#,
    )
    .expect("the mcpServers layout must extract");
    assert!(labels(&extraction, "mcp_server").contains(&"legacy".to_string()));
}

#[test]
fn a_non_mcp_mcp_json_no_longer_aborts_a_project_scan() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let output = fixture.path().join("managed/graphoxide-out");
    write(&project.join("app.py"), "def app():\n    return 1\n");
    write(
        &project.join(".vscode/mcp.json"),
        r#"{"someOtherKey": "value"}"#,
    );

    let prepared = extract_project_with_scan_options_deferred_manifest(
        &project,
        false,
        &output,
        false,
        &DetectOptions {
            output_dir: Some(output.clone()),
            ..DetectOptions::default()
        },
    )
    .expect("an unrecognised mcp.json must not abort the scan");

    assert!(prepared.warnings.is_empty(), "{:?}", prepared.warnings);
    assert!(prepared.progress.is_complete());
}

#[test]
fn a_file_that_cannot_be_extracted_is_skipped_with_a_warning() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let output = fixture.path().join("managed/graphoxide-out");
    write(&project.join("app.py"), "def app():\n    return 1\n");
    write(&project.join("tsconfig.json"), "{not valid json");

    let prepared = extract_project_with_scan_options_deferred_manifest(
        &project,
        false,
        &output,
        false,
        &DetectOptions {
            output_dir: Some(output.clone()),
            ..DetectOptions::default()
        },
    )
    .expect("one malformed file must not abort the scan");

    assert_eq!(prepared.warnings.len(), 1, "{:?}", prepared.warnings);
    assert!(
        prepared.warnings[0].contains("tsconfig.json"),
        "{:?}",
        prepared.warnings
    );
    assert!(!prepared.progress.is_complete());
    assert_eq!(prepared.progress.succeeded, prepared.progress.total - 1);
    // The healthy file still made it into the graph.
    assert!(prepared
        .extractions
        .iter()
        .any(|extraction| extraction.nodes.iter().any(|node| node.label == "app()")));
}
