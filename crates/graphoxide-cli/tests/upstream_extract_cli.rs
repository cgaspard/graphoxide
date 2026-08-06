//! Executable port of upstream `tests/test_extract_cli.py`.
//!
//! Graphoxide keeps structural document extraction offline. Tests whose
//! upstream assertions depend on a hosted LLM exercise the equivalent
//! semantic-pipeline/cache contract directly, while CLI lifecycle behavior is
//! still covered through the released binary.

use graphoxide_cli::build_guard::{commit_build, BuildArtifact, BuildProgress};
use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_extract::{
    cache::{check_semantic_cache, save_semantic_cache, SemanticCacheOptions},
    detect::{save_manifest, DetectedFiles, ManifestKind, SaveManifestOptions},
    semantic_pipeline::{
        extract_corpus, partial_source_files, stamped_manifest_files, SemanticChunkResult,
        SemanticCorpusOptions,
    },
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};
use tempfile::TempDir;

const BACKEND_ENVIRONMENT: &[&str] = &[
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

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn run(root: &Path, arguments: &[&str]) -> Output {
    run_with_env(root, arguments, &[])
}

fn run_with_env(root: &Path, arguments: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .args(arguments)
        .current_dir(root)
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT")
        .env_remove("GRAPHOXIDE_FORCE")
        .env_remove("GRAPHIFY_FORCE");
    for key in BACKEND_ENVIRONMENT {
        command.env_remove(key);
    }
    for (key, value) in environment {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", combined(output));
}

fn code_corpus(root: &Path) {
    write(
        &root.join("auth.py"),
        "def login(user):\n    return validate(user)\n\ndef validate(user):\n    return True\n",
    );
}

fn mixed_corpus(root: &Path) {
    write(&root.join("main.go"), "package main\nfunc main() {}\n");
    write(
        &root.join("README.md"),
        "# Notes\nThe main function entry point.\n",
    );
}

fn graph_path(output_root: &Path) -> PathBuf {
    output_root.join("graphoxide-out/graph.json")
}

fn manifest_path(output_root: &Path) -> PathBuf {
    output_root.join("graphoxide-out/manifest.json")
}

fn graph(output_root: &Path) -> KnowledgeGraph {
    graphoxide_core::read_graph(graph_path(output_root)).unwrap()
}

fn sources(output_root: &Path) -> BTreeSet<String> {
    graph(output_root)
        .nodes
        .into_iter()
        .map(|node| node.source_file.replace('\\', "/"))
        .collect()
}

fn manifest_value(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn detected(kind: &str, paths: impl IntoIterator<Item = PathBuf>) -> DetectedFiles {
    BTreeMap::from([(
        kind.to_owned(),
        paths
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
    )])
}

fn save_manifest_pass(
    root: &Path,
    manifest: &Path,
    files: &DetectedFiles,
    kind: ManifestKind,
    corpus: impl IntoIterator<Item = PathBuf>,
    clear: impl IntoIterator<Item = PathBuf>,
) {
    save_manifest(
        files,
        manifest,
        &SaveManifestOptions {
            kind,
            root: Some(root.to_path_buf()),
            scan_corpus: Some(
                corpus
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
            ),
            clear_semantic: clear
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
        },
    )
    .unwrap();
}

fn semantic_result(source_file: &str, bucket: &str) -> SemanticChunkResult {
    let item = json!({
        "id": format!("semantic-{source_file}"),
        "source": "a",
        "target": "b",
        "nodes": ["a", "b", "c"],
        "source_file": source_file,
    });
    let mut result = SemanticChunkResult::default();
    match bucket {
        "node" => result.nodes.push(item),
        "edge" => result.edges.push(item),
        "hyperedge" => result.hyperedges.push(item),
        _ => unreachable!(),
    }
    result
}

#[test]
fn test_extract_exits_nonzero_when_all_semantic_chunks_fail() {
    let fixture = TempDir::new().unwrap();
    let doc = fixture.path().join("README.md");
    write(&doc, "# Doc\n");
    let result = extract_corpus(
        &[doc],
        fixture.path(),
        &SemanticCorpusOptions {
            chunk_size: 1,
            max_concurrency: 1,
            checkpoint: false,
            ..SemanticCorpusOptions::default()
        },
        &|_| anyhow::bail!("backend unavailable"),
        None,
    )
    .unwrap();
    let error = BuildProgress::new(1, 1 - result.failed_chunks)
        .unwrap()
        .ensure_any_success("claude")
        .unwrap_err();
    assert!(error.to_string().contains("all semantic chunks failed"));
    assert!(error.to_string().contains("claude"));
    assert!(!fixture.path().join("graph.json").exists());
}

#[test]
fn test_extract_succeeds_when_at_least_one_chunk_completes() {
    let fixture = TempDir::new().unwrap();
    let first = fixture.path().join("one.md");
    let second = fixture.path().join("two.md");
    write(&first, "# One\n");
    write(&second, "# Two\n");
    let calls = AtomicUsize::new(0);
    let result = extract_corpus(
        &[first, second],
        fixture.path(),
        &SemanticCorpusOptions {
            chunk_size: 1,
            max_concurrency: 1,
            checkpoint: false,
            token_budget: None,
            ..SemanticCorpusOptions::default()
        },
        &|_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(SemanticChunkResult::default())
            } else {
                anyhow::bail!("one chunk failed")
            }
        },
        None,
    )
    .unwrap();
    let progress = BuildProgress::new(2, 2 - result.failed_chunks)
        .unwrap()
        .ensure_any_success("claude")
        .unwrap();
    let path = fixture.path().join("graph.json");
    commit_build(
        &path,
        BuildArtifact::Graph(&KnowledgeGraph::default()),
        progress,
        false,
        || Ok(()),
    )
    .unwrap();
    assert!(path.is_file());
}

#[test]
fn test_incremental_partial_run_preserves_untouched_semantic_hash() {
    let fixture = TempDir::new().unwrap();
    let readme = fixture.path().join("README.md");
    let other = fixture.path().join("OTHER.md");
    write(&readme, "# Readme\n");
    write(&other, "# Other\n");
    let manifest = fixture.path().join("manifest.json");
    let corpus = vec![readme.clone(), other.clone()];
    save_manifest_pass(
        fixture.path(),
        &manifest,
        &detected("document", corpus.clone()),
        ManifestKind::Both,
        corpus.clone(),
        [],
    );
    let before = manifest_value(&manifest);
    write(&readme, "# Readme changed\n");
    save_manifest_pass(
        fixture.path(),
        &manifest,
        &detected("document", [readme]),
        ManifestKind::Semantic,
        corpus,
        [],
    );
    let after = manifest_value(&manifest);
    assert_ne!(after["README.md"]["semantic_hash"], "");
    assert_eq!(
        after["OTHER.md"]["semantic_hash"],
        before["OTHER.md"]["semantic_hash"]
    );
}

#[test]
fn test_truncated_doc_semantic_hash_is_cleared_for_requeue() {
    let fixture = TempDir::new().unwrap();
    let doc = fixture.path().join("README.md");
    write(&doc, "# Complete\n");
    let manifest = fixture.path().join("manifest.json");
    save_manifest_pass(
        fixture.path(),
        &manifest,
        &detected("document", [doc.clone()]),
        ManifestKind::Both,
        [doc.clone()],
        [],
    );
    write(&doc, "# Truncated on this run\n");
    save_manifest_pass(
        fixture.path(),
        &manifest,
        &detected("document", std::iter::empty()),
        ManifestKind::Semantic,
        [doc.clone()],
        [doc],
    );
    assert_eq!(manifest_value(&manifest)["README.md"]["semantic_hash"], "");
}

#[test]
fn test_manifest_stamps_freshly_extracted_semantic_docs() {
    let fixture = TempDir::new().unwrap();
    let code = fixture.path().join("main.go");
    let readme = fixture.path().join("README.md");
    let omitted = fixture.path().join("OMITTED.md");
    write(&code, "package main\n");
    write(&readme, "# Readme\n");
    write(&omitted, "# Omitted\n");
    let files = BTreeMap::from([
        ("code".into(), vec![code]),
        ("document".into(), vec![readme, omitted]),
    ]);
    let stamped = stamped_manifest_files(
        &files,
        &semantic_result("README.md", "node"),
        fixture.path(),
        &BTreeSet::new(),
    );
    let stored = stamped
        .into_iter()
        .map(|(kind, paths)| {
            (
                kind,
                paths
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
            )
        })
        .collect();
    let manifest = fixture.path().join("manifest.json");
    save_manifest(
        &stored,
        &manifest,
        &SaveManifestOptions {
            kind: ManifestKind::Both,
            root: Some(fixture.path().to_path_buf()),
            ..SaveManifestOptions::default()
        },
    )
    .unwrap();
    let value = manifest_value(&manifest);
    assert_ne!(value["README.md"]["semantic_hash"], "");
    assert_ne!(value["main.go"]["semantic_hash"], "");
    assert!(value.get("OMITTED.md").is_none());
}

#[test]
fn test_stamped_manifest_files_normalizes_both_sides() {
    let fixture = TempDir::new().unwrap();
    let fresh = fixture.path().join("fresh.md");
    let cached = fixture.path().join("cached.md");
    let omitted = fixture.path().join("omitted.md");
    let code = fixture.path().join("app.py");
    for path in [&fresh, &cached, &omitted, &code] {
        write(path, "content\n");
    }
    let files = BTreeMap::from([
        ("code".into(), vec![code.clone()]),
        (
            "document".into(),
            vec![fresh.clone(), cached.clone(), omitted],
        ),
    ]);
    let mut result = semantic_result("fresh.md", "node");
    result.edges.push(json!({
        "source": "a", "target": "b", "source_file": cached
    }));
    let stamped = stamped_manifest_files(&files, &result, fixture.path(), &BTreeSet::new());
    assert_eq!(stamped["code"], vec![code]);
    assert_eq!(stamped["document"], vec![fresh, cached]);
}

#[test]
fn test_stamped_manifest_files_counts_hyperedge_only_docs() {
    let fixture = TempDir::new().unwrap();
    let hyper = fixture.path().join("hyper.md");
    let omitted = fixture.path().join("omitted.md");
    write(&hyper, "# Hyper\n");
    write(&omitted, "# Omitted\n");
    let stamped = stamped_manifest_files(
        &BTreeMap::from([("document".into(), vec![hyper.clone(), omitted])]),
        &semantic_result("hyper.md", "hyperedge"),
        fixture.path(),
        &BTreeSet::new(),
    );
    assert_eq!(stamped["document"], vec![hyper]);
}

#[test]
fn test_manifest_stamps_hyperedge_only_docs() {
    let fixture = TempDir::new().unwrap();
    let hyper = fixture.path().join("hyper.md");
    write(&hyper, "# Hyper\n");
    let result = semantic_result("hyper.md", "hyperedge");
    let stamped = stamped_manifest_files(
        &BTreeMap::from([("document".into(), vec![hyper])]),
        &result,
        fixture.path(),
        &partial_source_files(&result),
    );
    let stored = stamped
        .into_iter()
        .map(|(kind, paths)| {
            (
                kind,
                paths
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
            )
        })
        .collect();
    let manifest = fixture.path().join("manifest.json");
    save_manifest(
        &stored,
        &manifest,
        &SaveManifestOptions {
            kind: ManifestKind::Both,
            root: Some(fixture.path().to_path_buf()),
            ..SaveManifestOptions::default()
        },
    )
    .unwrap();
    assert_ne!(manifest_value(&manifest)["hyper.md"]["semantic_hash"], "");
}

#[test]
fn test_extract_mode_deep_is_rejected_until_semantic_cli_is_supported() {
    let fixture = TempDir::new().unwrap();
    let cli = run(
        fixture.path(),
        &["extract", ".", "--no-cluster", "--mode", "deep"],
    );
    assert!(!cli.status.success(), "{}", combined(&cli));
    assert!(combined(&cli).contains("unexpected argument '--mode'"));
}

fn cache_json_files(root: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, output: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, output);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                output.push(path);
            }
        }
    }
    let mut output = Vec::new();
    visit(root, &mut output);
    output
}

