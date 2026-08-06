//! Validation and fail-closed merging for untrusted semantic-agent chunks.

use crate::io::write_json_atomic;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::BTreeSet, fs, io::Read, path::Path};

pub const MAX_SEMANTIC_FRAGMENT_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_SEMANTIC_FRAGMENT_NODES: usize = 10_000;
pub const MAX_SEMANTIC_FRAGMENT_EDGES: usize = 100_000;
pub const MAX_SEMANTIC_FRAGMENT_HYPEREDGES: usize = 10_000;
pub const MAX_SEMANTIC_HYPEREDGE_NODES: usize = 256;
pub const MAX_SEMANTIC_ID_LENGTH: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticFragmentLimits {
    pub bytes: usize,
    pub nodes: usize,
    pub edges: usize,
    pub hyperedges: usize,
    pub hyperedge_nodes: usize,
    pub id_length: usize,
}

impl Default for SemanticFragmentLimits {
    fn default() -> Self {
        Self {
            bytes: MAX_SEMANTIC_FRAGMENT_BYTES,
            nodes: MAX_SEMANTIC_FRAGMENT_NODES,
            edges: MAX_SEMANTIC_FRAGMENT_EDGES,
            hyperedges: MAX_SEMANTIC_FRAGMENT_HYPEREDGES,
            hyperedge_nodes: MAX_SEMANTIC_HYPEREDGE_NODES,
            id_length: MAX_SEMANTIC_ID_LENGTH,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkMergeReport {
    pub input_chunks: usize,
    pub valid_chunks: usize,
    pub skipped_chunks: Vec<SkippedChunk>,
    pub nodes: usize,
    pub edges: usize,
    pub hyperedges: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedChunk {
    pub path: String,
    pub errors: Vec<String>,
}

/// Return every shape, cap, and identifier error in an untrusted fragment.
pub fn validate_semantic_fragment(fragment: &Value) -> Vec<String> {
    validate_semantic_fragment_with_limits(fragment, SemanticFragmentLimits::default())
}

pub fn validate_semantic_fragment_with_limits(
    fragment: &Value,
    limits: SemanticFragmentLimits,
) -> Vec<String> {
    let Some(object) = fragment.as_object() else {
        return vec!["fragment must be a JSON object".into()];
    };
    let mut errors = Vec::new();
    match serde_json::to_vec(fragment) {
        Ok(payload) if payload.len() > limits.bytes => errors.push(format!(
            "payload is {} bytes; max is {}",
            payload.len(),
            limits.bytes
        )),
        Err(error) => return vec![format!("fragment is not JSON-serializable: {error}")],
        _ => {}
    }

    let nodes = validated_array(object, "nodes", limits.nodes, &mut errors);
    for (index, node) in nodes.iter().enumerate() {
        let Some(node) = node.as_object() else {
            errors.push(format!("nodes[{index}] must be an object"));
            continue;
        };
        validate_semantic_id(
            &mut errors,
            &format!("nodes[{index}].id"),
            node.get("id"),
            limits.id_length,
        );
    }

    let edges = validated_array(object, "edges", limits.edges, &mut errors);
    for (index, edge) in edges.iter().enumerate() {
        let Some(edge) = edge.as_object() else {
            errors.push(format!("edges[{index}] must be an object"));
            continue;
        };
        validate_semantic_id(
            &mut errors,
            &format!("edges[{index}].source"),
            edge.get("source"),
            limits.id_length,
        );
        validate_semantic_id(
            &mut errors,
            &format!("edges[{index}].target"),
            edge.get("target"),
            limits.id_length,
        );
    }

    let hyperedges = match object.get("hyperedges") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(values)) => {
            if values.len() > limits.hyperedges {
                errors.push(format!(
                    "hyperedges has {} entries; max is {}",
                    values.len(),
                    limits.hyperedges
                ));
            }
            values
        }
        Some(_) => {
            errors.push("hyperedges must be a list".into());
            &[]
        }
    };
    for (index, hyperedge) in hyperedges.iter().enumerate() {
        let Some(hyperedge) = hyperedge.as_object() else {
            errors.push(format!("hyperedges[{index}] must be an object"));
            continue;
        };
        validate_semantic_id(
            &mut errors,
            &format!("hyperedges[{index}].id"),
            hyperedge.get("id"),
            limits.id_length,
        );
        let members = hyperedge
            .get("nodes")
            .or_else(|| hyperedge.get("members"))
            .or_else(|| hyperedge.get("node_ids"));
        let Some(members) = members.and_then(Value::as_array) else {
            errors.push(format!("hyperedges[{index}].nodes must be a list"));
            continue;
        };
        if members.len() > limits.hyperedge_nodes {
            errors.push(format!(
                "hyperedges[{index}].nodes has {} entries; max is {}",
                members.len(),
                limits.hyperedge_nodes
            ));
        }
        for (member_index, member) in members.iter().enumerate() {
            validate_semantic_id(
                &mut errors,
                &format!("hyperedges[{index}].nodes[{member_index}]"),
                Some(member),
                limits.id_length,
            );
        }
    }
    errors
}

fn validated_array<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    max: usize,
    errors: &mut Vec<String>,
) -> &'a [Value] {
    match object.get(field) {
        None => &[],
        Some(Value::Array(values)) => {
            if values.len() > max {
                errors.push(format!(
                    "{field} has {} entries; max is {max}",
                    values.len()
                ));
            }
            values
        }
        Some(_) => {
            errors.push(format!("{field} must be a list"));
            &[]
        }
    }
}

