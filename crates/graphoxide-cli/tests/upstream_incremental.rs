use filetime::{set_file_mtime, FileTime};
use graphoxide_core::{make_id, Extraction};
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs, path::Path, process::Command};
use tempfile::TempDir;

const LLM_ENVIRONMENT: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "MOONSHOT_API_KEY",
    "DEEPSEEK_API_KEY",
    "OLLAMA_BASE_URL",
    "AWS_PROFILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_ACCESS_KEY_ID",
];

fn run(root: &Path, arguments: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .args(arguments)
        .current_dir(root)
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT");
    for key in LLM_ENVIRONMENT {
        command.env_remove(key);
    }
    command.output().unwrap()
}

fn output_text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn docs_corpus(root: &Path) -> std::path::PathBuf {
    let docs = root.join("docs");
    write(
        &docs.join("intro.md"),
        "# Introduction\nThis doc introduces the system.",
    );
    write(
        &docs.join("api.md"),
        "# API Reference\nThe API has endpoints.",
    );
    docs
}

fn graph_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn graph_edges(value: &Value) -> &[Value] {
    value
        .get("links")
        .or_else(|| value.get("edges"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn extraction_edges<'a>(
    extraction: &'a Extraction,
    relation: &str,
    suffix: &str,
) -> Vec<&'a graphoxide_core::Edge> {
    extraction
        .edges
        .iter()
        .filter(|edge| edge.relation == relation && edge.source_file.ends_with(suffix))
        .collect()
}

#[test]
fn test_manifest_written_after_extract() {
    let fixture = TempDir::new().unwrap();
    let docs = docs_corpus(fixture.path());
    let output = run(
        fixture.path(),
        &["extract", docs.to_str().unwrap(), "--no-cluster"],
    );
    let manifest = docs.join("graphoxide-out/manifest.json");
    if output.status.success() {
        assert!(manifest.is_file(), "{}", output_text(&output));
    } else {
        assert!(!manifest.exists(), "{}", output_text(&output));
    }
}

#[test]
fn test_incremental_mode_detected_via_manifest() {
    let fixture = TempDir::new().unwrap();
    let docs = docs_corpus(fixture.path());
    let output_dir = docs.join("graphoxide-out");
    fs::create_dir_all(&output_dir).unwrap();
    fs::write(
        output_dir.join("graph.json"),
        serde_json::to_vec(&json!({"nodes": [], "links": []})).unwrap(),
    )
    .unwrap();
    fs::write(output_dir.join("manifest.json"), b"{}").unwrap();
    let output = run(
        fixture.path(),
        &["extract", docs.to_str().unwrap(), "--no-cluster"],
    );
    assert!(
        output_text(&output)
            .to_ascii_lowercase()
            .contains("incremental"),
        "{}",
        output_text(&output)
    );
}

#[test]
fn test_no_incremental_without_manifest() {
    let fixture = TempDir::new().unwrap();
    let docs = docs_corpus(fixture.path());
    let output = run(
        fixture.path(),
        &["extract", docs.to_str().unwrap(), "--no-cluster"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    assert!(!stdout.contains("incremental update"));
    assert!(!stdout.contains("incremental scan"));
}

#[test]
fn test_extract_no_cluster_incremental_noop_preserves_existing_graph() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("project");
    write(&project.join("app.py"), "def alpha():\n    return 1\n");
    let arguments = [
        "extract",
        project.to_str().unwrap(),
        "--code-only",
        "--no-cluster",
    ];
    let first = run(fixture.path(), &arguments);
    assert!(first.status.success(), "{}", output_text(&first));
    let graph_path = project.join("graphoxide-out/graph.json");
    let before = fs::read(&graph_path).unwrap();
    assert!(!graph_json(&graph_path)["nodes"]
        .as_array()
        .unwrap()
        .is_empty());

    let second = run(fixture.path(), &arguments);
    assert!(second.status.success(), "{}", output_text(&second));
    assert_eq!(fs::read(&graph_path).unwrap(), before);
}

#[test]
fn test_extract_no_cluster_incremental_changed_file_preserves_unchanged_files() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("proj");
    write(
        &project.join("src/components/ScanScreen.tsx"),
        "export function ScanScreen() {\n  return null;\n}\n",
    );
    let scan = project.join("app/add/scan.tsx");
    write(
        &scan,
        "import {ScanScreen} from '../../src/components/ScanScreen';\nexport default ScanScreen;\n",
    );
    let arguments = [
        "extract",
        project.to_str().unwrap(),
        "--code-only",
        "--no-cluster",
    ];
    let first = run(fixture.path(), &arguments);
    assert!(first.status.success(), "{}", output_text(&first));
    let graph_path = project.join("graphoxide-out/graph.json");
    let base = graph_json(&graph_path);
    let base_ids = base["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["id"].as_str())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    assert!(base_ids.is_superset(&BTreeSet::from([
        "app_add_scan".into(),
        "src_components_scanscreen".into(),
        "src_components_scanscreen_scanscreen".into(),
    ])));

    fs::write(
        &scan,
        format!("{}\n// touched\n", fs::read_to_string(&scan).unwrap()),
    )
    .unwrap();
    let second = run(fixture.path(), &arguments);
    assert!(second.status.success(), "{}", output_text(&second));
    assert!(String::from_utf8_lossy(&second.stdout)
        .to_ascii_lowercase()
        .contains("incremental scan"));
    let after = graph_json(&graph_path);
    let after_ids = after["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["id"].as_str())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    assert_eq!(after_ids, base_ids);
    assert!(graph_edges(&after).iter().any(|edge| {
        edge["relation"] == "contains"
            && edge["source"] == "src_components_scanscreen"
            && edge["target"] == "src_components_scanscreen_scanscreen"
    }));
    for edge in graph_edges(&after).iter().filter(|edge| {
        matches!(
            edge["relation"].as_str(),
            Some("imports_from" | "re_exports" | "contains" | "imports")
        )
    }) {
        assert!(after_ids.contains(edge["source"].as_str().unwrap()));
        assert!(after_ids.contains(edge["target"].as_str().unwrap()));
    }
}

