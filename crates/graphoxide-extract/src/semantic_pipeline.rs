//! Deterministic, bounded semantic chunk execution with adaptive truncation
//! recovery and incremental cache checkpoints.

use crate::cache::{save_semantic_cache, SemanticCacheOptions};
use graphoxide_core::{
    bisect_slice, expand_oversized_files, try_pack_chunks_by_tokens, unit_path, FileUnit,
    FILE_CHAR_CAP,
};
use rayon::prelude::*;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const CONTEXT_EXCEEDED_MARKERS: &[&str] = &[
    "context size",
    "context length",
    "context_length",
    "context window",
    "n_keep",
    "exceeds the available",
    "n_ctx",
    "maximum context",
    "too many tokens",
    "prompt is too long",
    "context_length_exceeded",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SemanticChunkResult {
    pub nodes: Vec<Value>,
    pub edges: Vec<Value>,
    pub hyperedges: Vec<Value>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model: Option<String>,
    pub finish_reason: String,
    pub partial_files: BTreeSet<PathBuf>,
    pub warnings: Vec<String>,
}

impl SemanticChunkResult {
    pub fn truncated(&self) -> bool {
        self.finish_reason == "length"
    }

    fn merge(left: Self, right: Self, model: Option<String>) -> Self {
        let mut result = Self {
            nodes: left.nodes,
            edges: left.edges,
            hyperedges: left.hyperedges,
            input_tokens: left.input_tokens.saturating_add(right.input_tokens),
            output_tokens: left.output_tokens.saturating_add(right.output_tokens),
            model,
            finish_reason: "stop".into(),
            partial_files: left.partial_files,
            warnings: left.warnings,
        };
        result.nodes.extend(right.nodes);
        result.edges.extend(right.edges);
        result.hyperedges.extend(right.hyperedges);
        result.partial_files.extend(right.partial_files);
        result.warnings.extend(right.warnings);
        result
    }

    fn mark_partial(&mut self, units: &[FileUnit], reason: String) {
        mark_partial_items(self);
        self.partial_files
            .extend(units.iter().map(|unit| unit_path(unit).to_path_buf()));
        self.warnings.push(reason);
    }

    fn strip_partial_markers(&mut self) {
        strip_partial_markers(self);
    }
}

/// Mark every returned graph item as an internal partial/truncated fragment.
pub fn mark_partial_items(result: &mut SemanticChunkResult) {
    for bucket in [&mut result.nodes, &mut result.edges, &mut result.hyperedges] {
        for item in bucket {
            if let Some(object) = item.as_object_mut() {
                object.insert("_partial".into(), Value::Bool(true));
            }
        }
    }
}

/// Union explicit empty-parse coverage with source files carried by marked
/// items. The former is essential when truncation produced no JSON objects.
pub fn partial_source_files(result: &SemanticChunkResult) -> BTreeSet<PathBuf> {
    let mut files = result.partial_files.clone();
    for item in result
        .nodes
        .iter()
        .chain(&result.edges)
        .chain(&result.hyperedges)
    {
        if item.get("_partial").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        if let Some(source) = item.get("source_file").and_then(Value::as_str) {
            files.insert(PathBuf::from(source));
        }
    }
    files
}

/// Remove cache-only partial markers before graph serialization.
pub fn strip_partial_markers(result: &mut SemanticChunkResult) {
    for bucket in [&mut result.nodes, &mut result.edges, &mut result.hyperedges] {
        for item in bucket {
            if let Some(object) = item.as_object_mut() {
                object.remove("_partial");
            }
        }
    }
}

/// Extract checkable ASCII identifier tokens from a semantic node label.
pub fn label_identifiers(label: &str) -> Vec<String> {
    let base = label.split_once('(').map_or(label, |(base, _)| base);
    let mut identifiers = Vec::new();
    let mut current = String::new();
    for character in base.chars() {
        if current.is_empty() {
            if character.is_ascii_alphabetic() || character == '_' {
                current.push(character);
            }
        } else if character.is_ascii_alphanumeric() || character == '_' {
            current.push(character);
        } else {
            if current.len() >= 3 {
                identifiers.push(std::mem::take(&mut current));
            }
            current.clear();
        }
    }
    if current.len() >= 3 {
        identifiers.push(current);
    }
    identifiers
}

/// Flag code-typed semantic nodes whose claimed symbol has no textual evidence
/// in the exact source units dispatched to the model. Nodes are retained and
/// marked `verification = "unverified"`; already-hedged nodes are unchanged.
pub fn bind_node_evidence(nodes: &mut [Value], units: &[FileUnit], root: &Path) -> usize {
    if !nodes.iter().any(|node| {
        node.get("file_type").and_then(Value::as_str) == Some("code")
            && node.get("source_file").and_then(Value::as_str).is_some()
    }) {
        return 0;
    }
    let mut source_by_path = BTreeMap::<PathBuf, String>::new();
    for unit in units {
        let path = resolved(root, unit_path(unit));
        let content = match unit {
            FileUnit::Slice(slice) => graphoxide_core::read_slice_text(slice),
            FileUnit::Path(path) => {
                std::fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            }
        };
        let Ok(content) = content else {
            continue;
        };
        let capped: String = content.chars().take(FILE_CHAR_CAP).collect();
        source_by_path
            .entry(path)
            .or_default()
            .push_str(&capped.to_lowercase());
    }
    let mut downgraded = 0;
    for node in nodes {
        let Some(object) = node.as_object_mut() else {
            continue;
        };
        if object.get("file_type").and_then(Value::as_str) != Some("code") {
            continue;
        }
        let Some(source_file) = object.get("source_file").and_then(Value::as_str) else {
            continue;
        };
        let path = resolved(root, Path::new(source_file));
        let Some(source) = source_by_path.get(&path) else {
            continue;
        };
        let mut identifiers = label_identifiers(
            object
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        identifiers.extend(label_identifiers(
            object.get("id").and_then(Value::as_str).unwrap_or_default(),
        ));
        if identifiers.is_empty()
            || identifiers
                .iter()
                .any(|identifier| source.contains(&identifier.to_lowercase()))
        {
            continue;
        }
        let confidence = object
            .get("confidence")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(confidence, "" | "EXTRACTED") && !object.contains_key("verification") {
            object.insert("verification".into(), "unverified".into());
            downgraded += 1;
        }
    }
    downgraded
}

/// Keep manifest stamps only for semantic-tier files that actually produced a
/// complete item in this run. Code and other locally extracted tiers pass
/// through unchanged.
pub fn stamped_manifest_files(
    files_by_type: &BTreeMap<String, Vec<PathBuf>>,
    result: &SemanticChunkResult,
    root: &Path,
    partial_files: &BTreeSet<PathBuf>,
) -> BTreeMap<String, Vec<PathBuf>> {
    let resolve = |path: &Path| resolved(root, path);
    let extracted = result
        .nodes
        .iter()
        .chain(&result.edges)
        .chain(&result.hyperedges)
        .filter_map(|item| item.get("source_file").and_then(Value::as_str))
        .map(|source| resolve(Path::new(source)))
        .collect::<BTreeSet<_>>();
    let partial = partial_files
        .iter()
        .map(|path| resolve(path))
        .collect::<BTreeSet<_>>();
    let semantic_types = BTreeSet::from(["document", "paper", "image"]);
    files_by_type
        .iter()
        .map(|(kind, files)| {
            let kept = files
                .iter()
                .filter(|path| {
                    !semantic_types.contains(kind.as_str())
                        || (extracted.contains(&resolve(path)) && !partial.contains(&resolve(path)))
                })
                .cloned()
                .collect();
            (kind.clone(), kept)
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct SemanticCorpusOptions {
    pub token_budget: Option<usize>,
    pub chunk_size: usize,
    pub max_concurrency: usize,
    pub max_retry_depth: usize,
    pub checkpoint: bool,
    pub cache: SemanticCacheOptions,
}

impl Default for SemanticCorpusOptions {
    fn default() -> Self {
        Self {
            token_budget: Some(60_000),
            chunk_size: 20,
            max_concurrency: 4,
            max_retry_depth: 3,
            checkpoint: true,
            cache: SemanticCacheOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SemanticCorpusResult {
    pub nodes: Vec<Value>,
    pub edges: Vec<Value>,
    pub hyperedges: Vec<Value>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub failed_chunks: usize,
    pub uncovered_files: Vec<PathBuf>,
    pub out_of_scope_dropped: usize,
    pub warnings: Vec<String>,
}

pub type SemanticChunkCallback<'a> = dyn Fn(usize, usize, &SemanticChunkResult) + Sync + 'a;

pub fn looks_like_context_exceeded(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_lowercase();
    CONTEXT_EXCEEDED_MARKERS
        .iter()
        .any(|marker| message.contains(marker))
}

pub fn extract_with_adaptive_retry<F>(
    units: &[FileUnit],
    max_depth: usize,
    extractor: &F,
) -> anyhow::Result<SemanticChunkResult>
where
    F: Fn(&[FileUnit]) -> anyhow::Result<SemanticChunkResult> + Sync,
{
    adaptive_retry_at_depth(units, max_depth, 0, extractor)
}

fn adaptive_retry_at_depth<F>(
    units: &[FileUnit],
    max_depth: usize,
    depth: usize,
    extractor: &F,
) -> anyhow::Result<SemanticChunkResult>
where
    F: Fn(&[FileUnit]) -> anyhow::Result<SemanticChunkResult> + Sync,
{
    let (attempted, context_error) = match extractor(units) {
        Ok(result) => (result, false),
        Err(error) if looks_like_context_exceeded(&error) => (
            SemanticChunkResult {
                finish_reason: "length".into(),
                warnings: vec![format!("context-window overflow: {error}")],
                ..SemanticChunkResult::default()
            },
            true,
        ),
        Err(error) => return Err(error),
    };
    if !attempted.truncated() {
        return Ok(attempted);
    }

    let split = if depth < max_depth && units.len() > 1 {
        let midpoint = units.len() / 2;
        Some((units[..midpoint].to_vec(), units[midpoint..].to_vec()))
    } else if depth < max_depth && units.len() == 1 {
        match &units[0] {
            FileUnit::Slice(slice) => bisect_slice(slice)
                .map(|(left, right)| (vec![FileUnit::Slice(left)], vec![FileUnit::Slice(right)])),
            FileUnit::Path(_) => None,
        }
    } else {
        None
    };

    let Some((left_units, right_units)) = split else {
        // A path cannot be divided safely here. A transport-level context
        // rejection contains no usable partial JSON, so return an empty,
        // successful fragment and let the rest of the corpus continue. An
        // actual truncated response is retained and marked below.
        if context_error && units.len() == 1 && matches!(units.first(), Some(FileUnit::Path(_))) {
            let mut empty = attempted;
            empty.finish_reason = "stop".into();
            empty
                .warnings
                .push("single-file context-window overflow produced no usable fragment".into());
            return Ok(empty);
        }
        let mut partial = attempted;
        let reason = if units.len() == 1 {
            "single-file chunk is still truncated; retained a marked partial result".into()
        } else {
            format!(
                "chunk is still truncated at adaptive retry depth {depth}/{max_depth}; retained a marked partial result"
            )
        };
        partial.mark_partial(units, reason);
        return Ok(partial);
    };

    let left = adaptive_retry_at_depth(&left_units, max_depth, depth + 1, extractor)?;
    let right = adaptive_retry_at_depth(&right_units, max_depth, depth + 1, extractor)?;
    Ok(SemanticChunkResult::merge(left, right, attempted.model))
}

pub fn extract_corpus<F>(
    files: &[PathBuf],
    root: &Path,
    options: &SemanticCorpusOptions,
    extractor: &F,
    on_chunk_done: Option<&SemanticChunkCallback<'_>>,
) -> anyhow::Result<SemanticCorpusResult>
where
    F: Fn(&[FileUnit]) -> anyhow::Result<SemanticChunkResult> + Sync,
{
    anyhow::ensure!(options.chunk_size > 0, "chunk_size must be positive");
    let units = expand_oversized_files(files, FILE_CHAR_CAP);
    let chunks = if let Some(budget) = options.token_budget {
        try_pack_chunks_by_tokens(&units, budget)?
    } else {
        units
            .chunks(options.chunk_size)
            .map(<[FileUnit]>::to_vec)
            .collect()
    };
    let total = chunks.len();
    let workers = options.max_concurrency.max(1).min(total.max(1));
    let execute = |(index, chunk): (usize, &Vec<FileUnit>)| {
        let result = extract_with_adaptive_retry(chunk, options.max_retry_depth, extractor);
        if let (Some(callback), Ok(result)) = (on_chunk_done, &result) {
            callback(index, total, result);
        }
        (index, result)
    };
    let mut completed: Vec<_> = if workers == 1 {
        chunks.iter().enumerate().map(execute).collect()
    } else {
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()?
            .install(|| chunks.par_iter().enumerate().map(execute).collect())
    };
    completed.sort_by_key(|(index, _)| *index);

    let mut merged = SemanticCorpusResult::default();
    for (index, outcome) in completed {
        let mut result = match outcome {
            Ok(result) => result,
            Err(error) => {
                merged.failed_chunks += 1;
                merged
                    .warnings
                    .push(format!("chunk {}/{} failed: {error}", index + 1, total));
                continue;
            }
        };
        let unverified = bind_node_evidence(&mut result.nodes, &chunks[index], root);
        if unverified > 0 {
            result.warnings.push(format!(
                "flagged {unverified} semantic code node(s) as unverified: no matching identifier was present in the dispatched source"
            ));
        }
        if options.checkpoint {
            let mut cache_options = options.cache.clone();
            cache_options.merge_existing = true;
            cache_options.allowed_source_files = Some(
                chunks[index]
                    .iter()
                    .map(|unit| unit_path(unit).to_path_buf())
                    .collect(),
            );
            cache_options.partial_source_files =
                (!result.partial_files.is_empty()).then(|| result.partial_files.clone());
            match save_semantic_cache(
                &result.nodes,
                &result.edges,
                &result.hyperedges,
                root,
                &cache_options,
            ) {
                Ok(report) => result.warnings.extend(report.warnings),
                Err(error) => result
                    .warnings
                    .push(format!("incremental cache checkpoint failed: {error}")),
            }
        }
        result.strip_partial_markers();
        merged.nodes.extend(result.nodes);
        merged.edges.extend(result.edges);
        merged.hyperedges.extend(result.hyperedges);
        merged.input_tokens = merged.input_tokens.saturating_add(result.input_tokens);
        merged.output_tokens = merged.output_tokens.saturating_add(result.output_tokens);
        merged.warnings.extend(result.warnings);
    }

    if merged.failed_chunks > 0 {
        merged.warnings.push(format!(
            "WARNING: {}/{} semantic extraction chunk(s) failed",
            merged.failed_chunks, total
        ));
    }
    reconcile_scope(&mut merged, &chunks, root);
    Ok(merged)
}

fn resolved(root: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn reconcile_scope(result: &mut SemanticCorpusResult, chunks: &[Vec<FileUnit>], root: &Path) {
    let dispatched = chunks
        .iter()
        .flatten()
        .map(|unit| resolved(root, unit_path(unit)))
        .collect::<BTreeSet<_>>();
    let mut returned = BTreeSet::new();
    let mut dropped_ids = BTreeSet::new();
    let mut dropped_files = BTreeSet::new();
    let out_of_scope = |item: &Value| {
        let Some(source) = item.get("source_file").and_then(Value::as_str) else {
            return false;
        };
        let path = resolved(root, Path::new(source));
        path.is_file() && !dispatched.contains(&path)
    };
    let node_count_before = result.nodes.len();
    result.nodes.retain(|node| {
        let Some(source) = node.get("source_file").and_then(Value::as_str) else {
            return true;
        };
        let path = resolved(root, Path::new(source));
        if out_of_scope(node) {
            if let Some(id) = node.get("id").and_then(Value::as_str) {
                dropped_ids.insert(id.to_owned());
            }
            dropped_files.insert(Path::new(source).to_path_buf());
            return false;
        }
        if dispatched.contains(&path) {
            returned.insert(path);
        }
        true
    });
    result.out_of_scope_dropped = node_count_before - result.nodes.len();
    if result.out_of_scope_dropped > 0 {
        result.edges.retain(|edge| {
            !out_of_scope(edge)
                && !edge
                    .get("source")
                    .and_then(Value::as_str)
                    .is_some_and(|id| dropped_ids.contains(id))
                && !edge
                    .get("target")
                    .and_then(Value::as_str)
                    .is_some_and(|id| dropped_ids.contains(id))
        });
        result.hyperedges.retain(|hyperedge| {
            !out_of_scope(hyperedge)
                && !hyperedge
                    .get("nodes")
                    .and_then(Value::as_array)
                    .is_some_and(|members| {
                        members.iter().any(|member| {
                            member.as_str().is_some_and(|id| dropped_ids.contains(id))
                        })
                    })
        });
        let shown = dropped_files
            .iter()
            .take(5)
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        let more = dropped_files
            .len()
            .checked_sub(5)
            .filter(|remaining| *remaining > 0)
            .map_or_else(String::new, |remaining| format!(" (+{remaining} more)"));
        result.warnings.push(format!(
            "dropped {} out-of-scope semantic node(s) attributed to file(s) not dispatched for extraction: {shown}{more}",
            result.out_of_scope_dropped
        ));
    }
    result.uncovered_files = dispatched.difference(&returned).cloned().collect();
    if !result.uncovered_files.is_empty() {
        result.warnings.push(format!(
            "{} dispatched file(s) produced no nodes: {}",
            result.uncovered_files.len(),
            result
                .uncovered_files
                .iter()
                .take(5)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
}
