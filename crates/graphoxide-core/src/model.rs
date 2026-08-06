//! Core schema types.
//!
//! The on-disk `graph.json` format must stay byte-compatible with the Python
//! upstream so existing graphs, the MCP clients, and the HTML viewers keep
//! working. Authority: upstream `export.to_json` (NetworkX node-link format).
//! Key facts (full detail in HANDOFF.md § "graph.json schema"):
//!
//! - Top-level keys: `directed`, `multigraph` (always false), `graph`,
//!   `nodes`, `links` (NOT `edges` — but readers must accept both),
//!   `hyperedges`, optional `built_at_commit`. There is NO version field.
//! - Storage is a *simple* graph; true edge direction lives in `_src`/`_tgt`
//!   attrs in memory and is restored onto `source`/`target` at write time.
//! - The raw `--no-cluster` writer emits `{nodes, edges, hyperedges, ...}`.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

/// Internal flattened attribute that ties recursively extracted container
/// facts to the scanned outer source that owns their lifecycle. The `!/`
/// spelling in `source_file` is reserved for virtual container members.
#[doc(hidden)]
pub const CONTAINER_SOURCE_ATTRIBUTE: &str = "_container_source";

/// Edge confidence labels, identical to upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Confidence {
    /// Relationship explicitly stated in source (import, direct call).
    #[default]
    Extracted,
    /// Reasonable deduction (call-graph second pass, co-occurrence).
    Inferred,
    /// Uncertain; flagged for human review in the report.
    Ambiguous,
}

impl Confidence {
    /// Upstream's confidence_score backfill values.
    pub fn default_score(self) -> f64 {
        match self {
            Confidence::Extracted => 1.0,
            Confidence::Inferred => 0.5,
            Confidence::Ambiguous => 0.2,
        }
    }
}

/// A node. Required upstream fields: id, label, file_type, source_file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// One of: code, document, paper, image, rationale, concept.
    #[serde(default = "default_file_type")]
    pub file_type: String,
    /// Repo-relative, forward slashes. Empty string for concept nodes. The
    /// `outer!/member` namespace is reserved for virtual container members.
    #[serde(default)]
    pub source_file: String,
    /// "L<line>" from AST extractors, absent/null from semantic extraction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_location: Option<String>,
    /// Community assignment set by clustering (absent pre-cluster).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community: Option<i64>,
    /// Catch-all for remaining upstream attrs (norm_label, community_name,
    /// _origin, repo, local_id, ...). Keeps round-tripping lossless.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// An edge ("link"). Required upstream fields: source, target, relation,
/// confidence, source_file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub source: String,
    pub target: String,
    #[serde(default = "default_relation")]
    pub relation: String,
    #[serde(default)]
    pub confidence: Confidence,
    #[serde(default)]
    pub source_file: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_file_type() -> String {
    "concept".into()
}

fn default_relation() -> String {
    String::new()
}

impl Edge {
    /// Directional source, preferring the in-memory marker used when an
    /// undirected storage graph canonicalized the serialized endpoints.
    pub fn true_source(&self) -> &str {
        self.extra
            .get("_src")
            .and_then(|value| value.as_str())
            .unwrap_or(&self.source)
    }

    /// Directional target paired with [`Edge::true_source`].
    pub fn true_target(&self) -> &str {
        self.extra
            .get("_tgt")
            .and_then(|value| value.as_str())
            .unwrap_or(&self.target)
    }
}

/// Output of one extractor run over one file: `{nodes, edges}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Extraction {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub hyperedges: Vec<serde_json::Value>,
}

/// The built knowledge graph as serialized to `graphoxide-out/graph.json`.
///
#[derive(Debug, Clone, Default, Serialize)]
pub struct KnowledgeGraph {
    #[serde(default)]
    pub directed: bool,
    #[serde(default)]
    pub multigraph: bool,
    pub nodes: Vec<Node>,
    /// NetworkX node-link naming: the edge list key is `links`.
    #[serde(default)]
    pub links: Vec<Edge>,
    #[serde(default)]
    pub hyperedges: Vec<serde_json::Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl<'de> Deserialize<'de> for KnowledgeGraph {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        normalize_graph_value(&mut value);
        #[derive(Deserialize)]
        struct WireGraph {
            #[serde(default)]
            directed: bool,
            #[serde(default)]
            multigraph: bool,
            #[serde(default)]
            nodes: Vec<Node>,
            #[serde(default)]
            links: Vec<Edge>,
            #[serde(default)]
            hyperedges: Vec<serde_json::Value>,
            #[serde(flatten)]
            extra: BTreeMap<String, serde_json::Value>,
        }
        let wire: WireGraph = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            directed: wire.directed,
            multigraph: wire.multigraph,
            nodes: wire.nodes,
            links: wire.links,
            hyperedges: wire.hyperedges,
            extra: wire.extra,
        })
    }
}