fn age_cache_files(paths: &[PathBuf]) {
    for path in paths {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(1, 0)).unwrap();
    }
}

fn runtime_execution_model(path: &Path) -> String {
    serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap()["runtime"]["execution_model"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn test_default_extract_force_does_not_reuse_legacy_ast_cache() {
    let fixture = TempDir::new().unwrap();
    code_corpus(fixture.path());
    assert_success(&run(
        fixture.path(),
        &["extract", ".", "--no-cluster", "--legacy-executor"],
    ));
    let cache = cache_json_files(&fixture.path().join("graphoxide-out/cache/ast"));
    assert!(!cache.is_empty());
    age_cache_files(&cache);
    let report = fixture.path().join("runtime.json");
    assert_success(&run(
        fixture.path(),
        &[
            "extract",
            ".",
            "--no-cluster",
            "--force",
            "--runtime-report",
            report.to_str().unwrap(),
        ],
    ));
    assert!(cache.iter().all(|path| {
        filetime::FileTime::from_last_modification_time(&fs::metadata(path).unwrap()).unix_seconds()
            == 1
    }));
    assert_eq!(runtime_execution_model(&report), "isolated");
    assert_ne!(
        manifest_value(&manifest_path(fixture.path()))["auth.py"]["ast_hash"],
        ""
    );
}

#[test]
fn test_default_extract_graphify_force_does_not_reuse_legacy_ast_cache() {
    let fixture = TempDir::new().unwrap();
    code_corpus(fixture.path());
    assert_success(&run(
        fixture.path(),
        &["extract", ".", "--no-cluster", "--legacy-executor"],
    ));
    let cache = cache_json_files(&fixture.path().join("graphoxide-out/cache/ast"));
    assert!(!cache.is_empty());
    age_cache_files(&cache);
    let report = fixture.path().join("runtime.json");
    assert_success(&run_with_env(
        fixture.path(),
        &[
            "extract",
            ".",
            "--no-cluster",
            "--runtime-report",
            report.to_str().unwrap(),
        ],
        &[("GRAPHIFY_FORCE", "1")],
    ));
    assert!(cache.iter().all(|path| {
        filetime::FileTime::from_last_modification_time(&fs::metadata(path).unwrap()).unix_seconds()
            == 1
    }));
    assert_eq!(runtime_execution_model(&report), "isolated");
}

#[test]
fn default_isolated_extract_cleans_fact_runs_and_enforces_graph_stage_budget() {
    let fixture = TempDir::new().unwrap();
    code_corpus(fixture.path());
    let report = fixture.path().join("runtime.json");
    assert_success(&run(
        fixture.path(),
        &[
            "extract",
            ".",
            "--force",
            "--memory-budget-bytes",
            "2097152",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
            "--runtime-report",
            report.to_str().unwrap(),
        ],
    ));
    assert_eq!(runtime_execution_model(&report), "isolated");
    let staging = fixture.path().join("graphoxide-out/staging");
    assert_eq!(
        fs::read_dir(&staging)
            .unwrap()
            .flatten()
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("fact-runs-v1-"))
            .count(),
        0,
        "successful graph staging must be cleaned"
    );

    write(
        &fixture.path().join("auth.py"),
        "def login(user):\n    return validate(user)\n\ndef validate(user):\n    return user != 'blocked'\n",
    );
    let update_report = fixture.path().join("update-runtime.json");
    assert_success(&run(
        fixture.path(),
        &[
            "update",
            ".",
            "--memory-budget-bytes",
            "2097152",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
            "--runtime-report",
            update_report.to_str().unwrap(),
        ],
    ));
    assert_eq!(runtime_execution_model(&update_report), "isolated");
    assert_eq!(
        fs::read_dir(&staging)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("fact-runs-v1-")
            })
            .count(),
        0,
        "successful update staging must be cleaned"
    );

    let constrained = TempDir::new().unwrap();
    let mut source = String::new();
    for ordinal in 0..5_000 {
        source.push_str(&format!(
            "def generated_{ordinal}(value):\n    return value + {ordinal}\n\n"
        ));
    }
    write(&constrained.path().join("oversized.py"), &source);
    let output = run(
        constrained.path(),
        &[
            "extract",
            ".",
            "--force",
            "--memory-budget-bytes",
            "8388608",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
        ],
    );
    assert!(!output.status.success(), "{}", combined(&output));
    let output_text = combined(&output);
    assert!(
        output_text.contains("isolated retained extraction output exceeds")
            || output_text.contains("graph-stage budget"),
        "{output_text}"
    );
    assert!(!graph_path(constrained.path()).exists());
    let constrained_staging = constrained.path().join("graphoxide-out/staging");
    let retained_fact_runs = match fs::read_dir(constrained_staging) {
        Ok(entries) => entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("fact-runs-v1-")
            })
            .count(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("inspect constrained staging cleanup: {error}"),
    };
    assert_eq!(
        retained_fact_runs, 0,
        "ordinary admission and graph-stage errors must clean their temporary fact runs"
    );
}

