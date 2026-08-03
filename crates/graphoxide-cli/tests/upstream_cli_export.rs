//! Subprocess-level port of upstream `tests/test_cli_export.py` plus the two
//! CLI cases from `tests/test_callflow_html.py`.

use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path, process::Command};
use tempfile::tempdir;

fn run(root: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(arguments)
        .current_dir(root)
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT")
        .output()
        .unwrap()
}

fn run_with_out(root: &Path, arguments: &[&str], output_directory: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(arguments)
        .current_dir(root)
        .env_remove("GRAPHOXIDE_OUT")
        .env("GRAPHIFY_OUT", output_directory)
        .output()
        .unwrap()
}

fn text(output: &[u8]) -> String {
    String::from_utf8_lossy(output).into_owned()
}

fn make_graph(root: &Path) -> std::path::PathBuf {
    let output = root.join("graphoxide-out");
    fs::create_dir_all(&output).unwrap();
    let make_node = |id: &str, label: &str, source: &str, community: i64| Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source.into(),
        source_location: None,
        community: Some(community),
        extra: BTreeMap::from([(
            "community_name".into(),
            json!(format!("Community {community}")),
        )]),
    };
    let make_edge = |source: &str, target: &str, relation: &str| Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: String::new(),
        extra: BTreeMap::new(),
    };
    let graph = KnowledgeGraph {
        nodes: vec![
            make_node("n_transformer", "Transformer", "model.py", 0),
            make_node("n_attention", "MultiHeadAttention", "model.py", 0),
            make_node("n_layernorm", "LayerNorm", "model.py", 1),
            make_node("n_concept", "attention mechanism", "paper.md", 1),
        ],
        links: vec![
            make_edge("n_transformer", "n_attention", "contains"),
            make_edge("n_transformer", "n_layernorm", "contains"),
            make_edge("n_attention", "n_concept", "implements"),
        ],
        ..Default::default()
    };
    graphoxide_core::write_graph_atomic(output.join("graph.json"), &graph, true).unwrap();
    graphoxide_core::write_json_atomic(
        output.join(".graphoxide_analysis.json"),
        &json!({"communities":{"0":["n_transformer","n_attention"],"1":["n_layernorm","n_concept"]},"cohesion":{"0":0.5,"1":0.5},"gods":[],"surprises":[],"questions":[]}),
        true,
    ).unwrap();
    graphoxide_core::write_json_atomic(
        output.join(".graphoxide_labels.json"),
        &json!({"0":"Community 0","1":"Community 1"}),
        true,
    )
    .unwrap();
    fs::write(
        output.join("GRAPH_REPORT.md"),
        "# Graph Report\n\n## Summary\n- 4 nodes · 3 edges · 2 communities detected\n\n## God Nodes\n1. `Transformer` - 2 edges\n",
    ).unwrap();
    output
}

#[test]
fn test_export_html_creates_file() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    let result = run(tmp.path(), &["export", "html"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(
        tmp.path()
            .join("graphoxide-out/graph.html")
            .metadata()
            .unwrap()
            .len()
            > 0
    );
}

#[test]
fn test_export_html_no_viz_removes_file() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    fs::write(output.join("graph.html"), "<html/>").unwrap();
    let result = run(tmp.path(), &["export", "html", "--no-viz"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(!output.join("graph.html").exists());
}

#[test]
fn test_export_html_error_without_graph() {
    let tmp = tempdir().unwrap();
    assert!(!run(tmp.path(), &["export", "html"]).status.success());
}

#[test]
fn test_export_obsidian_creates_vault() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    let result = run(tmp.path(), &["export", "obsidian"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    let vault = tmp.path().join("graphoxide-out/obsidian");
    assert!(vault.is_dir());
    assert!(fs::read_dir(vault)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("md")));
}

#[test]
fn test_export_obsidian_custom_dir() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    let custom = tmp.path().join("my-vault");
    let result = run(
        tmp.path(),
        &["export", "obsidian", "--dir", custom.to_str().unwrap()],
    );
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(custom.is_dir());
    assert!(fs::read_dir(custom)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("md")));
}

