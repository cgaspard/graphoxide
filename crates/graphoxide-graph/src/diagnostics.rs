//! Read-only diagnostics for parallel-edge loss in simple graphs.
//!
//! The normal Graphoxide graph format is intentionally compatible with the
//! simple node-link graph emitted by Graphify.  That format cannot explain
//! which raw producer facts were overwritten when several edges share the same
//! endpoints.  This module audits a raw extraction (or an already-built graph)
//! without changing it and quantifies that risk before a migration to a true
//! multigraph representation.

use anyhow::Context;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// Controls one in-memory or file-backed diagnostic run.
#[derive(Debug, Clone)]
pub struct DiagnosticOptions {
    /// Simulate a directed (`DiGraph`) or undirected (`Graph`) simple build.
    pub directed: bool,
    /// Bound the detailed same-endpoint examples retained in the report.
    pub max_examples: usize,
    /// Optional extractor source to scan for producer-side `seen_*` sets.
    pub extract_path: Option<PathBuf>,
}

impl Default for DiagnosticOptions {
    fn default() -> Self {
        Self {
            directed: true,
            max_examples: 5,
            extract_path: None,
        }
    }
}

/// One likely producer-side suppression set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuppressionSite {
    pub line: usize,
    pub name: String,
    pub tuple_arity: usize,
    pub sample: String,
}

/// Result of scanning extractor source for `seen_*` deduplication sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerSuppression {
    pub path: String,
    pub total_sites: usize,
    pub sites: Vec<SuppressionSite>,
    pub error: String,
}

impl ProducerSuppression {
    fn compiled_extractor() -> Self {
        Self {
            path: "<compiled extractor>".into(),
            total_sites: 0,
            sites: Vec::new(),
            error: String::new(),
        }
    }
}

/// A bounded example of several facts sharing the same directed endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SameEndpointExample {
    pub source: String,
    pub target: String,
    pub edge_count: usize,
    pub relations: Vec<String>,
    pub source_files: Vec<String>,
    pub source_locations: Vec<String>,
    pub contexts: Vec<String>,
}

/// Machine-readable parallel-edge loss audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultigraphDiagnosticSummary {
    pub node_count: usize,
    pub unverified_node_count: usize,
    pub raw_edge_count: usize,
    pub non_object_edges: usize,
    pub missing_endpoint_edges: usize,
    pub dangling_endpoint_edges: usize,
    pub self_loop_edges: usize,
    pub valid_candidate_edges: usize,
    pub exact_duplicate_edges: usize,
    pub directed_unique_endpoint_pairs: usize,
    pub directed_same_endpoint_collapsed_edges: usize,
    pub undirected_unique_endpoint_pairs: usize,
    pub undirected_same_endpoint_collapsed_edges: usize,
    pub same_endpoint_group_count: usize,
    pub relation_variant_groups: usize,
    pub source_file_variant_groups: usize,
    pub source_location_variant_groups: usize,
    pub context_variant_groups: usize,
    pub post_build_graph_type: String,
    pub post_build_node_count: Option<usize>,
    pub post_build_edge_count: Option<usize>,
    pub post_build_error: String,
    pub producer_suppression: ProducerSuppression,
    pub examples: Vec<SameEndpointExample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_directed: Option<bool>,
}

#[derive(Debug, Clone)]
struct CanonicalEdge {
    source: String,
    target: String,
    relation: String,
    source_file: String,
    source_location: String,
    context: String,
    invalid: bool,
}

#[derive(Debug, Clone, Default)]
struct PairGroup {
    first_index: usize,
    edges: Vec<CanonicalEdge>,
}