fn validate_semantic_id(
    errors: &mut Vec<String>,
    field: &str,
    value: Option<&Value>,
    max_length: usize,
) {
    let Some(value) = value.and_then(Value::as_str) else {
        errors.push(format!("{field} must be a string"));
        return;
    };
    if value.is_empty() {
        errors.push(format!("{field} must not be empty"));
        return;
    }
    if value.chars().count() > max_length {
        errors.push(format!(
            "{field} is {} chars; max is {max_length}",
            value.chars().count(),
        ));
    }
    if value.contains('/') || value.contains('\\') || value.contains("..") {
        errors.push(format!("{field} must not contain path separators or '..'"));
    }
    if !value
        .chars()
        .all(|character| character.is_alphanumeric() || matches!(character, '_' | '.' | ':' | '-'))
    {
        errors.push(format!("{field} contains unsupported characters"));
    }
}

/// Load and validate one chunk, checking its byte size before allocating JSON.
pub fn load_validated_semantic_fragment(path: &Path) -> Result<Value, Vec<String>> {
    load_validated_semantic_fragment_with_limits(path, SemanticFragmentLimits::default())
}

pub fn load_validated_semantic_fragment_with_limits(
    path: &Path,
    limits: SemanticFragmentLimits,
) -> Result<Value, Vec<String>> {
    let file = fs::File::open(path)
        .map_err(|error| vec![format!("could not stat {}: {error}", path.display())])?;
    load_validated_semantic_fragment_from_open_file(path, file, limits)
}

fn load_validated_semantic_fragment_from_open_file(
    path: &Path,
    mut file: fs::File,
    limits: SemanticFragmentLimits,
) -> Result<Value, Vec<String>> {
    let size = file
        .metadata()
        .map_err(|error| vec![format!("could not stat {}: {error}", path.display())])?
        .len();
    load_validated_semantic_fragment_from_reader(path, &mut file, size, limits)
}

fn load_validated_semantic_fragment_from_reader(
    path: &Path,
    reader: impl Read,
    observed_size: u64,
    limits: SemanticFragmentLimits,
) -> Result<Value, Vec<String>> {
    let byte_limit = u64::try_from(limits.bytes).unwrap_or(u64::MAX);
    if observed_size > byte_limit {
        return Err(vec![format!(
            "payload is {observed_size} bytes; max is {}",
            limits.bytes
        )]);
    }
    // The metadata result is only a snapshot. Admit at most one byte beyond the
    // ceiling so an in-place growth race is rejected without allocating the
    // complete replacement payload.
    let mut bytes = Vec::new();
    reader
        .take(byte_limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| vec![format!("could not read {}: {error}", path.display())])?;
    if bytes.len() > limits.bytes {
        return Err(vec![format!(
            "payload is {} bytes; max is {}",
            bytes.len(),
            limits.bytes
        )]);
    }
    let mut fragment: Value =
        serde_json::from_slice(&bytes).map_err(|error| vec![format!("invalid JSON: {error}")])?;
    let errors = validate_semantic_fragment_with_limits(&fragment, limits);
    if !errors.is_empty() {
        return Err(errors);
    }
    normalize_hyperedge_aliases(&mut fragment);
    Ok(fragment)
}