#[test]
fn test_export_wiki_creates_articles() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    let result = run(tmp.path(), &["export", "wiki"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(tmp.path().join("graphoxide-out/wiki/index.md").is_file());
}

#[test]
fn test_export_wiki_accepts_edges_only_graph_json() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    let path = output.join("graph.json");
    let mut graph: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    graph["edges"] = graph["links"].take();
    graph.as_object_mut().unwrap().remove("links");
    fs::write(&path, serde_json::to_vec(&graph).unwrap()).unwrap();
    let result = run(tmp.path(), &["export", "wiki"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(output.join("wiki/index.md").is_file());
}

#[test]
fn test_export_graphml_creates_file() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    let result = run(tmp.path(), &["export", "graphml"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    let content = fs::read_to_string(tmp.path().join("graphoxide-out/graph.graphml")).unwrap();
    assert!(content.contains("<graphml"));
}

#[test]
fn test_export_neo4j_creates_cypher() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    let result = run(tmp.path(), &["export", "neo4j"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(
        fs::read_to_string(tmp.path().join("graphoxide-out/cypher.txt"))
            .unwrap()
            .contains("MERGE")
    );
}

#[test]
fn test_export_falkordb_creates_cypher() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    let result = run(tmp.path(), &["export", "falkordb"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(
        fs::read_to_string(tmp.path().join("graphoxide-out/cypher.txt"))
            .unwrap()
            .contains("MERGE")
    );
}

#[test]
fn test_query_returns_output() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    let result = run(tmp.path(), &["query", "Transformer"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(!result.stdout.is_empty());
}

#[test]
fn test_query_dfs_flag() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    assert!(run(tmp.path(), &["query", "Transformer", "--dfs"])
        .status
        .success());
}

#[test]
fn test_query_budget_flag() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    assert!(
        run(tmp.path(), &["query", "Transformer", "--budget", "500"])
            .status
            .success()
    );
}

#[test]
fn test_query_missing_graph_fails() {
    let tmp = tempdir().unwrap();
    assert!(!run(tmp.path(), &["query", "anything"]).status.success());
}

#[test]
fn test_query_uses_graphify_out_env() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    fs::rename(output, tmp.path().join("custom-graph")).unwrap();
    let result = run_with_out(tmp.path(), &["query", "Transformer"], "custom-graph");
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(!result.stdout.is_empty());
}

#[test]
fn test_extract_writes_to_graphify_out_env() {
    let tmp = tempdir().unwrap();
    fs::write(
        tmp.path().join("m.py"),
        "def a():\n    return b()\n\ndef b():\n    return 1\n",
    )
    .unwrap();
    let result = run_with_out(tmp.path(), &["extract", "."], "custom-out");
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(tmp.path().join("custom-out/graph.json").is_file());
    assert!(tmp.path().join("custom-out/manifest.json").is_file());
    assert!(!tmp.path().join("graphoxide-out").exists());
    let manifest: Value =
        serde_json::from_slice(&fs::read(tmp.path().join("custom-out/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(
        manifest.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["m.py"]
    );
}

#[test]
fn test_path_runs_without_error() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    assert!(run(tmp.path(), &["path", "Transformer", "LayerNorm"])
        .status
        .success());
}

#[test]
fn test_path_missing_graph_fails() {
    let tmp = tempdir().unwrap();
    assert!(!run(tmp.path(), &["path", "a", "b"]).status.success());
}

#[test]
fn test_path_uses_graphify_out_env() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    fs::rename(output, tmp.path().join("custom-graph")).unwrap();
    assert!(run_with_out(
        tmp.path(),
        &["path", "Transformer", "LayerNorm"],
        "custom-graph"
    )
    .status
    .success());
}

#[test]
fn test_explain_runs_without_error() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    assert!(run(tmp.path(), &["explain", "Transformer"])
        .status
        .success());
}

#[test]
fn test_explain_missing_graph_fails() {
    let tmp = tempdir().unwrap();
    assert!(!run(tmp.path(), &["explain", "anything"]).status.success());
}

#[test]
fn test_explain_uses_graphify_out_env() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    fs::rename(output, tmp.path().join("custom-graph")).unwrap();
    assert!(
        run_with_out(tmp.path(), &["explain", "Transformer"], "custom-graph")
            .status
            .success()
    );
}

#[test]
fn test_export_unknown_format_fails() {
    let tmp = tempdir().unwrap();
    assert!(!run(tmp.path(), &["export", "pdf"]).status.success());
}

#[test]
fn test_update_no_cluster_writes_raw_graph() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("sample.py"), "def f():\n    return 1\n").unwrap();
    let result = run(tmp.path(), &["update", ".", "--no-cluster"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    let graph: Value =
        serde_json::from_slice(&fs::read(tmp.path().join("graphoxide-out/graph.json")).unwrap())
            .unwrap();
    assert!(graph["nodes"].is_array() && graph["links"].is_array());
    assert!(graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|node| node.get("community").is_none()));
}

