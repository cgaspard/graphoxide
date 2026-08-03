//! Executable CLI cases from upstream `tests/test_reflect.py`.

use filetime::{set_file_mtime, FileTime};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::tempdir;

fn run(arguments: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(arguments)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn make_graph(root: &Path) -> PathBuf {
    let output = root.join("graphoxide-out");
    fs::create_dir_all(&output).unwrap();
    fs::write(
        output.join("graph.json"),
        serde_json::to_vec(&json!({
            "directed": true,
            "multigraph": false,
            "graph": {},
            "nodes": [{
                "id": "auth_login",
                "label": "login()",
                "file_type": "code",
                "source_file": "auth.py",
                "community": 0
            }],
            "links": []
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        output.join(".graphify_analysis.json"),
        serde_json::to_vec(&json!({"communities":{"0":["auth_login"]}})).unwrap(),
    )
    .unwrap();
    fs::write(
        output.join(".graphify_labels.json"),
        serde_json::to_vec(&json!({"0":"Authentication"})).unwrap(),
    )
    .unwrap();
    output
}

fn save(root: &Path, question: &str, nodes: &[&str]) -> Output {
    let mut arguments = vec![
        "save-result",
        "--question",
        question,
        "--answer",
        "answer",
        "--nodes",
    ];
    arguments.extend(nodes);
    arguments.extend(["--outcome", "useful"]);
    run(&arguments, root)
}

#[test]
fn test_cli_reflect_end_to_end() {
    let temp = tempdir().unwrap();
    let saved = save(temp.path(), "how does auth work?", &["AuthMiddleware"]);
    assert!(saved.status.success(), "{}", text(&saved.stderr));
    let reflected = run(&["reflect"], temp.path());
    assert!(reflected.status.success(), "{}", text(&reflected.stderr));
    assert!(text(&reflected.stdout).contains("Reflected 1 memories"));
    let lessons = temp.path().join("graphoxide-out/reflections/LESSONS.md");
    assert!(lessons.exists());
    assert!(fs::read_to_string(lessons)
        .unwrap()
        .contains("`AuthMiddleware`"));
}

#[test]
fn test_cli_save_result_rejects_bad_outcome() {
    let temp = tempdir().unwrap();
    let result = run(
        &[
            "save-result",
            "--question",
            "q",
            "--answer",
            "a",
            "--outcome",
            "great",
        ],
        temp.path(),
    );
    assert!(!result.status.success());
    assert!(format!("{}{}", text(&result.stdout), text(&result.stderr)).contains("great"));
}

#[test]
fn test_cli_save_result_reads_answer_from_file() {
    let temp = tempdir().unwrap();
    let answer = temp.path().join("answer.txt");
    fs::write(&answer, "line one\nline two with a \"quote\"\n").unwrap();
    let result = run(
        &[
            "save-result",
            "--question",
            "how does auth work?",
            "--answer-file",
            answer.to_str().unwrap(),
            "--outcome",
            "useful",
        ],
        temp.path(),
    );
    assert!(result.status.success(), "{}", text(&result.stderr));
    let document = fs::read_dir(temp.path().join("graphoxide-out/memory"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let body = fs::read_to_string(document).unwrap();
    assert!(body.contains("line one") && body.contains("line two"));
}

#[test]
fn test_cli_save_result_requires_answer_or_answer_file() {
    let temp = tempdir().unwrap();
    let result = run(
        &["save-result", "--question", "q", "--outcome", "useful"],
        temp.path(),
    );
    assert!(!result.status.success());
    assert!(format!("{}{}", text(&result.stdout), text(&result.stderr)).contains("--answer"));
}

#[test]
fn test_cli_reflect_cold_start_writes_empty_lessons() {
    let temp = tempdir().unwrap();
    let result = run(&["reflect"], temp.path());
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(text(&result.stdout).contains("Reflected 0 memories"));
    let lessons = temp.path().join("graphoxide-out/reflections/LESSONS.md");
    assert!(lessons.exists());
    assert!(fs::read_to_string(lessons)
        .unwrap()
        .contains("from 0 session memories"));
}

#[test]
fn test_cli_reflect_respects_out_flag() {
    let temp = tempdir().unwrap();
    assert!(save(temp.path(), "q", &["X"]).status.success());
    let destination = temp.path().join("custom/lessons.md");
    let result = run(
        &["reflect", "--out", destination.to_str().unwrap()],
        temp.path(),
    );
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(destination.exists());
}

#[test]
fn test_cli_reflect_groups_by_community_when_graph_present() {
    let temp = tempdir().unwrap();
    let output = make_graph(temp.path());
    assert!(save(temp.path(), "q", &["login()"]).status.success());
    let result = run(&["reflect"], temp.path());
    assert!(result.status.success(), "{}", text(&result.stderr));
    let body = fs::read_to_string(output.join("reflections/LESSONS.md")).unwrap();
    assert!(body.contains("## By topic"));
    assert!(body.contains("### Authentication"));
    assert!(!body.contains("### Uncategorized"));
}

#[test]
fn test_cli_node_existence_gate_drops_stale_node_end_to_end() {
    let temp = tempdir().unwrap();
    let output = make_graph(temp.path());
    assert!(save(temp.path(), "q", &["login()", "GhostNode"])
        .status
        .success());
    let result = run(&["reflect"], temp.path());
    assert!(result.status.success(), "{}", text(&result.stderr));
    let body = fs::read_to_string(output.join("reflections/LESSONS.md")).unwrap();
    assert!(!body.contains("GhostNode"));
    assert!(body.contains("`login()`"));
}

#[test]
fn test_cli_reflect_if_stale_skips_when_fresh() {
    let temp = tempdir().unwrap();
    let output = make_graph(temp.path());
    assert!(save(temp.path(), "q", &["login()"]).status.success());
    assert!(run(&["reflect"], temp.path()).status.success());
    let lessons = output.join("reflections/LESSONS.md");
    let before = fs::read_to_string(&lessons).unwrap();
    let skipped = run(&["reflect", "--if-stale"], temp.path());
    assert!(skipped.status.success());
    assert!(
        format!("{}{}", text(&skipped.stdout), text(&skipped.stderr))
            .to_lowercase()
            .contains("up to date")
    );
    assert_eq!(fs::read_to_string(&lessons).unwrap(), before);

    assert!(save(temp.path(), "q2", &["login()"]).status.success());
    set_file_mtime(
        fs::read_dir(output.join("memory"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .max()
            .unwrap(),
        FileTime::from_unix_time(4_000, 0),
    )
    .unwrap();
    set_file_mtime(&lessons, FileTime::from_unix_time(3_000, 0)).unwrap();
    let rebuilt = run(&["reflect", "--if-stale"], temp.path());
    assert!(rebuilt.status.success());
    assert!(
        !format!("{}{}", text(&rebuilt.stdout), text(&rebuilt.stderr))
            .to_lowercase()
            .contains("up to date")
    );
}

#[test]
fn test_cli_reflect_if_stale_reruns_when_labels_newer() {
    let temp = tempdir().unwrap();
    let output = make_graph(temp.path());
    assert!(save(temp.path(), "q", &["login()"]).status.success());
    assert!(run(&["reflect"], temp.path()).status.success());
    let lessons = output.join("reflections/LESSONS.md");
    let labels = output.join(".graphify_labels.json");
    fs::write(
        &labels,
        serde_json::to_vec(&json!({"0":"Renamed Topic"})).unwrap(),
    )
    .unwrap();
    set_file_mtime(&lessons, FileTime::from_unix_time(1_500, 0)).unwrap();
    set_file_mtime(&labels, FileTime::from_unix_time(2_000, 0)).unwrap();
    let result = run(&["reflect", "--if-stale"], temp.path());
    assert!(result.status.success(), "{}", text(&result.stderr));
    assert!(!format!("{}{}", text(&result.stdout), text(&result.stderr))
        .to_lowercase()
        .contains("up to date"));
    assert!(fs::read_to_string(lessons)
        .unwrap()
        .contains("### Renamed Topic"));
}
