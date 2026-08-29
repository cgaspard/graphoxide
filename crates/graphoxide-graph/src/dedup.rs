//! Deterministic semantic-node deduplication.
//!
//! This mirrors Graphify's three important layers: deterministic exact-ID
//! collision handling, guarded normalized-label matching, and high-entropy
//! fuzzy matching.  In particular, code and file-anchored prose remain scoped
//! to their defining file while real concept duplicates may converge across
//! files.

use graphoxide_core::{Edge, KnowledgeGraph, Node};
use rapidfuzz::distance::{damerau_levenshtein, jaro, jaro_winkler};
use regex::Regex;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::OnceLock;
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

const ENTROPY_THRESHOLD: f64 = 2.5;
const LSH_THRESHOLD: f64 = 0.7;
const MERGE_THRESHOLD: f64 = 0.92;
const COMMUNITY_BOOST: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupDiagnosticLevel {
    Note,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DedupDiagnostic {
    pub level: DedupDiagnosticLevel,
    pub node_id: String,
    pub kept_label: String,
    pub kept_source_file: String,
    pub dropped_label: String,
    pub dropped_source_file: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EntityDeduplicationReport {
    pub exact_merges: usize,
    pub fuzzy_merges: usize,
    pub source_endpoints_remapped: usize,
    pub target_endpoints_remapped: usize,
    pub self_loops_dropped: usize,
    pub diagnostics: Vec<DedupDiagnostic>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DeduplicationReport {
    pub same_label_nodes_merged: usize,
    pub fuzzy_label_nodes_merged: usize,
    pub source_endpoints_remapped: usize,
    pub target_endpoints_remapped: usize,
    pub self_loops_dropped: usize,
    pub duplicate_edges_dropped: usize,
    pub hyperedge_members_remapped: usize,
    pub hyperedge_duplicate_members_removed: usize,
}

/// Normalize a label like upstream `_norm`: NFKC, lowercase, and collapse
/// punctuation/underscore runs to one ASCII space.
pub fn normalized_label(label: &str) -> String {
    let folded: String = label.nfkc().case_fold().collect();
    let mut out = String::new();
    let mut pending_space = false;
    for ch in folded.chars() {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            out.push(ch);
            pending_space = false;
        } else {
            pending_space = true;
        }
    }
    out
}

/// Shannon entropy in bits per normalized character.
pub fn label_entropy(label: &str) -> f64 {
    let normalized = normalized_label(label);
    if normalized.is_empty() {
        return 0.0;
    }
    let mut counts = HashMap::<char, usize>::new();
    for ch in normalized.chars() {
        *counts.entry(ch).or_default() += 1;
    }
    let len = normalized.chars().count() as f64;
    counts
        .values()
        .map(|count| {
            let probability = *count as f64 / len;
            -probability * probability.log2()
        })
        .sum()
}

/// Return character k-grams. Short strings form one shingle, including the
/// empty string, matching the pinned implementation.
pub fn shingles(text: &str, k: usize) -> BTreeSet<String> {
    let chars: Vec<_> = text.chars().collect();
    if chars.len() < k {
        return BTreeSet::from([text.to_owned()]);
    }
    (0..=chars.len() - k)
        .map(|start| chars[start..start + k].iter().collect())
        .collect()
}

pub fn is_variant_pair(a: &str, b: &str) -> bool {
    if a == b || a.chars().count().max(b.chars().count()) >= 12 {
        return false;
    }
    let pattern = variant_suffix_pattern();
    let Some(left) = pattern.captures(a) else {
        return false;
    };
    let Some(right) = pattern.captures(b) else {
        return false;
    };
    left.get(1).map(|m| m.as_str()) == right.get(1).map(|m| m.as_str())
        && left.get(2).map(|m| m.as_str()) != right.get(2).map(|m| m.as_str())
}

fn variant_suffix_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^(.*[a-z])([0-9]+[a-z]*|[a-z]{2,})$").expect("variant regex is valid")
    })
}

pub fn short_label_blocked(a: &str, b: &str, score: f64) -> bool {
    let len_a = a.chars().count();
    let len_b = b.chars().count();
    if len_a.max(len_b) >= 12 {
        return false;
    }
    !(score >= 0.97 && len_a == len_b && damerau_levenshtein::distance(a.chars(), b.chars()) <= 1)
}

