//! Deterministic extraction merge and endpoint repair.

use crate::provenance::origin_is_structural;
use graphoxide_core::{
    normalize_id, Edge, Extraction, KnowledgeGraph, Node, CONTAINER_SOURCE_ATTRIBUTE,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Why an input node was discarded before it could enter the built graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeDropReason {
    EmptyId,
}

/// Why two input nodes were represented by one output node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeMergeReason {
    DuplicateId,
    SemanticToAst,
    DocumentTwin,
    SemanticSameLabel,
    SemanticFuzzyLabel,
}

/// Why an input edge was not represented in the built graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeDropReason {
    UnresolvedSource,
    UnresolvedTarget,
    UnresolvedSourceAndTarget,
    ImportSelfLoop,
    CrossLanguageInferredCall,
    CrossLanguageImportOrReference,
    ExactDuplicate,
    UndirectedReverseDuplicate,
    SelfLoopAfterNodeMerge,
    ExactDuplicateAfterNodeMerge,
}

/// Knobs that correspond to upstream `build(..., directed=, dedup=)` while
/// keeping Graphoxide's lossless reciprocal-edge representation as the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildOptions {
    pub directed: bool,
    pub deduplicate_semantic_nodes: bool,
    /// Reproduce NetworkX `Graph`'s first-direction-wins behavior for a pair of
    /// equal-relation reciprocal edges. The normal Graphoxide build leaves this
    /// off so reciprocal program relationships are not discarded.
    pub collapse_undirected_reverse_edges: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            directed: false,
            deduplicate_semantic_nodes: true,
            collapse_undirected_reverse_edges: false,
        }
    }
}

/// A repair or normalization applied to an edge while retaining it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeRepairReason {
    SourceSemanticRemap,
    TargetSemanticRemap,
    SourceNormalizedId,
    TargetNormalizedId,
    SourceLegacyId,
    TargetLegacyId,
    DirectionalSourceRestored,
    DirectionalTargetRestored,
    SourceAfterNodeMerge,
    TargetAfterNodeMerge,
    WeightNormalized,
    ConfidenceScoreNormalized,
    SourceFileBackfilled,
    ConfidenceScoreBackfilled,
    TargetFileMetadataRemoved,
    LocalAliasMetadataRemoved,
}

/// Why an input hyperedge was not represented in the built graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HyperedgeDropReason {
    NonObject,
    MissingOrInvalidMembers,
    NoResolvedMembers,
}

/// A repair applied to a hyperedge or one of its member references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HyperedgeRepairReason {
    MembersAliasNormalized,
    MemberSemanticRemap,
    MemberNormalizedId,
    MemberLegacyId,
    MemberAfterNodeMerge,
    NonStringMemberRemoved,
    UnresolvedMemberRemoved,
    DuplicateMemberRemoved,
    DuplicateMemberAfterNodeMerge,
    MissingIdBackfilled,
    DuplicateIdRemapped,
}

/// Conservation accounting for one graph build.
///
/// Node and edge drop/merge maps contain only non-zero reason categories. Endpoint
/// repairs may outnumber retained edges because a single edge can require repairs
/// to both endpoints and can subsequently be dropped for another documented reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildReport {
    pub input_extractions: usize,
    pub input_nodes: usize,
    pub input_edges: usize,
    pub input_hyperedges: usize,
    pub output_nodes: usize,
    pub output_edges: usize,
    pub output_hyperedges: usize,
    pub node_drops: BTreeMap<NodeDropReason, usize>,
    pub node_merges: BTreeMap<NodeMergeReason, usize>,
    pub edge_drops: BTreeMap<EdgeDropReason, usize>,
    pub edge_repairs: BTreeMap<EdgeRepairReason, usize>,
    pub hyperedge_drops: BTreeMap<HyperedgeDropReason, usize>,
    pub hyperedge_repairs: BTreeMap<HyperedgeRepairReason, usize>,
}

impl BuildReport {
    pub fn dropped_node_count(&self) -> usize {
        self.node_drops.values().sum()
    }

    pub fn merged_node_count(&self) -> usize {
        self.node_merges.values().sum()
    }

    pub fn dropped_edge_count(&self) -> usize {
        self.edge_drops.values().sum()
    }

    pub fn dropped_hyperedge_count(&self) -> usize {
        self.hyperedge_drops.values().sum()
    }

    /// Whether every input node is represented by an output, merge, or drop.
    pub fn nodes_accounted_for(&self) -> bool {
        self.input_nodes == self.output_nodes + self.merged_node_count() + self.dropped_node_count()
    }

    /// Whether every input edge is represented by an output or documented drop.
    pub fn edges_accounted_for(&self) -> bool {
        self.input_edges == self.output_edges + self.dropped_edge_count()
    }

    /// Whether every input hyperedge is represented by an output or documented drop.
    pub fn hyperedges_accounted_for(&self) -> bool {
        self.input_hyperedges == self.output_hyperedges + self.dropped_hyperedge_count()
    }

    fn drop_node(&mut self, reason: NodeDropReason) {
        increment(&mut self.node_drops, reason);
    }

    fn merge_node(&mut self, reason: NodeMergeReason) {
        increment(&mut self.node_merges, reason);
    }

    fn merge_nodes(&mut self, reason: NodeMergeReason, count: usize) {
        increment_by(&mut self.node_merges, reason, count);
    }

    fn drop_edge(&mut self, reason: EdgeDropReason) {
        increment(&mut self.edge_drops, reason);
    }

    fn drop_edges(&mut self, reason: EdgeDropReason, count: usize) {
        increment_by(&mut self.edge_drops, reason, count);
    }

    fn repair_edge(&mut self, reason: EdgeRepairReason) {
        increment(&mut self.edge_repairs, reason);
    }

    fn repair_edges(&mut self, reason: EdgeRepairReason, count: usize) {
        increment_by(&mut self.edge_repairs, reason, count);
    }

    fn drop_hyperedge(&mut self, reason: HyperedgeDropReason) {
        increment(&mut self.hyperedge_drops, reason);
    }

    fn repair_hyperedge(&mut self, reason: HyperedgeRepairReason) {
        increment(&mut self.hyperedge_repairs, reason);
    }

    fn repair_hyperedges(&mut self, reason: HyperedgeRepairReason, count: usize) {
        increment_by(&mut self.hyperedge_repairs, reason, count);
    }
}

fn increment<K: Ord>(counts: &mut BTreeMap<K, usize>, reason: K) {
    increment_by(counts, reason, 1);
}

fn increment_by<K: Ord>(counts: &mut BTreeMap<K, usize>, reason: K, count: usize) {
    if count > 0 {
        *counts.entry(reason).or_default() += count;
    }
}

/// Whether `label` identifies the file represented by `source_file`.
///
/// File labels begin as basenames and may later become directory-qualified
/// suffixes when two files share a basename. Symbol labels from the same file
/// must not be mistaken for file nodes.
pub fn is_file_node_label(label: &str, source_file: &str) -> bool {
    if label.is_empty() || source_file.is_empty() {
        return false;
    }
    let source_file = source_file.replace('\\', "/");
    if label == source_file.rsplit('/').next().unwrap_or_default() {
        return true;
    }
    label.contains('/') && (source_file == label || source_file.ends_with(&format!("/{label}")))
}