/// Drop non-object entries from untrusted LLM buckets. Non-list node/edge
/// values become empty lists; an explicit null hyperedge value stays null for
/// backward-compatible absent-key semantics.
pub fn sanitize_fragment_shape(fragment: &Value) -> Value {
    let Some(source) = fragment.as_object() else {
        return serde_json::json!({"nodes": [], "edges": [], "hyperedges": []});
    };
    let mut output = source.clone();
    for bucket in ["nodes", "edges", "hyperedges"] {
        match source.get(bucket) {
            Some(Value::Array(items)) => {
                output.insert(
                    bucket.into(),
                    Value::Array(
                        items
                            .iter()
                            .filter(|item| item.is_object())
                            .cloned()
                            .collect(),
                    ),
                );
            }
            Some(Value::Null) if bucket == "hyperedges" => {}
            _ => {
                output.insert(bucket.into(), Value::Array(Vec::new()));
            }
        }
    }
    Value::Object(output)
}

/// Parse a plain or Markdown-fenced LLM JSON object and sanitize bucket shapes
/// at the single trust boundary.
pub fn parse_llm_json(raw: &str) -> anyhow::Result<Value> {
    let empty = || serde_json::json!({"nodes": [], "edges": [], "hyperedges": []});
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(empty());
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        return Ok(sanitize_fragment_shape(&parsed));
    }

    // Models frequently prefix a fenced payload with conversational prose.
    // Treat the language marker case-insensitively and tolerate a response
    // truncated before its closing backticks.
    let mut cursor = 0;
    while let Some(relative) = raw[cursor..].find("```") {
        let opening = cursor + relative + 3;
        let remainder = &raw[opening..];
        let line_end = remainder.find('\n').unwrap_or(remainder.len());
        let tag = remainder[..line_end].trim().trim_end_matches('\r');
        let body_start = if line_end < remainder.len() {
            opening + line_end + 1
        } else {
            opening
        };
        let body_end = raw[body_start..]
            .find("```")
            .map_or(raw.len(), |end| body_start + end);
        if (tag.is_empty() || tag.eq_ignore_ascii_case("json"))
            && let Some(candidate) = first_balanced_object(&raw[body_start..body_end])
            && let Ok(parsed) = serde_json::from_str::<Value>(candidate)
        {
            return Ok(sanitize_fragment_shape(&parsed));
        }
        cursor = body_end.saturating_add(3);
        if cursor >= raw.len() {
            break;
        }
    }

    if let Some(candidate) = first_balanced_object(raw)
        && let Ok(parsed) = serde_json::from_str::<Value>(candidate)
    {
        return Ok(sanitize_fragment_shape(&parsed));
    }
    Ok(empty())
}

