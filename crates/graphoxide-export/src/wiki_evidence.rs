//! Deterministic, graph-only evidence projection for wiki synthesis.

use crate::wiki::{catalog_sources, citation};
use graphoxide_core::{KnowledgeGraph, Node};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Source-grouped evidence retained from one graph projection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct WikiEvidenceProjection {
    pub sources: Vec<WikiEvidenceSource>,
}

/// Evidence and provenance for one catalog source/capture identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct WikiEvidenceSource {
    pub source_id: String,
    pub capture_id: String,
    pub graph_ref: String,
    pub citation: String,
    pub sha256: String,
    pub title_candidates: Vec<String>,
    pub source_system: Option<String>,
    pub url: Option<String>,
    pub location: Option<String>,
    pub source_path: Option<String>,
    pub representation: Option<String>,
    pub captured_at: Option<String>,
    pub accessed_at: Option<String>,
    pub updated_at: Option<String>,
    pub origins: Vec<String>,
    pub capabilities: Vec<String>,
    pub statuses: Vec<String>,
    pub diagnostics: Vec<WikiEvidenceDiagnostic>,
    pub blocks: Vec<WikiEvidenceBlock>,
}

/// A retained extractor or safe projection diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct WikiEvidenceDiagnostic {
    pub node_id: Option<String>,
    pub code: String,
    pub detail: Value,
}

/// One deterministic evidence block backed by a graph node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct WikiEvidenceBlock {
    pub id: String,
    pub node_id: String,
    pub kind: String,
    pub node_type: Option<String>,
    pub label: String,
    pub value: Option<Value>,
    pub value_type: Option<String>,
    pub heading_ancestry: Vec<String>,
    pub structured_path: Option<String>,
    pub source_file: String,
    pub source_location: Option<String>,
    pub page_number: Option<u64>,
    pub unit_ordinal: Option<u64>,
    pub unit_path: Option<String>,
    pub line: Option<u64>,
    pub line_end: Option<u64>,
    pub redacted_indicators: Vec<String>,
    pub truncated_indicators: Vec<String>,
}