#[test]
fn test_extract_no_cluster_incremental_code_only_preserves_doc_nodes() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("proj");
    let util = project.join("util.py");
    write(&util, "def alpha():\n    return 1\n");
    write(&project.join("notes.md"), "# Notes\nSome prose.\n");
    let arguments = [
        "extract",
        project.to_str().unwrap(),
        "--code-only",
        "--no-cluster",
    ];
    let first = run(fixture.path(), &arguments);
    assert!(first.status.success(), "{}", output_text(&first));
    let graph_path = project.join("graphoxide-out/graph.json");
    let mut seeded = graph_json(&graph_path);
    seeded["nodes"].as_array_mut().unwrap().push(json!({
        "id": "notes",
        "label": "notes.md",
        "type": "document",
        "source_file": "notes.md"
    }));
    fs::write(&graph_path, serde_json::to_vec(&seeded).unwrap()).unwrap();

    fs::write(
        &util,
        "def alpha():\n    return 1\n\ndef beta():\n    return 2\n",
    )
    .unwrap();
    let second = run(fixture.path(), &arguments);
    assert!(second.status.success(), "{}", output_text(&second));
    assert!(String::from_utf8_lossy(&second.stdout)
        .to_ascii_lowercase()
        .contains("incremental scan"));
    let after = graph_json(&graph_path);
    let by_id = after["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| Some((node["id"].as_str()?.to_owned(), node)))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(by_id["notes"]["source_file"], "notes.md");
    assert!(by_id.keys().any(|id| id.contains("beta")));
}

#[test]
fn test_incremental_python_relative_import_target_canonicalizes() {
    let fixture = TempDir::new().unwrap();
    let root = fs::canonicalize(fixture.path()).unwrap();
    let package = root.join("pkg");
    write(
        &package.join("b.py"),
        "class Thing:\n    def go(self):\n        return 1\n",
    );
    let importer = package.join("a.py");
    write(
        &importer,
        "from .b import Thing\n\n\ndef use():\n    return Thing().go()\n",
    );
    let full = graphoxide_extract::extract_files(
        &[importer.clone(), package.join("b.py")],
        Some(&root),
        true,
    )
    .unwrap();
    let full = flatten(full.extractions);
    let full_imports = extraction_edges(&full, "imports_from", "a.py");
    assert!(!full_imports.is_empty());
    let canonical = full_imports[0].target.clone();
    assert_eq!(canonical, "pkg_b");

    let incremental = graphoxide_extract::extract_files(&[importer], Some(&root), true).unwrap();
    let incremental = flatten(incremental.extractions);
    let imports = extraction_edges(&incremental, "imports_from", "a.py");
    assert!(!imports.is_empty());
    assert_eq!(imports[0].target, canonical);
    assert!(!imports[0].target.ends_with("_py"));
    let root_slug = make_id(&[root.to_string_lossy().as_ref()]);
    for edge in &incremental.edges {
        assert!(!edge.target.contains(&root_slug));
        assert!(!edge.extra.contains_key("target_file"));
    }
}

