//! Executable port of upstream `tests/test_extract_code_only_cli.py`.
//!
//! Graphoxide intentionally keeps structural extraction offline even without
//! `--code-only`; the corresponding no-key regression records that stronger
//! guarantee while preserving the upstream flag-discoverability contract.

use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path, process::Command};
use tempfile::TempDir;

const KEY_VARS: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "ANTHROPIC_API_KEY",
    "MOONSHOT_API_KEY",
    "DEEPSEEK_API_KEY",
];

fn run(root: &Path, arguments: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .args(arguments)
        .current_dir(root)
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT");
    for key in KEY_VARS {
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

fn write(root: &Path, relative: &str, body: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn mixed_repo() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), "app.py", "def hello():\n    return 1\n");
    write(temp.path(), "README.md", "# Design\n\nHow it works.\n");
    write(
        temp.path(),
        "NOTES.txt",
        "Architecture notes and rationale.\n",
    );
    temp
}

fn graph_at(output_root: &Path) -> Value {
    serde_json::from_slice(&fs::read(output_root.join("graphoxide-out/graph.json")).unwrap())
        .unwrap()
}

fn sources(output_root: &Path) -> BTreeSet<String> {
    graph_at(output_root)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["source_file"].as_str())
        .map(|path| path.replace('\\', "/"))
        .collect()
}

#[test]
fn test_code_only_succeeds_without_key() {
    let repo = mixed_repo();
    let result = run(repo.path(), &["extract", ".", "--code-only"]);
    assert!(result.status.success(), "{}", output_text(&result));
    assert!(output_text(&result).contains("--code-only: skipping"));
    let graph = graph_at(repo.path());
    assert!(graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["label"].as_str())
        .any(|label| label.contains("hello")));
    assert!(sources(repo.path())
        .iter()
        .all(|source| source.ends_with("app.py")));
}

#[test]
fn test_mixed_repo_without_key_remains_intentionally_offline() {
    let repo = mixed_repo();
    let result = run(repo.path(), &["extract", ".", "--no-cluster"]);
    assert!(result.status.success(), "{}", output_text(&result));
    let indexed = sources(repo.path());
    assert!(indexed.contains("app.py"));
    assert!(indexed.contains("README.md"));
    assert!(indexed.contains("NOTES.txt"));
}

#[test]
fn test_extract_usage_advertises_code_only() {
    let temp = tempfile::tempdir().unwrap();
    let result = run(temp.path(), &["extract", "--help"]);
    assert!(result.status.success(), "{}", output_text(&result));
    assert!(output_text(&result).contains("--code-only"));
}

#[test]
fn test_output_flag_is_alias_of_out() {
    let repo = tempfile::tempdir().unwrap();
    write(repo.path(), "app.py", "def hello():\n    return 1\n");
    let destination = tempfile::tempdir().unwrap();
    let destination_text = destination.path().to_str().unwrap();
    let result = run(
        repo.path(),
        &[
            "extract",
            ".",
            "--code-only",
            "--no-cluster",
            "--output",
            destination_text,
        ],
    );
    assert!(result.status.success(), "{}", output_text(&result));
    assert!(destination
        .path()
        .join("graphoxide-out/graph.json")
        .is_file());
    assert!(!repo.path().join("graphoxide-out").exists());
}

#[test]
fn test_output_flag_inline_form() {
    let repo = tempfile::tempdir().unwrap();
    write(repo.path(), "app.py", "def hello():\n    return 1\n");
    let destination = tempfile::tempdir().unwrap();
    let argument = format!("--output={}", destination.path().display());
    let result = run(
        repo.path(),
        &["extract", ".", "--code-only", "--no-cluster", &argument],
    );
    assert!(result.status.success(), "{}", output_text(&result));
    assert!(destination
        .path()
        .join("graphoxide-out/graph.json")
        .is_file());
}

#[test]
fn test_no_gitignore_indexes_vcs_ignored_code_but_keeps_graphifyignore() {
    let repo = tempfile::tempdir().unwrap();
    write(repo.path(), ".git/info/exclude", "local/\n");
    write(repo.path(), "proj/.gitignore", "generated/\n");
    write(repo.path(), "proj/.graphifyignore", "hidden/\n");
    write(
        repo.path(),
        "proj/deep/generated/Gen.cs",
        "namespace N { public class Gen {} }\n",
    );
    write(
        repo.path(),
        "local/Local.cs",
        "namespace N { public class Local {} }\n",
    );
    write(
        repo.path(),
        "proj/hidden/Hidden.cs",
        "namespace N { public class Hidden {} }\n",
    );
    let result = run(
        repo.path(),
        &["extract", ".", "--no-gitignore", "--no-cluster"],
    );
    assert!(result.status.success(), "{}", output_text(&result));
    let indexed = sources(repo.path());
    assert!(indexed.contains("proj/deep/generated/Gen.cs"));
    assert!(indexed.contains("local/Local.cs"));
    assert!(!indexed.contains("proj/hidden/Hidden.cs"));
}