/// Find likely `seen_*` producer-suppression sets in an extractor source file.
pub fn scan_producer_suppression_sites(path: impl AsRef<Path>) -> ProducerSuppression {
    let path = path.as_ref();
    let rendered_path = path.display().to_string();
    if !path.exists() {
        return ProducerSuppression {
            path: rendered_path,
            total_sites: 0,
            sites: Vec::new(),
            error: "file not found".into(),
        };
    }

    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            return ProducerSuppression {
                path: rendered_path,
                total_sites: 0,
                sites: Vec::new(),
                error: error.to_string(),
            };
        }
    };
    let declaration = Regex::new(r"^\s*(seen_[A-Za-z0-9_]+)\s*[:=]")
        .expect("static suppression declaration regex");
    let tuple = Regex::new(r"set\[tuple\[([^\]]*)\]\]").expect("static tuple annotation regex");
    let mut sites = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let Some(captures) = declaration.captures(line) else {
            continue;
        };
        let inside = tuple
            .captures(line)
            .and_then(|capture| capture.get(1))
            .map(|capture| capture.as_str().trim())
            .unwrap_or_default();
        let tuple_arity = if inside.is_empty() {
            0
        } else {
            inside.matches(',').count() + 1
        };
        sites.push(SuppressionSite {
            line: index + 1,
            name: captures[1].to_owned(),
            tuple_arity,
            sample: line.trim().chars().take(120).collect(),
        });
    }
    ProducerSuppression {
        path: rendered_path,
        total_sites: sites.len(),
        sites,
        error: String::new(),
    }
}

/// Summarize same-endpoint edge-collapse risk without mutating `extraction`.
pub fn diagnose_extraction(
    extraction: &Value,
    options: &DiagnosticOptions,
) -> MultigraphDiagnosticSummary {
    let node_values = extraction
        .get("nodes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let node_ids = node_values
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|node| node.get("id"))
        .filter(|id| !id.is_null())
        .map(safe_text)
        .collect::<BTreeSet<_>>();
    let unverified_node_count = node_values
        .iter()
        .filter_map(Value::as_object)
        .filter(|node| node.get("verification").and_then(Value::as_str) == Some("unverified"))
        .count();
    let edge_value = match extraction.get("edges") {
        None | Some(Value::Null) => extraction.get("links"),
        edges => edges,
    };
    let raw_edges = edge_value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    let mut exact_counts = BTreeMap::<String, usize>::new();
    let mut directed_pairs = BTreeMap::<(String, String), usize>::new();
    let mut undirected_pairs = BTreeMap::<(String, String), usize>::new();
    let mut grouped = BTreeMap::<(String, String), PairGroup>::new();
    let mut non_object_edges = 0;
    let mut missing_endpoint_edges = 0;
    let mut dangling_endpoint_edges = 0;
    let mut self_loop_edges = 0;
    let mut valid_candidate_edges = 0;

    for (index, raw_edge) in raw_edges.iter().enumerate() {
        *exact_counts.entry(exact_signature(raw_edge)).or_default() += 1;
        let edge = canonical_edge(raw_edge);
        if edge.invalid {
            non_object_edges += 1;
            continue;
        }
        if edge.source.is_empty() || edge.target.is_empty() {
            missing_endpoint_edges += 1;
            continue;
        }
        if !node_ids.contains(&edge.source) || !node_ids.contains(&edge.target) {
            dangling_endpoint_edges += 1;
            continue;
        }
        if edge.source == edge.target {
            self_loop_edges += 1;
        }
        valid_candidate_edges += 1;
        let directed_pair = (edge.source.clone(), edge.target.clone());
        let undirected_pair = if edge.source <= edge.target {
            directed_pair.clone()
        } else {
            (edge.target.clone(), edge.source.clone())
        };
        *directed_pairs.entry(directed_pair.clone()).or_default() += 1;
        *undirected_pairs.entry(undirected_pair).or_default() += 1;
        grouped
            .entry(directed_pair)
            .or_insert_with(|| PairGroup {
                first_index: index,
                edges: Vec::new(),
            })
            .edges
            .push(edge);
    }

    let mut example_groups = grouped
        .iter()
        .filter(|(_, group)| group.edges.len() > 1)
        .collect::<Vec<_>>();
    example_groups
        .sort_by_key(|(_, group)| (std::cmp::Reverse(group.edges.len()), group.first_index));
    let examples = example_groups
        .into_iter()
        .take(options.max_examples)
        .map(|((source, target), group)| SameEndpointExample {
            source: source.clone(),
            target: target.clone(),
            edge_count: group.edges.len(),
            relations: distinct_sorted(&group.edges, |edge| &edge.relation),
            source_files: distinct_sorted(&group.edges, |edge| &edge.source_file),
            source_locations: distinct_sorted(&group.edges, |edge| &edge.source_location),
            contexts: distinct_sorted(&group.edges, |edge| &edge.context),
        })
        .collect();

    let malformed_build = malformed_build_error(node_values, raw_edges);
    let (post_build_graph_type, post_build_node_count, post_build_edge_count, post_build_error) =
        if let Some(error) = malformed_build {
            (String::new(), None, None, error)
        } else {
            let edge_count = if options.directed {
                directed_pairs.len()
            } else {
                undirected_pairs.len()
            };
            (
                if options.directed { "DiGraph" } else { "Graph" }.into(),
                Some(node_ids.len()),
                Some(edge_count),
                String::new(),
            )
        };
    let producer_suppression = options.extract_path.as_ref().map_or_else(
        ProducerSuppression::compiled_extractor,
        scan_producer_suppression_sites,
    );

    MultigraphDiagnosticSummary {
        node_count: node_ids.len(),
        unverified_node_count,
        raw_edge_count: raw_edges.len(),
        non_object_edges,
        missing_endpoint_edges,
        dangling_endpoint_edges,
        self_loop_edges,
        valid_candidate_edges,
        exact_duplicate_edges: count_extra(exact_counts.values().copied()),
        directed_unique_endpoint_pairs: directed_pairs.len(),
        directed_same_endpoint_collapsed_edges: count_extra(directed_pairs.values().copied()),
        undirected_unique_endpoint_pairs: undirected_pairs.len(),
        undirected_same_endpoint_collapsed_edges: count_extra(undirected_pairs.values().copied()),
        same_endpoint_group_count: directed_pairs.values().filter(|count| **count > 1).count(),
        relation_variant_groups: variant_group_count(&grouped, Field::Relation, false),
        source_file_variant_groups: variant_group_count(&grouped, Field::SourceFile, true),
        source_location_variant_groups: variant_group_count(&grouped, Field::SourceLocation, true),
        context_variant_groups: variant_group_count(&grouped, Field::Context, true),
        post_build_graph_type,
        post_build_node_count,
        post_build_edge_count,
        post_build_error,
        producer_suppression,
        examples,
        input_path: None,
        effective_directed: None,
    }
}

