//! Deterministic graph facts for explicit, model-authored enrichment.
//!
//! This module is deliberately transport-free. Callers must finish consent,
//! redaction, provider I/O, response validation, and compare-and-swap checks
//! before applying records here. Validation finishes before the input graph is
//! mutated, preserving the caller's all-or-nothing publication boundary.

use graphoxide_core::{make_id, Confidence, Edge, KnowledgeGraph, Node};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};
use thiserror::Error;

pub const MEDIA_TRANSCRIPT_SUMMARY_PROFILE: &str = "media-transcript-summary-v1";
pub const ENRICHMENT_SCHEMA_VERSION: u32 = 1;
pub const REDACTION_VERSION: &str = "redaction-v1";
pub const ENRICHMENT_DATA_BOUNDARY: &str = "redacted_transcript_text_only";
pub const MAX_ENRICHMENT_SUMMARY_BYTES: usize = 4 * 1024;
pub const MAX_ENRICHMENT_TOPICS: usize = 16;
pub const MAX_ENRICHMENT_TOPIC_BYTES: usize = 256;
pub const MAX_ENRICHMENT_MODEL_BYTES: usize = 256;
pub const MAX_ENRICHMENT_SOURCE_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaTranscriptSummaryRecord {
    pub source_node_id: String,
    pub source_file: String,
    pub profile: String,
    pub provider: String,
    pub model: String,
    pub redacted_input_sha256: String,
    pub redaction_count: u64,
    pub summary: String,
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EnrichmentApplyReport {
    pub added: usize,
    pub replaced: usize,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EnrichmentApplyError {
    #[error("invalid enrichment record: {0}")]
    InvalidRecord(&'static str),
    #[error("media inventory node not found: {0}")]
    MissingMediaNode(String),
    #[error("graph contains duplicate node ID: {0}")]
    DuplicateGraphNodeId(String),
    #[error("node is not an eligible media inventory fact: {0}")]
    NotMediaInventory(String),
    #[error("media inventory source mismatch for {0}")]
    SourceMismatch(String),
    #[error("duplicate enrichment record for {source_file} and {profile}")]
    DuplicateRecord {
        source_file: String,
        profile: String,
    },
    #[error("enrichment node ID collides with a foreign graph fact: {0}")]
    ForeignIdCollision(String),
    #[error("foreign graph fact references enrichment node being inserted or replaced: {0}")]
    ForeignReference(String),
    #[error("graph allocation refused while applying enrichment facts")]
    Allocation,
}

/// Whether a graph node is an offline inventory fact that a caller may further
/// qualify for this media profile. The byte adapter intentionally does not need
/// to parse arbitrary audio/video payloads: an extension-owned input can retain
/// a generic `format_inventory` root when its bytes are opaque or malformed.
/// Callers must additionally require the canonical registry's `Video`
/// classification for `source_file`; this graph-only crate cannot own that
/// extraction-layer policy.
pub fn is_media_inventory_node(node: &Node) -> bool {
    if node.extra.get("_origin").and_then(Value::as_str) == Some("enrichment") {
        return false;
    }
    let node_type = node.extra.get("type").and_then(Value::as_str);
    let format = node.extra.get("format").and_then(Value::as_str);
    matches!(node_type, Some("media" | "format_inventory"))
        || matches!(format, Some("media" | "additional-media"))
        || node.file_type == "video"
}

/// A stable source/profile-owned ID. The digest suffix keeps punctuation,
/// case, and Unicode-normalization-distinct source paths from aliasing through
/// the compatibility `make_id` normalization.
pub fn media_transcript_summary_id(source_file: &str, profile: &str) -> String {
    let readable = make_id(&["enrichment", source_file, profile]);
    let mut digest = Sha256::new();
    digest.update(b"graphoxide-enrichment-node-v1\0");
    digest.update(source_file.as_bytes());
    digest.update(b"\0");
    digest.update(profile.as_bytes());
    let suffix = hex::encode(&digest.finalize()[..12]);
    format!("{readable}_{suffix}")
}

/// Apply a complete batch of validated summaries. Every fallible validation
/// and reservation occurs before removal or insertion into `graph`.
pub fn apply_media_transcript_summaries(
    graph: &mut KnowledgeGraph,
    records: &[MediaTranscriptSummaryRecord],
) -> Result<EnrichmentApplyReport, EnrichmentApplyError> {
    let mut ordered = records.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        (
            left.source_file.as_str(),
            left.profile.as_str(),
            left.source_node_id.as_str(),
        )
            .cmp(&(
                right.source_file.as_str(),
                right.profile.as_str(),
                right.source_node_id.as_str(),
            ))
    });

    let mut node_by_id = BTreeMap::new();
    for node in &graph.nodes {
        if node_by_id.insert(node.id.as_str(), node).is_some() {
            return Err(EnrichmentApplyError::DuplicateGraphNodeId(node.id.clone()));
        }
    }
    let mut requested = BTreeSet::new();
    let mut replacement_ids = BTreeSet::new();
    let mut staged_ids = BTreeSet::new();
    let mut staged_nodes = Vec::new();
    let mut staged_edges = Vec::new();

    for record in ordered {
        validate_record(record)?;
        let key = (record.source_file.clone(), record.profile.clone());
        if !requested.insert(key.clone()) {
            return Err(EnrichmentApplyError::DuplicateRecord {
                source_file: key.0,
                profile: key.1,
            });
        }
        let source = node_by_id
            .get(record.source_node_id.as_str())
            .ok_or_else(|| EnrichmentApplyError::MissingMediaNode(record.source_node_id.clone()))?;
        if !is_media_inventory_node(source) {
            return Err(EnrichmentApplyError::NotMediaInventory(
                record.source_node_id.clone(),
            ));
        }
        if source.source_file != record.source_file {
            return Err(EnrichmentApplyError::SourceMismatch(
                record.source_node_id.clone(),
            ));
        }

        for node in &graph.nodes {
            if enrichment_key(node).as_ref() == Some(&key) {
                replacement_ids.insert(node.id.clone());
            }
        }

        let id = media_transcript_summary_id(&record.source_file, &record.profile);
        if let Some(existing) = node_by_id.get(id.as_str())
            && enrichment_key(existing).as_ref() != Some(&key)
        {
            return Err(EnrichmentApplyError::ForeignIdCollision(id));
        }
        if !staged_ids.insert(id.clone()) {
            return Err(EnrichmentApplyError::ForeignIdCollision(id));
        }

        let mut topics = record.topics.clone();
        topics.sort();
        topics.dedup();
        let label = Path::new(&record.source_file)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(&record.source_file);
        let extra = BTreeMap::from([
            ("_origin".into(), Value::from("enrichment")),
            (
                "data_boundary".into(),
                Value::from(ENRICHMENT_DATA_BOUNDARY),
            ),
            ("model".into(), Value::from(record.model.clone())),
            ("profile".into(), Value::from(record.profile.clone())),
            ("provider".into(), Value::from(record.provider.clone())),
            (
                "redacted_input_sha256".into(),
                Value::from(record.redacted_input_sha256.clone()),
            ),
            (
                "redaction_count".into(),
                Value::from(record.redaction_count),
            ),
            ("redaction_version".into(), Value::from(REDACTION_VERSION)),
            (
                "schema_version".into(),
                Value::from(ENRICHMENT_SCHEMA_VERSION),
            ),
            ("summary".into(), Value::from(record.summary.clone())),
            ("topics".into(), Value::from(topics)),
            ("type".into(), Value::from("media_transcript_summary")),
            (
                "verification".into(),
                Value::from("unverified_model_output"),
            ),
        ]);
        staged_nodes.push(Node {
            id: id.clone(),
            label: format!("Transcript summary: {label}"),
            file_type: "concept".into(),
            source_file: record.source_file.clone(),
            source_location: None,
            community: source.community,
            extra,
        });
        staged_edges.push(Edge {
            source: record.source_node_id.clone(),
            target: id.clone(),
            relation: "has_enrichment".into(),
            confidence: Confidence::Ambiguous,
            source_file: record.source_file.clone(),
            extra: BTreeMap::from([
                ("_origin".into(), Value::from("enrichment")),
                ("_src".into(), Value::from(record.source_node_id.clone())),
                ("_tgt".into(), Value::from(id)),
                ("profile".into(), Value::from(record.profile.clone())),
                (
                    "schema_version".into(),
                    Value::from(ENRICHMENT_SCHEMA_VERSION),
                ),
            ]),
        });
    }

    validate_replacement_references(graph, &replacement_ids, &staged_ids)?;
    graph
        .nodes
        .try_reserve(staged_nodes.len())
        .map_err(|_| EnrichmentApplyError::Allocation)?;
    graph
        .links
        .try_reserve(staged_edges.len())
        .map_err(|_| EnrichmentApplyError::Allocation)?;

    let replaced = replacement_ids.len();
    graph
        .nodes
        .retain(|node| !replacement_ids.contains(&node.id));
    graph.links.retain(|edge| {
        !replacement_ids.contains(edge.true_source())
            && !replacement_ids.contains(edge.true_target())
    });
    graph
        .hyperedges
        .retain(|hyperedge| !hyperedge_mentions_any(hyperedge, &replacement_ids));
    graph.nodes.extend(staged_nodes);
    graph.links.extend(staged_edges);
    Ok(EnrichmentApplyReport {
        added: records.len(),
        replaced,
    })
}

fn validate_record(record: &MediaTranscriptSummaryRecord) -> Result<(), EnrichmentApplyError> {
    if record.profile != MEDIA_TRANSCRIPT_SUMMARY_PROFILE {
        return Err(EnrichmentApplyError::InvalidRecord("unsupported profile"));
    }
    if record.provider != "openai-compatible" {
        return Err(EnrichmentApplyError::InvalidRecord("unsupported provider"));
    }
    if record.source_node_id.is_empty()
        || record.source_file.is_empty()
        || record.source_file.len() > MAX_ENRICHMENT_SOURCE_BYTES
        || !safe_relative_source(&record.source_file)
    {
        return Err(EnrichmentApplyError::InvalidRecord("invalid source"));
    }
    if record.model.is_empty()
        || record.model.len() > MAX_ENRICHMENT_MODEL_BYTES
        || has_forbidden_controls(&record.model, false)
    {
        return Err(EnrichmentApplyError::InvalidRecord("invalid model"));
    }
    if record.redacted_input_sha256.len() != 64
        || !record
            .redacted_input_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(EnrichmentApplyError::InvalidRecord("invalid input digest"));
    }
    if record.summary.is_empty()
        || record.summary.len() > MAX_ENRICHMENT_SUMMARY_BYTES
        || has_forbidden_controls(&record.summary, true)
    {
        return Err(EnrichmentApplyError::InvalidRecord("invalid summary"));
    }
    if record.topics.is_empty() || record.topics.len() > MAX_ENRICHMENT_TOPICS {
        return Err(EnrichmentApplyError::InvalidRecord("invalid topics"));
    }
    if record.topics.iter().any(|topic| {
        topic.is_empty()
            || topic.len() > MAX_ENRICHMENT_TOPIC_BYTES
            || has_forbidden_controls(topic, false)
    }) {
        return Err(EnrichmentApplyError::InvalidRecord("invalid topic"));
    }
    Ok(())
}

