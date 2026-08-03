//! Safe semantic-file loading and multimodal request construction.
//!
//! Raster images must never be decoded as source text. This module partitions
//! semantic inputs, confines reads to the corpus root, and renders the payload
//! shape expected by each supported vision backend without performing network
//! I/O. Backend adapters can therefore be tested without SDKs or credentials.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use graphoxide_core::{unit_path, FileUnit, FILE_CHAR_CAP};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// Inline limit shared by Anthropic and Bedrock-compatible payloads.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// Canonical path used only by path-capable backends such as Claude CLI.
    pub path: PathBuf,
    /// Stable corpus-relative source identity.
    pub rel: String,
    pub media_type: String,
    /// Omitted for unreadable, oversized, path-only, or non-vision inputs.
    pub raw: Option<Vec<u8>>,
}

impl ImageRef {
    pub fn b64(&self) -> String {
        self.raw
            .as_deref()
            .filter(|bytes| !bytes.is_empty())
            .map(|bytes| STANDARD.encode(bytes))
            .unwrap_or_default()
    }

    pub fn bedrock_format(&self) -> &str {
        self.media_type
            .split_once('/')
            .map_or(self.media_type.as_str(), |(_, format)| format)
    }

    fn has_pixels(&self) -> bool {
        self.raw.as_ref().is_some_and(|bytes| !bytes.is_empty())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SemanticReadResult {
    pub text: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageRefBuildResult {
    pub refs: Vec<ImageRef>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BedrockContentBlock {
    Image { format: String, bytes: Vec<u8> },
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BedrockRequestPlan {
    pub model: String,
    pub system: String,
    pub content: Vec<BedrockContentBlock>,
    pub max_tokens: usize,
    pub read_timeout_seconds: f64,
    pub connect_timeout_seconds: u64,
    pub max_attempts: usize,
    pub retry_mode: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedBedrockResponse {
    pub fragment: Value,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model: String,
    pub finish_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCliVisionPlan {
    pub add_dirs: Vec<PathBuf>,
    pub user_message: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PreparedVisionInputs {
    pub text: String,
    pub images: Vec<ImageRef>,
    pub warnings: Vec<String>,
}

pub fn is_vision_image(path: &Path) -> bool {
    graphoxide_core::is_vision_image(path)
}

pub fn partition_semantic_files(units: &[FileUnit]) -> (Vec<FileUnit>, Vec<PathBuf>) {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for unit in units {
        if matches!(unit, FileUnit::Path(path) if is_vision_image(path)) {
            images.push(unit_path(unit).to_path_buf());
        } else {
            text.push(unit.clone());
        }
    }
    (text, images)
}

/// Resolve a file and reject symlinks or paths that escape the corpus root.
pub fn resolve_under_root(path: &Path, root: &Path) -> Option<PathBuf> {
    let root = fs::canonicalize(root).ok()?;
    let path = fs::canonicalize(path).ok()?;
    path.starts_with(&root).then_some(path)
}

/// Read a semantic source, routing PDFs through a real PDF text extractor.
pub fn file_to_text(path: &Path) -> anyhow::Result<String> {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        return Ok(crate::detect::extract_pdf_text(path));
    }
    Ok(String::from_utf8_lossy(&fs::read(path)?).into_owned())
}

/// Testable form of [`read_semantic_files`] with an injectable PDF reader.
pub fn read_semantic_files_with_pdf<F>(
    units: &[FileUnit],
    root: &Path,
    mut pdf_text: F,
) -> SemanticReadResult
where
    F: FnMut(&Path) -> anyhow::Result<String>,
{
    let mut result = SemanticReadResult::default();
    let mut blocks = Vec::new();
    for unit in units {
        let original = unit_path(unit);
        let Some(safe_path) = resolve_under_root(original, root) else {
            result.warnings.push(format!(
                "skipping {}: symlink target or path is outside corpus root",
                original.display()
            ));
            continue;
        };
        let content: anyhow::Result<String> = match unit {
            FileUnit::Slice(slice) => graphoxide_core::read_slice_text(slice).map_err(Into::into),
            FileUnit::Path(_) if is_pdf(&safe_path) => pdf_text(&safe_path),
            FileUnit::Path(_) => fs::read(&safe_path)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .map_err(Into::into),
        };
        let Ok(content) = content else {
            result.warnings.push(format!(
                "could not read semantic source {}",
                original.display()
            ));
            continue;
        };
        let content = content.chars().take(FILE_CHAR_CAP).collect::<String>();
        let relative = relative_path(original, root);
        let digest = hex::encode(Sha256::digest(content.as_bytes()));
        blocks.push(format!(
            "<untrusted_source path=\"{}\" sha256=\"{digest}\">\n{content}\n</untrusted_source>",
            escape_attribute(&relative)
        ));
    }
    result.text = blocks.join("\n\n");
    result
}

pub fn read_semantic_files(units: &[FileUnit], root: &Path) -> SemanticReadResult {
    read_semantic_files_with_pdf(units, root, file_to_text)
}

pub fn build_image_refs(
    image_files: &[PathBuf],
    root: &Path,
    read_bytes: bool,
) -> ImageRefBuildResult {
    build_image_refs_with_limit(image_files, root, read_bytes, MAX_IMAGE_BYTES)
}

pub fn build_image_refs_with_limit(
    image_files: &[PathBuf],
    root: &Path,
    read_bytes: bool,
    max_image_bytes: usize,
) -> ImageRefBuildResult {
    let mut result = ImageRefBuildResult::default();
    for original in image_files {
        let Some(path) = resolve_under_root(original, root) else {
            result.warnings.push(format!(
                "skipping image {}: symlink target or path is outside corpus root",
                original.display()
            ));
            continue;
        };
        let rel = relative_path(original, root);
        let media_type = image_media_type(original).to_owned();
        let raw = if read_bytes {
            match fs::metadata(&path) {
                Ok(metadata) if metadata.len() > max_image_bytes as u64 => {
                    result.warnings.push(format!(
                        "image {rel} exceeds the {max_image_bytes}-byte inline-image limit; sending a reference without pixels"
                    ));
                    None
                }
                Ok(_) => match fs::read(&path) {
                    Ok(bytes) => Some(bytes),
                    Err(error) => {
                        result
                            .warnings
                            .push(format!("could not read image {rel}: {error}"));
                        None
                    }
                },
                Err(error) => {
                    result
                        .warnings
                        .push(format!("could not inspect image {rel}: {error}"));
                    None
                }
            }
        } else {
            None
        };
        result.refs.push(ImageRef {
            path,
            rel,
            media_type,
            raw,
        });
    }
    result
}

pub fn strip_pixels(refs: &[ImageRef]) -> Vec<ImageRef> {
    refs.iter()
        .cloned()
        .map(|mut image| {
            image.raw = None;
            image
        })
        .collect()
}

pub fn backend_supports_vision(backend: &str, environment: &BTreeMap<String, String>) -> bool {
    if backend == "ollama" {
        return environment
            .get("GRAPHIFY_OLLAMA_VISION")
            .is_some_and(|value| value.trim() == "1");
    }
    matches!(
        backend,
        "claude" | "claude-cli" | "openai" | "gemini" | "bedrock" | "kimi" | "azure"
    )
}

pub fn image_notes(refs: &[ImageRef], with_paths: bool) -> String {
    if refs.is_empty() {
        return String::new();
    }
    let header = if with_paths {
        "Use the Read tool to open and view each image file at the path below, then emit one node per image"
    } else {
        "The following image file(s) are attached as visual input. Emit one node per image"
    };
    let mut lines = vec![
        "=== IMAGES ===".into(),
        format!(
            "{header} with \"file_type\":\"image\" and the listed source_file, a label describing what it depicts (diagram, screenshot, chart, photo, UI, logo), and edges to any code/doc nodes the image clearly references."
        ),
    ];
    for (index, image) in refs.iter().enumerate() {
        let mut note = format!("[image {}] source_file: {}", index + 1, image.rel);
        if with_paths {
            note.push_str(&format!("  path: {}", image.path.display()));
        } else if !image.has_pixels() {
            note.push_str(" (not shown: unreadable or exceeds size limit)");
        }
        lines.push(note);
    }
    lines.join("\n")
}

pub fn with_image_notes(user_message: &str, refs: &[ImageRef], with_paths: bool) -> String {
    let notes = image_notes(refs, with_paths);
    if notes.is_empty() {
        return user_message.into();
    }
    if user_message.trim().is_empty() {
        return notes;
    }
    format!("{user_message}\n\n{notes}")
}

/// Anthropic message content: a string without pixels, otherwise image blocks
/// followed by the text block.
pub fn anthropic_content(user_message: &str, refs: &[ImageRef]) -> Value {
    let mut blocks = refs
        .iter()
        .filter(|image| image.has_pixels())
        .map(|image| {
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.media_type,
                    "data": image.b64(),
                }
            })
        })
        .collect::<Vec<_>>();
    let text = with_image_notes(user_message, refs, false);
    if blocks.is_empty() {
        return Value::String(text);
    }
    blocks.push(json!({"type": "text", "text": text}));
    Value::Array(blocks)
}

/// OpenAI/Gemini/Kimi message content using inline data URIs.
pub fn openai_content(user_message: &str, refs: &[ImageRef]) -> Value {
    let mut pixels = refs
        .iter()
        .filter(|image| image.has_pixels())
        .map(|image| {
            json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", image.media_type, image.b64()),
                    "detail": "auto",
                }
            })
        })
        .collect::<Vec<_>>();
    let text = with_image_notes(user_message, refs, false);
    if pixels.is_empty() {
        return Value::String(text);
    }
    let mut content = vec![json!({"type": "text", "text": text})];
    content.append(&mut pixels);
    Value::Array(content)
}

/// Bedrock Converse takes raw bytes; the AWS SDK performs wire encoding.
pub fn bedrock_content(user_message: &str, refs: &[ImageRef]) -> Vec<BedrockContentBlock> {
    let mut content = refs
        .iter()
        .filter_map(|image| {
            image
                .raw
                .as_ref()
                .filter(|bytes| !bytes.is_empty())
                .map(|bytes| BedrockContentBlock::Image {
                    format: image.bedrock_format().into(),
                    bytes: bytes.clone(),
                })
        })
        .collect::<Vec<_>>();
    content.push(BedrockContentBlock::Text {
        text: with_image_notes(user_message, refs, false),
    });
    content
}

pub fn bedrock_response_text(response: &Value, default: &str) -> String {
    response
        .pointer("/output/message/content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find_map(|block| {
                block
                    .as_object()
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
            })
        })
        .unwrap_or(default)
        .into()
}