#[test]
fn default_update_uses_the_isolated_executor_and_legacy_is_explicit() {
    let fixture = TempDir::new().unwrap();
    code_corpus(fixture.path());
    assert_success(&run(
        fixture.path(),
        &["extract", ".", "--no-cluster", "--legacy-executor"],
    ));

    write(
        &fixture.path().join("auth.py"),
        "def login(user):\n    return validate(user)\n\ndef validate(user):\n    return True\n\ndef audit(user):\n    return login(user)\n",
    );
    let isolated_report = fixture.path().join("runtime-isolated.json");
    assert_success(&run(
        fixture.path(),
        &[
            "update",
            ".",
            "--no-cluster",
            "--runtime-report",
            isolated_report.to_str().unwrap(),
        ],
    ));
    assert_eq!(runtime_execution_model(&isolated_report), "isolated");

    write(
        &fixture.path().join("auth.py"),
        "def login(user):\n    return validate(user)\n\ndef validate(user):\n    return True\n\ndef audit(user):\n    return login(user)\n\ndef legacy_audit(user):\n    return audit(user)\n",
    );
    let legacy_report = fixture.path().join("runtime-legacy.json");
    assert_success(&run(
        fixture.path(),
        &[
            "update",
            ".",
            "--no-cluster",
            "--legacy-executor",
            "--runtime-report",
            legacy_report.to_str().unwrap(),
        ],
    ));
    assert_eq!(runtime_execution_model(&legacy_report), "legacy");
}