/// Diagnose one graph/extraction JSON file using the configured graph size cap.
pub fn diagnose_file(
    path: impl AsRef<Path>,
    directed: Option<bool>,
    max_examples: usize,
    extract_path: Option<&Path>,
) -> anyhow::Result<MultigraphDiagnosticSummary> {
    diagnose_file_with_cap(
        path,
        directed,
        max_examples,
        extract_path,
        graphoxide_core::max_graph_bytes(),
    )
}

/// Explicit-size-cap form used by embedders and tests.
pub fn diagnose_file_with_cap(
    path: impl AsRef<Path>,
    directed: Option<bool>,
    max_examples: usize,
    extract_path: Option<&Path>,
    max_bytes: u64,
) -> anyhow::Result<MultigraphDiagnosticSummary> {
    let path = path.as_ref();
    graphoxide_core::check_graph_file_size_cap_with(path, max_bytes)?;
    let bytes = fs::read(path).with_context(|| format!("Cannot parse {}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "Cannot parse {}: {error}. The file may be corrupted; re-run 'graphoxide extract'",
            path.display()
        )
    })?;
    anyhow::ensure!(value.is_object(), "diagnostic input must be a JSON object");
    let effective_directed = directed.unwrap_or_else(|| {
        value
            .get("directed")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    });
    let options = DiagnosticOptions {
        directed: effective_directed,
        max_examples,
        extract_path: extract_path.map(Path::to_path_buf),
    };
    let mut summary = diagnose_extraction(&value, &options);
    summary.input_path = Some(path.display().to_string());
    summary.effective_directed = Some(effective_directed);
    Ok(summary)
}