pub fn numeric_tokens_differ(a: &str, b: &str) -> bool {
    if a == b {
        return false;
    }
    fn tokens(value: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut token = String::new();
        for ch in value.chars() {
            if ch.is_ascii_digit() {
                token.push(ch);
            } else if !token.is_empty() {
                let stripped = token.trim_start_matches('0');
                tokens.push(if stripped.is_empty() { "0" } else { stripped }.to_owned());
                token.clear();
            }
        }
        if !token.is_empty() {
            let stripped = token.trim_start_matches('0');
            tokens.push(if stripped.is_empty() { "0" } else { stripped }.to_owned());
        }
        tokens.sort();
        tokens
    }
    tokens(a) != tokens(b)
}

fn id_prefixes(source_file: &str) -> BTreeSet<String> {
    let normalized = source_file.replace('\\', "/");
    let stem = normalized
        .rsplit_once('/')
        .map_or(normalized.as_str(), |_| normalized.as_str());
    let stem = match stem.rsplit_once('.') {
        Some((prefix, extension)) if !extension.contains('/') => prefix,
        _ => stem,
    };
    let segments: Vec<String> = stem
        .split('/')
        .map(|segment| {
            let mut slug = String::new();
            let mut separator = false;
            for ch in segment.to_lowercase().chars() {
                if ch.is_ascii_alphanumeric() {
                    if separator && !slug.is_empty() {
                        slug.push('_');
                    }
                    slug.push(ch);
                    separator = false;
                } else {
                    separator = true;
                }
            }
            slug
        })
        .filter(|segment| !segment.is_empty())
        .collect();
    (0..segments.len())
        .map(|start| segments[start..].join("_"))
        .collect()
}

/// Whether a node's own source path is encoded by its ID.
pub fn defines_id(node: &Node) -> bool {
    if node.id.is_empty() || node.source_file.is_empty() {
        return false;
    }
    id_prefixes(&node.source_file)
        .iter()
        .any(|prefix| node.id == *prefix || node.id.starts_with(&format!("{prefix}_")))
}

fn collision_rank(node: &Node) -> (bool, usize, &str, &str) {
    (
        !defines_id(node),
        node.label.chars().count(),
        &node.label,
        &node.source_file,
    )
}

fn merge_missing_attributes(survivor: &mut Node, duplicate: &Node) {
    if survivor.source_location.is_none() {
        survivor
            .source_location
            .clone_from(&duplicate.source_location);
    }
    if survivor.community.is_none() {
        survivor.community = duplicate.community;
    }
    for (key, value) in &duplicate.extra {
        if key == "_origin" || value.is_null() {
            continue;
        }
        if survivor
            .extra
            .get(key)
            .is_none_or(serde_json::Value::is_null)
        {
            survivor.extra.insert(key.clone(), value.clone());
        }
    }
}

fn pick_winner(indices: &[usize], nodes: &[Node]) -> usize {
    indices
        .iter()
        .copied()
        .min_by_key(|index| {
            let node = &nodes[*index];
            let chunk_suffix = chunk_suffix_pattern().is_match(&node.id);
            (chunk_suffix, node.id.len(), *index)
        })
        .expect("winner group is non-empty")
}

fn chunk_suffix_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"_c\d+$").expect("chunk suffix regex is valid"))
}

#[derive(Debug)]
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
        }
    }

    fn find(&mut self, value: usize) -> usize {
        let parent = self.parent[value];
        if parent != value {
            self.parent[value] = self.find(parent);
        }
        self.parent[value]
    }

    fn union(&mut self, left: usize, right: usize) -> bool {
        let left_root = self.find(left);
        let right_root = self.find(right);
        if left_root == right_root {
            return false;
        }
        self.parent[right_root] = left_root;
        true
    }
}

#[derive(Debug)]
struct EngineResult {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    remap: BTreeMap<String, String>,
    report: EntityDeduplicationReport,
}

/// Deduplicate entity records and rewire their edges.
pub fn deduplicate_entities(
    nodes: &[Node],
    edges: &[Edge],
    communities: &BTreeMap<String, i64>,
) -> anyhow::Result<(Vec<Node>, Vec<Edge>, EntityDeduplicationReport)> {
    let result = deduplicate_engine(nodes, edges, communities, true)?;
    Ok((result.nodes, result.edges, result.report))
}