fn safe_relative_source(source: &str) -> bool {
    if source.contains(['\\', '\0']) || source.contains("!/") || source.contains(':') {
        return false;
    }
    let path = Path::new(source);
    !path.is_absolute()
        && path.components().all(|component| {
            matches!(component, Component::Normal(_))
                && component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|value| !value.is_empty() && value != "." && value != "..")
        })
}

fn has_forbidden_controls(value: &str, allow_layout: bool) -> bool {
    value.chars().any(|character| {
        character.is_control() && !(allow_layout && matches!(character, '\n' | '\r' | '\t'))
    })
}

fn enrichment_key(node: &Node) -> Option<(String, String)> {
    if node.extra.get("_origin")?.as_str()? != "enrichment" {
        return None;
    }
    if node.extra.get("type")?.as_str()? != "media_transcript_summary" {
        return None;
    }
    Some((
        node.source_file.clone(),
        node.extra.get("profile")?.as_str()?.to_owned(),
    ))
}

fn validate_replacement_references(
    graph: &KnowledgeGraph,
    replacement_ids: &BTreeSet<String>,
    staged_ids: &BTreeSet<String>,
) -> Result<(), EnrichmentApplyError> {
    let first_insert_ids = staged_ids
        .difference(replacement_ids)
        .cloned()
        .collect::<BTreeSet<_>>();
    for edge in &graph.links {
        let first_insert_reference = first_insert_ids.contains(edge.true_source())
            || first_insert_ids.contains(edge.true_target());
        let foreign_replacement_reference = (replacement_ids.contains(edge.true_source())
            || replacement_ids.contains(edge.true_target()))
            && edge.extra.get("_origin").and_then(Value::as_str) != Some("enrichment");
        if first_insert_reference || foreign_replacement_reference {
            return Err(EnrichmentApplyError::ForeignReference(
                edge.relation.clone(),
            ));
        }
    }
    for hyperedge in &graph.hyperedges {
        let first_insert_reference = hyperedge_mentions_any(hyperedge, &first_insert_ids);
        let foreign_replacement_reference = hyperedge_mentions_any(hyperedge, replacement_ids)
            && hyperedge.get("_origin").and_then(Value::as_str) != Some("enrichment");
        if first_insert_reference || foreign_replacement_reference {
            return Err(EnrichmentApplyError::ForeignReference("hyperedge".into()));
        }
    }
    Ok(())
}

