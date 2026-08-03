//! Executable port of upstream `tests/test_transcribe.py`.

use anyhow::anyhow;
use graphoxide_cli::transcribe::{
    build_whisper_prompt_with_override, transcribe_all_with, transcribe_with, VIDEO_EXTENSIONS,
};
use serde_json::json;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_video_extensions_set() {
    for extension in [".mp4", ".mp3", ".wav", ".mov"] {
        assert!(VIDEO_EXTENSIONS.contains(&extension));
    }
    assert!(!VIDEO_EXTENSIONS.contains(&".py"));
}

#[test]
fn test_build_whisper_prompt_no_nodes() {
    let prompt = build_whisper_prompt_with_override(&[], None);
    assert!(!prompt.is_empty());
    assert!(prompt.to_ascii_lowercase().contains("punctuation"));
}

#[test]
fn test_build_whisper_prompt_env_override() {
    let nodes = [json!({"label":"Python"}), json!({"label":"FastAPI"})];
    assert_eq!(
        build_whisper_prompt_with_override(&nodes, Some("Custom domain hint.")),
        "Custom domain hint."
    );
}

#[test]
fn test_build_whisper_prompt_returns_topic_string() {
    let nodes = [
        json!({"label":"neural networks"}),
        json!({"label":"transformers"}),
        json!({"label":"attention"}),
    ];
    let prompt = build_whisper_prompt_with_override(&nodes, None).to_ascii_lowercase();
    assert!(prompt.contains("neural networks") || prompt.contains("transformers"));
    assert!(prompt.contains("punctuation"));
}

#[test]
fn test_build_whisper_prompt_nodes_without_labels() {
    let nodes = [json!({"id":"1"}), json!({"id":"2","label":""})];
    assert!(!build_whisper_prompt_with_override(&nodes, None).is_empty());
}

#[test]
fn test_transcribe_uses_cache() {
    let temp = TempDir::new().unwrap();
    let media = temp.path().join("lecture.mp4");
    fs::write(&media, b"fake").unwrap();
    let output_directory = temp.path().join("transcripts");
    fs::create_dir(&output_directory).unwrap();
    let cached = output_directory.join("lecture.txt");
    fs::write(&cached, "Cached transcript content.").unwrap();
    let result = transcribe_with(&media, &output_directory, "prompt", false, |_, _| {
        panic!("cached transcription invoked the backend")
    })
    .unwrap();
    assert_eq!(result, cached);
}

#[test]
fn test_transcribe_force_reruns() {
    let temp = TempDir::new().unwrap();
    let media = temp.path().join("talk.mp4");
    fs::write(&media, b"fake").unwrap();
    let output_directory = temp.path().join("transcripts");
    fs::create_dir(&output_directory).unwrap();
    fs::write(output_directory.join("talk.txt"), "Old transcript.").unwrap();
    let result = transcribe_with(&media, &output_directory, "prompt", true, |_, _| {
        Ok("New transcript segment.".to_owned())
    })
    .unwrap();
    assert_eq!(
        fs::read_to_string(result).unwrap(),
        "New transcript segment."
    );
}

#[test]
fn test_transcribe_missing_faster_whisper() {
    let temp = TempDir::new().unwrap();
    let media = temp.path().join("clip.mp4");
    fs::write(&media, b"fake").unwrap();
    let error = transcribe_with(&media, &temp.path().join("out"), "prompt", false, |_, _| {
        Err(anyhow!("transcription backend is not installed"))
    })
    .unwrap_err();
    assert!(error.to_string().contains("could not transcribe"));
}

#[test]
fn test_transcribe_all_empty() {
    let result = transcribe_all_with(
        &[],
        std::path::Path::new("unused"),
        "prompt",
        false,
        |_, _| Ok(String::new()),
    );
    assert!(result.is_empty());
}

#[test]
fn test_transcribe_all_uses_cache() {
    let temp = TempDir::new().unwrap();
    let media = temp.path().join("lecture.mp4");
    fs::write(&media, b"fake").unwrap();
    let output_directory = temp.path().join("transcripts");
    fs::create_dir(&output_directory).unwrap();
    let cached = output_directory.join("lecture.txt");
    fs::write(&cached, "Cached.").unwrap();
    let results = transcribe_all_with(
        std::slice::from_ref(&media),
        &output_directory,
        "prompt",
        false,
        |_, _| panic!("cached transcription invoked the backend"),
    );
    assert_eq!(results, vec![cached]);
}

#[test]
fn test_transcribe_all_skips_failed() {
    let temp = TempDir::new().unwrap();
    let media = temp.path().join("broken.mp4");
    fs::write(&media, b"fake").unwrap();
    let results = transcribe_all_with(
        &[media],
        &temp.path().join("out"),
        "prompt",
        false,
        |_, _| Err(anyhow!("boom")),
    );
    assert!(results.is_empty());
}
