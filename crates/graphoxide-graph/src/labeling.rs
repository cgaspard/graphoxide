//! LLM-backed community naming.
//!
//! The transport is deliberately supplied by the caller. This keeps community
//! selection, batching, retry, response parsing, and usage accounting testable
//! without a network and lets the CLI share the same behavior with other
//! frontends.

use crate::GodNode;
use graphoxide_core::KnowledgeGraph;
use rayon::prelude::*;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

pub const DEFAULT_TOP_K: usize = 12;
pub const DEFAULT_BATCH_SIZE: usize = 100;
const LABEL_MAX_CHARS: usize = 60;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelUsage {
    pub input: u64,
    pub output: u64,
}

impl LabelUsage {
    fn add(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelResponse {
    pub content: String,
    pub usage: LabelUsage,
}

impl From<String> for LabelResponse {
    fn from(content: String) -> Self {
        Self {
            content,
            usage: LabelUsage::default(),
        }
    }
}

impl From<&str> for LabelResponse {
    fn from(content: &str) -> Self {
        content.to_owned().into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRequest {
    pub prompt: String,
    pub backend: String,
    pub model: Option<String>,
    pub max_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelingOptions {
    pub backend: String,
    pub model: Option<String>,
    pub max_communities: Option<usize>,
    pub top_k: usize,
    pub batch_size: usize,
    pub max_concurrency: usize,
    pub max_retry_depth: usize,
    pub allow_ollama_parallel: bool,
    pub allow_claude_cli_parallel: bool,
}

impl LabelingOptions {
    pub fn new(backend: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            model: None,
            max_communities: None,
            top_k: DEFAULT_TOP_K,
            batch_size: DEFAULT_BATCH_SIZE,
            max_concurrency: 4,
            max_retry_depth: 3,
            allow_ollama_parallel: false,
            allow_claude_cli_parallel: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelSource {
    Llm,
    Placeholder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedLabels {
    pub labels: BTreeMap<i64, String>,
    pub source: LabelSource,
    pub usage: LabelUsage,
}

#[derive(Debug)]
pub struct LabelingError {
    cause: anyhow::Error,
    pub usage: LabelUsage,
}

impl LabelingError {
    pub fn cause(&self) -> &anyhow::Error {
        &self.cause
    }
}

impl fmt::Display for LabelingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.cause.fmt(formatter)
    }
}

impl std::error::Error for LabelingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.source()
    }
}

#[derive(Debug)]
struct BatchSuccess {
    labels: BTreeMap<i64, String>,
    usage: LabelUsage,
}

#[derive(Debug)]
struct BatchFailure {
    error: anyhow::Error,
    usage: LabelUsage,
}

#[derive(Debug)]
struct BatchResult {
    index: usize,
    result: Result<BatchSuccess, BatchFailure>,
}

pub fn placeholder_community_labels(
    communities: &BTreeMap<i64, Vec<String>>,
) -> BTreeMap<i64, String> {
    communities
        .keys()
        .map(|community| (*community, format!("Community {community}")))
        .collect()
}

/// Build prompt rows, largest communities first, with god nodes leading each
/// community's representative sample.
pub fn community_label_lines(
    graph: &KnowledgeGraph,
    communities: &BTreeMap<i64, Vec<String>>,
    gods: &[GodNode],
    max_communities: Option<usize>,
    top_k: usize,
) -> (Vec<String>, Vec<i64>) {
    let nodes = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let god_ids = gods
        .iter()
        .map(|god| god.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut ordered = communities.iter().collect::<Vec<_>>();
    ordered.sort_by(|(left_id, left), (right_id, right)| {
        right
            .len()
            .cmp(&left.len())
            .then_with(|| left_id.cmp(right_id))
    });
    let cap = max_communities.unwrap_or(ordered.len());
    let mut lines = Vec::new();
    let mut labeled = Vec::new();
    for (community, members) in ordered.into_iter().take(cap) {
        let ranked = members
            .iter()
            .filter(|member| god_ids.contains(member.as_str()))
            .chain(
                members
                    .iter()
                    .filter(|member| !god_ids.contains(member.as_str())),
            );
        let mut names = Vec::new();
        let mut seen = BTreeSet::new();
        for member in ranked {
            let raw = nodes
                .get(member.as_str())
                .map_or(member.as_str(), |node| node.label.as_str());
            let label = raw
                .trim()
                .trim_matches(['(', ')'])
                .chars()
                .take(LABEL_MAX_CHARS)
                .collect::<String>();
            if !label.is_empty() && seen.insert(label.to_lowercase()) {
                names.push(label);
            }
            if names.len() >= top_k {
                break;
            }
        }
        if !names.is_empty() {
            lines.push(format!("Community {community}: {}", names.join(", ")));
            labeled.push(*community);
        }
    }
    (lines, labeled)
}

/// Parse a JSON `{community: name}` response, accepting markdown fences,
/// surrounding prose, and complete pairs from a truncated object.
pub fn parse_label_response(
    text: &str,
    labeled_communities: &[i64],
) -> anyhow::Result<BTreeMap<i64, String>> {
    let fence = Regex::new(r"(?i)^\s*```(?:json)?\s*|\s*```\s*$")
        .expect("the static label fence regex must compile");
    let mut cleaned = fence.replace_all(text.trim(), "").into_owned();
    if !cleaned.starts_with('{') {
        if let (Some(start), Some(end)) = (cleaned.find('{'), cleaned.rfind('}')) {
            if end > start {
                cleaned = cleaned[start..=end].to_owned();
            }
        }
    }
    let parsed = serde_json::from_str::<serde_json::Value>(&cleaned)
        .ok()
        .and_then(|value| value.as_object().cloned());
    let values = if let Some(parsed) = parsed {
        parsed
            .into_iter()
            .filter_map(|(key, value)| value.as_str().map(|value| (key, value.to_owned())))
            .collect::<BTreeMap<_, _>>()
    } else {
        let pair = Regex::new(r#"\"?(-?\d+)\"?\s*:\s*\"([^\"\\]*(?:\\.[^\"\\]*)*)\""#)
            .expect("the static label-pair regex must compile");
        let salvaged = pair
            .captures_iter(&cleaned)
            .filter_map(|captures| {
                let key = captures.get(1)?.as_str().to_owned();
                let encoded = format!("\"{}\"", captures.get(2)?.as_str());
                let value = serde_json::from_str::<String>(&encoded).ok()?;
                Some((key, value))
            })
            .collect::<BTreeMap<_, _>>();
        anyhow::ensure!(
            !salvaged.is_empty(),
            "label response is not parseable JSON: {:?}",
            text.chars().take(120).collect::<String>()
        );
        salvaged
    };
    Ok(labeled_communities
        .iter()
        .filter_map(|community| {
            values
                .get(&community.to_string())
                .map(|label| label.trim())
                .filter(|label| !label.is_empty())
                .map(|label| (*community, label.to_owned()))
        })
        .collect())
}

fn request_for_batch(
    community_ids: &[i64],
    lines: &[String],
    options: &LabelingOptions,
) -> LabelRequest {
    LabelRequest {
        prompt: format!(
            "You are naming clusters in a knowledge graph. For each community below, return a concise 2-5 word plain-language name describing what it is about (e.g. \"Order Management\", \"Payment Flow\", \"Auth Middleware\"). Respond ONLY with a JSON object mapping the community id (as a string) to its name - no prose, no markdown fences.\n\n{}",
            lines.join("\n")
        ),
        backend: options.backend.clone(),
        model: options.model.clone(),
        max_tokens: (256 + 48 * community_ids.len()).min(8192),
    }
}

fn label_batch_with_retry<F>(
    community_ids: &[i64],
    lines: &[String],
    options: &LabelingOptions,
    depth: usize,
    call: &F,
) -> Result<BatchSuccess, BatchFailure>
where
    F: Fn(&LabelRequest) -> anyhow::Result<LabelResponse> + Sync,
{
    let response =
        call(&request_for_batch(community_ids, lines, options)).map_err(|error| BatchFailure {
            error,
            usage: LabelUsage::default(),
        })?;
    let own_usage = response.usage.clone();
    match parse_label_response(&response.content, community_ids) {
        Ok(labels) => Ok(BatchSuccess {
            labels,
            usage: own_usage,
        }),
        Err(_error) if community_ids.len() > 1 && depth < options.max_retry_depth => {
            let middle = community_ids.len() / 2;
            let mut left = label_batch_with_retry(
                &community_ids[..middle],
                &lines[..middle],
                options,
                depth + 1,
                call,
            )
            .map_err(|mut failure| {
                failure.usage.add(&own_usage);
                failure
            })?;
            let right = label_batch_with_retry(
                &community_ids[middle..],
                &lines[middle..],
                options,
                depth + 1,
                call,
            )
            .map_err(|mut failure| {
                failure.usage.add(&own_usage);
                failure.usage.add(&left.usage);
                failure
            })?;
            left.labels.extend(right.labels);
            left.usage.add(&right.usage);
            left.usage.add(&own_usage);
            Ok(left)
        }
        Err(error) => Err(BatchFailure {
            error,
            usage: own_usage,
        }),
    }
}

/// Label every resolvable community, preserving placeholders for missing or
/// failed batches. An error is returned only when all attempted batches fail.
pub fn label_communities_with<F>(
    graph: &KnowledgeGraph,
    communities: &BTreeMap<i64, Vec<String>>,
    gods: &[GodNode],
    options: &LabelingOptions,
    call: F,
) -> Result<(BTreeMap<i64, String>, LabelUsage), LabelingError>
where
    F: Fn(&LabelRequest) -> anyhow::Result<LabelResponse> + Sync,
{
    if options.batch_size == 0 || options.top_k == 0 {
        return Err(LabelingError {
            cause: anyhow::anyhow!("batch_size and top_k must be greater than zero"),
            usage: LabelUsage::default(),
        });
    }
    let mut labels = placeholder_community_labels(communities);
    let (lines, community_ids) = community_label_lines(
        graph,
        communities,
        gods,
        options.max_communities,
        options.top_k,
    );
    if lines.is_empty() {
        return Ok((labels, LabelUsage::default()));
    }
    let batches = community_ids
        .chunks(options.batch_size)
        .zip(lines.chunks(options.batch_size))
        .enumerate()
        .map(|(index, (ids, lines))| (index, ids, lines))
        .collect::<Vec<_>>();
    let force_serial = (options.backend == "ollama" && !options.allow_ollama_parallel)
        || (options.backend == "claude-cli" && !options.allow_claude_cli_parallel);
    let workers = if force_serial {
        1
    } else {
        options.max_concurrency.max(1).min(batches.len())
    };
    let run = |(index, ids, lines): &(usize, &[i64], &[String])| BatchResult {
        index: *index,
        result: label_batch_with_retry(ids, lines, options, 0, &call),
    };
    let mut results = if workers == 1 {
        batches.iter().map(run).collect::<Vec<_>>()
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .map_err(|error| LabelingError {
                cause: anyhow::anyhow!("could not create labeling thread pool: {error}"),
                usage: LabelUsage::default(),
            })?;
        pool.install(|| batches.par_iter().map(run).collect::<Vec<_>>())
    };
    results.sort_by_key(|result| result.index);
    let mut usage = LabelUsage::default();
    let mut written = 0;
    let mut first_error = None;
    for result in results {
        match result.result {
            Ok(success) => {
                written += success.labels.len();
                labels.extend(success.labels);
                usage.add(&success.usage);
            }
            Err(failure) => {
                usage.add(&failure.usage);
                if first_error.is_none() {
                    first_error = Some(failure.error);
                }
            }
        }
    }
    if written == 0 {
        if let Some(error) = first_error {
            let message = format!(
                "all {} community-label batches failed: {error} (input tokens: {}, output tokens: {})",
                batches.len(), usage.input, usage.output
            );
            return Err(LabelingError {
                cause: error.context(message),
                usage,
            });
        }
    }
    Ok((labels, usage))
}

/// Graceful wrapper used by frontends: no backend or any total failure becomes
/// a complete placeholder map instead of an error.
pub fn generate_community_labels_with<F>(
    graph: &KnowledgeGraph,
    communities: &BTreeMap<i64, Vec<String>>,
    gods: &[GodNode],
    options: Option<&LabelingOptions>,
    call: F,
) -> GeneratedLabels
where
    F: Fn(&LabelRequest) -> anyhow::Result<LabelResponse> + Sync,
{
    let Some(options) = options else {
        return GeneratedLabels {
            labels: placeholder_community_labels(communities),
            source: LabelSource::Placeholder,
            usage: LabelUsage::default(),
        };
    };
    match label_communities_with(graph, communities, gods, options, call) {
        Ok((labels, usage)) => GeneratedLabels {
            labels,
            source: LabelSource::Llm,
            usage,
        },
        Err(error) => GeneratedLabels {
            labels: placeholder_community_labels(communities),
            source: LabelSource::Placeholder,
            usage: error.usage,
        },
    }
}