fn first_balanced_object(text: &str) -> Option<&str> {
    let mut start = None;
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' if start.is_some() => in_string = true,
            '{' => {
                start.get_or_insert(index);
                depth += 1;
            }
            '}' if start.is_some() => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return start.map(|start| &text[start..index + character.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

fn sentence_like(label: &str) -> bool {
    label.split_whitespace().count() >= 8
        && label
            .chars()
            .any(|character| matches!(character, '.' | ':' | '!' | '?'))
}

/// Remove rationale artifacts, attach sentence rationales to their declared
/// targets, and repair hyperedges after node removal.
pub fn sanitize_semantic_fragment(fragment: &Value) -> Value {
    let mut fragment = sanitize_fragment_shape(fragment);
    normalize_hyperedge_aliases(&mut fragment);
    let object = fragment
        .as_object_mut()
        .expect("shape sanitizer returns object");
    let nodes = object
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let edges = object
        .get("edges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let rationale_sources = edges
        .iter()
        .filter(|edge| edge.get("relation").and_then(Value::as_str) == Some("rationale_for"))
        .filter_map(|edge| edge.get("source").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    let removed = nodes
        .iter()
        .filter_map(|node| {
            let id = node.get("id").and_then(Value::as_str)?;
            let rationale_type = node.get("file_type").and_then(Value::as_str) == Some("rationale");
            let prose_rationale = rationale_sources.contains(id)
                && node
                    .get("label")
                    .and_then(Value::as_str)
                    .is_some_and(sentence_like);
            (rationale_type || prose_rationale).then_some(id.to_owned())
        })
        .collect::<BTreeSet<_>>();
    let mut kept_nodes = nodes
        .iter()
        .filter(|node| {
            !node
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| removed.contains(id))
        })
        .cloned()
        .collect::<Vec<_>>();
    for edge in &edges {
        if edge.get("relation").and_then(Value::as_str) != Some("rationale_for") {
            continue;
        }
        let Some(source) = edge.get("source").and_then(Value::as_str) else {
            continue;
        };
        let Some(target) = edge.get("target").and_then(Value::as_str) else {
            continue;
        };
        let Some(text) = nodes
            .iter()
            .find(|node| node.get("id").and_then(Value::as_str) == Some(source))
            .and_then(|node| node.get("label"))
            .and_then(Value::as_str)
            .filter(|label| sentence_like(label))
        else {
            continue;
        };
        let Some(target_node) = kept_nodes
            .iter_mut()
            .find(|node| node.get("id").and_then(Value::as_str) == Some(target))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let combined = target_node
            .get("rationale")
            .and_then(Value::as_str)
            .filter(|prior| !prior.is_empty())
            .map_or_else(|| text.to_owned(), |prior| format!("{prior}\n{text}"));
        target_node.insert("rationale".into(), Value::String(combined));
    }
    let kept_ids = kept_nodes
        .iter()
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect::<BTreeSet<_>>();
    let kept_edges = edges
        .into_iter()
        .filter(|edge| {
            !edge
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|id| removed.contains(id))
                && !edge
                    .get("target")
                    .and_then(Value::as_str)
                    .is_some_and(|id| removed.contains(id))
        })
        .collect::<Vec<_>>();
    let kept_hyperedges = object
        .get("hyperedges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|hyperedge| {
            let mut hyperedge = hyperedge.clone();
            let members = hyperedge
                .get("nodes")
                .and_then(Value::as_array)?
                .iter()
                .filter(|member| member.as_str().is_some_and(|id| kept_ids.contains(id)))
                .cloned()
                .collect::<Vec<_>>();
            if members.len() < 2 {
                return None;
            }
            hyperedge["nodes"] = Value::Array(members);
            Some(hyperedge)
        })
        .collect::<Vec<_>>();
    object.insert("nodes".into(), Value::Array(kept_nodes));
    object.insert("edges".into(), Value::Array(kept_edges));
    object.insert("hyperedges".into(), Value::Array(kept_hyperedges));
    fragment
}

fn normalize_hyperedge_aliases(fragment: &mut Value) {
    let Some(hyperedges) = fragment.get_mut("hyperedges").and_then(Value::as_array_mut) else {
        return;
    };
    for hyperedge in hyperedges {
        let Some(object) = hyperedge.as_object_mut() else {
            continue;
        };
        if !object.get("nodes").is_some_and(Value::is_array) {
            for alias in ["members", "node_ids"] {
                if object.get(alias).is_some_and(Value::is_array) {
                    let members = object.remove(alias).expect("alias was checked");
                    object.insert("nodes".into(), members);
                    break;
                }
            }
        }
        object.remove("members");
        object.remove("node_ids");
    }
}

/// Merge validated chunk files, skipping invalid siblings and failing closed when
/// none are valid. The destination is not touched on fail-closed errors.
pub fn merge_semantic_chunk_files(
    paths: &[impl AsRef<Path>],
    output: impl AsRef<Path>,
) -> anyhow::Result<ChunkMergeReport> {
    let mut report = ChunkMergeReport {
        input_chunks: paths.len(),
        ..ChunkMergeReport::default()
    };
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut hyperedges = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut input_tokens = 0.0_f64;
    let mut output_tokens = 0.0_f64;
    for path in paths {
        let path = path.as_ref();
        let chunk = match load_validated_semantic_fragment(path) {
            Ok(chunk) => chunk,
            Err(errors) => {
                report.skipped_chunks.push(SkippedChunk {
                    path: path.display().to_string(),
                    errors,
                });
                continue;
            }
        };
        report.valid_chunks += 1;
        let object = chunk.as_object().expect("validated object");
        for node in object
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = node
                .get("id")
                .and_then(Value::as_str)
                .expect("validated node id");
            if seen_ids.insert(id.to_owned()) {
                nodes.push(node.clone());
            }
        }
        edges.extend(
            object
                .get("edges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        hyperedges.extend(
            object
                .get("hyperedges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        input_tokens += object
            .get("input_tokens")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .unwrap_or_default();
        output_tokens += object
            .get("output_tokens")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .unwrap_or_default();
    }
    anyhow::ensure!(
        report.valid_chunks > 0,
        "no valid chunks to merge; refusing to write {}",
        output.as_ref().display()
    );
    report.nodes = nodes.len();
    report.edges = edges.len();
    report.hyperedges = hyperedges.len();
    let merged = serde_json::json!({
        "nodes": nodes,
        "edges": edges,
        "hyperedges": hyperedges,
        "input_tokens": json_number(input_tokens),
        "output_tokens": json_number(output_tokens),
    });
    write_json_atomic(output, &merged, false)?;
    Ok(report)
}

fn json_number(value: f64) -> Value {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}

#[cfg(test)]
mod bounded_fragment_loader_tests {
    use super::*;
    use std::io::{self, Read, Write};
    use tempfile::tempdir;

    fn fragment(label: &str) -> Value {
        serde_json::json!({
            "nodes": [{"id": "module_func", "label": label, "file_type": "code"}],
            "edges": [],
            "hyperedges": []
        })
    }

    #[cfg(unix)]
    #[test]
    fn opened_fragment_handle_is_not_swapped_by_path_replacement() {
        let temp = tempdir().expect("temporary fragment directory");
        let path = temp.path().join("chunk.json");
        let replacement = temp.path().join("replacement.json");
        let expected = fragment("opened generation");
        fs::write(
            &path,
            serde_json::to_vec(&expected).expect("serialize fragment"),
        )
        .expect("write original fragment");
        fs::write(
            &replacement,
            serde_json::to_vec(&fragment("replacement generation")).expect("serialize replacement"),
        )
        .expect("write replacement fragment");

        let file = fs::File::open(&path).expect("open original fragment");
        fs::rename(&replacement, &path).expect("atomically replace fragment path");

        let loaded = load_validated_semantic_fragment_from_open_file(
            &path,
            file,
            SemanticFragmentLimits::default(),
        )
        .expect("load opened generation");
        assert_eq!(loaded, expected);
    }

    #[test]
    fn fragment_growth_after_metadata_is_rejected_at_cap_plus_one() {
        let temp = tempdir().expect("temporary fragment directory");
        let path = temp.path().join("chunk.json");
        let bytes = serde_json::to_vec(&fragment("bounded")).expect("serialize fragment");
        fs::write(&path, &bytes).expect("write fragment");
        let file = fs::File::open(&path).expect("open fragment");
        let observed_size = file.metadata().expect("fragment metadata").len();
        let mut writer = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open fragment for growth");
        writer.write_all(b" ").expect("grow fragment");

        let limits = SemanticFragmentLimits {
            bytes: bytes.len(),
            ..SemanticFragmentLimits::default()
        };
        let errors =
            load_validated_semantic_fragment_from_reader(&path, file, observed_size, limits)
                .expect_err("growth beyond the observed cap must fail");
        assert_eq!(
            errors,
            vec![format!(
                "payload is {} bytes; max is {}",
                bytes.len() + 1,
                bytes.len()
            )]
        );
    }

    struct MustNotRead;

    impl Read for MustNotRead {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            panic!("oversized metadata must reject before reading")
        }
    }

    #[test]
    fn initially_oversized_fragment_preserves_the_size_error_and_is_not_read() {
        let limits = SemanticFragmentLimits {
            bytes: 64,
            ..SemanticFragmentLimits::default()
        };
        let errors = load_validated_semantic_fragment_from_reader(
            Path::new("chunk.json"),
            MustNotRead,
            65,
            limits,
        )
        .expect_err("oversized fragment must fail");
        assert_eq!(errors, vec!["payload is 65 bytes; max is 64"]);
    }
}