#[test]
fn test_no_gitignore_setting_persists_across_flagless_extract() {
    let repo = tempfile::tempdir().unwrap();
    write(repo.path(), ".gitignore", "generated/\n");
    write(repo.path(), "app.py", "def hello():\n    return 1\n");
    write(
        repo.path(),
        "generated/Gen.py",
        "def generated():\n    return 2\n",
    );
    let first = run(
        repo.path(),
        &[
            "extract",
            ".",
            "--no-gitignore",
            "--code-only",
            "--no-cluster",
        ],
    );
    assert!(first.status.success(), "{}", output_text(&first));
    assert!(sources(repo.path()).contains("generated/Gen.py"));
    let second = run(
        repo.path(),
        &["extract", ".", "--code-only", "--no-cluster"],
    );
    assert!(second.status.success(), "{}", output_text(&second));
    assert!(sources(repo.path()).contains("generated/Gen.py"));
}

#[test]
fn test_exclude_setting_persists_across_flagless_extract() {
    let repo = tempfile::tempdir().unwrap();
    write(repo.path(), "app.py", "def app():\n    return 1\n");
    write(
        repo.path(),
        "vendor/lib.py",
        "def vendor():\n    return 2\n",
    );
    let first = run(
        repo.path(),
        &[
            "extract",
            ".",
            "--exclude",
            "vendor/",
            "--code-only",
            "--no-cluster",
        ],
    );
    assert!(first.status.success(), "{}", output_text(&first));
    assert!(!sources(repo.path()).contains("vendor/lib.py"));
    let second = run(
        repo.path(),
        &["extract", ".", "--code-only", "--no-cluster"],
    );
    assert!(second.status.success(), "{}", output_text(&second));
    assert!(!sources(repo.path()).contains("vendor/lib.py"));
}

#[test]
fn test_explicit_exclude_replaces_persisted_setting_with_custom_out() {
    let repo = tempfile::tempdir().unwrap();
    write(repo.path(), "app.py", "def app():\n    return 1\n");
    write(
        repo.path(),
        "vendor/lib.py",
        "def vendor():\n    return 2\n",
    );
    write(
        repo.path(),
        "generated/gen.py",
        "def generated():\n    return 3\n",
    );
    let destination = tempfile::tempdir().unwrap();
    let destination_text = destination.path().to_str().unwrap();
    let base = [
        "extract",
        ".",
        "--out",
        destination_text,
        "--code-only",
        "--no-cluster",
    ];
    let mut first_args = base.to_vec();
    first_args.extend(["--exclude", "vendor/"]);
    let first = run(repo.path(), &first_args);
    assert!(first.status.success(), "{}", output_text(&first));
    let output_root = destination.path();
    let indexed = sources(output_root);
    assert!(!indexed.contains("vendor/lib.py"));
    assert!(indexed.contains("generated/gen.py"));

    let mut persisted_args = base.to_vec();
    persisted_args.push("--force");
    let persisted = run(repo.path(), &persisted_args);
    assert!(persisted.status.success(), "{}", output_text(&persisted));
    assert!(!sources(output_root).contains("vendor/lib.py"));

    let mut replacement_args = base.to_vec();
    replacement_args.extend(["--exclude", "generated/", "--force"]);
    let replacement = run(repo.path(), &replacement_args);
    assert!(
        replacement.status.success(),
        "{}",
        output_text(&replacement)
    );
    let indexed = sources(output_root);
    assert!(indexed.contains("vendor/lib.py"));
    assert!(!indexed.contains("generated/gen.py"));
    let config: Value = serde_json::from_slice(
        &fs::read(
            destination
                .path()
                .join("graphoxide-out/.graphify_build.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(config["excludes"], serde_json::json!(["generated/"]));
}

#[test]
fn test_extract_names_skipped_sensitive_files() {
    let repo = tempfile::tempdir().unwrap();
    write(repo.path(), "app.py", "def hello():\n    return 1\n");
    write(repo.path(), "github_token.txt", "ghp_secretvalue\n");
    let result = run(
        repo.path(),
        &["extract", ".", "--code-only", "--no-cluster"],
    );
    assert!(result.status.success(), "{}", output_text(&result));
    let text = output_text(&result);
    assert!(text.contains("skipped as potentially sensitive"), "{text}");
    assert!(text.contains("github_token.txt"), "{text}");
}