fn numeric_id(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Coerce only numeric graph identifiers to their exact JSON string spelling.
/// Booleans, nulls, arrays, and objects remain untouched so validation can
/// reject them rather than inventing misleading identities.
pub fn coerce_non_string_ids(value: &mut serde_json::Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    if let Some(nodes) = root
        .get_mut("nodes")
        .and_then(serde_json::Value::as_array_mut)
    {
        for node in nodes
            .iter_mut()
            .filter_map(serde_json::Value::as_object_mut)
        {
            if let Some(id) = node.get("id").and_then(numeric_id) {
                node.insert("id".into(), id.into());
            }
        }
    }
    for bucket in ["edges", "links"] {
        if let Some(edges) = root
            .get_mut(bucket)
            .and_then(serde_json::Value::as_array_mut)
        {
            for edge in edges
                .iter_mut()
                .filter_map(serde_json::Value::as_object_mut)
            {
                for field in ["source", "target", "from", "to"] {
                    if let Some(id) = edge.get(field).and_then(numeric_id) {
                        edge.insert(field.into(), id.into());
                    }
                }
            }
        }
    }
    if let Some(hyperedges) = root
        .get_mut("hyperedges")
        .and_then(serde_json::Value::as_array_mut)
    {
        for hyperedge in hyperedges
            .iter_mut()
            .filter_map(serde_json::Value::as_object_mut)
        {
            for field in ["nodes", "members", "node_ids"] {
                if let Some(members) = hyperedge
                    .get_mut(field)
                    .and_then(serde_json::Value::as_array_mut)
                {
                    for member in members {
                        if let Some(id) = numeric_id(member) {
                            *member = id.into();
                        }
                    }
                }
            }
        }
    }
}

/// Canonicalize legacy Graphify aliases in a raw extraction or built graph.
///
/// Validation intentionally does not call this automatically: callers that
/// accept legacy input should normalize first, exactly as Graphify's builder
/// does before applying its schema validator.
pub fn normalize_graph_value(value: &mut serde_json::Value) {
    coerce_non_string_ids(value);
    let Some(root) = value.as_object_mut() else {
        return;
    };
    if !root.contains_key("links") {
        if let Some(edges) = root.remove("edges") {
            root.insert("links".into(), edges);
        }
    } else {
        root.remove("edges");
    }

    if let Some(nodes) = root.get_mut("nodes").and_then(|v| v.as_array_mut()) {
        for node in nodes {
            let Some(node) = node.as_object_mut() else {
                continue;
            };
            // Synthetic/aggregate nodes in older graphs legitimately used JSON
            // null for these presentation fields. Exporters treat that as an
            // empty string; normalize before strongly typed deserialization.
            for field in ["label", "source_file"] {
                if node.get(field).is_some_and(serde_json::Value::is_null) {
                    node.insert(field.into(), serde_json::Value::String(String::new()));
                }
            }
            if let Some(id) = node.get("id").and_then(numeric_id) {
                node.insert("id".into(), id.into());
            }
            fold_string_alias(node, "label", "name");
            fold_string_alias(node, "source_file", "path");
            fold_string_alias(node, "source_file", "source");
            let valid = ["code", "document", "paper", "image", "rationale", "concept"];
            let file_type = node.get("file_type").and_then(|v| v.as_str()).unwrap_or("");
            if !valid.contains(&file_type) {
                let replacement = match file_type {
                    "markdown" | "text" => "document",
                    "tool" | "library" => "code",
                    _ => "concept",
                };
                node.insert("file_type".into(), replacement.into());
            }
        }
    }
    if let Some(edges) = root.get_mut("links").and_then(|v| v.as_array_mut()) {
        for edge in edges {
            let Some(edge) = edge.as_object_mut() else {
                continue;
            };
            for field in ["source", "target", "from", "to"] {
                if let Some(id) = edge.get(field).and_then(numeric_id) {
                    edge.insert(field.into(), id.into());
                }
            }
            fold_value_alias(edge, "source", "from");
            fold_value_alias(edge, "target", "to");
            fold_string_alias(edge, "relation", "type");
            if edge.get("confidence").is_none() && edge.get("confidence_score").is_some() {
                edge.insert("confidence".into(), "INFERRED".into());
            }
            if edge.get("source_file").is_none() {
                edge.insert("source_file".into(), "".into());
            }
        }
    }
    if let Some(hyperedges) = root.get_mut("hyperedges").and_then(|v| v.as_array_mut()) {
        for hyperedge in hyperedges {
            let Some(hyperedge) = hyperedge.as_object_mut() else {
                continue;
            };
            if !hyperedge.get("nodes").is_some_and(|v| v.is_array()) {
                for alias in ["members", "node_ids"] {
                    if hyperedge.get(alias).is_some_and(|v| v.is_array()) {
                        if let Some(members) = hyperedge.remove(alias) {
                            hyperedge.insert("nodes".into(), members);
                        }
                        break;
                    }
                }
            }
            hyperedge.remove("members");
            hyperedge.remove("node_ids");
        }
    }
}

fn fold_string_alias(
    object: &mut serde_json::Map<String, serde_json::Value>,
    canonical: &str,
    alias: &str,
) {
    let empty = object
        .get(canonical)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty();
    if empty
        && object
            .get(alias)
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty())
    {
        fold_value_alias(object, canonical, alias);
    }
}

fn fold_value_alias(
    object: &mut serde_json::Map<String, serde_json::Value>,
    canonical: &str,
    alias: &str,
) {
    if !object.contains_key(canonical)
        && let Some(value) = object.remove(alias)
    {
        object.insert(canonical.into(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerates_raw_and_legacy_graph_shapes() {
        let graph: KnowledgeGraph = serde_json::from_value(serde_json::json!({
            "nodes": [{"id": 10, "name": "Thing", "path": "src/a.py", "file_type": "tool"}],
            "edges": [{"from": 10, "to": 11, "type": "calls", "confidence_score": 0.8}],
            "hyperedges": [{"id": "h", "members": ["10", "11"]}],
            "input_tokens": 5
        }))
        .unwrap();
        assert_eq!(graph.nodes[0].id, "10");
        assert_eq!(graph.nodes[0].label, "Thing");
        assert_eq!(graph.nodes[0].file_type, "code");
        assert_eq!(graph.links[0].source, "10");
        assert_eq!(graph.links[0].target, "11");
        assert_eq!(graph.links[0].confidence, Confidence::Inferred);
        assert_eq!(
            graph.hyperedges[0]["nodes"],
            serde_json::json!(["10", "11"])
        );
        assert_eq!(graph.extra["input_tokens"], 5);
    }
}