/// Wrap a summary in the stable machine-readable report envelope.
pub fn format_diagnostic_json(summary: &MultigraphDiagnosticSummary) -> Value {
    let mut summary_value =
        serde_json::to_value(summary).expect("diagnostic summary is serializable");
    if let Some(object) = summary_value.as_object_mut() {
        object.remove("examples");
        object.remove("producer_suppression");
    }
    serde_json::json!({
        "schema_version": 1,
        "summary": summary_value,
        "examples": summary.examples,
        "producer_suppression": summary.producer_suppression,
        "notes": [
            "Diagnostics are read-only.",
            "A normal graph.json is already post-build and cannot recover raw producer edges.",
            "Producer suppression sites are heuristic source-code evidence."
        ]
    })
}

/// Render the human-facing parallel-edge loss report.
pub fn format_diagnostic_report(summary: &MultigraphDiagnosticSummary) -> String {
    let suppression = &summary.producer_suppression;
    let mut lines = vec![
        "[graphoxide] MultiDiGraph edge-collapse diagnostic".into(),
        format!(
            "input: {}",
            summary.input_path.as_deref().unwrap_or("<in-memory>")
        ),
        "input_stage: provided JSON (normal graph.json is post-build)".into(),
        format!(
            "effective_directed: {}",
            summary
                .effective_directed
                .map(|value| if value { "True" } else { "False" })
                .unwrap_or("<direct-call>")
        ),
        format!("nodes: {}", summary.node_count),
        format!("unverified_code_nodes: {}", summary.unverified_node_count),
        format!("raw_edges: {}", summary.raw_edge_count),
        format!("valid_candidate_edges: {}", summary.valid_candidate_edges),
        format!("missing_endpoint_edges: {}", summary.missing_endpoint_edges),
        format!(
            "dangling_endpoint_edges: {}",
            summary.dangling_endpoint_edges
        ),
        format!("self_loop_edges: {}", summary.self_loop_edges),
        format!("exact_duplicate_edges: {}", summary.exact_duplicate_edges),
        format!(
            "directed_unique_endpoint_pairs: {}",
            summary.directed_unique_endpoint_pairs
        ),
        format!(
            "directed_same_endpoint_collapsed_edges: {}",
            summary.directed_same_endpoint_collapsed_edges
        ),
        format!(
            "undirected_unique_endpoint_pairs: {}",
            summary.undirected_unique_endpoint_pairs
        ),
        format!(
            "undirected_same_endpoint_collapsed_edges: {}",
            summary.undirected_same_endpoint_collapsed_edges
        ),
        format!(
            "same_endpoint_group_count: {}",
            summary.same_endpoint_group_count
        ),
        format!(
            "relation_variant_groups: {}",
            summary.relation_variant_groups
        ),
        format!(
            "source_file_variant_groups: {}",
            summary.source_file_variant_groups
        ),
        format!(
            "source_location_variant_groups: {}",
            summary.source_location_variant_groups
        ),
        format!("context_variant_groups: {}", summary.context_variant_groups),
        format!("post_build_graph_type: {}", summary.post_build_graph_type),
        format!(
            "post_build_edges: {}",
            summary
                .post_build_edge_count
                .map_or_else(|| "None".into(), |count| count.to_string())
        ),
        format!("producer_suppression_sites: {}", suppression.total_sites),
    ];
    if !summary.post_build_error.is_empty() {
        lines.push(format!("post_build_error: {}", summary.post_build_error));
    }
    if !suppression.error.is_empty() {
        lines.push(format!("producer_suppression_error: {}", suppression.error));
    }
    if !suppression.sites.is_empty() {
        lines.push("producer_suppression_examples:".into());
        for site in suppression.sites.iter().take(8) {
            let arity = if site.tuple_arity == 0 {
                "unknown".into()
            } else {
                site.tuple_arity.to_string()
            };
            lines.push(format!("  - L{} {} arity={arity}", site.line, site.name));
        }
    }
    if !summary.examples.is_empty() {
        lines.push("examples:".into());
        for example in &summary.examples {
            lines.push(format!(
                "  - {} -> {} edges={} relations={:?} locations={:?} contexts={:?}",
                example.source,
                example.target,
                example.edge_count,
                example.relations,
                example.source_locations,
                example.contexts
            ));
        }
    }
    lines.push(
        "note: normal graph.json is post-build; raw producer loss must be measured earlier.".into(),
    );
    lines.join("\n")
}