fn deduplicate_engine(
    nodes: &[Node],
    edges: &[Edge],
    communities: &BTreeMap<String, i64>,
    enforce_repo_guard: bool,
) -> anyhow::Result<EngineResult> {
    if enforce_repo_guard {
        let repos: BTreeSet<_> = nodes
            .iter()
            .filter_map(|node| node.extra.get("repo").and_then(serde_json::Value::as_str))
            .filter(|repo| !repo.is_empty())
            .collect();
        anyhow::ensure!(
            repos.len() <= 1,
            "deduplicate_entities: nodes span multiple repos; cross-project dedup is disabled"
        );
    }

    let mut report = EntityDeduplicationReport::default();
    let mut grouped = BTreeMap::<String, Vec<(usize, &Node)>>::new();
    let mut id_order = Vec::new();
    for (position, node) in nodes.iter().enumerate() {
        if node.id.is_empty() {
            continue;
        }
        if !grouped.contains_key(&node.id) {
            id_order.push(node.id.clone());
        }
        grouped
            .entry(node.id.clone())
            .or_default()
            .push((position, node));
    }

    let mut unique_nodes = Vec::with_capacity(grouped.len());
    for id in id_order {
        let group = &grouped[&id];
        let winner = group
            .iter()
            .min_by(|(_, left), (_, right)| collision_rank(left).cmp(&collision_rank(right)))
            .expect("ID group is non-empty")
            .1;
        let mut survivor = winner.clone();
        let mut losers: Vec<_> = group
            .iter()
            .filter_map(|(_, node)| (!std::ptr::eq(*node, winner)).then_some(*node))
            .collect();
        losers.sort_by(|left, right| collision_rank(left).cmp(&collision_rank(right)));
        for loser in &losers {
            if !survivor.source_file.is_empty() && survivor.source_file == loser.source_file {
                merge_missing_attributes(&mut survivor, loser);
            }
        }
        for loser in losers {
            let level = if loser.source_file == survivor.source_file {
                if normalized_label(&loser.label) == normalized_label(&survivor.label) {
                    continue;
                }
                DedupDiagnosticLevel::Note
            } else if defines_id(&survivor) && !defines_id(loser) {
                continue;
            } else {
                DedupDiagnosticLevel::Warning
            };
            report.diagnostics.push(DedupDiagnostic {
                level,
                node_id: id.clone(),
                kept_label: survivor.label.clone(),
                kept_source_file: survivor.source_file.clone(),
                dropped_label: loser.label.clone(),
                dropped_source_file: loser.source_file.clone(),
            });
        }
        unique_nodes.push(survivor);
    }

    if unique_nodes.len() <= 1 {
        return Ok(EngineResult {
            nodes: unique_nodes,
            edges: edges.to_vec(),
            remap: BTreeMap::new(),
            report,
        });
    }

    let mut union_find = UnionFind::new(unique_nodes.len());
    let mut norm_groups = BTreeMap::<String, Vec<usize>>::new();
    for (index, node) in unique_nodes.iter().enumerate() {
        if preserves_declared_identity(node) {
            continue;
        }
        let normalized = normalized_label(&node.label);
        if !normalized.is_empty() {
            norm_groups.entry(normalized).or_default().push(index);
        }
    }

    for indices in norm_groups.values() {
        if indices.len() <= 1 {
            continue;
        }
        let mut by_file = BTreeMap::<&str, Vec<usize>>::new();
        for index in indices {
            by_file
                .entry(&unique_nodes[*index].source_file)
                .or_default()
                .push(*index);
        }
        for (source_file, file_group) in by_file {
            if source_file.is_empty() || file_group.len() <= 1 {
                continue;
            }
            let winner = pick_winner(&file_group, &unique_nodes);
            for index in file_group {
                report.exact_merges += usize::from(union_find.union(winner, index));
            }
        }

        let mut mergeable: Vec<_> = indices
            .iter()
            .copied()
            .filter(|index| {
                let node = &unique_nodes[*index];
                node.file_type == "concept"
                    && !node.source_file.is_empty()
                    && label_entropy(&node.label) >= ENTROPY_THRESHOLD
            })
            .collect();
        mergeable.sort_by(|left, right| unique_nodes[*left].id.cmp(&unique_nodes[*right].id));
        if mergeable.len() > 1 {
            let winner = pick_winner(&mergeable, &unique_nodes);
            for index in mergeable {
                report.exact_merges += usize::from(union_find.union(winner, index));
            }
        }
    }

    let mut candidates = Vec::new();
    let mut candidate_norms = BTreeMap::new();
    let mut seen_norms = BTreeSet::new();
    for (index, node) in unique_nodes.iter().enumerate() {
        if preserves_declared_identity(node) {
            continue;
        }
        let normalized = normalized_label(&node.label);
        if !normalized.is_empty()
            && seen_norms.insert(normalized.clone())
            && label_entropy(&node.label) >= ENTROPY_THRESHOLD
        {
            candidates.push(index);
            candidate_norms.insert(index, normalized);
        }
    }

    // Exact shingle blocking is deterministic and avoids the quadratic scan
    // that a direct all-pairs implementation would impose on large corpora.
    // A pair whose Jaccard score can clear the gate necessarily shares at
    // least one shingle, so the inverted index does not change decisions.
    let mut shingle_cache = BTreeMap::<usize, BTreeSet<String>>::new();
    let mut inverted = HashMap::<String, Vec<usize>>::new();
    let mut candidate_pairs = BTreeSet::new();
    for index in candidates {
        let normalized = &candidate_norms[&index];
        let node_shingles = shingles(&normalized.replace(' ', ""), 3);
        for shingle in &node_shingles {
            for prior in inverted.get(shingle).into_iter().flatten() {
                candidate_pairs.insert((*prior, index));
            }
            inverted.entry(shingle.clone()).or_default().push(index);
        }
        shingle_cache.insert(index, node_shingles);
    }

    for (left_index, right_index) in candidate_pairs {
        if union_find.find(left_index) == union_find.find(right_index) {
            continue;
        }
        let left = &unique_nodes[left_index];
        let right = &unique_nodes[right_index];
        let left_norm = &candidate_norms[&left_index];
        let right_norm = &candidate_norms[&right_index];
        if shingle_similarity_sets(&shingle_cache[&left_index], &shingle_cache[&right_index])
            < LSH_THRESHOLD
        {
            continue;
        }
        let cross_file = left.source_file != right.source_file;
        let max_len = left_norm.chars().count().max(right_norm.chars().count());
        let mut score = if cross_file && max_len >= 12 {
            jaro::similarity(left_norm.chars(), right_norm.chars())
        } else {
            jaro_winkler::similarity(left_norm.chars(), right_norm.chars())
        };
        if is_variant_pair(left_norm, right_norm)
            || short_label_blocked(left_norm, right_norm, score)
            || strict_prefix_pair(left_norm, right_norm)
            || numeric_tokens_differ(left_norm, right_norm)
            || crossfile_file_anchored(left, right)
        {
            continue;
        }
        if communities.get(&left.id).is_some()
            && communities.get(&left.id) == communities.get(&right.id)
            && left_norm.chars().count().min(right_norm.chars().count()) >= 12
        {
            score += COMMUNITY_BOOST;
        }
        if score >= MERGE_THRESHOLD {
            let winner = pick_winner(&[left_index, right_index], &unique_nodes);
            let loser = if winner == left_index {
                right_index
            } else {
                left_index
            };
            report.fuzzy_merges += usize::from(union_find.union(winner, loser));
        }
    }

    let mut components = BTreeMap::<usize, Vec<usize>>::new();
    for index in 0..unique_nodes.len() {
        let root = union_find.find(index);
        components.entry(root).or_default().push(index);
    }
    let mut remap = BTreeMap::new();
    for indices in components.values() {
        if indices.len() <= 1 {
            continue;
        }
        let winner = pick_winner(indices, &unique_nodes);
        for index in indices {
            if *index != winner {
                remap.insert(
                    unique_nodes[*index].id.clone(),
                    unique_nodes[winner].id.clone(),
                );
            }
        }
    }

    let deduped_nodes = unique_nodes
        .into_iter()
        .filter(|node| !remap.contains_key(&node.id))
        .collect();
    let mut deduped_edges = Vec::with_capacity(edges.len());
    for original in edges {
        let mut edge = original.clone();
        let source = resolve_endpoint(original.true_source(), &remap);
        let target = resolve_endpoint(original.true_target(), &remap);
        if source != original.true_source() {
            report.source_endpoints_remapped += 1;
        }
        if target != original.true_target() {
            report.target_endpoints_remapped += 1;
        }
        edge.source = source.clone();
        edge.target = target.clone();
        if edge.extra.contains_key("_src") {
            edge.extra.insert("_src".into(), source.into());
        }
        if edge.extra.contains_key("_tgt") {
            edge.extra.insert("_tgt".into(), target.into());
        }
        let declared_structural_self_loop = original.true_source() == original.true_target()
            && (original
                .extra
                .get("diagram_format")
                .and_then(serde_json::Value::as_str)
                == Some("graphviz")
                || original
                    .extra
                    .get("_origin")
                    .and_then(serde_json::Value::as_str)
                    == Some("document_package"));
        if edge.source == edge.target && !declared_structural_self_loop {
            report.self_loops_dropped += 1;
        } else {
            deduped_edges.push(edge);
        }
    }

    Ok(EngineResult {
        nodes: deduped_nodes,
        edges: deduped_edges,
        remap,
        report,
    })
}

