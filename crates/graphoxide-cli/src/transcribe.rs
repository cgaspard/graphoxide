//! Cache-aware audio/video transcription orchestration.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::{
    env, fs,
    path::{Path, PathBuf},
};

pub const VIDEO_EXTENSIONS: &[&str] = &[
    ".mp4", ".mp3", ".wav", ".mov", ".m4a", ".webm", ".avi", ".mkv", ".flac", ".ogg",
];

const FALLBACK_PROMPT: &str =
    "Transcribe accurately with clear punctuation, preserving technical names and terminology.";

pub fn build_whisper_prompt(nodes: &[Value]) -> String {
    let override_value = env::var("GRAPHOXIDE_WHISPER_PROMPT")
        .ok()
        .or_else(|| env::var("GRAPHIFY_WHISPER_PROMPT").ok());
    build_whisper_prompt_with_override(nodes, override_value.as_deref())
}

pub fn build_whisper_prompt_with_override(nodes: &[Value], override_value: Option<&str>) -> String {
    if let Some(prompt) = override_value.filter(|value| !value.trim().is_empty()) {
        return prompt.to_owned();
    }
    let labels: Vec<_> = nodes
        .iter()
        .filter_map(|node| node.get("label").and_then(Value::as_str))
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .take(24)
        .collect();
    if labels.is_empty() {
        return FALLBACK_PROMPT.to_owned();
    }
    format!(
        "Transcribe accurately with clear punctuation. Preserve technical terms related to: {}.",
        labels.join(", ")
    )
}

fn transcript_path(media: &Path, output_directory: &Path) -> Result<PathBuf> {
    let extension = media
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{}", value.to_ascii_lowercase()))
        .ok_or_else(|| anyhow!("media file has no UTF-8 extension: {}", media.display()))?;
    if !VIDEO_EXTENSIONS.contains(&extension.as_str()) {
        return Err(anyhow!("unsupported media extension {extension}"));
    }
    let stem = media
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("media file has no UTF-8 stem: {}", media.display()))?;
    Ok(output_directory.join(format!("{stem}.txt")))
}

/// Transcribe with an injected backend that returns the complete transcript.
/// Existing non-empty transcripts are reused unless `force` is set.
pub fn transcribe_with<F>(
    media: &Path,
    output_directory: &Path,
    prompt: &str,
    force: bool,
    mut backend: F,
) -> Result<PathBuf>
where
    F: FnMut(&Path, &str) -> Result<String>,
{
    let output = transcript_path(media, output_directory)?;
    if !force && output.metadata().is_ok_and(|metadata| metadata.len() > 0) {
        return Ok(output);
    }
    if !media.is_file() {
        return Err(anyhow!("media file does not exist: {}", media.display()));
    }
    let transcript = backend(media, prompt)
        .with_context(|| format!("could not transcribe {}", media.display()))?;
    fs::create_dir_all(output_directory)?;
    let temporary = output.with_extension("txt.tmp");
    fs::write(&temporary, transcript)?;
    fs::rename(&temporary, &output)?;
    Ok(output)
}

/// Transcribe every media path, retaining successful outputs and skipping
/// individual backend failures.
pub fn transcribe_all_with<F>(
    media: &[PathBuf],
    output_directory: &Path,
    prompt: &str,
    force: bool,
    mut backend: F,
) -> Vec<PathBuf>
where
    F: FnMut(&Path, &str) -> Result<String>,
{
    media
        .iter()
        .filter_map(|path| {
            transcribe_with(path, output_directory, prompt, force, |media, prompt| {
                backend(media, prompt)
            })
            .ok()
        })
        .collect()
}