fn safe_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Bool(value) => {
            if *value {
                "True".into()
            } else {
                "False".into()
            }
        }
        Value::Number(value) => value.to_string(),
        value => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn canonical_edge(raw: &Value) -> CanonicalEdge {
    let Some(edge) = raw.as_object() else {
        return CanonicalEdge {
            source: String::new(),
            target: String::new(),
            relation: String::new(),
            source_file: String::new(),
            source_location: String::new(),
            context: String::new(),
            invalid: true,
        };
    };
    CanonicalEdge {
        source: edge
            .get("source")
            .or_else(|| edge.get("from"))
            .map(safe_text)
            .unwrap_or_default(),
        target: edge
            .get("target")
            .or_else(|| edge.get("to"))
            .map(safe_text)
            .unwrap_or_default(),
        relation: edge.get("relation").map(safe_text).unwrap_or_default(),
        source_file: edge.get("source_file").map(safe_text).unwrap_or_default(),
        source_location: edge
            .get("source_location")
            .map(safe_text)
            .unwrap_or_default(),
        context: edge.get("context").map(safe_text).unwrap_or_default(),
        invalid: false,
    }
}

fn exact_signature(raw: &Value) -> String {
    let Some(object) = raw.as_object() else {
        return "<non-object>".into();
    };
    let mut normalized = object.clone();
    if !normalized.contains_key("source") {
        if let Some(source) = normalized.get("from").cloned() {
            normalized.insert("source".into(), source);
        }
    }
    if !normalized.contains_key("target") {
        if let Some(target) = normalized.get("to").cloned() {
            normalized.insert("target".into(), target);
        }
    }
    normalized.remove("from");
    normalized.remove("to");
    serde_json::to_string(&normalized).expect("JSON map serialization cannot fail")
}

fn count_extra(counts: impl IntoIterator<Item = usize>) -> usize {
    counts
        .into_iter()
        .map(|count| count.saturating_sub(1))
        .sum()
}

fn distinct_sorted<'a>(
    edges: &'a [CanonicalEdge],
    field: impl Fn(&'a CanonicalEdge) -> &'a String,
) -> Vec<String> {
    edges
        .iter()
        .map(field)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[derive(Clone, Copy)]
enum Field {
    Relation,
    SourceFile,
    SourceLocation,
    Context,
}

fn field(edge: &CanonicalEdge, requested: Field) -> &str {
    match requested {
        Field::Relation => &edge.relation,
        Field::SourceFile => &edge.source_file,
        Field::SourceLocation => &edge.source_location,
        Field::Context => &edge.context,
    }
}

fn variant_group_count(
    grouped: &BTreeMap<(String, String), PairGroup>,
    requested: Field,
    relation_sensitive: bool,
) -> usize {
    grouped
        .values()
        .map(|group| {
            if relation_sensitive {
                let mut by_relation = BTreeMap::<&str, BTreeSet<&str>>::new();
                for edge in &group.edges {
                    by_relation
                        .entry(&edge.relation)
                        .or_default()
                        .insert(field(edge, requested));
                }
                by_relation
                    .values()
                    .filter(|variants| variants.len() > 1)
                    .count()
            } else if group
                .edges
                .iter()
                .map(|edge| field(edge, requested))
                .collect::<BTreeSet<_>>()
                .len()
                > 1
            {
                1
            } else {
                0
            }
        })
        .sum()
}

fn malformed_build_error(nodes: &[Value], edges: &[Value]) -> Option<String> {
    if nodes.iter().any(|node| !node.is_object()) {
        return Some("TypeError: node data must be a JSON object".into());
    }
    if edges.iter().any(|edge| {
        let Some(edge) = edge.as_object() else {
            return true;
        };
        edge.get("source")
            .or_else(|| edge.get("from"))
            .into_iter()
            .chain(edge.get("target").or_else(|| edge.get("to")))
            .any(|endpoint| endpoint.is_array() || endpoint.is_object())
    }) {
        return Some("TypeError: edge data or endpoint is not hashable".into());
    }
    None
}