/// Project a knowledge graph into deterministic, source-grouped wiki evidence.
///
/// This is pure graph projection: it performs no source or catalog reads. When
/// active annotations are supplied, their source/capture bindings are checked
/// by the same identity rules as `render_structured_wiki_with_catalog`.
pub fn project_wiki_evidence(
    graph: &KnowledgeGraph,
    active_annotations: Option<&BTreeMap<String, Value>>,
) -> anyhow::Result<WikiEvidenceProjection> {
    let empty = BTreeMap::new();
    let sources = catalog_sources(graph, active_annotations.unwrap_or(&empty))?;
    let source_refs = sources
        .values()
        .map(|source| {
            (
                (source.id.clone(), source.capture.clone()),
                source.graph_ref.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut nodes_by_id = BTreeMap::new();
    let mut node_sources = BTreeMap::new();
    let mut grouped = BTreeMap::<String, Vec<&Node>>::new();
    for node in &graph.nodes {
        anyhow::ensure!(
            nodes_by_id.insert(node.id.as_str(), node).is_none(),
            "wiki evidence requires unique node IDs"
        );
        let Some(identity) = node_catalog_identity(node) else {
            continue;
        };
        let graph_ref = source_refs
            .get(&identity)
            .expect("catalog_sources validated every catalog node");
        node_sources.insert(node.id.as_str(), graph_ref.as_str());
        grouped.entry(graph_ref.clone()).or_default().push(node);
    }

    let mut parents = BTreeMap::<&str, BTreeSet<&str>>::new();
    for edge in &graph.links {
        if edge.relation == "contains" {
            parents
                .entry(edge.true_target())
                .or_default()
                .insert(edge.true_source());
        }
    }

    let mut projected = Vec::with_capacity(sources.len());
    for source in sources.values() {
        let source_citation = citation(source);
        let mut source_nodes = grouped.remove(&source.graph_ref).unwrap_or_default();
        source_nodes.sort_by(|left, right| left.id.cmp(&right.id));
        let mut titles = source_nodes
            .iter()
            .map(|node| node.label.clone())
            .filter(|label| !label.is_empty())
            .collect::<BTreeSet<_>>();
        if titles.is_empty() {
            titles.insert(source.label.clone());
        }
        let origins = string_values(&source_nodes, &["_origin"]);
        let capabilities = string_values(&source_nodes, &["format_capability", "capability"]);
        let statuses = string_values(&source_nodes, &["parse_status", "status"]);
        let mut diagnostics = retained_diagnostics(&source_nodes);
        let mut blocks = Vec::with_capacity(source_nodes.len());

        for node in source_nodes {
            let heading_ancestry = match same_source_ancestors(
                node,
                &source.graph_ref,
                &parents,
                &nodes_by_id,
                &node_sources,
            ) {
                Ok(ancestors) => ancestors
                    .into_iter()
                    .filter(|ancestor| node_type(ancestor) == Some("document_heading"))
                    .map(|ancestor| ancestor.label.clone())
                    .collect(),
                Err((code, detail)) => {
                    diagnostics.push(WikiEvidenceDiagnostic {
                        node_id: Some(node.id.clone()),
                        code: code.into(),
                        detail: Value::String(detail),
                    });
                    Vec::new()
                }
            };
            blocks.push(project_block(
                source_citation.as_str(),
                node,
                heading_ancestry,
            ));
        }
        blocks.sort_by(block_order);
        diagnostics.sort_by(|left, right| {
            left.node_id
                .cmp(&right.node_id)
                .then_with(|| left.code.cmp(&right.code))
                .then_with(|| diagnostic_text(&left.detail).cmp(&diagnostic_text(&right.detail)))
        });

        let provenance = source.provenance.as_ref();
        projected.push(WikiEvidenceSource {
            source_id: source.id.clone(),
            capture_id: source.capture.clone(),
            graph_ref: source.graph_ref.clone(),
            citation: source_citation,
            sha256: source.sha256.clone(),
            title_candidates: titles.into_iter().collect(),
            source_system: provenance.and_then(|value| value.source_system.clone()),
            url: provenance.and_then(|value| value.url.clone()),
            location: provenance.and_then(|value| value.location.clone()),
            source_path: provenance.and_then(|value| value.source_path.clone()),
            representation: provenance.and_then(|value| value.representation.clone()),
            captured_at: provenance.and_then(|value| value.captured_at.clone()),
            accessed_at: provenance.and_then(|value| value.accessed_at.clone()),
            updated_at: provenance.and_then(|value| value.updated_at.clone()),
            origins,
            capabilities,
            statuses,
            diagnostics,
            blocks,
        });
    }
    Ok(WikiEvidenceProjection { sources: projected })
}

fn node_catalog_identity(node: &Node) -> Option<(String, String)> {
    let catalog = node.extra.get("catalog")?.as_object()?;
    Some((
        catalog.get("source_id")?.as_str()?.to_owned(),
        catalog.get("capture_id")?.as_str()?.to_owned(),
    ))
}

fn string_values(nodes: &[&Node], keys: &[&str]) -> Vec<String> {
    nodes
        .iter()
        .flat_map(|node| keys.iter().filter_map(|key| node.extra.get(*key)))
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn retained_diagnostics(nodes: &[&Node]) -> Vec<WikiEvidenceDiagnostic> {
    nodes
        .iter()
        .flat_map(|node| {
            node.extra
                .iter()
                .filter(|(key, _)| is_diagnostic_key(key))
                .map(move |(key, value)| WikiEvidenceDiagnostic {
                    node_id: Some(node.id.clone()),
                    code: key.clone(),
                    detail: value.clone(),
                })
        })
        .collect()
}

fn is_diagnostic_key(key: &str) -> bool {
    matches!(key, "diagnostic" | "diagnostics")
        || key.ends_with("_diagnostic")
        || key.ends_with("_diagnostics")
}

fn same_source_ancestors<'a>(
    node: &'a Node,
    source_ref: &str,
    parents: &BTreeMap<&str, BTreeSet<&str>>,
    nodes_by_id: &BTreeMap<&str, &'a Node>,
    node_sources: &BTreeMap<&str, &str>,
) -> Result<Vec<&'a Node>, (&'static str, String)> {
    let mut current = node.id.as_str();
    let mut visited = BTreeSet::from([current]);
    let mut ancestors = Vec::new();
    while let Some(candidates) = parents.get(current) {
        if candidates.len() != 1 {
            return Err((
                "containment_ambiguous_parent",
                format!("node {current} has {} contains parents", candidates.len()),
            ));
        }
        let parent_id = *candidates.first().expect("one parent");
        let Some(parent) = nodes_by_id.get(parent_id).copied() else {
            return Err((
                "containment_missing_parent",
                format!("contains parent {parent_id} is missing"),
            ));
        };
        if node_sources.get(parent_id).copied() != Some(source_ref) {
            return Err((
                "containment_foreign_parent",
                format!("contains parent {parent_id} belongs to another source"),
            ));
        }
        if !visited.insert(parent_id) {
            return Err((
                "containment_cycle",
                format!("contains ancestry for {} is cyclic", node.id),
            ));
        }
        ancestors.push(parent);
        current = parent_id;
    }
    ancestors.reverse();
    Ok(ancestors)
}

fn project_block(
    source_ref: &str,
    node: &Node,
    heading_ancestry: Vec<String>,
) -> WikiEvidenceBlock {
    let (value, value_type) = evidence_value(node);
    WikiEvidenceBlock {
        id: stable_block_id(source_ref, &node.id),
        node_id: node.id.clone(),
        kind: node.file_type.clone(),
        node_type: node_type(node).map(str::to_owned),
        label: node.label.clone(),
        value,
        value_type,
        heading_ancestry,
        structured_path: string_field(node, "structured_path"),
        source_file: node.source_file.clone(),
        source_location: node.source_location.clone(),
        page_number: number_field(node, "page_number"),
        unit_ordinal: number_field(node, "unit_ordinal"),
        unit_path: string_field(node, "internal_part"),
        line: number_field(node, "line_start")
            .or_else(|| number_field(node, "line"))
            .or_else(|| node.source_location.as_deref().and_then(location_line)),
        line_end: number_field(node, "line_end"),
        redacted_indicators: true_indicators(node, "redacted"),
        truncated_indicators: true_indicators(node, "truncated"),
    }
}

fn evidence_value(node: &Node) -> (Option<Value>, Option<String>) {
    for (value_key, type_key) in [
        ("structured_value", "structured_value_type"),
        ("structured_text", "structured_text_type"),
        ("text", "text_type"),
    ] {
        if let Some(value) = node.extra.get(value_key) {
            return (Some(value.clone()), string_field(node, type_key));
        }
    }
    (None, None)
}

fn node_type(node: &Node) -> Option<&str> {
    node.extra.get("type").and_then(Value::as_str)
}

fn string_field(node: &Node, key: &str) -> Option<String> {
    node.extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn number_field(node: &Node, key: &str) -> Option<u64> {
    node.extra.get(key).and_then(Value::as_u64)
}

fn location_line(location: &str) -> Option<u64> {
    location
        .strip_prefix('L')?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

fn true_indicators(node: &Node, marker: &str) -> Vec<String> {
    node.extra
        .iter()
        .filter(|(key, value)| key.contains(marker) && value.as_bool() == Some(true))
        .map(|(key, _)| key.clone())
        .collect()
}

fn stable_block_id(source_ref: &str, node_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"graphoxide-wiki-evidence-v1\0");
    digest.update(source_ref.as_bytes());
    digest.update(b"\0");
    digest.update(node_id.as_bytes());
    format!("wiki-evidence-{}", hex::encode(digest.finalize()))
}

fn block_order(left: &WikiEvidenceBlock, right: &WikiEvidenceBlock) -> std::cmp::Ordering {
    left.page_number
        .cmp(&right.page_number)
        .then_with(|| left.unit_ordinal.cmp(&right.unit_ordinal))
        .then_with(|| left.line.cmp(&right.line))
        .then_with(|| left.structured_path.cmp(&right.structured_path))
        .then_with(|| left.node_id.cmp(&right.node_id))
}

fn diagnostic_text(value: &Value) -> String {
    serde_json::to_string(value).expect("JSON values serialize")
}