pub fn parse_bedrock_response(
    response: &Value,
    model: &str,
) -> anyhow::Result<ParsedBedrockResponse> {
    let text = bedrock_response_text(response, "{}");
    let fragment = graphoxide_core::parse_llm_json(&text)?;
    let hollow = ["nodes", "edges", "hyperedges"].iter().all(|bucket| {
        fragment
            .get(*bucket)
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    });
    let finish_reason =
        if response.get("stopReason").and_then(Value::as_str) == Some("max_tokens") || hollow {
            "length"
        } else {
            "stop"
        };
    Ok(ParsedBedrockResponse {
        fragment,
        input_tokens: response
            .pointer("/usage/inputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: response
            .pointer("/usage/outputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        model: model.into(),
        finish_reason: finish_reason.into(),
    })
}

pub fn build_bedrock_request_plan(
    model: &str,
    system: &str,
    user_message: &str,
    refs: &[ImageRef],
    max_tokens: usize,
    environment: &BTreeMap<String, String>,
) -> BedrockRequestPlan {
    let timeout = environment
        .get("GRAPHIFY_API_TIMEOUT")
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(600.0);
    let retries = environment
        .get("GRAPHIFY_MAX_RETRIES")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(6);
    BedrockRequestPlan {
        model: model.into(),
        system: system.into(),
        content: bedrock_content(user_message, refs),
        max_tokens,
        read_timeout_seconds: timeout,
        connect_timeout_seconds: 10,
        max_attempts: retries.saturating_add(1),
        retry_mode: "adaptive".into(),
    }
}

pub fn claude_cli_vision_plan(user_message: &str, refs: &[ImageRef]) -> ClaudeCliVisionPlan {
    let add_dirs = refs
        .iter()
        .filter_map(|image| image.path.parent().map(Path::to_path_buf))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    ClaudeCliVisionPlan {
        add_dirs,
        user_message: with_image_notes(user_message, refs, true),
    }
}

pub fn prepare_vision_inputs(
    units: &[FileUnit],
    backend: &str,
    root: &Path,
    environment: &BTreeMap<String, String>,
) -> PreparedVisionInputs {
    let (text_units, image_files) = partition_semantic_files(units);
    let read = read_semantic_files(&text_units, root);
    let vision = backend_supports_vision(backend, environment);
    let image_result = build_image_refs(&image_files, root, vision && backend != "claude-cli");
    let images = if vision {
        image_result.refs
    } else {
        strip_pixels(&image_result.refs)
    };
    PreparedVisionInputs {
        text: read.text,
        images,
        warnings: read
            .warnings
            .into_iter()
            .chain(image_result.warnings)
            .collect(),
    }
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

fn image_media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