#[test]
fn test_cluster_only_creates_output_dir_when_missing() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    let backup = tmp.path().join("backup");
    fs::create_dir(&backup).unwrap();
    fs::copy(output.join("graph.json"), backup.join("graph.json")).unwrap();
    fs::remove_dir_all(output).unwrap();
    let result = run(
        tmp.path(),
        &[
            "cluster-only",
            ".",
            "--graph",
            backup.join("graph.json").to_str().unwrap(),
            "--no-viz",
        ],
    );
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(tmp.path().join("graphoxide-out/GRAPH_REPORT.md").is_file());
}

#[test]
fn test_cluster_only_graph_in_graphify_out_writes_beside_it() {
    let tmp = tempdir().unwrap();
    let project = tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    let output = make_graph(&project);
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let result = run(
        &elsewhere,
        &[
            "cluster-only",
            ".",
            "--graph",
            output.join("graph.json").to_str().unwrap(),
            "--no-viz",
            "--no-label",
        ],
    );
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(output.join("GRAPH_REPORT.md").is_file());
    assert!(!elsewhere.join("graphoxide-out").exists());
}

#[test]
fn test_extract_out_does_not_pollute_corpus() {
    let tmp = tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    fs::create_dir(&corpus).unwrap();
    fs::write(corpus.join("a.py"), "def main():\n    return 1\n").unwrap();
    let scratch = tmp.path().join("scratch");
    let result = run(
        tmp.path(),
        &[
            "extract",
            corpus.to_str().unwrap(),
            "--out",
            scratch.to_str().unwrap(),
            "--no-cluster",
            "--code-only",
        ],
    );
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(scratch.join("graphoxide-out/graph.json").is_file());
    assert!(!corpus.join("graphoxide-out").exists());
}