#[test]
fn test_cache_check_mode_deep_reads_deep_namespace() {
    let fixture = TempDir::new().unwrap();
    let doc = fixture.path().join("doc.md");
    write(&doc, "# Doc\n");
    let deep = SemanticCacheOptions {
        mode: Some("deep".into()),
        ..SemanticCacheOptions::default()
    };
    save_semantic_cache(
        &[json!({"id": "d", "source_file": "doc.md"})],
        &[],
        &[],
        fixture.path(),
        &deep,
    )
    .unwrap();
    assert_eq!(
        check_semantic_cache(
            std::slice::from_ref(&doc),
            fixture.path(),
            &SemanticCacheOptions::default()
        )
        .uncached,
        vec![doc.clone()]
    );
    assert!(check_semantic_cache(&[doc], fixture.path(), &deep)
        .uncached
        .is_empty());
}

#[test]
fn test_extract_codeonly_succeeds_without_api_key() {
    let fixture = TempDir::new().unwrap();
    code_corpus(fixture.path());
    let output = run(fixture.path(), &["extract", ".", "--code-only"]);
    assert_success(&output);
    assert!(!graph(fixture.path()).nodes.is_empty());
}

#[test]
fn test_missing_manifest_code_only_preserves_semantic_layer() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("project");
    let output_root = fixture.path().join("output");
    write(&project.join("keep.py"), "def keep():\n    return 1\n");
    write(&project.join("README.md"), "# Notes\nCurated docs.\n");
    let arguments = [
        "extract",
        project.to_str().unwrap(),
        "--code-only",
        "--no-cluster",
        "--out",
        output_root.to_str().unwrap(),
    ];
    assert_success(&run(fixture.path(), &arguments));
    let mut seeded = graph(&output_root);
    for (id, label) in [("doc_readme_a", "Concept A"), ("doc_readme_b", "Concept B")] {
        seeded.nodes.push(Node {
            id: id.into(),
            label: label.into(),
            file_type: "document".into(),
            source_file: "README.md".into(),
            source_location: None,
            community: None,
            extra: BTreeMap::new(),
        });
    }
    seeded.links.push(Edge {
        source: "doc_readme_a".into(),
        target: "doc_readme_b".into(),
        relation: "relates_to".into(),
        confidence: Confidence::Extracted,
        source_file: "README.md".into(),
        extra: BTreeMap::new(),
    });
    seeded.hyperedges.push(json!({
        "id": "h1", "nodes": ["doc_readme_a", "doc_readme_b"],
        "relation": "participate_in", "source_file": "README.md"
    }));
    graphoxide_core::write_graph_atomic(graph_path(&output_root), &seeded, true).unwrap();
    fs::remove_file(manifest_path(&output_root)).unwrap();

    assert_success(&run(fixture.path(), &arguments));
    let preserved = graph(&output_root);
    assert_eq!(
        preserved
            .nodes
            .iter()
            .filter(|node| node.source_file == "README.md")
            .count(),
        2
    );
    assert!(preserved
        .hyperedges
        .iter()
        .any(|hyperedge| hyperedge["id"] == "h1"));

    fs::remove_file(project.join("README.md")).unwrap();
    fs::remove_file(manifest_path(&output_root)).unwrap();
    assert_success(&run(fixture.path(), &arguments));
    assert!(!graph(&output_root)
        .nodes
        .iter()
        .any(|node| node.source_file == "README.md"));
}

