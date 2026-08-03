use graphoxide_core::FileUnit;
use graphoxide_extract::{
    llm::{build_claude_cli_file_request, build_claude_cli_request},
    semantic_pipeline::{extract_corpus, SemanticChunkResult, SemanticCorpusOptions},
};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::TempDir;

const UNICODE_CONTENT: &str = "→ means implies. ✅ done. Score ≥ 90.";

fn write_unicode(root: &Path, name: &str) -> std::path::PathBuf {
    let path = root.join(name);
    fs::write(&path, UNICODE_CONTENT).unwrap();
    path
}

fn file_request(root: &Path, path: &Path) -> graphoxide_extract::llm::ClaudeCliRequest {
    build_claude_cli_file_request(
        "claude",
        &[FileUnit::Path(path.to_path_buf())],
        root,
        None,
        false,
        &BTreeMap::new(),
    )
    .unwrap()
}

fn corpus_options() -> SemanticCorpusOptions {
    SemanticCorpusOptions {
        token_budget: None,
        chunk_size: 1,
        max_concurrency: 1,
        checkpoint: false,
        ..SemanticCorpusOptions::default()
    }
}

#[test]
fn test_subprocess_called_with_utf8_encoding() {
    let request = build_claude_cli_request(UNICODE_CONTENT, None);
    assert_eq!(
        std::str::from_utf8(request.stdin_utf8()).unwrap(),
        request.stdin
    );
    assert!(request
        .stdin_utf8()
        .windows(3)
        .any(|window| window == "→".as_bytes()));
}

#[test]
fn test_subprocess_does_not_use_text_true_without_encoding() {
    let request = build_claude_cli_request(UNICODE_CONTENT, None);
    let child_stdin: &[u8] = request.stdin_utf8();
    assert_eq!(
        String::from_utf8(child_stdin.to_vec()).unwrap(),
        request.stdin
    );
}

#[test]
fn test_unicode_chars_survive_subprocess_roundtrip() {
    let temp = TempDir::new().unwrap();
    let path = write_unicode(temp.path(), "u.md");
    let request = file_request(temp.path(), &path);
    let decoded = String::from_utf8(request.stdin_utf8().to_vec()).unwrap();
    assert!(decoded.contains(UNICODE_CONTENT));
}

#[test]
fn test_call_llm_claude_cli_subprocess_encoding() {
    let request = build_claude_cli_request(UNICODE_CONTENT, None);
    assert!(request.stdin_utf8().ends_with(UNICODE_CONTENT.as_bytes()));
    assert!(std::str::from_utf8(request.stdin_utf8()).is_ok());
}

#[test]
fn test_failure_count_in_merged_result() {
    let temp = TempDir::new().unwrap();
    let files = (0..3)
        .map(|index| {
            let path = temp.path().join(format!("f{index}.py"));
            fs::write(&path, format!("x = {index}\n")).unwrap();
            path
        })
        .collect::<Vec<_>>();
    let result = extract_corpus(
        &files,
        temp.path(),
        &corpus_options(),
        &|_| anyhow::bail!("charmap error"),
        None,
    )
    .unwrap();
    assert_eq!(result.failed_chunks, 3);
}

#[test]
fn test_summary_printed_when_chunks_fail() {
    let temp = TempDir::new().unwrap();
    let files = (0..2)
        .map(|index| {
            let path = temp.path().join(format!("g{index}.py"));
            fs::write(&path, format!("y = {index}\n")).unwrap();
            path
        })
        .collect::<Vec<_>>();
    let result = extract_corpus(
        &files,
        temp.path(),
        &corpus_options(),
        &|_| anyhow::bail!("charmap error"),
        None,
    )
    .unwrap();
    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.starts_with("WARNING:")
            && warning.contains("2/2")
            && warning.contains("failed")));
}

#[test]
fn test_no_false_alarm_when_all_chunks_succeed() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("ok.py");
    fs::write(&path, "z = 1\n").unwrap();
    let source_file = path.to_string_lossy().into_owned();
    let result = extract_corpus(
        std::slice::from_ref(&path),
        temp.path(),
        &corpus_options(),
        &|_| {
            Ok(SemanticChunkResult {
                nodes: vec![json!({
                    "id": "n1",
                    "label": "N1",
                    "file_type": "code",
                    "source_file": source_file,
                })],
                finish_reason: "stop".into(),
                ..SemanticChunkResult::default()
            })
        },
        None,
    )
    .unwrap();
    assert_eq!(result.failed_chunks, 0);
    assert!(!result
        .warnings
        .iter()
        .any(|warning| warning.starts_with("WARNING:") && warning.contains("failed")));
}

#[test]
fn test_read_files_produces_utf8_safe_prompt() {
    let temp = TempDir::new().unwrap();
    let path = write_unicode(temp.path(), "unicode_chunk.md");
    let request = file_request(temp.path(), &path);
    assert!(request.stdin.contains(UNICODE_CONTENT));
    assert!(!request.stdin_utf8().is_empty());
    assert_eq!(
        String::from_utf8(request.stdin_utf8().to_vec()).unwrap(),
        request.stdin
    );
}

#[test]
fn test_cp1252_would_fail_but_utf8_succeeds() {
    let request = build_claude_cli_request(UNICODE_CONTENT, None);
    assert!(std::str::from_utf8(request.stdin_utf8()).is_ok());
    // These code points are outside the single-byte Windows-1252 repertoire
    // that triggered the upstream failure; UTF-8 represents all of them.
    for character in ['→', '✅', '≥'] {
        assert!(character as u32 > u8::MAX as u32);
        assert!(request.stdin.contains(character));
    }
}

#[test]
fn test_subprocess_encoding_kwarg_in_extract_files_direct() {
    let temp = TempDir::new().unwrap();
    let path = write_unicode(temp.path(), "unicode_chunk.md");
    let request = file_request(temp.path(), &path);
    let wire_input = request.stdin_utf8();
    assert!(wire_input
        .windows("✅".len())
        .any(|window| window == "✅".as_bytes()));
    assert_eq!(std::str::from_utf8(wire_input).unwrap(), request.stdin);
}