#[test]
fn test_cluster_only_persists_analysis_sidecar() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    fs::remove_file(output.join(".graphoxide_analysis.json")).unwrap();
    let result = run(tmp.path(), &["cluster-only", ".", "--no-viz"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    let analysis: Value =
        serde_json::from_slice(&fs::read(output.join(".graphoxide_analysis.json")).unwrap())
            .unwrap();
    assert!(analysis["communities"]
        .as_object()
        .is_some_and(|value| !value.is_empty()));
    assert!(analysis["cohesion"].is_object());
    assert!(analysis.get("gods").is_some());
    assert!(analysis.get("surprises").is_some());
    assert!(analysis.get("questions").is_some());
    let graph: Value =
        serde_json::from_slice(&fs::read(output.join("graph.json")).unwrap()).unwrap();
    let graph_ids: std::collections::BTreeSet<_> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["community"].as_i64())
        .map(|id| id.to_string())
        .collect();
    let analysis_ids: std::collections::BTreeSet<_> = analysis["communities"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(graph_ids, analysis_ids);
}

#[test]
fn test_cluster_only_remaps_labels_to_previous_cids() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    let path = output.join("graph.json");
    let mut graph: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let nodes = graph["nodes"].as_array_mut().unwrap();
    let half = nodes.len() / 2;
    for (index, node) in nodes.iter_mut().enumerate() {
        node["community"] = json!(if index < half { 4242 } else { 9999 });
        node.as_object_mut().unwrap().remove("community_name");
    }
    fs::write(&path, serde_json::to_vec(&graph).unwrap()).unwrap();
    fs::write(
        output.join(".graphoxide_labels.json"),
        r#"{"4242":"First Group","9999":"Second Group"}"#,
    )
    .unwrap();
    let result = run(tmp.path(), &["cluster-only", ".", "--no-viz"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    let final_graph: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let final_labels: Value =
        serde_json::from_slice(&fs::read(output.join(".graphoxide_labels.json")).unwrap()).unwrap();
    let graph_ids: std::collections::BTreeSet<_> = final_graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["community"].as_i64())
        .collect();
    let label_ids: std::collections::BTreeSet<_> = final_labels
        .as_object()
        .unwrap()
        .keys()
        .filter_map(|key| key.parse::<i64>().ok())
        .collect();
    assert!(!graph_ids
        .intersection(&label_ids)
        .collect::<Vec<_>>()
        .is_empty());
    assert!(label_ids.contains(&4242) || label_ids.contains(&9999));
}

#[test]
fn test_export_html_falls_back_to_node_community_attribute() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    fs::remove_file(output.join(".graphoxide_analysis.json")).unwrap();
    let result = run(tmp.path(), &["export", "html"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(output.join("graph.html").metadata().unwrap().len() > 0);
    assert!(!text(&result.stdout).contains("Single community"));
}

#[test]
fn test_export_html_fallback_recovers_multiple_communities() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    let graph: Value =
        serde_json::from_slice(&fs::read(output.join("graph.json")).unwrap()).unwrap();
    let expected = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["community"].as_i64())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    assert!(expected > 1);
    fs::remove_file(output.join(".graphoxide_analysis.json")).unwrap();
    let result = run(tmp.path(), &["export", "html"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(output.join("graph.html").is_file());
}

#[test]
fn test_export_html_no_community_data_at_all_still_succeeds() {
    let tmp = tempdir().unwrap();
    let output = make_graph(tmp.path());
    let path = output.join("graph.json");
    let mut graph: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for node in graph["nodes"].as_array_mut().unwrap() {
        node.as_object_mut().unwrap().remove("community");
    }
    fs::write(path, serde_json::to_vec(&graph).unwrap()).unwrap();
    fs::remove_file(output.join(".graphoxide_analysis.json")).unwrap();
    assert!(run(tmp.path(), &["export", "html"]).status.success());
}

#[test]
fn test_graph_json_node_ids_are_portable_across_checkout_paths() {
    let tmp = tempdir().unwrap();
    fn build(root: &Path) -> Vec<String> {
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(root.join("pkg/mod.py"), "def f(): return 1\n").unwrap();
        fs::write(
            root.join("pkg/app.py"),
            "from pkg.mod import f\ndef g(): return f()\n",
        )
        .unwrap();
        let result = run(root, &["extract", ".", "--code-only", "--no-cluster"]);
        assert!(result.status.success(), "{}", text(&result.stderr));
        let graph: Value =
            serde_json::from_slice(&fs::read(root.join("graphoxide-out/graph.json")).unwrap())
                .unwrap();
        let mut ids: Vec<_> = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|node| node["id"].as_str())
            .map(str::to_owned)
            .collect();
        ids.sort();
        ids
    }
    let first = build(&tmp.path().join("alice_home/proj"));
    let second = build(&tmp.path().join("bob_elsewhere/checkout/proj"));
    assert_eq!(first, second);
    let leak = [
        "alice_home",
        "bob_elsewhere",
        "checkout",
        "tmp",
        "private",
        "users",
        "home",
        "var",
    ];
    assert!(!first
        .iter()
        .any(|id| id.split('_').any(|part| leak.contains(&part))));
}

#[test]
fn test_export_callflow_html_cli_creates_file() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    let result = run(
        tmp.path(),
        &[
            "export",
            "callflow-html",
            "--output",
            "graphoxide-out/from-cli.html",
            "--max-sections",
            "4",
        ],
    );
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(tmp.path().join("graphoxide-out/from-cli.html").is_file());
    assert!(text(&result.stdout).contains("callflow HTML written"));
}

#[test]
fn test_export_callflow_html_cli_accepts_positional_graph_path() {
    let tmp = tempdir().unwrap();
    make_graph(tmp.path());
    let external_root = tmp.path().join("GitNexus");
    fs::create_dir(&external_root).unwrap();
    let external = make_graph(&external_root);
    let graph_path = external.join("graph.json");
    let mut graph: Value = serde_json::from_slice(&fs::read(&graph_path).unwrap()).unwrap();
    graph["nodes"].as_array_mut().unwrap()[0]["label"] = json!("ExternalOnly");
    fs::write(&graph_path, serde_json::to_vec(&graph).unwrap()).unwrap();
    fs::write(
        external.join("GRAPH_REPORT.md"),
        "## God Nodes\n1. `ExternalGod` - 1 edges\n",
    )
    .unwrap();
    let result = run(
        tmp.path(),
        &[
            "export",
            "callflow-html",
            graph_path.to_str().unwrap(),
            "--output",
            "positional.html",
            "--max-sections",
            "4",
        ],
    );
    assert!(result.status.success(), "{}", text(&result.stderr));
    let html = fs::read_to_string(tmp.path().join("positional.html")).unwrap();
    assert!(html.contains("ExternalOnly"));
    assert!(html.contains("ExternalGod"));
    assert!(!html.contains("ApiClient"));
    assert!(!html.contains("Transformer"));
}