/// Return the shortest trailing path suffix that distinguishes `source_file`
/// from every other source path in the collision group.
pub fn shortest_unique_suffix(source_file: &str, all_source_files: &BTreeSet<String>) -> String {
    let parts: Vec<_> = source_file
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    let others: Vec<Vec<String>> = all_source_files
        .iter()
        .filter(|other| other.as_str() != source_file)
        .map(|other| {
            other
                .replace('\\', "/")
                .split('/')
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .collect();
    for count in 1..=parts.len() {
        let suffix = &parts[parts.len() - count..];
        if others
            .iter()
            .all(|other| other.len() < count || other[other.len() - count..] != *suffix)
        {
            return suffix.join("/");
        }
    }
    parts.join("/")
}

/// Qualify colliding file-node labels in place while leaving IDs, edges, and
/// symbol nodes untouched. Labels are derived from source paths, making this
/// operation idempotent across incremental rebuilds.
pub fn disambiguate_file_labels_in_nodes(nodes: &mut [Node]) {
    let mut groups: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
    for (index, node) in nodes.iter().enumerate() {
        if is_declared_graphviz_entity(node)
            || is_document_package_child(node)
            || !is_file_node_label(&node.label, &node.source_file)
        {
            continue;
        }
        let normalized = node.source_file.replace('\\', "/");
        let basename = normalized.rsplit('/').next().unwrap_or_default();
        groups
            .entry(basename.to_owned())
            .or_default()
            .push((index, node.source_file.clone()));
    }

    for members in groups.values() {
        let distinct: BTreeSet<_> = members
            .iter()
            .map(|(_, source_file)| source_file.clone())
            .collect();
        if distinct.len() < 2 {
            continue;
        }
        for (index, source_file) in members {
            nodes[*index].label = shortest_unique_suffix(source_file, &distinct);
        }
    }
}

/// Apply file-label disambiguation to the flattened raw extraction stream used
/// by the `extract --no-cluster` path.
pub fn disambiguate_file_labels_in_extractions(extractions: &mut [Extraction]) {
    let mut flattened: Vec<Node> = extractions
        .iter()
        .flat_map(|extraction| extraction.nodes.iter().cloned())
        .collect();
    disambiguate_file_labels_in_nodes(&mut flattened);
    let mut labels = flattened.into_iter().map(|node| node.label);
    for node in extractions
        .iter_mut()
        .flat_map(|extraction| extraction.nodes.iter_mut())
    {
        node.label = labels.next().expect("flattened node count is unchanged");
    }
}

pub fn build_graph(extractions: &[Extraction]) -> anyhow::Result<KnowledgeGraph> {
    build_graph_with_report(extractions).map(|(graph, _)| graph)
}

/// Build with explicit upstream-compatible direction and deduplication knobs.
pub fn build_graph_with_options(
    extractions: &[Extraction],
    options: BuildOptions,
) -> anyhow::Result<KnowledgeGraph> {
    build_graph_with_report_and_options(extractions, options).map(|(graph, _)| graph)
}

/// Build after making absolute source paths repository-relative. This mirrors
/// upstream's `build_from_json(root=...)` contract for nodes, edges, and
/// hyperedges alike.
pub fn build_graph_with_root(
    extractions: &[Extraction],
    root: impl AsRef<std::path::Path>,
) -> anyhow::Result<KnowledgeGraph> {
    build_graph_with_report_and_root(extractions, root).map(|(graph, _)| graph)
}

/// Root-aware build with explicit build options.
pub fn build_graph_with_options_and_root(
    extractions: &[Extraction],
    root: impl AsRef<std::path::Path>,
    options: BuildOptions,
) -> anyhow::Result<KnowledgeGraph> {
    build_graph_with_report_and_options_and_root(extractions, root, options).map(|(graph, _)| graph)
}

/// Root-aware build with conservation accounting.
pub fn build_graph_with_report_and_root(
    extractions: &[Extraction],
    root: impl AsRef<std::path::Path>,
) -> anyhow::Result<(KnowledgeGraph, BuildReport)> {
    build_graph_with_report_and_options_and_root(extractions, root, BuildOptions::default())
}

/// Root-aware build with conservation accounting and explicit options.
pub fn build_graph_with_report_and_options_and_root(
    extractions: &[Extraction],
    root: impl AsRef<std::path::Path>,
    options: BuildOptions,
) -> anyhow::Result<(KnowledgeGraph, BuildReport)> {
    let root = root.as_ref();
    let normalized = normalize_extractions(extractions, Some(root));
    build_graph_with_report_normalized(&normalized, options)
}

fn relativize_source_file(value: &str, root: &std::path::Path) -> String {
    if value.is_empty() {
        return String::new();
    }
    let normalized = value.replace('\\', "/");
    let path = std::path::Path::new(&normalized);
    let relative = if path.is_absolute() {
        path.strip_prefix(root).unwrap_or(path)
    } else {
        path
    };
    relative.to_string_lossy().replace('\\', "/")
}

fn normalize_container_source(
    extra: &mut BTreeMap<String, serde_json::Value>,
    root: Option<&std::path::Path>,
) {
    let Some(source) = extra
        .get(CONTAINER_SOURCE_ATTRIBUTE)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
    else {
        return;
    };
    let normalized = match root {
        Some(root) => relativize_source_file(&source, root),
        None => source.replace('\\', "/"),
    };
    extra.insert(CONTAINER_SOURCE_ATTRIBUTE.into(), normalized.into());
}

fn normalize_extractions(
    extractions: &[Extraction],
    root: Option<&std::path::Path>,
) -> Vec<Extraction> {
    let mut normalized = extractions.to_vec();
    for extraction in &mut normalized {
        for node in &mut extraction.nodes {
            let original_source = node.source_file.replace('\\', "/");
            node.source_file = match root {
                Some(root) => relativize_source_file(&node.source_file, root),
                None => original_source.clone(),
            };
            if root.is_some()
                && std::path::Path::new(&original_source).is_absolute()
                && original_source != node.source_file
            {
                let mut absolute_stem = std::path::PathBuf::from(&original_source);
                absolute_stem.set_extension("");
                node.extra.insert(
                    "_absolute_source_stem".into(),
                    normalize_id(&absolute_stem.to_string_lossy()).into(),
                );
            }
            normalize_container_source(&mut node.extra, root);
            node.file_type = canonical_file_type(&node.file_type).to_owned();
        }
        for edge in &mut extraction.edges {
            edge.source_file = match root {
                Some(root) => relativize_source_file(&edge.source_file, root),
                None => edge.source_file.replace('\\', "/"),
            };
            normalize_container_source(&mut edge.extra, root);
        }
        for hyperedge in &mut extraction.hyperedges {
            let Some(object) = hyperedge.as_object_mut() else {
                continue;
            };
            for key in ["source_file", CONTAINER_SOURCE_ATTRIBUTE] {
                if let Some(source_file) = object
                    .get(key)
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
                {
                    let normalized = match root {
                        Some(root) => relativize_source_file(&source_file, root),
                        None => source_file.replace('\\', "/"),
                    };
                    object.insert(key.into(), normalized.into());
                }
            }
        }
    }
    normalized
}

pub(crate) fn canonical_file_type(value: &str) -> &'static str {
    match value {
        "code" | "document" | "paper" | "image" | "rationale" | "concept" => {
            // These literals are all static; spell out the return to satisfy
            // the static lifetime without leaking the borrowed input.
            match value {
                "code" => "code",
                "document" => "document",
                "paper" => "paper",
                "image" => "image",
                "rationale" => "rationale",
                _ => "concept",
            }
        }
        "markdown" | "text" => "document",
        "tool" | "library" => "code",
        "pattern" | "principle" | "constraint" | "tech" | "technology" | "data-source"
        | "data_source" | "gotcha" | "framework" => "concept",
        _ => "concept",
    }
}

/// Attach new hyperedges to a built graph, ignoring missing IDs and preserving
/// the first hyperedge for each ID.
pub fn attach_hyperedges(graph: &mut KnowledgeGraph, values: &[serde_json::Value]) {
    let mut seen: BTreeSet<String> = graph
        .hyperedges
        .iter()
        .filter_map(|value| {
            value
                .get("id")
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .collect();
    for value in values {
        let Some(id) = value
            .get("id")
            .and_then(|id| id.as_str())
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if seen.insert(id.to_owned()) {
            graph.hyperedges.push(value.clone());
        }
    }
}

/// Build a graph and return accounting for every node merge and edge loss/repair.
pub fn build_graph_with_report(
    extractions: &[Extraction],
) -> anyhow::Result<(KnowledgeGraph, BuildReport)> {
    build_graph_with_report_and_options(extractions, BuildOptions::default())
}

/// Build with conservation accounting and explicit options.
pub fn build_graph_with_report_and_options(
    extractions: &[Extraction],
    options: BuildOptions,
) -> anyhow::Result<(KnowledgeGraph, BuildReport)> {
    let normalized = normalize_extractions(extractions, None);
    build_graph_with_report_normalized(&normalized, options)
}

fn build_graph_with_report_normalized(
    extractions: &[Extraction],
    options: BuildOptions,
) -> anyhow::Result<(KnowledgeGraph, BuildReport)> {
    let mut report = BuildReport {
        input_extractions: extractions.len(),
        input_nodes: extractions.iter().map(|value| value.nodes.len()).sum(),
        input_edges: extractions.iter().map(|value| value.edges.len()).sum(),
        input_hyperedges: extractions.iter().map(|value| value.hyperedges.len()).sum(),
        ..BuildReport::default()
    };
    let mut remap = semantic_id_remap(extractions);
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    for extraction in extractions {
        for node in &extraction.nodes {
            if node.id.is_empty() {
                report.drop_node(NodeDropReason::EmptyId);
                continue;
            }
            let mut node = node.clone();
            node.extra.remove("_absolute_source_stem");
            if let Some(canonical) = remap.get(&node.id) {
                node.id = canonical.clone();
            }
            if let Some(existing) = nodes.get_mut(&node.id) {
                merge_node(existing, &node);
                report.merge_node(NodeMergeReason::DuplicateId);
            } else {
                nodes.insert(node.id.clone(), node);
            }
        }
    }

    // Markdown's deterministic scan historically emitted `<stem>` while the
    // semantic pass emitted `<stem>_doc`. They are two producers for the same
    // document, not two graph entities. Prefer the richer `_doc` record and
    // repoint every relationship to it.
    let doc_twin_remap = document_twin_remap(&nodes);
    remap.extend(doc_twin_remap.clone());

    // Merge source-less semantic/annotation ghosts onto a unique sourced AST node
    // with the same normalized (source_file, label) identity.
    let mut canonical_by_key: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for node in nodes.values() {
        if !node.source_file.is_empty() && is_structural_node(node) {
            canonical_by_key
                .entry((node_path_key(node), node.label.to_lowercase()))
                .or_default()
                .push(node.id.clone());
        }
    }
    for node in nodes.values() {
        let key = (node_path_key(node), node.label.to_lowercase());
        if let Some(candidates) = canonical_by_key.get(&key)
            && candidates.len() == 1
            && candidates[0] != node.id
            && !is_structural_node(node)
        {
            remap.insert(node.id.clone(), candidates[0].clone());
        }
    }
    for (old, canonical) in &remap {
        let incoming = nodes.get(old).cloned();
        if !doc_twin_remap.contains_key(old)
            && let (Some(incoming), Some(existing)) = (incoming, nodes.get_mut(canonical))
        {
            merge_node(existing, &incoming);
        }
        if nodes.remove(old).is_some() {
            report.merge_node(if doc_twin_remap.contains_key(old) {
                NodeMergeReason::DocumentTwin
            } else {
                NodeMergeReason::SemanticToAst
            });
        }
    }

    let mut normalized: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in nodes.keys() {
        normalized
            .entry(normalize_id(id))
            .or_default()
            .push(id.clone());
    }
    let mut legacy: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in nodes.values() {
        let path = std::path::Path::new(&node.source_file);
        let stem = path.file_stem().and_then(|v| v.to_str()).unwrap_or("");
        if !is_document_package_child(node) {
            add_legacy_alias(
                &mut legacy,
                normalize_id(&format!(
                    "{stem}_{}",
                    node.label.trim_start_matches('.').trim_end_matches("()")
                )),
                &node.id,
            );
        }
        let basename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if !is_document_package_child(node)
            && !stem.is_empty()
            && (node.label == basename
                || node
                    .label
                    .replace('\\', "/")
                    .ends_with(&format!("/{basename}")))
        {
            // A file node claims the old extension-less filename alias even
            // when its canonical ID was salted after a .h/.cpp collision.
            add_legacy_alias(&mut legacy, normalize_id(stem), &node.id);
            if let Some(parent) = path
                .parent()
                .and_then(|value| value.file_name())
                .and_then(|value| value.to_str())
            {
                add_legacy_alias(
                    &mut legacy,
                    normalize_id(&format!("{parent}_{stem}")),
                    &node.id,
                );
            }
        }
    }

    let mut all_edges: Vec<Edge> = extractions
        .iter()
        .flat_map(|e| e.edges.iter().cloned())
        .collect();
    all_edges.sort_by(|a, b| {
        (a.true_source(), a.true_target(), a.relation.as_str()).cmp(&(
            b.true_source(),
            b.true_target(),
            b.relation.as_str(),
        ))
    });
    let mut links = Vec::new();
    let mut seen_edges: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for mut edge in all_edges {
        let directional_source = edge.true_source().to_owned();
        let directional_target = edge.true_target().to_owned();
        if directional_source != edge.source {
            report.repair_edge(EdgeRepairReason::DirectionalSourceRestored);
        }
        if directional_target != edge.target {
            report.repair_edge(EdgeRepairReason::DirectionalTargetRestored);
        }
        let source = repair(&directional_source, &nodes, &normalized, &legacy, &remap);
        let target = repair(&directional_target, &nodes, &normalized, &legacy, &remap);
        record_endpoint_repair(&mut report, source.as_ref(), true);
        record_endpoint_repair(&mut report, target.as_ref(), false);
        let (source, target) = match (source, target) {
            (Some(source), Some(target)) => (source.id, target.id),
            (None, None) => {
                report.drop_edge(EdgeDropReason::UnresolvedSourceAndTarget);
                continue;
            }
            (None, Some(_)) => {
                report.drop_edge(EdgeDropReason::UnresolvedSource);
                continue;
            }
            (Some(_), None) => {
                report.drop_edge(EdgeDropReason::UnresolvedTarget);
                continue;
            }
        };
        if source == target
            && matches!(
                edge.relation.as_str(),
                "imports" | "imports_from" | "re_exports"
            )
        {
            report.drop_edge(EdgeDropReason::ImportSelfLoop);
            continue;
        }
        let source_node = &nodes[&source];
        let target_node = &nodes[&target];
        let source_family = language_family(&source_node.source_file);
        let target_family = language_family(&target_node.source_file);
        let cross_language =
            source_family.is_some() && target_family.is_some() && source_family != target_family;
        if cross_language
            && ((edge.relation == "calls"
                && edge.confidence != graphoxide_core::Confidence::Extracted)
                || matches!(
                    edge.relation.as_str(),
                    "imports" | "imports_from" | "references"
                ))
        {
            if edge.relation == "calls" {
                report.drop_edge(EdgeDropReason::CrossLanguageInferredCall);
            } else {
                report.drop_edge(EdgeDropReason::CrossLanguageImportOrReference);
            }
            continue;
        }
        edge.source = source.clone();
        edge.target = target.clone();
        edge.extra.insert("_src".into(), source.into());
        edge.extra.insert("_tgt".into(), target.into());
        if edge.extra.remove("target_file").is_some() {
            report.repair_edge(EdgeRepairReason::TargetFileMetadataRemoved);
        }
        if edge.extra.remove("local_alias").is_some() {
            report.repair_edge(EdgeRepairReason::LocalAliasMetadataRemoved);
        }
        for key in ["weight", "confidence_score"] {
            if let Some(value) = edge.extra.get(key).cloned() {
                let number = value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                    .filter(|number| number.is_finite() && *number >= 0.0)
                    .unwrap_or(1.0);
                let normalized = serde_json::Value::from(number);
                if normalized != value {
                    report.repair_edge(match key {
                        "weight" => EdgeRepairReason::WeightNormalized,
                        _ => EdgeRepairReason::ConfidenceScoreNormalized,
                    });
                }
                edge.extra.insert(key.into(), normalized);
            }
        }
        if edge.source_file.is_empty() {
            let source_file = if !source_node.source_file.is_empty() {
                source_node.source_file.clone()
            } else {
                target_node.source_file.clone()
            };
            if !source_file.is_empty() {
                edge.source_file = source_file;
                report.repair_edge(EdgeRepairReason::SourceFileBackfilled);
            }
        }
        if !edge.extra.contains_key("confidence_score") {
            edge.extra.insert(
                "confidence_score".into(),
                edge.confidence.default_score().into(),
            );
            report.repair_edge(EdgeRepairReason::ConfidenceScoreBackfilled);
        }
        let key = (
            edge.source.clone(),
            edge.target.clone(),
            edge.relation.clone(),
        );
        if let Some(position) = seen_edges.get(&key).copied() {
            links[position] = edge;
            report.drop_edge(EdgeDropReason::ExactDuplicate);
        } else {
            seen_edges.insert(key, links.len());
            links.push(edge);
        }
    }
    let node_ids: BTreeSet<_> = nodes.keys().cloned().collect();
    let mut hyperedges = Vec::new();
    let mut hyperedge_ids = BTreeSet::new();
    for extraction in extractions {
        for raw in &extraction.hyperedges {
            let Some(mut object) = raw.as_object().cloned() else {
                report.drop_hyperedge(HyperedgeDropReason::NonObject);
                continue;
            };
            if !object.get("nodes").is_some_and(|v| v.is_array()) {
                for alias in ["members", "node_ids"] {
                    if object.get(alias).is_some_and(|value| value.is_array()) {
                        let value = object.remove(alias).expect("checked above");
                        object.insert("nodes".into(), value);
                        report.repair_hyperedge(HyperedgeRepairReason::MembersAliasNormalized);
                        break;
                    }
                }
            }
            object.remove("members");
            object.remove("node_ids");
            let Some(raw_members) = object.get("nodes").and_then(|value| value.as_array()) else {
                report.drop_hyperedge(HyperedgeDropReason::MissingOrInvalidMembers);
                continue;
            };
            let mut members = Vec::new();
            for raw_member in raw_members {
                let Some(member) = raw_member.as_str() else {
                    report.repair_hyperedge(HyperedgeRepairReason::NonStringMemberRemoved);
                    continue;
                };
                let Some(repaired) = repair(member, &nodes, &normalized, &legacy, &remap) else {
                    report.repair_hyperedge(HyperedgeRepairReason::UnresolvedMemberRemoved);
                    continue;
                };
                record_hyperedge_member_repair(&mut report, repaired.repair);
                if node_ids.contains(&repaired.id) {
                    members.push(repaired.id);
                } else {
                    report.repair_hyperedge(HyperedgeRepairReason::UnresolvedMemberRemoved);
                }
            }
            let member_count = members.len();
            let mut seen_members = BTreeSet::new();
            members.retain(|member| seen_members.insert(member.clone()));
            for _ in members.len()..member_count {
                report.repair_hyperedge(HyperedgeRepairReason::DuplicateMemberRemoved);
            }
            if members.is_empty() {
                report.drop_hyperedge(HyperedgeDropReason::NoResolvedMembers);
                continue;
            }
            object.insert("nodes".into(), serde_json::json!(members));
            let declared_id = object
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|value| !value.is_empty());
            let base = declared_id.unwrap_or("hyperedge");
            if declared_id.is_none() {
                report.repair_hyperedge(HyperedgeRepairReason::MissingIdBackfilled);
            }
            let mut id = base.to_owned();
            if !hyperedge_ids.insert(id.clone()) {
                let source_file = object
                    .get("source_file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("duplicate");
                let mut ordinal = 1usize;
                loop {
                    let ordinal_text = ordinal.to_string();
                    let candidate = if ordinal == 1 {
                        graphoxide_core::make_id(&[base, source_file])
                    } else {
                        graphoxide_core::make_id(&[base, source_file, &ordinal_text])
                    };
                    if hyperedge_ids.insert(candidate.clone()) {
                        id = candidate;
                        break;
                    }
                    ordinal += 1;
                }
                report.repair_hyperedge(HyperedgeRepairReason::DuplicateIdRemapped);
            }
            object.insert("id".into(), id.into());
            hyperedges.push(object.into());
        }
    }
    let mut graph = KnowledgeGraph {
        directed: options.directed,
        multigraph: false,
        nodes: nodes.into_values().collect(),
        links,
        hyperedges,
        extra: BTreeMap::from([("graph".into(), serde_json::json!({}))]),
    };
    let dedup = if options.deduplicate_semantic_nodes {
        crate::dedup::deduplicate_with_report(&mut graph)
    } else {
        crate::dedup::DeduplicationReport::default()
    };
    report.merge_nodes(
        NodeMergeReason::SemanticSameLabel,
        dedup.same_label_nodes_merged,
    );
    report.merge_nodes(
        NodeMergeReason::SemanticFuzzyLabel,
        dedup.fuzzy_label_nodes_merged,
    );
    report.repair_edges(
        EdgeRepairReason::SourceAfterNodeMerge,
        dedup.source_endpoints_remapped,
    );
    report.repair_edges(
        EdgeRepairReason::TargetAfterNodeMerge,
        dedup.target_endpoints_remapped,
    );
    report.drop_edges(
        EdgeDropReason::SelfLoopAfterNodeMerge,
        dedup.self_loops_dropped,
    );
    report.drop_edges(
        EdgeDropReason::ExactDuplicateAfterNodeMerge,
        dedup.duplicate_edges_dropped,
    );
    report.repair_hyperedges(
        HyperedgeRepairReason::MemberAfterNodeMerge,
        dedup.hyperedge_members_remapped,
    );
    report.repair_hyperedges(
        HyperedgeRepairReason::DuplicateMemberAfterNodeMerge,
        dedup.hyperedge_duplicate_members_removed,
    );
    if !options.directed && options.collapse_undirected_reverse_edges {
        let mut seen = BTreeSet::new();
        graph.links.retain(|edge| {
            let (left, right) = if edge.source <= edge.target {
                (edge.source.clone(), edge.target.clone())
            } else {
                (edge.target.clone(), edge.source.clone())
            };
            if seen.insert((left, right, edge.relation.clone())) {
                true
            } else {
                report.drop_edge(EdgeDropReason::UndirectedReverseDuplicate);
                false
            }
        });
    }
    disambiguate_file_labels_in_nodes(&mut graph.nodes);
    report.output_nodes = graph.nodes.len();
    report.output_edges = graph.links.len();
    report.output_hyperedges = graph.hyperedges.len();
    Ok((graph, report))
}

fn merge_node(existing: &mut Node, incoming: &Node) {
    let existing_structural = is_structural_node(existing);
    let incoming_structural = is_structural_node(incoming);
    if incoming_structural && !existing_structural {
        let old_extra = existing.extra.clone();
        *existing = incoming.clone();
        for (key, value) in old_extra {
            existing.extra.entry(key).or_insert(value);
        }
    } else if existing_structural && !incoming_structural {
        for (key, value) in &incoming.extra {
            // Parser-authored identity/type metadata is authoritative. Semantic
            // annotations enrich missing keys but may never downgrade `_origin`
            // or overwrite an AST declaration's structural type.
            existing
                .extra
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        if existing.source_location.is_none() {
            existing.source_location = incoming.source_location.clone();
        }
    } else {
        let mut merged = incoming.clone();
        for (key, value) in &existing.extra {
            merged
                .extra
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        if merged.label.is_empty() {
            merged.label = existing.label.clone();
        }
        if merged.source_file.is_empty() {
            merged.source_file = existing.source_file.clone();
        }
        if merged.source_location.is_none() {
            merged.source_location = existing.source_location.clone();
        }
        *existing = merged;
    }
}

fn is_structural_node(node: &Node) -> bool {
    node.extra
        .get("_origin")
        .and_then(|value| value.as_str())
        .and_then(origin_is_structural)
        == Some(true)
}

/// Graphviz IDs and labels are grammar-defined entity data, even when a node's
/// display label happens to equal the source filename. File-node heuristics
/// must not rewrite or merge those declared identities.
fn is_declared_graphviz_entity(node: &Node) -> bool {
    node.extra
        .get("diagram_format")
        .and_then(|value| value.as_str())
        == Some("graphviz")
        && node
            .extra
            .get("dot_id")
            .and_then(|value| value.as_str())
            .is_some()
}

fn is_document_package_entity(node: &Node) -> bool {
    node.extra.get("_origin").and_then(|value| value.as_str()) == Some("document_package")
}

fn is_document_package_child(node: &Node) -> bool {
    is_document_package_entity(node)
        && (node.extra.contains_key("unit_ordinal") || node.extra.contains_key("internal_part"))
}

fn document_twin_remap(nodes: &BTreeMap<String, Node>) -> BTreeMap<String, String> {
    let mut remap = BTreeMap::new();
    for (id, canonical) in nodes {
        let Some(bare_id) = id.strip_suffix("_doc") else {
            continue;
        };
        let Some(bare) = nodes.get(bare_id) else {
            continue;
        };
        if !is_declared_graphviz_entity(canonical)
            && !is_declared_graphviz_entity(bare)
            && !is_document_package_entity(canonical)
            && !is_document_package_entity(bare)
            && canonical.file_type == "document"
            && bare.file_type == "document"
            && !canonical.source_file.is_empty()
            && canonical.source_file == bare.source_file
        {
            remap.insert(bare_id.to_owned(), id.clone());
        }
    }
    remap
}

fn add_legacy_alias(aliases: &mut BTreeMap<String, Vec<String>>, alias: String, id: &str) {
    if alias.is_empty() {
        return;
    }
    let candidates = aliases.entry(alias).or_default();
    if !candidates.iter().any(|candidate| candidate == id) {
        candidates.push(id.to_owned());
    }
}

pub fn semantic_id_remap(extractions: &[Extraction]) -> BTreeMap<String, String> {
    let mut remap = BTreeMap::new();
    for node in extractions.iter().flat_map(|extraction| &extraction.nodes) {
        if is_structural_node(node) || node.id.is_empty() || node.source_file.is_empty() {
            continue;
        }
        let path = std::path::Path::new(&node.source_file);
        if path.is_absolute() {
            continue;
        }
        let canonical_stem = normalize_id(&source_file_stem(path));
        if canonical_stem.is_empty()
            || node.id == canonical_stem
            || node.id.starts_with(&format!("{canonical_stem}_"))
        {
            continue;
        }
        let file_stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(normalize_id)
            .unwrap_or_default();
        let parent_stem = path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str())
            .map(|parent| normalize_id(&format!("{parent}_{file_stem}")))
            .unwrap_or_default();
        let mut replacement = None;
        let absolute_stem = node
            .extra
            .get("_absolute_source_stem")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_owned();
        for old_stem in [absolute_stem, parent_stem, file_stem] {
            if old_stem.is_empty() || old_stem == canonical_stem {
                continue;
            }
            if node.id == old_stem {
                replacement = Some(canonical_stem.clone());
                break;
            }
            if let Some(suffix) = node.id.strip_prefix(&format!("{old_stem}_")) {
                replacement = Some(graphoxide_core::make_id(&[&canonical_stem, suffix]));
                break;
            }
        }
        if let Some(replacement) = replacement.filter(|replacement| replacement != &node.id) {
            remap.insert(node.id.clone(), replacement);
        }
    }
    remap
}

/// Return the extension-free portable source path used as an ID stem. Project
/// root markers such as `.` intentionally have no per-file stem.
pub fn source_file_stem(path: &std::path::Path) -> String {
    if path.file_name().is_none() {
        return String::new();
    }
    let mut stem = path.to_path_buf();
    if !stem.set_extension("") {
        return String::new();
    }
    stem.to_string_lossy().replace('\\', "/")
}

/// Detect a file-level node still carrying one of the pre-full-path ID stems.
/// Only `L1` nodes are considered, matching upstream's false-positive guard for
/// package-scoped symbol IDs.
pub fn graph_has_legacy_ids(nodes: &[Node], root: Option<&std::path::Path>) -> bool {
    for node in nodes
        .iter()
        .filter(|node| node.source_location.as_deref() == Some("L1"))
    {
        if node.id.is_empty() || node.source_file.is_empty() {
            continue;
        }
        let source_file = match root {
            Some(root) => relativize_source_file(&node.source_file, root),
            None => node.source_file.replace('\\', "/"),
        };
        let path = std::path::Path::new(&source_file);
        if path.is_absolute() || path.file_name().is_none() {
            continue;
        }
        let mut stem_path = path.to_path_buf();
        stem_path.set_extension("");
        let canonical = normalize_id(&stem_path.to_string_lossy());
        if canonical.is_empty() {
            continue;
        }
        let id = normalize_id(&node.id);
        if id == canonical || id.starts_with(&format!("{canonical}_")) {
            continue;
        }
        let file_stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(normalize_id)
            .unwrap_or_default();
        let parent_stem = path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str())
            .map(|parent| normalize_id(&format!("{parent}_{file_stem}")))
            .unwrap_or_default();
        if [parent_stem, file_stem].into_iter().any(|legacy| {
            !legacy.is_empty()
                && legacy != canonical
                && (id == legacy || id.starts_with(&format!("{legacy}_")))
        }) {
            return true;
        }
    }
    false
}

fn language_family(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|v| v.to_str())?
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => "js",
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "cu" | "cuh" | "metal" | "m"
        | "mm" => "c",
        "cs" | "razor" | "cshtml" => "csharp",
        "py" | "pyi" => "python",
        "java" | "kt" | "scala" | "groovy" => "jvm",
        "go" => "go",
        "rs" => "rust",
        "rb" | "rake" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "lua" => "lua",
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointRepair {
    Exact,
    SemanticRemap,
    NormalizedId,
    LegacyId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepairedEndpoint {
    id: String,
    repair: EndpointRepair,
}

fn record_endpoint_repair(
    report: &mut BuildReport,
    endpoint: Option<&RepairedEndpoint>,
    source: bool,
) {
    let Some(endpoint) = endpoint else {
        return;
    };
    let reason = match (source, endpoint.repair) {
        (_, EndpointRepair::Exact) => return,
        (true, EndpointRepair::SemanticRemap) => EdgeRepairReason::SourceSemanticRemap,
        (false, EndpointRepair::SemanticRemap) => EdgeRepairReason::TargetSemanticRemap,
        (true, EndpointRepair::NormalizedId) => EdgeRepairReason::SourceNormalizedId,
        (false, EndpointRepair::NormalizedId) => EdgeRepairReason::TargetNormalizedId,
        (true, EndpointRepair::LegacyId) => EdgeRepairReason::SourceLegacyId,
        (false, EndpointRepair::LegacyId) => EdgeRepairReason::TargetLegacyId,
    };
    report.repair_edge(reason);
}

fn record_hyperedge_member_repair(report: &mut BuildReport, repair: EndpointRepair) {
    let reason = match repair {
        EndpointRepair::Exact => return,
        EndpointRepair::SemanticRemap => HyperedgeRepairReason::MemberSemanticRemap,
        EndpointRepair::NormalizedId => HyperedgeRepairReason::MemberNormalizedId,
        EndpointRepair::LegacyId => HyperedgeRepairReason::MemberLegacyId,
    };
    report.repair_hyperedge(reason);
}

fn repair(
    value: &str,
    nodes: &BTreeMap<String, Node>,
    normalized: &BTreeMap<String, Vec<String>>,
    legacy: &BTreeMap<String, Vec<String>>,
    remap: &BTreeMap<String, String>,
) -> Option<RepairedEndpoint> {
    if remap.contains_key(value) {
        let mut id = value.to_owned();
        for _ in 0..=remap.len() {
            let Some(next) = remap.get(&id) else {
                break;
            };
            if next == &id {
                break;
            }
            id = next.clone();
        }
        if nodes.contains_key(&id) {
            return Some(RepairedEndpoint {
                id,
                repair: EndpointRepair::SemanticRemap,
            });
        }
    }
    if nodes.contains_key(value) {
        return Some(RepairedEndpoint {
            id: value.into(),
            repair: EndpointRepair::Exact,
        });
    }
    let key = normalize_id(value);
    if let Some(ids) = normalized.get(&key)
        && ids.len() == 1
    {
        return Some(RepairedEndpoint {
            id: ids[0].clone(),
            repair: EndpointRepair::NormalizedId,
        });
    }
    if let Some(ids) = legacy.get(&key)
        && ids.len() == 1
    {
        return Some(RepairedEndpoint {
            id: ids[0].clone(),
            repair: EndpointRepair::LegacyId,
        });
    }
    None
}
fn path_key(value: &str) -> String {
    value
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_lowercase()
}

fn node_path_key(node: &Node) -> String {
    let source = if node.source_file.is_empty() {
        node.extra
            .get("origin_file")
            .and_then(|value| value.as_str())
            .unwrap_or("")
    } else {
        &node.source_file
    };
    path_key(source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_core::{Confidence, Edge};

    fn node(id: &str) -> Node {
        node_at(id, id, "code", "a.py")
    }

    fn node_at(id: &str, label: &str, file_type: &str, source_file: &str) -> Node {
        Node {
            id: id.into(),
            label: label.into(),
            file_type: file_type.into(),
            source_file: source_file.into(),
            source_location: None,
            community: None,
            extra: BTreeMap::new(),
        }
    }

    fn edge(source: &str, target: &str, relation: &str) -> Edge {
        Edge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            confidence: Confidence::Extracted,
            source_file: "a.py".into(),
            extra: BTreeMap::new(),
        }
    }

    fn count<K: Ord>(counts: &BTreeMap<K, usize>, reason: &K) -> usize {
        counts.get(reason).copied().unwrap_or_default()
    }

    #[test]
    fn specialized_and_fallback_origins_remain_structurally_authoritative() {
        for origin in ["fallback", "terraform", "sql", "dotnet", "scip"] {
            let mut structural = node_at("shared", "Structural", "code", "pkg/mod.py");
            structural.extra.insert("_origin".into(), origin.into());
            structural.extra.insert("type".into(), "function".into());
            let mut semantic = node_at("shared", "Semantic", "concept", "pkg/mod.py");
            semantic.extra.insert("_origin".into(), "semantic".into());
            semantic.extra.insert("type".into(), "concept".into());
            semantic.extra.insert("annotation".into(), true.into());

            let graph = build_graph(&[Extraction {
                nodes: vec![structural, semantic],
                ..Extraction::default()
            }])
            .unwrap();
            let canonical = graph.nodes.iter().find(|node| node.id == "shared").unwrap();
            assert_eq!(canonical.label, "Structural", "origin={origin}");
            assert_eq!(canonical.file_type, "code", "origin={origin}");
            assert_eq!(canonical.extra["_origin"], origin, "origin={origin}");
            assert_eq!(canonical.extra["type"], "function", "origin={origin}");
            assert_eq!(canonical.extra["annotation"], true, "origin={origin}");

            let mut legacy = node_at("mod_symbol", "Symbol", "code", "pkg/mod.py");
            legacy.extra.insert("_origin".into(), origin.into());
            assert!(
                semantic_id_remap(&[Extraction {
                    nodes: vec![legacy],
                    ..Extraction::default()
                }])
                .is_empty(),
                "deterministic {origin} IDs must not enter semantic rekeying"
            );
        }
    }

    #[test]
    fn repairs_normalized_endpoints_and_drops_dangling() {
        let extraction = Extraction {
            nodes: vec![node("foo_bar"), node("target")],
            edges: vec![
                Edge {
                    source: "Foo-Bar".into(),
                    target: "target".into(),
                    relation: "calls".into(),
                    confidence: Confidence::Inferred,
                    source_file: "a.py".into(),
                    extra: BTreeMap::new(),
                },
                Edge {
                    source: "missing".into(),
                    target: "target".into(),
                    relation: "calls".into(),
                    confidence: Confidence::Inferred,
                    source_file: "a.py".into(),
                    extra: BTreeMap::new(),
                },
            ],
            hyperedges: Vec::new(),
        };
        let graph = build_graph(&[extraction]).unwrap();
        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.links[0].source, "foo_bar");
    }

    #[test]
    fn preserves_direction_and_relation_while_deduplicating_exact_identity() {
        let mut duplicate = edge("a", "b", "calls");
        duplicate.extra.insert("winner".into(), 2.into());
        let extraction = Extraction {
            nodes: vec![node("a"), node("b")],
            edges: vec![
                edge("a", "b", "calls"),
                duplicate,
                edge("a", "b", "imports"),
                edge("b", "a", "calls"),
            ],
            hyperedges: Vec::new(),
        };

        let (graph, report) = build_graph_with_report(std::slice::from_ref(&extraction)).unwrap();
        let identities: BTreeSet<_> = graph
            .links
            .iter()
            .map(|value| {
                (
                    value.source.as_str(),
                    value.target.as_str(),
                    value.relation.as_str(),
                )
            })
            .collect();
        assert_eq!(
            identities,
            BTreeSet::from([
                ("a", "b", "calls"),
                ("a", "b", "imports"),
                ("b", "a", "calls"),
            ])
        );
        assert_eq!(
            graph
                .links
                .iter()
                .find(|value| {
                    value.source == "a" && value.target == "b" && value.relation == "calls"
                })
                .unwrap()
                .extra["winner"],
            2
        );
        assert_eq!(
            count(&report.edge_drops, &EdgeDropReason::ExactDuplicate),
            1
        );
        assert_eq!(report.input_edges, 4);
        assert_eq!(report.output_edges, 3);
        assert!(report.edges_accounted_for());

        let wrapped = build_graph(&[extraction]).unwrap();
        assert_eq!(
            serde_json::to_value(wrapped).unwrap(),
            serde_json::to_value(graph).unwrap()
        );
    }

    #[test]
    fn reports_each_endpoint_and_policy_drop() {
        let js = node_at("js", "js", "code", "src/a.js");
        let py = node_at("py", "py", "code", "src/b.py");
        let mut inferred_call = edge("js", "py", "calls");
        inferred_call.confidence = Confidence::Inferred;
        let extraction = Extraction {
            nodes: vec![js, py],
            edges: vec![
                edge("missing", "js", "calls"),
                edge("js", "missing", "calls"),
                edge("missing-a", "missing-b", "calls"),
                edge("js", "js", "imports"),
                inferred_call,
                edge("js", "py", "references"),
            ],
            hyperedges: Vec::new(),
        };

        let (graph, report) = build_graph_with_report(&[extraction]).unwrap();
        assert!(graph.links.is_empty());
        for reason in [
            EdgeDropReason::UnresolvedSource,
            EdgeDropReason::UnresolvedTarget,
            EdgeDropReason::UnresolvedSourceAndTarget,
            EdgeDropReason::ImportSelfLoop,
            EdgeDropReason::CrossLanguageInferredCall,
            EdgeDropReason::CrossLanguageImportOrReference,
        ] {
            assert_eq!(count(&report.edge_drops, &reason), 1, "{reason:?}");
        }
        assert_eq!(report.input_edges, 6);
        assert_eq!(report.dropped_edge_count(), 6);
        assert!(report.edges_accounted_for());
    }

    #[test]
    fn reports_endpoint_and_metadata_repairs_and_serializes_reason_names() {
        let extraction = Extraction {
            nodes: vec![
                node_at("foo_bar", "Foo", "code", "src/a.py"),
                node_at("opaque", "Handler", "code", "src/demo.py"),
            ],
            edges: vec![
                Edge {
                    source: "opaque".into(),
                    target: "foo_bar".into(),
                    relation: "calls".into(),
                    confidence: Confidence::Inferred,
                    source_file: String::new(),
                    extra: BTreeMap::from([
                        ("_src".into(), "Foo-Bar".into()),
                        ("_tgt".into(), "demo_Handler".into()),
                        ("weight".into(), "-2".into()),
                        ("confidence_score".into(), "0.25".into()),
                        ("target_file".into(), "src/demo.py".into()),
                        ("local_alias".into(), "handler".into()),
                    ]),
                },
                edge("foo_bar", "opaque", "references"),
            ],
            hyperedges: Vec::new(),
        };

        let (graph, report) = build_graph_with_report(&[extraction]).unwrap();
        let repaired = graph
            .links
            .iter()
            .find(|value| value.relation == "calls")
            .unwrap();
        assert_eq!(
            (repaired.source.as_str(), repaired.target.as_str()),
            ("foo_bar", "opaque")
        );
        assert_eq!(repaired.source_file, "src/a.py");
        assert_eq!(repaired.extra["weight"], 1.0);
        assert_eq!(repaired.extra["confidence_score"], 0.25);
        assert!(!repaired.extra.contains_key("target_file"));
        assert!(!repaired.extra.contains_key("local_alias"));

        for reason in [
            EdgeRepairReason::SourceNormalizedId,
            EdgeRepairReason::TargetLegacyId,
            EdgeRepairReason::DirectionalSourceRestored,
            EdgeRepairReason::DirectionalTargetRestored,
            EdgeRepairReason::WeightNormalized,
            EdgeRepairReason::ConfidenceScoreNormalized,
            EdgeRepairReason::SourceFileBackfilled,
            EdgeRepairReason::ConfidenceScoreBackfilled,
            EdgeRepairReason::TargetFileMetadataRemoved,
            EdgeRepairReason::LocalAliasMetadataRemoved,
        ] {
            assert_eq!(count(&report.edge_repairs, &reason), 1, "{reason:?}");
        }
        assert!(report.edges_accounted_for());

        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["edge_repairs"]["source_normalized_id"], 1);
        let round_trip: BuildReport = serde_json::from_value(value).unwrap();
        assert_eq!(round_trip, report);
    }

    #[test]
    fn accounts_for_node_merges_and_dedup_induced_edge_losses() {
        let mut ast = node_at("ast", "Widget", "code", "src/widget.py");
        ast.extra.insert("_origin".into(), "ast".into());
        ast.extra.insert("type".into(), "function".into());
        let mut ghost = node_at("ghost", "Widget", "code", "");
        ghost.extra.insert("_origin".into(), "semantic".into());
        ghost.extra.insert("type".into(), "concept".into());
        ghost
            .extra
            .insert("origin_file".into(), "src/widget.py".into());
        ghost.extra.insert("annotation".into(), true.into());

        let extraction = Extraction {
            nodes: vec![
                node(""),
                node("duplicate"),
                node("duplicate"),
                ast,
                ghost,
                node_at("concept-a", "Architecture", "concept", "architecture.md"),
                node_at("concept-b", "Architecture", "concept", "architecture.md"),
                node_at("fuzzy-a", "payment service", "concept", ""),
                node_at("fuzzy-b", "payment servixe", "concept", ""),
                node("target"),
            ],
            edges: vec![
                edge("concept-a", "target", "calls"),
                edge("concept-b", "target", "calls"),
                edge("concept-a", "concept-b", "calls"),
                edge("ghost", "target", "references"),
            ],
            hyperedges: vec![serde_json::json!({
                "id": "concept-group",
                "nodes": ["concept-a", "concept-b"]
            })],
        };

        let (graph, report) = build_graph_with_report(&[extraction]).unwrap();
        assert_eq!(count(&report.node_drops, &NodeDropReason::EmptyId), 1);
        for reason in [
            NodeMergeReason::DuplicateId,
            NodeMergeReason::SemanticToAst,
            NodeMergeReason::SemanticSameLabel,
            NodeMergeReason::SemanticFuzzyLabel,
        ] {
            assert_eq!(count(&report.node_merges, &reason), 1, "{reason:?}");
        }
        assert_eq!(report.input_nodes, 10);
        assert_eq!(report.output_nodes, 5);
        assert!(report.nodes_accounted_for());
        assert_eq!(
            graph
                .nodes
                .iter()
                .find(|value| value.id == "ast")
                .unwrap()
                .extra["annotation"],
            true
        );
        let canonical = graph.nodes.iter().find(|value| value.id == "ast").unwrap();
        assert_eq!(canonical.extra["_origin"], "ast");
        assert_eq!(canonical.extra["type"], "function");

        assert_eq!(
            count(&report.edge_drops, &EdgeDropReason::SelfLoopAfterNodeMerge),
            1
        );
        assert_eq!(
            count(
                &report.edge_drops,
                &EdgeDropReason::ExactDuplicateAfterNodeMerge
            ),
            1
        );
        assert_eq!(
            count(&report.edge_repairs, &EdgeRepairReason::SourceSemanticRemap),
            1
        );
        assert_eq!(
            count(
                &report.edge_repairs,
                &EdgeRepairReason::SourceAfterNodeMerge
            ),
            1
        );
        assert_eq!(
            count(
                &report.edge_repairs,
                &EdgeRepairReason::TargetAfterNodeMerge
            ),
            1
        );
        assert_eq!(report.input_edges, 4);
        assert_eq!(report.output_edges, 2);
        assert!(report.edges_accounted_for());
        assert_eq!(
            count(
                &report.hyperedge_repairs,
                &HyperedgeRepairReason::MemberAfterNodeMerge
            ),
            1
        );
        assert_eq!(
            count(
                &report.hyperedge_repairs,
                &HyperedgeRepairReason::DuplicateMemberAfterNodeMerge
            ),
            1
        );
        assert_eq!(
            graph.hyperedges[0]["nodes"],
            serde_json::json!(["concept-a"])
        );
        assert!(report.hyperedges_accounted_for());
    }

    #[test]
    fn semantic_legacy_rekey_chases_into_ast_canonical_remap() {
        let mut ast = node_at("ast_widget", "Widget", "code", "pkg/mod.py");
        ast.extra.insert("_origin".into(), "ast".into());
        let mut semantic = node_at("mod_widget", "Widget", "code", "pkg/mod.py");
        semantic.extra.insert("_origin".into(), "semantic".into());
        let extraction = Extraction {
            nodes: vec![ast, semantic, node("target")],
            edges: vec![edge("mod_widget", "target", "references")],
            hyperedges: vec![serde_json::json!({"id": "h", "nodes": ["mod_widget"]})],
        };

        let graph = build_graph(&[extraction]).unwrap();
        assert!(graph.nodes.iter().any(|node| node.id == "ast_widget"));
        assert!(!graph.nodes.iter().any(|node| node.id == "pkg_mod_widget"));
        assert_eq!(graph.links[0].source, "ast_widget");
        assert_eq!(
            graph.hyperedges[0]["nodes"],
            serde_json::json!(["ast_widget"])
        );
    }

    #[test]
    fn accounts_for_hyperedge_drops_member_repairs_and_unique_ids() {
        let mut ast = node_at("ast", "Widget", "code", "src/widget.py");
        ast.extra.insert("_origin".into(), "ast".into());
        let mut ghost = node_at("ghost", "Widget", "code", "");
        ghost.extra.insert("_origin".into(), "semantic".into());
        ghost
            .extra
            .insert("origin_file".into(), "src/widget.py".into());
        let extraction = Extraction {
            nodes: vec![node("foo_bar"), ast, ghost],
            edges: Vec::new(),
            hyperedges: vec![
                serde_json::json!("not-an-object"),
                serde_json::json!({"id": "missing-members"}),
                serde_json::json!({"id": "empty", "nodes": ["missing", 42]}),
                serde_json::json!({
                    "id": "h",
                    "members": ["Foo-Bar", "foo_bar", "ghost", 42, "missing"]
                }),
                serde_json::json!({"id": "h", "source_file": "a.py", "nodes": ["foo_bar"]}),
                serde_json::json!({"id": "h", "source_file": "a.py", "nodes": ["foo_bar"]}),
                serde_json::json!({"nodes": ["foo_bar"]}),
            ],
        };

        let (graph, report) = build_graph_with_report(&[extraction]).unwrap();
        for reason in [
            HyperedgeDropReason::NonObject,
            HyperedgeDropReason::MissingOrInvalidMembers,
            HyperedgeDropReason::NoResolvedMembers,
        ] {
            assert_eq!(count(&report.hyperedge_drops, &reason), 1, "{reason:?}");
        }
        assert_eq!(
            count(
                &report.hyperedge_repairs,
                &HyperedgeRepairReason::MembersAliasNormalized
            ),
            1
        );
        assert_eq!(
            count(
                &report.hyperedge_repairs,
                &HyperedgeRepairReason::MemberNormalizedId
            ),
            1
        );
        assert_eq!(
            count(
                &report.hyperedge_repairs,
                &HyperedgeRepairReason::MemberSemanticRemap
            ),
            1
        );
        assert_eq!(
            count(
                &report.hyperedge_repairs,
                &HyperedgeRepairReason::NonStringMemberRemoved
            ),
            2
        );
        assert_eq!(
            count(
                &report.hyperedge_repairs,
                &HyperedgeRepairReason::UnresolvedMemberRemoved
            ),
            2
        );
        assert_eq!(
            count(
                &report.hyperedge_repairs,
                &HyperedgeRepairReason::DuplicateMemberRemoved
            ),
            1
        );
        assert_eq!(
            count(
                &report.hyperedge_repairs,
                &HyperedgeRepairReason::DuplicateIdRemapped
            ),
            2
        );
        assert_eq!(
            count(
                &report.hyperedge_repairs,
                &HyperedgeRepairReason::MissingIdBackfilled
            ),
            1
        );
        assert_eq!(report.input_hyperedges, 7);
        assert_eq!(report.output_hyperedges, 4);
        assert!(report.hyperedges_accounted_for());

        let ids: BTreeSet<_> = graph
            .hyperedges
            .iter()
            .map(|value| value["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), graph.hyperedges.len());
        assert_eq!(
            graph
                .hyperedges
                .iter()
                .find(|value| value["id"] == "h")
                .unwrap()["nodes"],
            serde_json::json!(["foo_bar", "ast"])
        );
    }
}