fn shingle_similarity_sets(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    let union = left.union(right).count();
    if union == 0 {
        1.0
    } else {
        left.intersection(right).count() as f64 / union as f64
    }
}

fn strict_prefix_pair(left: &str, right: &str) -> bool {
    match left.chars().count().cmp(&right.chars().count()) {
        Ordering::Less => right.starts_with(left),
        Ordering::Greater => left.starts_with(right),
        Ordering::Equal => false,
    }
}

fn crossfile_file_anchored(left: &Node, right: &Node) -> bool {
    if left.source_file == right.source_file {
        return false;
    }
    matches!(left.file_type.as_str(), "rationale" | "document")
        || matches!(right.file_type.as_str(), "rationale" | "document")
}

/// Code symbols and grammar-defined DOT entities carry identity that is
/// stronger than their display label. Two DOT nodes may intentionally share a
/// label, including within the same source graph, without denoting one entity.
fn preserves_declared_identity(node: &Node) -> bool {
    node.file_type == "code"
        || node
            .extra
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind.starts_with("document_"))
        || node
            .extra
            .get("_origin")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|origin| matches!(origin, "document_package" | "enrichment"))
        || (node
            .extra
            .get("diagram_format")
            .and_then(serde_json::Value::as_str)
            == Some("graphviz")
            && node.extra.contains_key("dot_id"))
}