fn hyperedge_mentions_any(value: &Value, ids: &BTreeSet<String>) -> bool {
    ["nodes", "members", "node_ids"].into_iter().any(|field| {
        value
            .get(field)
            .and_then(Value::as_array)
            .is_some_and(|members| {
                members
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|member| ids.contains(member))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn media(id: &str, source_file: &str) -> Node {
        Node {
            id: id.into(),
            label: source_file.into(),
            file_type: "video".into(),
            source_file: source_file.into(),
            source_location: None,
            community: Some(7),
            extra: BTreeMap::from([
                ("type".into(), json!("format_inventory")),
                ("format".into(), json!("media")),
            ]),
        }
    }

    fn record(source: &str, summary: &str) -> MediaTranscriptSummaryRecord {
        MediaTranscriptSummaryRecord {
            source_node_id: "talk".into(),
            source_file: source.into(),
            profile: MEDIA_TRANSCRIPT_SUMMARY_PROFILE.into(),
            provider: "openai-compatible".into(),
            model: "recorded-model".into(),
            redacted_input_sha256: "a".repeat(64),
            redaction_count: 2,
            summary: summary.into(),
            topics: vec!["zeta".into(), "alpha".into(), "alpha".into()],
        }
    }

    #[test]
    fn stable_id_distinguishes_paths_lost_by_compatibility_normalization() {
        assert_ne!(
            media_transcript_summary_id("media/a-b.mp4", MEDIA_TRANSCRIPT_SUMMARY_PROFILE),
            media_transcript_summary_id("media/a_b.mp4", MEDIA_TRANSCRIPT_SUMMARY_PROFILE)
        );
    }

    #[test]
    fn generic_inventory_root_can_be_qualified_by_the_caller_registry() {
        let mut node = media("talk", "media/talk.mp4");
        node.extra.remove("format");
        assert_eq!(node.extra.get("type"), Some(&json!("format_inventory")));
        assert!(is_media_inventory_node(&node));
    }

    #[test]
    fn replacement_is_idempotent_and_keeps_source_owned_provenance() {
        let mut graph = KnowledgeGraph {
            nodes: vec![media("talk", "media/talk.mp4")],
            ..KnowledgeGraph::default()
        };
        let first = apply_media_transcript_summaries(
            &mut graph,
            &[record("media/talk.mp4", "First summary")],
        )
        .unwrap();
        assert_eq!(
            first,
            EnrichmentApplyReport {
                added: 1,
                replaced: 0
            }
        );
        let enrichment_id = graph
            .nodes
            .iter()
            .find(|node| node.extra.get("_origin") == Some(&json!("enrichment")))
            .unwrap()
            .id
            .clone();
        assert_eq!(graph.links[0].confidence, Confidence::Ambiguous);
        assert_eq!(graph.links[0].true_source(), "talk");
        assert_eq!(graph.links[0].true_target(), enrichment_id);

        let second = apply_media_transcript_summaries(
            &mut graph,
            &[record("media/talk.mp4", "Replacement summary")],
        )
        .unwrap();
        assert_eq!(
            second,
            EnrichmentApplyReport {
                added: 1,
                replaced: 1
            }
        );
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.links.len(), 1);
        let enriched = graph
            .nodes
            .iter()
            .find(|node| node.id == enrichment_id)
            .unwrap();
        assert_eq!(enriched.extra["summary"], "Replacement summary");
        assert_eq!(enriched.extra["topics"], json!(["alpha", "zeta"]));
        assert_eq!(enriched.community, Some(7));
    }

    #[test]
    fn foreign_id_or_reference_collision_leaves_graph_unchanged() {
        let candidate = record("media/talk.mp4", "Summary");
        let id = media_transcript_summary_id(&candidate.source_file, &candidate.profile);
        let foreign = Node {
            id,
            label: "foreign".into(),
            file_type: "concept".into(),
            source_file: "foreign.md".into(),
            source_location: None,
            community: None,
            extra: BTreeMap::new(),
        };
        let mut graph = KnowledgeGraph {
            nodes: vec![media("talk", "media/talk.mp4"), foreign],
            ..KnowledgeGraph::default()
        };
        let before = serde_json::to_value(&graph).unwrap();
        assert!(matches!(
            apply_media_transcript_summaries(&mut graph, &[candidate]),
            Err(EnrichmentApplyError::ForeignIdCollision(_))
        ));
        assert_eq!(serde_json::to_value(&graph).unwrap(), before);
    }

    #[test]
    fn foreign_reference_blocks_replacement_without_partial_mutation() {
        let candidate = record("media/talk.mp4", "First");
        let mut graph = KnowledgeGraph {
            nodes: vec![media("talk", "media/talk.mp4")],
            ..KnowledgeGraph::default()
        };
        apply_media_transcript_summaries(&mut graph, std::slice::from_ref(&candidate)).unwrap();
        let enrichment_id = media_transcript_summary_id(&candidate.source_file, &candidate.profile);
        graph.links.push(Edge {
            source: "foreign".into(),
            target: enrichment_id,
            relation: "references".into(),
            confidence: Confidence::Extracted,
            source_file: "foreign.md".into(),
            extra: BTreeMap::new(),
        });
        let before = serde_json::to_value(&graph).unwrap();
        let mut replacement = candidate;
        replacement.summary = "Replacement".into();
        assert!(matches!(
            apply_media_transcript_summaries(&mut graph, &[replacement]),
            Err(EnrichmentApplyError::ForeignReference(_))
        ));
        assert_eq!(serde_json::to_value(&graph).unwrap(), before);
    }

    #[test]
    fn foreign_dangling_references_cannot_claim_a_first_insert_id() {
        fn assert_rejected(mut graph: KnowledgeGraph) {
            let before = serde_json::to_value(&graph).unwrap();
            assert!(matches!(
                apply_media_transcript_summaries(
                    &mut graph,
                    &[record("media/talk.mp4", "Summary")]
                ),
                Err(EnrichmentApplyError::ForeignReference(_))
            ));
            assert_eq!(serde_json::to_value(&graph).unwrap(), before);
        }

        let enrichment_id =
            media_transcript_summary_id("media/talk.mp4", MEDIA_TRANSCRIPT_SUMMARY_PROFILE);
        for (source, target) in [
            (enrichment_id.as_str(), "talk"),
            ("talk", enrichment_id.as_str()),
        ] {
            assert_rejected(KnowledgeGraph {
                nodes: vec![media("talk", "media/talk.mp4")],
                links: vec![Edge {
                    source: source.into(),
                    target: target.into(),
                    relation: "foreign_dangling".into(),
                    confidence: Confidence::Extracted,
                    source_file: "foreign.md".into(),
                    extra: BTreeMap::new(),
                }],
                ..KnowledgeGraph::default()
            });
        }

        for field in ["nodes", "members", "node_ids"] {
            let mut hyperedge = json!({
                "id": format!("foreign-{field}"),
                "_origin": "ast",
            });
            hyperedge
                .as_object_mut()
                .unwrap()
                .insert(field.into(), json!([enrichment_id.clone()]));
            assert_rejected(KnowledgeGraph {
                nodes: vec![media("talk", "media/talk.mp4")],
                hyperedges: vec![hyperedge],
                ..KnowledgeGraph::default()
            });
        }
    }

    #[test]
    fn replacement_removes_all_owned_edges_to_the_prior_fact() {
        let mut graph = KnowledgeGraph {
            nodes: vec![media("talk", "media/talk.mp4")],
            ..KnowledgeGraph::default()
        };
        let mut candidate = record("media/talk.mp4", "First");
        apply_media_transcript_summaries(&mut graph, std::slice::from_ref(&candidate)).unwrap();
        let enrichment_id = media_transcript_summary_id(&candidate.source_file, &candidate.profile);
        graph.links.push(Edge {
            source: enrichment_id.clone(),
            target: "talk".into(),
            relation: "owned_auxiliary".into(),
            confidence: Confidence::Ambiguous,
            source_file: candidate.source_file.clone(),
            extra: BTreeMap::from([("_origin".into(), json!("enrichment"))]),
        });
        candidate.summary = "Replacement".into();
        apply_media_transcript_summaries(&mut graph, &[candidate]).unwrap();
        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.links[0].relation, "has_enrichment");
        assert_eq!(graph.links[0].true_target(), enrichment_id);
    }

    #[test]
    fn later_graph_build_preserves_source_owned_enrichment_identity() {
        let mut graph = KnowledgeGraph {
            nodes: vec![media("talk_a", "a/talk.mp4"), media("talk_b", "b/talk.mp4")],
            ..KnowledgeGraph::default()
        };
        let mut left = record("a/talk.mp4", "Same summary");
        left.source_node_id = "talk_a".into();
        let mut right = record("b/talk.mp4", "Same summary");
        right.source_node_id = "talk_b".into();
        apply_media_transcript_summaries(&mut graph, &[right, left]).unwrap();
        let expected = graph
            .nodes
            .iter()
            .filter(|node| node.extra.get("_origin") == Some(&json!("enrichment")))
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        let rebuilt = crate::build_graph(&[graphoxide_core::Extraction {
            nodes: graph.nodes,
            edges: graph.links,
            hyperedges: graph.hyperedges,
        }])
        .unwrap();
        let actual = rebuilt
            .nodes
            .iter()
            .filter(|node| node.extra.get("_origin") == Some(&json!("enrichment")))
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 2);
        assert_eq!(
            rebuilt
                .links
                .iter()
                .filter(|edge| edge.relation == "has_enrichment")
                .count(),
            2
        );
    }

    #[test]
    fn incremental_structural_rebuild_preserves_enrichment_for_live_source() {
        let temporary = tempfile::tempdir().unwrap();
        let graph_path = temporary.path().join("graph.json");
        let mut graph = KnowledgeGraph {
            nodes: vec![media("talk", "media/talk.mp4")],
            ..KnowledgeGraph::default()
        };
        apply_media_transcript_summaries(&mut graph, &[record("media/talk.mp4", "Stable summary")])
            .unwrap();
        graphoxide_core::write_graph_atomic_strict(&graph_path, &graph, true).unwrap();

        let mut rebuilt_media = media("talk", "media/talk.mp4");
        rebuilt_media
            .extra
            .insert("_origin".into(), json!("fallback"));
        let rebuilt = crate::build_merge(
            &[graphoxide_core::Extraction {
                nodes: vec![rebuilt_media],
                edges: Vec::new(),
                hyperedges: Vec::new(),
            }],
            &graph_path,
            &[],
            Some(temporary.path()),
        )
        .unwrap();
        assert_eq!(
            rebuilt
                .nodes
                .iter()
                .filter(|node| node.extra.get("_origin") == Some(&json!("enrichment")))
                .count(),
            1
        );
        assert_eq!(
            rebuilt
                .links
                .iter()
                .filter(|edge| edge.relation == "has_enrichment")
                .count(),
            1
        );
    }

    #[test]
    fn later_structural_id_collision_fails_without_touching_committed_graph() {
        let temporary = tempfile::tempdir().unwrap();
        let graph_path = temporary.path().join("graph.json");
        let mut graph = KnowledgeGraph {
            nodes: vec![media("talk", "media/talk.mp4")],
            ..KnowledgeGraph::default()
        };
        let candidate = record("media/talk.mp4", "Stable summary");
        apply_media_transcript_summaries(&mut graph, std::slice::from_ref(&candidate)).unwrap();
        graphoxide_core::write_graph_atomic_strict(&graph_path, &graph, true).unwrap();
        let before = std::fs::read(&graph_path).unwrap();
        let collision_id = media_transcript_summary_id(&candidate.source_file, &candidate.profile);

        let collision = graphoxide_core::Extraction {
            nodes: vec![Node {
                id: collision_id,
                label: "adversarial structural fact".into(),
                file_type: "code".into(),
                source_file: "src/collision.rs".into(),
                source_location: Some("L1".into()),
                community: None,
                extra: BTreeMap::from([("_origin".into(), json!("ast"))]),
            }],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };
        let error =
            crate::build_merge(&[collision], &graph_path, &[], Some(temporary.path())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate node ID involving enrichment"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&graph_path).unwrap(), before);
    }

    #[test]
    fn rejects_non_media_absolute_and_duplicate_records_without_mutation() {
        let mut graph = KnowledgeGraph {
            nodes: vec![Node {
                id: "talk".into(),
                label: "talk".into(),
                file_type: "document".into(),
                source_file: "media/talk.mp4".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::new(),
            }],
            ..KnowledgeGraph::default()
        };
        let before = serde_json::to_value(&graph).unwrap();
        assert!(matches!(
            apply_media_transcript_summaries(&mut graph, &[record("media/talk.mp4", "Summary")]),
            Err(EnrichmentApplyError::NotMediaInventory(_))
        ));
        assert_eq!(serde_json::to_value(&graph).unwrap(), before);

        graph.nodes[0] = media("talk", "media/talk.mp4");
        let mut unsafe_record = record("/media/talk.mp4", "Summary");
        unsafe_record.source_node_id = "talk".into();
        assert!(matches!(
            apply_media_transcript_summaries(&mut graph, &[unsafe_record]),
            Err(EnrichmentApplyError::InvalidRecord(_))
        ));
        let duplicate = record("media/talk.mp4", "Summary");
        assert!(matches!(
            apply_media_transcript_summaries(&mut graph, &[duplicate.clone(), duplicate]),
            Err(EnrichmentApplyError::DuplicateRecord { .. })
        ));
    }
}
