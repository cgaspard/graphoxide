//! Tolerant ingestion of the loose extraction dictionaries accepted upstream.
//!
//! Extractors normally construct typed [`Extraction`] values. Semantic/LLM
//! backends and old `graph.json` files are less strict, so this module provides
//! the canonicalization boundary used when the input is arbitrary JSON.

use crate::build::{
    build_graph_with_options, build_graph_with_options_and_root, canonical_file_type, BuildOptions,
};
use graphoxide_core::{coerce_non_string_ids, Confidence, Edge, Extraction, KnowledgeGraph, Node};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// Flatten and deduplicate typed extraction chunks for the raw `--no-cluster`
/// artifact. Node IDs retain first-appearance order but use the last complete
/// record, while exact `(source, target, relation)` edge parallels keep their
/// first record. This mirrors the clustered graph boundary and ensures shrink
/// checks count unique nodes rather than duplicate vector entries.
pub fn dedupe_raw_extractions(extractions: &[Extraction]) -> Extraction {
    let mut node_order = Vec::new();
    let mut nodes_by_id = BTreeMap::new();
    let mut seen_edges = BTreeSet::new();
    let mut edges = Vec::new();
    let mut hyperedges = Vec::new();

    for extraction in extractions {
        for node in &extraction.nodes {
            if !nodes_by_id.contains_key(&node.id) {
                node_order.push(node.id.clone());
            }
            nodes_by_id.insert(node.id.clone(), node.clone());
        }
        for edge in &extraction.edges {
            let key = (
                edge.source.clone(),
                edge.target.clone(),
                edge.relation.clone(),
            );
            if seen_edges.insert(key) {
                edges.push(edge.clone());
            }
        }
        hyperedges.extend(extraction.hyperedges.iter().cloned());
    }

    Extraction {
        nodes: node_order
            .into_iter()
            .filter_map(|id| nodes_by_id.remove(&id))
            .collect(),
        edges,
        hyperedges,
    }
}

/// Structured counterpart of upstream's schema-warning summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestReport {
    pub issues: BTreeMap<String, usize>,
    pub skipped_nodes: usize,
    pub skipped_edges: usize,
}

impl IngestReport {
    pub fn issue_count(&self, cause: &str) -> usize {
        self.issues.get(cause).copied().unwrap_or_default()
    }

    fn issue(&mut self, cause: &str) {
        *self.issues.entry(cause.to_owned()).or_default() += 1;
    }
}

/// Collapse raw nodes by ID, retaining first-appearance order and the complete
/// last record for each ID.
pub fn dedupe_raw_nodes(nodes: &[Value]) -> Vec<Value> {
    let mut order = Vec::new();
    let mut by_id = BTreeMap::new();
    for node in nodes {
        let Some(id) = node.get("id").and_then(id_text) else {
            continue;
        };
        if !by_id.contains_key(&id) {
            order.push(id.clone());
        }
        by_id.insert(id, node.clone());
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

/// Collapse raw edges by `(source, target, relation)`, preserving the first
/// record and its metadata.
pub fn dedupe_raw_edges(edges: &[Value]) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for edge in edges {
        let key = (
            edge.get("source").and_then(id_text),
            edge.get("target").and_then(id_text),
            edge.get("relation")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
        if seen.insert(key) {
            output.push(edge.clone());
        }
    }
    output
}

/// Canonicalize a loose extraction object without allowing malformed IDs or
/// endpoints to abort the rest of a build.
pub fn canonicalize_extraction(value: &Value) -> (Extraction, IngestReport) {
    let mut report = IngestReport::default();
    let mut normalized = value.clone();
    coerce_non_string_ids(&mut normalized);
    let Some(root) = normalized.as_object() else {
        report.issue("extraction must be an object");
        return (Extraction::default(), report);
    };

    let mut extraction = Extraction::default();
    if let Some(nodes) = root.get("nodes").and_then(Value::as_array) {
        for raw in nodes {
            match parse_node(raw, &mut report) {
                Some(node) => extraction.nodes.push(node),
                None => report.skipped_nodes += 1,
            }
        }
    }
    let edge_values = root
        .get("edges")
        .or_else(|| root.get("links"))
        .and_then(Value::as_array);
    if let Some(edges) = edge_values {
        for raw in edges {
            match parse_edge(raw, &mut report) {
                Some(edge) => extraction.edges.push(edge),
                None => report.skipped_edges += 1,
            }
        }
    }
    extraction.hyperedges = root
        .get("hyperedges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    (extraction, report)
}

/// Canonicalize and build arbitrary extraction JSON.
pub fn build_graph_from_value(
    value: &Value,
    options: BuildOptions,
    root: Option<&Path>,
) -> anyhow::Result<(KnowledgeGraph, IngestReport)> {
    let (extraction, report) = canonicalize_extraction(value);
    let graph = if let Some(root) = root {
        build_graph_with_options_and_root(&[extraction], root, options)?
    } else {
        build_graph_with_options(&[extraction], options)?
    };
    Ok((graph, report))
}

/// Return all serialized edge records joining `u` and `v`. For an undirected
/// graph either endpoint order matches; a directed graph requires `u -> v`.
pub fn edge_datas<'a>(graph: &'a KnowledgeGraph, u: &str, v: &str) -> Vec<&'a Edge> {
    graph
        .links
        .iter()
        .filter(|edge| {
            (edge.true_source() == u && edge.true_target() == v)
                || (!graph.directed && edge.true_source() == v && edge.true_target() == u)
        })
        .collect()
}

/// Return the first edge record joining `u` and `v`, including when the loaded
/// graph declares `multigraph: true` and contains parallel links.
pub fn edge_data<'a>(graph: &'a KnowledgeGraph, u: &str, v: &str) -> Option<&'a Edge> {
    edge_datas(graph, u, v).into_iter().next()
}