#[test]
fn test_incremental_md_reference_target_canonicalizes() {
    let fixture = TempDir::new().unwrap();
    let root = fs::canonicalize(fixture.path()).unwrap();
    let setup = root.join("docs/setup.md");
    write(&setup, "# Setup\nInstall the thing.\n");
    let overview = root.join("CLAUDE.md");
    write(
        &overview,
        "# Overview\nSee [setup](docs/setup.md) for install steps.\n",
    );
    let full =
        graphoxide_extract::extract_files(&[overview.clone(), setup], Some(&root), true).unwrap();
    let full = flatten(full.extractions);
    let references = extraction_edges(&full, "references", "CLAUDE.md");
    assert!(!references.is_empty());
    let canonical = references[0].target.clone();
    assert_eq!(canonical, "docs_setup");

    let incremental = graphoxide_extract::extract_files(&[overview], Some(&root), true).unwrap();
    let incremental = flatten(incremental.extractions);
    let references = extraction_edges(&incremental, "references", "CLAUDE.md");
    assert!(!references.is_empty());
    assert_eq!(references[0].target, canonical);
    let root_slug = make_id(&[root.to_string_lossy().as_ref()]);
    for edge in &incremental.edges {
        assert!(!edge.target.contains(&root_slug));
        assert!(!edge.extra.contains_key("target_file"));
    }
}

#[test]
fn test_update_prunes_a_removed_imports_edge() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("proj");
    write(&project.join("pkg/b.py"), "def helper():\n    return 1\n");
    let importer = project.join("pkg/a.py");
    write(
        &importer,
        "from pkg.b import helper\ndef use():\n    return helper()\n",
    );
    let first = run(
        fixture.path(),
        &["extract", project.to_str().unwrap(), "--no-cluster"],
    );
    assert!(first.status.success(), "{}", output_text(&first));
    let graph_path = project.join("graphoxide-out/graph.json");
    let before = graph_json(&graph_path);
    assert!(graph_edges(&before).iter().any(|edge| {
        matches!(edge["relation"].as_str(), Some("imports" | "imports_from"))
            && edge["source_file"]
                .as_str()
                .is_some_and(|source| source.ends_with("a.py"))
    }));

    fs::write(&importer, "def use():\n    return 1\n").unwrap();
    let second = run(fixture.path(), &["update", project.to_str().unwrap()]);
    assert!(second.status.success(), "{}", output_text(&second));
    let after = graph_json(&graph_path);
    assert!(!graph_edges(&after).iter().any(|edge| {
        matches!(edge["relation"].as_str(), Some("imports" | "imports_from"))
            && edge["source_file"]
                .as_str()
                .is_some_and(|source| source.ends_with("a.py"))
    }));
}

#[test]
fn test_update_json_reports_no_change_then_one_file_incremental_work() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("project");
    let changed = project.join("a.py");
    write(&changed, "def alpha():\n    return 1\n");
    write(&project.join("b.py"), "def beta():\n    return 2\n");
    let first = run(
        fixture.path(),
        &[
            "extract",
            project.to_str().unwrap(),
            "--code-only",
            "--no-cluster",
        ],
    );
    assert!(first.status.success(), "{}", output_text(&first));

    let unchanged = run(
        fixture.path(),
        &[
            "update",
            project.to_str().unwrap(),
            "--no-cluster",
            "--json",
        ],
    );
    assert!(unchanged.status.success(), "{}", output_text(&unchanged));
    let unchanged: Value = serde_json::from_slice(&unchanged.stdout).unwrap();
    assert_eq!(unchanged["operation"], "update");
    assert_eq!(unchanged["mode"], "incremental");
    assert_eq!(unchanged["status"], "unchanged");
    assert_eq!(unchanged["files"]["detected"], 2);
    assert_eq!(unchanged["files"]["processed"], 0);
    assert_eq!(unchanged["files"]["changed"], 0);
    assert_eq!(unchanged["files"]["unchanged"], 2);
    assert!(unchanged["elapsed_ms"].as_u64().is_some());
    assert!(unchanged["stages_ms"]["detect"].as_u64().is_some());

    let previous = FileTime::from_last_modification_time(&fs::metadata(&changed).unwrap());
    fs::write(
        &changed,
        "def alpha():\n    return 1\n\ndef added():\n    return alpha()\n",
    )
    .unwrap();
    set_file_mtime(
        &changed,
        FileTime::from_unix_time(previous.unix_seconds().saturating_add(2), 0),
    )
    .unwrap();

    let updated = run(
        fixture.path(),
        &[
            "update",
            project.to_str().unwrap(),
            "--no-cluster",
            "--json",
        ],
    );
    assert!(updated.status.success(), "{}", output_text(&updated));
    let updated: Value = serde_json::from_slice(&updated.stdout).unwrap();
    assert_eq!(updated["operation"], "update");
    assert_eq!(updated["mode"], "incremental");
    assert_eq!(updated["status"], "rebuilt");
    assert_eq!(updated["files"]["detected"], 2);
    assert_eq!(updated["files"]["processed"], 1);
    assert_eq!(updated["files"]["changed"], 1);
    assert_eq!(updated["files"]["unchanged"], 1);
    assert!(updated["graph"]["nodes"].as_u64().unwrap() > 0);
}

fn flatten(extractions: Vec<Extraction>) -> Extraction {
    let mut output = Extraction::default();
    for extraction in extractions {
        output.nodes.extend(extraction.nodes);
        output.edges.extend(extraction.edges);
        output.hyperedges.extend(extraction.hyperedges);
    }
    output
}