fn resolve_endpoint(value: &str, remap: &BTreeMap<String, String>) -> String {
    let mut current = value.to_owned();
    for _ in 0..=remap.len() {
        let Some(next) = remap.get(&current) else {
            break;
        };
        if *next == current {
            break;
        }
        current.clone_from(next);
    }
    current
}

pub fn deduplicate(graph: &mut KnowledgeGraph) -> usize {
    let report = deduplicate_with_report(graph);
    report.same_label_nodes_merged + report.fuzzy_label_nodes_merged
}

pub(crate) fn deduplicate_with_report(graph: &mut KnowledgeGraph) -> DeduplicationReport {
    let communities = graph
        .nodes
        .iter()
        .filter_map(|node| node.community.map(|community| (node.id.clone(), community)))
        .collect();
    let result = deduplicate_engine(&graph.nodes, &graph.links, &communities, false)
        .expect("repo guard is disabled for graph-build integration");
    let mut report = DeduplicationReport {
        same_label_nodes_merged: result.report.exact_merges,
        fuzzy_label_nodes_merged: result.report.fuzzy_merges,
        source_endpoints_remapped: result.report.source_endpoints_remapped,
        target_endpoints_remapped: result.report.target_endpoints_remapped,
        self_loops_dropped: result.report.self_loops_dropped,
        ..DeduplicationReport::default()
    };
    graph.nodes = result.nodes;
    graph.links = result.edges;

    for hyperedge in &mut graph.hyperedges {
        let Some(raw_members) = hyperedge
            .get("nodes")
            .and_then(serde_json::Value::as_array)
            .cloned()
        else {
            continue;
        };
        let mut members = Vec::with_capacity(raw_members.len());
        for member in raw_members {
            let Some(member) = member.as_str() else {
                continue;
            };
            let resolved = resolve_endpoint(member, &result.remap);
            report.hyperedge_members_remapped += usize::from(resolved != member);
            members.push(resolved);
        }
        let original_len = members.len();
        let mut seen = BTreeSet::new();
        members.retain(|member| seen.insert(member.clone()));
        report.hyperedge_duplicate_members_removed += original_len - members.len();
        if let Some(object) = hyperedge.as_object_mut() {
            object.insert("nodes".into(), serde_json::json!(members));
        }
    }

    let mut seen = BTreeSet::new();
    graph.links.retain(|edge| {
        if seen.insert((
            edge.source.clone(),
            edge.target.clone(),
            edge.relation.clone(),
        )) {
            true
        } else {
            report.duplicate_edges_dropped += 1;
            false
        }
    });
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_core::Confidence;
    use serde_json::json;

    #[test]
    fn entropy_distinguishes_repetition() {
        assert!(label_entropy("architecture") > label_entropy("aaaaaaaa"));
    }

    #[test]
    fn numeric_tokens_ignore_zero_padding() {
        assert!(!numeric_tokens_differ("phase 09", "phase 9"));
        assert!(numeric_tokens_differ("adr 11", "adr 13"));
    }

    #[test]
    fn normalization_uses_unicode_casefolding() {
        assert_eq!(normalized_label("Straße"), "strasse");
        assert_eq!(normalized_label("Ｇｒａｐｈ＿Ｏｘｉｄｅ"), "graph oxide");
    }

    #[test]
    fn dot_identity_and_declared_self_loops_survive_semantic_deduplication() {
        let node = |id: &str| Node {
            id: id.into(),
            label: "Same display label".into(),
            file_type: "document".into(),
            source_file: "architecture.dot".into(),
            source_location: Some("L1".into()),
            community: None,
            extra: BTreeMap::from([
                ("diagram_format".into(), json!("graphviz")),
                ("dot_id".into(), json!(id)),
            ]),
        };
        let mut graph = KnowledgeGraph {
            nodes: vec![node("dot_a"), node("dot_b")],
            links: vec![
                Edge {
                    source: "dot_a".into(),
                    target: "dot_a".into(),
                    relation: "flows_to".into(),
                    confidence: Confidence::Extracted,
                    source_file: "architecture.dot".into(),
                    extra: BTreeMap::from([("diagram_format".into(), json!("graphviz"))]),
                },
                Edge {
                    source: "dot_b".into(),
                    target: "dot_b".into(),
                    relation: "generic_self_loop".into(),
                    confidence: Confidence::Extracted,
                    source_file: "architecture.dot".into(),
                    extra: BTreeMap::new(),
                },
            ],
            ..KnowledgeGraph::default()
        };

        let report = deduplicate_with_report(&mut graph);
        assert_eq!(report.same_label_nodes_merged, 0);
        assert_eq!(report.self_loops_dropped, 1);
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.links[0].source, graph.links[0].target);
        assert_eq!(graph.links[0].relation, "flows_to");
    }

    #[test]
    fn document_package_units_and_declared_self_loops_survive_deduplication() {
        let unit = |id: &str, ordinal: u64| Node {
            id: id.into(),
            label: "Repeated chapter".into(),
            file_type: "document".into(),
            source_file: "book.epub".into(),
            source_location: None,
            community: None,
            extra: BTreeMap::from([
                ("_origin".into(), json!("document_package")),
                ("type".into(), json!("publication_section")),
                ("unit_ordinal".into(), json!(ordinal)),
            ]),
        };
        let mut graph = KnowledgeGraph {
            nodes: vec![unit("chapter_1", 1), unit("chapter_2", 2)],
            links: vec![Edge {
                source: "chapter_1".into(),
                target: "chapter_1".into(),
                relation: "references".into(),
                confidence: Confidence::Extracted,
                source_file: "book.epub".into(),
                extra: BTreeMap::from([("_origin".into(), json!("document_package"))]),
            }],
            ..KnowledgeGraph::default()
        };

        let report = deduplicate_with_report(&mut graph);
        assert_eq!(report.same_label_nodes_merged, 0);
        assert_eq!(report.self_loops_dropped, 0);
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.links[0].source, graph.links[0].target);
    }
}
