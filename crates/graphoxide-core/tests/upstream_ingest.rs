//! Executable port of upstream `tests/test_ingest.py`.

use chrono::{TimeZone, Utc};
use graphoxide_core::{save_query_result, SaveResultOptions};
use std::fs;
use tempfile::tempdir;

fn options() -> SaveResultOptions {
    SaveResultOptions {
        now: Some(Utc.with_ymd_and_hms(2026, 8, 2, 12, 34, 56).unwrap()),
        ..Default::default()
    }
}

#[test]
fn test_file_created() {
    let temp = tempdir().unwrap();
    let out = save_query_result(
        "what is attention?",
        "Attention is...",
        &temp.path().join("memory"),
        &options(),
    )
    .unwrap();
    assert!(out.exists());
}

#[test]
fn test_filename_format() {
    let temp = tempdir().unwrap();
    let out = save_query_result(
        "what connects A to B?",
        "They share...",
        &temp.path().join("memory"),
        &options(),
    )
    .unwrap();
    assert!(out
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("query_"));
    assert_eq!(out.extension().and_then(|value| value.to_str()), Some("md"));
}

#[test]
fn test_frontmatter_question() {
    let temp = tempdir().unwrap();
    let question = "what is attention?";
    let out = save_query_result(
        question,
        "Attention is softmax.",
        &temp.path().join("memory"),
        &options(),
    )
    .unwrap();
    let content = fs::read_to_string(out).unwrap();
    assert!(content.contains("question:"));
    assert!(content.to_lowercase().contains("attention"));
}

#[test]
fn test_frontmatter_type() {
    let temp = tempdir().unwrap();
    let mut save_options = options();
    save_options.query_type = "path_query".into();
    let out = save_query_result("q", "a", &temp.path().join("memory"), &save_options).unwrap();
    let content = fs::read_to_string(out).unwrap();
    assert!(content.contains("type: \"path_query\""));
}

#[test]
fn test_source_nodes_included() {
    let temp = tempdir().unwrap();
    let mut save_options = options();
    save_options.source_nodes = vec!["AttentionLayer".into(), "SoftmaxFunc".into()];
    let out = save_query_result("q", "a", &temp.path().join("memory"), &save_options).unwrap();
    let content = fs::read_to_string(out).unwrap();
    assert!(content.contains("AttentionLayer"));
    assert!(content.contains("SoftmaxFunc"));
}

#[test]
fn test_source_nodes_capped_at_10() {
    let temp = tempdir().unwrap();
    let mut save_options = options();
    save_options.source_nodes = (0..20).map(|index| format!("Node{index}")).collect();
    let out = save_query_result("q", "a", &temp.path().join("memory"), &save_options).unwrap();
    let content = fs::read_to_string(out).unwrap();
    let frontmatter_line = content
        .lines()
        .find(|line| line.starts_with("source_nodes:"))
        .unwrap();
    assert_eq!(frontmatter_line.matches("\"Node").count(), 10);
}

#[test]
fn test_memory_dir_created() {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("deep/memory");
    assert!(!memory.exists());
    save_query_result("q", "a", &memory, &options()).unwrap();
    assert!(memory.exists());
}

#[test]
fn test_answer_in_body() {
    let temp = tempdir().unwrap();
    let answer = "The answer is forty-two.";
    let out = save_query_result(
        "what is the answer?",
        answer,
        &temp.path().join("memory"),
        &options(),
    )
    .unwrap();
    assert!(fs::read_to_string(out).unwrap().contains(answer));
}

#[test]
fn test_outcome_in_frontmatter_and_body() {
    let temp = tempdir().unwrap();
    let mut save_options = options();
    save_options.outcome = Some("useful".into());
    let out = save_query_result("q", "a", &temp.path().join("memory"), &save_options).unwrap();
    let content = fs::read_to_string(out).unwrap();
    assert!(content.contains("outcome: \"useful\""));
    assert!(content.contains("## Outcome"));
    assert!(content.contains("- Signal: useful"));
}

#[test]
fn test_correction_in_frontmatter_and_body() {
    let temp = tempdir().unwrap();
    let correction = "It's bcrypt, see PasswordHasher";
    let mut save_options = options();
    save_options.outcome = Some("corrected".into());
    save_options.correction = Some(correction.into());
    let out = save_query_result(
        "what hashes passwords?",
        "MD5",
        &temp.path().join("memory"),
        &save_options,
    )
    .unwrap();
    let content = fs::read_to_string(out).unwrap();
    assert!(content.contains(&format!("correction: \"{correction}\"")));
    assert!(content.contains(&format!("- Correction: {correction}")));
}

#[test]
fn test_no_outcome_means_no_outcome_section() {
    let temp = tempdir().unwrap();
    let out = save_query_result("q", "a", &temp.path().join("memory"), &options()).unwrap();
    let content = fs::read_to_string(out).unwrap();
    assert!(!content.contains("outcome:"));
    assert!(!content.contains("## Outcome"));
}

#[test]
fn test_invalid_outcome_rejected() {
    let temp = tempdir().unwrap();
    let mut save_options = options();
    save_options.outcome = Some("great".into());
    let error = save_query_result("q", "a", &temp.path().join("memory"), &save_options)
        .unwrap_err()
        .to_string();
    assert!(error.contains("outcome must be one of useful, dead_end, corrected"));
}

/// Adversarial parity check: Python treats an empty optional correction as
/// absent, so it must not create an otherwise-empty Outcome section.
#[test]
fn test_empty_correction_does_not_create_outcome_section() {
    let temp = tempdir().unwrap();
    let mut save_options = options();
    save_options.correction = Some(String::new());
    let out = save_query_result("q", "a", &temp.path().join("memory"), &save_options).unwrap();
    let content = fs::read_to_string(out).unwrap();
    assert!(!content.contains("correction:"));
    assert!(!content.contains("## Outcome"));
}