#[test]
fn test_extract_out_keeps_project_root_clean() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("project");
    let external = fixture.path().join("external");
    code_corpus(&project);
    let output = run(
        &project,
        &[
            "extract",
            ".",
            "--out",
            external.to_str().unwrap(),
            "--code-only",
        ],
    );
    assert_success(&output);
    assert!(graph_path(&external).is_file());
    assert!(manifest_path(&external).is_file());
    assert!(!project.join("graphoxide-out").exists());
    assert_eq!(
        fs::read_dir(&project)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        vec![std::ffi::OsString::from("auth.py")]
    );
}

#[test]
fn test_extract_without_key_remains_intentionally_offline_when_docs_present() {
    let fixture = TempDir::new().unwrap();
    mixed_corpus(fixture.path());
    let output = run(fixture.path(), &["extract", ".", "--no-cluster"]);
    assert_success(&output);
    let indexed = sources(fixture.path());
    assert!(indexed.contains("main.go"));
    assert!(indexed.contains("README.md"));
}

#[test]
fn test_extract_accepts_vscode_jsonc_in_the_parallel_project_path() {
    let fixture = TempDir::new().unwrap();
    code_corpus(fixture.path());
    write(
        &fixture.path().join(".vscode/tasks.json"),
        r#"{
            // VS Code permits comments and trailing commas.
            "version": "2.0.0",
            "tasks": [
                {
                    "label": "build",
                    "type": "shell",
                    "command": "cargo build",
                },
            ],
        }"#,
    );
    write(
        &fixture.path().join(".vscode/launch.json"),
        r#"{
            /* This file is JSONC despite its .json suffix. */
            "version": "0.2.0",
            "configurations": [
                {
                    "name": "Run",
                    "type": "lldb",
                    "request": "launch",
                },
            ],
        }"#,
    );

    let output = run(fixture.path(), &["extract", ".", "--no-cluster"]);
    assert_success(&output);
    assert!(sources(fixture.path()).contains("auth.py"));
}