fn parse_node(raw: &Value, report: &mut IngestReport) -> Option<Node> {
    let mut object = raw.as_object()?.clone();
    let id = object.get("id").and_then(id_text)?;
    object.remove("id");

    let label = fold_string_alias(&mut object, "label", "name").unwrap_or_default();
    if label.is_empty() {
        report.issue("missing required field 'label'");
    }
    let source_file = fold_node_source(&mut object);
    let file_type = object
        .remove("file_type")
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let source_location = object
        .remove("source_location")
        .and_then(|value| value.as_str().map(str::to_owned));
    let community = object.remove("community").and_then(|value| value.as_i64());

    Some(Node {
        id,
        label,
        file_type: canonical_file_type(&file_type).to_owned(),
        source_file,
        source_location,
        community,
        extra: object.into_iter().collect(),
    })
}

fn parse_edge(raw: &Value, report: &mut IngestReport) -> Option<Edge> {
    let mut object = raw.as_object()?.clone();
    let source = fold_id_alias(&mut object, "source", "from")?;
    let target = fold_id_alias(&mut object, "target", "to")?;
    let relation = fold_string_alias(&mut object, "relation", "type").unwrap_or_default();
    if relation.is_empty() {
        report.issue("missing required field 'relation'");
    }
    let confidence = match object.remove("confidence").as_ref().and_then(Value::as_str) {
        Some("INFERRED") => Confidence::Inferred,
        Some("AMBIGUOUS") => Confidence::Ambiguous,
        Some("EXTRACTED") => Confidence::Extracted,
        _ if object.contains_key("confidence_score") => Confidence::Inferred,
        _ => Confidence::Extracted,
    };
    let source_file = object
        .remove("source_file")
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    Some(Edge {
        source,
        target,
        relation,
        confidence,
        source_file,
        extra: object.into_iter().collect(),
    })
}

fn fold_node_source(object: &mut Map<String, Value>) -> String {
    if object
        .get("source_file")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
    {
        return object
            .remove("source_file")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
    }
    object.remove("source_file");
    for alias in ["source", "path"] {
        if object
            .get(alias)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        {
            return object
                .remove(alias)
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default();
        }
    }
    String::new()
}

fn fold_string_alias(
    object: &mut Map<String, Value>,
    canonical: &str,
    alias: &str,
) -> Option<String> {
    if object
        .get(canonical)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
    {
        return object
            .remove(canonical)
            .and_then(|value| value.as_str().map(str::to_owned));
    }
    object.remove(canonical);
    if object
        .get(alias)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
    {
        return object
            .remove(alias)
            .and_then(|value| value.as_str().map(str::to_owned));
    }
    None
}

fn fold_id_alias(object: &mut Map<String, Value>, canonical: &str, alias: &str) -> Option<String> {
    if let Some(value) = object.remove(canonical) {
        if let Some(id) = id_text(&value) {
            return Some(id);
        }
        return None;
    }
    object.remove(alias).as_ref().and_then(id_text)
}

fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}