/// A malformed file is skipped and named by its relative path; the rest of the
/// corpus still gets a graph (#4).
#[test]
fn test_extract_reports_the_relative_path_for_malformed_jsonc() {
    let fixture = TempDir::new().unwrap();
    code_corpus(fixture.path());
    write(
        &fixture.path().join(".vscode/tasks.json"),
        r#"{
            "version": "2.0.0",
            "tasks": [this is not JSONC],
        }"#,
    );

    let output = run(fixture.path(), &["extract", ".", "--no-cluster"]);
    assert_success(&output);
    let report = combined(&output);
    assert!(report.contains("skipped .vscode/tasks.json"), "{report}");
    assert!(
        !report.contains(&fixture.path().to_string_lossy().to_string()),
        "report leaked the absolute fixture path: {report}"
    );
    assert!(sources(fixture.path()).contains("auth.py"));
}

#[test]
fn test_extract_timing_flag_emits_stage_timings() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("project");
    code_corpus(&project);
    let timed_root = fixture.path().join("timed");
    let timed = run(
        fixture.path(),
        &[
            "extract",
            project.to_str().unwrap(),
            "--no-cluster",
            "--out",
            timed_root.to_str().unwrap(),
            "--timing",
        ],
    );
    assert_success(&timed);
    let stderr = String::from_utf8_lossy(&timed.stderr);
    assert!(stderr.contains("[graphoxide timing] detect/extract:"));
    assert!(stderr.contains("[graphoxide timing] build:"));
    assert!(stderr.contains("[graphoxide timing] write:"));
    assert!(stderr.contains("[graphoxide timing] total:"));

    let plain_root = fixture.path().join("plain");
    let plain = run(
        fixture.path(),
        &[
            "extract",
            project.to_str().unwrap(),
            "--no-cluster",
            "--out",
            plain_root.to_str().unwrap(),
        ],
    );
    assert_success(&plain);
    assert!(!String::from_utf8_lossy(&plain.stderr).contains("graphoxide timing"));
}

#[test]
fn test_extract_json_emits_one_structured_build_report() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path().join("project");
    code_corpus(&project);
    let output_root = fixture.path().join("telemetry");
    let output = run(
        fixture.path(),
        &[
            "extract",
            project.to_str().unwrap(),
            "--no-cluster",
            "--out",
            output_root.to_str().unwrap(),
            "--json",
        ],
    );
    assert_success(&output);

    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(stdout.trim()).expect("one valid JSON report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "extract");
    assert_eq!(report["mode"], "full");
    assert_eq!(report["status"], "rebuilt");
    assert_eq!(report["files"]["detected"], 1);
    assert_eq!(report["files"]["processed"], 1);
    assert!(report["graph"]["nodes"].as_u64().unwrap() > 0);
    assert!(report["graph"]["edges"].as_u64().is_some());
    assert_eq!(report["graph"]["clustered"], false);
    assert!(report["elapsed_ms"].as_u64().is_some());
    assert!(report["stages_ms"]["scan_extract"].as_u64().is_some());
    assert!(!stdout.contains("Wrote"));
    assert!(!stdout.contains("Incremental scan"));
}

fn two_file_corpus(root: &Path) -> PathBuf {
    let project = root.join("project");
    write(
        &project.join("x.py"),
        "def secret_helper():\n    return 42\n\ndef secret_caller():\n    return secret_helper()\n",
    );
    write(
        &project.join("keep.py"),
        "def kept():\n    return still_here()\n\ndef still_here():\n    return 1\n",
    );
    project
}

fn exclusion_arguments<'a>(project: &'a Path, output: &'a Path, no_cluster: bool) -> Vec<&'a str> {
    let mut arguments = vec![
        "extract",
        project.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
    ];
    if no_cluster {
        arguments.push("--no-cluster");
    }
    arguments
}

#[test]
fn test_incremental_extract_prunes_newly_excluded_file_not_in_manifest() {
    let fixture = TempDir::new().unwrap();
    let project = two_file_corpus(fixture.path());
    let output_root = fixture.path().join("output");
    let arguments = exclusion_arguments(&project, &output_root, false);
    assert_success(&run(fixture.path(), &arguments));
    assert!(sources(&output_root).contains("x.py"));
    let path = manifest_path(&output_root);
    let mut manifest = manifest_value(&path);
    manifest.as_object_mut().unwrap().remove("x.py");
    graphoxide_core::write_json_atomic(&path, &manifest, true).unwrap();
    write(&project.join(".graphifyignore"), "x.py\n");
    assert_success(&run(fixture.path(), &arguments));
    let indexed = sources(&output_root);
    assert!(!indexed.contains("x.py"));
    assert!(indexed.contains("keep.py"));
    assert!(manifest_value(&path).get("x.py").is_none());
}

#[test]
fn test_incremental_extract_prunes_excluded_file_listed_in_manifest() {
    let fixture = TempDir::new().unwrap();
    let project = two_file_corpus(fixture.path());
    let output_root = fixture.path().join("output");
    let arguments = exclusion_arguments(&project, &output_root, false);
    assert_success(&run(fixture.path(), &arguments));
    assert!(manifest_value(&manifest_path(&output_root))
        .get("x.py")
        .is_some());
    write(&project.join(".graphifyignore"), "x.py\n");
    assert_success(&run(fixture.path(), &arguments));
    assert!(!sources(&output_root).contains("x.py"));
    assert!(sources(&output_root).contains("keep.py"));
    assert!(manifest_value(&manifest_path(&output_root))
        .get("x.py")
        .is_none());
    assert_success(&run(fixture.path(), &arguments));
    assert!(!sources(&output_root).contains("x.py"));
    assert!(sources(&output_root).contains("keep.py"));
}

#[test]
fn test_no_cluster_incremental_prunes_newly_excluded_file() {
    let fixture = TempDir::new().unwrap();
    let project = two_file_corpus(fixture.path());
    let output_root = fixture.path().join("output");
    let arguments = exclusion_arguments(&project, &output_root, true);
    assert_success(&run(fixture.path(), &arguments));
    assert!(sources(&output_root).contains("x.py"));
    write(&project.join(".graphifyignore"), "x.py\n");
    let second = run(fixture.path(), &arguments);
    assert_success(&second);
    assert!(!combined(&second).contains("1 deleted"));
    assert!(!sources(&output_root).contains("x.py"));
    assert!(sources(&output_root).contains("keep.py"));
}

#[test]
fn test_cache_check_prompt_file_scopes_hits_to_that_prompt() {
    let fixture = TempDir::new().unwrap();
    let doc = fixture.path().join("doc.md");
    let prompt = fixture.path().join("extraction-spec.md");
    write(&doc, "# Doc\n");
    write(&prompt, "PROMPT V1");
    let options = SemanticCacheOptions {
        prompt_file: Some(prompt.clone()),
        ..SemanticCacheOptions::default()
    };
    save_semantic_cache(
        &[json!({"id": "d", "source_file": "doc.md"})],
        &[],
        &[],
        fixture.path(),
        &options,
    )
    .unwrap();
    assert!(
        check_semantic_cache(std::slice::from_ref(&doc), fixture.path(), &options)
            .uncached
            .is_empty()
    );
    write(&prompt, "PROMPT V2 - rewritten by an upgrade");
    assert_eq!(
        check_semantic_cache(std::slice::from_ref(&doc), fixture.path(), &options).uncached,
        vec![doc]
    );
}
