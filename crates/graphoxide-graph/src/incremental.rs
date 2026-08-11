//! Incremental graph merge with source replacement and deletion pruning.

use crate::provenance::origin_is_structural;
use crate::{build_graph_with_options, build_graph_with_options_and_root, BuildOptions};
use anyhow::Context;
use graphoxide_core::{
    read_graph, read_graph_with_cap, Edge, Extraction, KnowledgeGraph, Node,
    CONTAINER_SOURCE_ATTRIBUTE,
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Conservative expansion from serialized graph facts to the peak working set
/// used while retaining, deduplicating, normalizing, and writing them.
///
/// Incremental CLI callers reserve a separate share of their graph-stage
/// budget for the parsed baseline and use this multiplier to derive its file
/// cap. The same charge is applied before a raw merge clones retained facts.
pub const INCREMENTAL_GRAPH_WORKING_SET_MULTIPLIER: usize = 8;

/// Incremental equivalents of upstream's optional `directed=`/`dedup=` knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncrementalOptions {
    /// `None` inherits the existing graph's flag; a missing graph defaults false.
    pub directed: Option<bool>,
    pub deduplicate_semantic_nodes: bool,
    pub collapse_undirected_reverse_edges: bool,
    /// Optional tighter cap for the existing graph file.
    pub max_graph_bytes: Option<u64>,
}

impl Default for IncrementalOptions {
    fn default() -> Self {
        Self {
            directed: None,
            deduplicate_semantic_nodes: true,
            collapse_undirected_reverse_edges: false,
            max_graph_bytes: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceIdentity {
    source_file: String,
    container_source: Option<String>,
}

/// Best-effort scan root for a graph when an incremental caller omitted it.
pub fn infer_merge_root(graph_path: impl AsRef<Path>) -> Option<PathBuf> {
    let graph_path = graph_path.as_ref();
    for marker_name in [".graphoxide_root", ".graphify_root"] {
        let marker = graph_path.parent()?.join(marker_name);
        if let Ok(recorded) = fs::read_to_string(marker) {
            let recorded = recorded.trim();
            if !recorded.is_empty()
                && let Ok(root) = canonicalize_with_missing_tail(Path::new(recorded))
            {
                return Some(root);
            }
        }
    }
    canonicalize_with_missing_tail(graph_path.parent()?.parent()?).ok()
}

/// Load an existing graph and merge replacement chunks. Contributions owned by
/// re-extracted source files are replaced, unchanged hyperedges survive, and
/// deleted sources are pruned using raw, relative, and canonical path identity.
pub fn build_merge(
    new_chunks: &[Extraction],
    graph_path: impl AsRef<Path>,
    prune_sources: &[PathBuf],
    root: Option<&Path>,
) -> anyhow::Result<KnowledgeGraph> {
    build_merge_with_options(
        new_chunks,
        graph_path,
        prune_sources,
        root,
        IncrementalOptions::default(),
    )
}

/// Incremental merge with explicit direction, deduplication, and load-cap policy.
pub fn build_merge_with_options(
    new_chunks: &[Extraction],
    graph_path: impl AsRef<Path>,
    prune_sources: &[PathBuf],
    root: Option<&Path>,
    options: IncrementalOptions,
) -> anyhow::Result<KnowledgeGraph> {
    let graph_path = graph_path.as_ref();
    let existing = if graph_path.exists() {
        let loaded = match options.max_graph_bytes {
            Some(cap) => read_graph_with_cap(graph_path, cap),
            None => read_graph(graph_path),
        };
        Some(loaded.map_err(|error| {
            anyhow::anyhow!(
                "Cannot read {} for incremental merge: {error}; rebuild the graph",
                graph_path.display()
            )
        })?)
    } else {
        None
    };
    let effective_root = root
        .map(Path::to_path_buf)
        .or_else(|| infer_merge_root(graph_path));
    let inherited_directed = existing.as_ref().is_some_and(|graph| graph.directed);
    let directed = options.directed.unwrap_or(inherited_directed);
    let (new_ast_sources, new_semantic_sources) =
        tier_sources(new_chunks, effective_root.as_deref());
    let new_container_sources = unowned_sources(
        &new_ast_sources,
        &new_semantic_sources,
        effective_root.as_deref(),
    );
    let mut chunks = Vec::new();
    let mut carried_chunk_index = None;
    if let Some(existing) = existing {
        let mut carried = Extraction {
            nodes: existing.nodes,
            edges: existing.links,
            hyperedges: existing.hyperedges,
        };
        carried.nodes.retain(|node| {
            !is_replaced_node(
                node,
                &new_ast_sources,
                &new_semantic_sources,
                &new_container_sources,
                effective_root.as_deref(),
            )
        });
        carried.edges.retain(|edge| {
            !is_replaced_edge(
                edge,
                &new_ast_sources,
                &new_semantic_sources,
                &new_container_sources,
                effective_root.as_deref(),
            )
        });
        carried.hyperedges.retain(|hyperedge| {
            !is_replaced_hyperedge(
                hyperedge,
                &new_ast_sources,
                &new_semantic_sources,
                &new_container_sources,
                effective_root.as_deref(),
            )
        });
        carried_chunk_index = Some(chunks.len());
        chunks.push(carried);
    }
    chunks.extend_from_slice(new_chunks);

    // Replacement wins over a contradictory deletion for the same source.
    let effective_prunes: Vec<&PathBuf> = prune_sources
        .iter()
        .filter(|prune| {
            !new_ast_sources
                .iter()
                .chain(&new_semantic_sources)
                .any(|identity| {
                    identity.container_source.is_none()
                        && same_source(
                            &prune.to_string_lossy(),
                            &identity.source_file,
                            effective_root.as_deref(),
                        )
                })
        })
        .collect();
    let pruned_node_ids = carried_chunk_index
        .and_then(|index| chunks.get(index))
        .into_iter()
        .flat_map(|chunk| &chunk.nodes)
        .filter(|node| node_owned_by_prunes(node, &effective_prunes, effective_root.as_deref()))
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    let surviving_node_ids = chunks
        .iter()
        .flat_map(|chunk| &chunk.nodes)
        .filter(|node| !node_owned_by_prunes(node, &effective_prunes, effective_root.as_deref()))
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    let removed_pruned_node_ids = pruned_node_ids
        .into_iter()
        .filter(|id| !surviving_node_ids.contains(id.as_str()))
        .collect::<BTreeSet<_>>();
    drop(surviving_node_ids);
    for (index, chunk) in chunks.iter_mut().enumerate() {
        chunk.nodes.retain(|node| {
            !node_owned_by_prunes(node, &effective_prunes, effective_root.as_deref())
        });
        chunk.edges.retain(|edge| {
            !edge_owned_by_prunes(edge, &effective_prunes, effective_root.as_deref())
                && (carried_chunk_index != Some(index)
                    || !edge_references_removed_node(edge, &removed_pruned_node_ids))
        });
        chunk.hyperedges.retain(|hyperedge| {
            !hyperedge_owned_by_prunes(hyperedge, &effective_prunes, effective_root.as_deref())
                && (carried_chunk_index != Some(index)
                    || hyperedge_survives_removed_members(hyperedge, &removed_pruned_node_ids))
        });
        if carried_chunk_index == Some(index) {
            for hyperedge in &mut chunk.hyperedges {
                prune_hyperedge_members(hyperedge, &removed_pruned_node_ids);
            }
        }
    }
    let build_options = BuildOptions {
        directed,
        deduplicate_semantic_nodes: options.deduplicate_semantic_nodes,
        collapse_undirected_reverse_edges: options.collapse_undirected_reverse_edges,
    };
    if let Some(root) = effective_root {
        build_graph_with_options_and_root(&chunks, root, build_options)
    } else {
        build_graph_with_options(&chunks, build_options)
    }
}

/// Raw `extract --no-cluster` mirror of [`build_merge_with_options`].
pub fn merge_raw_extraction(
    new: &Extraction,
    graph_path: impl AsRef<Path>,
    prune_sources: &[PathBuf],
    root: Option<&Path>,
) -> anyhow::Result<Extraction> {
    let graph_path = graph_path.as_ref();
    if !graph_path.exists() {
        return Ok(new.clone());
    }
    let existing = read_graph(graph_path).with_context(|| {
        format!(
            "Cannot read {} for incremental merge; rebuild the graph",
            graph_path.display()
        )
    })?;
    let effective_root = root
        .map(Path::to_path_buf)
        .or_else(|| infer_merge_root(graph_path));
    merge_raw_extraction_from_graph(
        new.clone(),
        &existing,
        prune_sources,
        effective_root.as_deref(),
    )
}

/// Merge a raw extraction against an already-loaded graph.
///
/// This is the allocation-safe counterpart used by callers that also need the
/// baseline for stale-source detection or community remapping: the graph is
/// parsed once, rather than once for inspection and again for merging.
pub fn merge_raw_extraction_from_graph(
    new: Extraction,
    existing: &KnowledgeGraph,
    prune_sources: &[PathBuf],
    root: Option<&Path>,
) -> anyhow::Result<Extraction> {
    merge_raw_extraction_from_graph_impl(
        new,
        existing,
        IncrementalBaselinePrunes {
            deletion_sources: prune_sources,
            ownership_reset_sources: &[],
        },
        None,
        &[],
        root,
        None,
    )
}

/// Merge against an already-loaded graph only when the conservative working
/// set of the merged raw extraction fits `max_materialized_bytes`.
///
/// Admission is checked before retained baseline facts are cloned. A caller
/// can therefore fail a low-budget incremental operation without first
/// materializing another whole-graph copy.
pub fn merge_raw_extraction_from_graph_with_materialization_limit(
    new: Extraction,
    existing: &KnowledgeGraph,
    prune_sources: &[PathBuf],
    root: Option<&Path>,
    max_materialized_bytes: usize,
) -> anyhow::Result<Extraction> {
    anyhow::ensure!(
        max_materialized_bytes > 0,
        "incremental graph materialization limit must be greater than zero"
    );
    merge_raw_extraction_from_graph_impl(
        new,
        existing,
        IncrementalBaselinePrunes {
            deletion_sources: prune_sources,
            ownership_reset_sources: &[],
        },
        None,
        &[],
        root,
        Some(max_materialized_bytes),
    )
}

/// Merge against an already-loaded graph using authoritative successful scan
/// ownership and a conservative materialization limit.
///
/// `rebuilt_sources` must contain only physical inputs whose extraction
/// completed successfully. `rebuilt_provider_sources` carries equally
/// authoritative non-filesystem owners produced by an explicitly requested
/// provider scan. Direct facts retain tier-scoped replacement, while facts
/// marked with `_container_source` follow the rebuilt outer input across both
/// tiers. Keeping this evidence separate from `prune_sources` preserves
/// fail-open behavior for unreadable inputs and deletion semantics for inputs
/// that are actually gone.
pub fn merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
    new: Extraction,
    existing: &KnowledgeGraph,
    rebuilt_sources: &[PathBuf],
    rebuilt_provider_sources: &[String],
    prune_sources: &[PathBuf],
    root: Option<&Path>,
    max_materialized_bytes: usize,
) -> anyhow::Result<Extraction> {
    anyhow::ensure!(
        max_materialized_bytes > 0,
        "incremental graph materialization limit must be greater than zero"
    );
    merge_raw_extraction_from_graph_impl(
        new,
        existing,
        IncrementalBaselinePrunes {
            deletion_sources: prune_sources,
            ownership_reset_sources: &[],
        },
        Some(rebuilt_sources),
        rebuilt_provider_sources,
        root,
        Some(max_materialized_bytes),
    )
}

/// Baseline-only removals for a bounded incremental merge.
#[derive(Debug, Clone, Copy, Default)]
pub struct IncrementalBaselinePrunes<'a> {
    /// Ordinary deletions, suppressed when the same source was successfully
    /// rebuilt in this merge.
    pub deletion_sources: &'a [PathBuf],
    /// Verified cross-tier ownership resets, never suppressed by rebuilding
    /// the same source and never applied to fresh facts.
    pub ownership_reset_sources: &'a [PathBuf],
}

/// Merge with both tier-scoped replacement and authoritative ownership resets.
///
/// Unlike ordinary deletion prunes, `ownership_reset_sources` are never
/// suppressed when the same source was rebuilt. They remove every carried
/// baseline fact owned by the source across structural and semantic tiers,
/// while fresh facts in `new` remain untouched. Callers must include a source
/// only after successfully verifying that its current generation authoritatively
/// supersedes all previously committed representations.
pub fn merge_raw_extraction_from_graph_with_rebuilt_sources_and_ownership_resets_and_materialization_limit(
    new: Extraction,
    existing: &KnowledgeGraph,
    rebuilt_sources: &[PathBuf],
    rebuilt_provider_sources: &[String],
    baseline_prunes: IncrementalBaselinePrunes<'_>,
    root: Option<&Path>,
    max_materialized_bytes: usize,
) -> anyhow::Result<Extraction> {
    anyhow::ensure!(
        max_materialized_bytes > 0,
        "incremental graph materialization limit must be greater than zero"
    );
    merge_raw_extraction_from_graph_impl(
        new,
        existing,
        baseline_prunes,
        Some(rebuilt_sources),
        rebuilt_provider_sources,
        root,
        Some(max_materialized_bytes),
    )
}

fn merge_raw_extraction_from_graph_impl(
    mut new: Extraction,
    existing: &KnowledgeGraph,
    baseline_prunes: IncrementalBaselinePrunes<'_>,
    rebuilt_sources: Option<&[PathBuf]>,
    rebuilt_provider_sources: &[String],
    effective_root: Option<&Path>,
    max_materialized_bytes: Option<usize>,
) -> anyhow::Result<Extraction> {
    let IncrementalBaselinePrunes {
        deletion_sources,
        ownership_reset_sources,
    } = baseline_prunes;
    let (mut new_ast_sources, mut new_semantic_sources) =
        tier_sources(std::slice::from_ref(&new), effective_root);
    if let Some(rebuilt_sources) = rebuilt_sources {
        new_ast_sources.retain(|identity| {
            identity_owned_by_rebuilt_source(identity, rebuilt_sources, effective_root)
                || identity_owned_by_rebuilt_provider(identity, rebuilt_provider_sources)
        });
        new_semantic_sources.retain(|identity| {
            identity_owned_by_rebuilt_source(identity, rebuilt_sources, effective_root)
                || identity_owned_by_rebuilt_provider(identity, rebuilt_provider_sources)
        });
    }
    let fresh_container_sources = rebuilt_sources.map_or_else(Vec::new, |sources| {
        container_root_sources(&new.nodes, sources, effective_root)
    });
    promote_container_representation_sources(
        &mut new_ast_sources,
        &mut new_semantic_sources,
        &fresh_container_sources,
        effective_root,
    );
    let new_container_sources = rebuilt_sources.map_or_else(
        || unowned_sources(&new_ast_sources, &new_semantic_sources, effective_root),
        |sources| rebuilt_source_forms(sources, effective_root),
    );
    let replaced_container_representation = rebuilt_sources
        .map_or_else(ContainerRepresentationFamily::default, |sources| {
            container_representation_family(existing, sources, effective_root)
        });
    let mut effective_prunes: Vec<&PathBuf> = deletion_sources
        .iter()
        .filter(|prune| {
            if let Some(rebuilt_sources) = rebuilt_sources {
                !rebuilt_sources.iter().any(|rebuilt| {
                    same_source(
                        &prune.to_string_lossy(),
                        &rebuilt.to_string_lossy(),
                        effective_root,
                    )
                })
            } else {
                !new_ast_sources
                    .iter()
                    .chain(&new_semantic_sources)
                    .any(|identity| {
                        identity.container_source.is_none()
                            && same_source(
                                &prune.to_string_lossy(),
                                &identity.source_file,
                                effective_root,
                            )
                    })
            }
        })
        .collect();
    effective_prunes.extend(ownership_reset_sources);
    effective_prunes.sort();
    effective_prunes.dedup();
    let retained_node = |node: &Node| {
        !is_replaced_node(
            node,
            &new_ast_sources,
            &new_semantic_sources,
            &new_container_sources,
            effective_root,
        ) && !replaced_container_representation
            .node_ids
            .contains(&node.id)
            && !effective_prunes.iter().any(|prune| {
                same_source_or_container_prune(
                    &node.source_file,
                    container_source(&node.extra),
                    &prune.to_string_lossy(),
                    effective_root,
                )
            })
    };
    let pruned_node_ids = existing
        .nodes
        .iter()
        .filter(|node| node_owned_by_prunes(node, &effective_prunes, effective_root))
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    let surviving_node_ids = existing
        .nodes
        .iter()
        .filter(|node| retained_node(node))
        .map(|node| node.id.as_str())
        .chain(new.nodes.iter().map(|node| node.id.as_str()))
        .collect::<BTreeSet<_>>();
    let removed_pruned_node_ids = pruned_node_ids
        .difference(&surviving_node_ids)
        .map(|id| (*id).to_owned())
        .collect::<BTreeSet<_>>();
    let retained_edge = |edge: &Edge| {
        !is_replaced_edge(
            edge,
            &new_ast_sources,
            &new_semantic_sources,
            &new_container_sources,
            effective_root,
        ) && !is_replaced_container_representation_edge(
            edge,
            &replaced_container_representation,
            effective_root,
        ) && !effective_prunes.iter().any(|prune| {
            same_source_or_container_prune(
                &edge.source_file,
                container_source(&edge.extra),
                &prune.to_string_lossy(),
                effective_root,
            )
        }) && !edge_references_removed_node(edge, &removed_pruned_node_ids)
    };
    let retained_hyperedge = |hyperedge: &serde_json::Value| {
        let source_file = hyperedge
            .get("source_file")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        !is_replaced_hyperedge(
            hyperedge,
            &new_ast_sources,
            &new_semantic_sources,
            &new_container_sources,
            effective_root,
        ) && !effective_prunes.iter().any(|prune| {
            same_source_or_container_prune(
                source_file,
                hyperedge
                    .get(CONTAINER_SOURCE_ATTRIBUTE)
                    .and_then(|value| value.as_str())
                    .filter(|source| !source.is_empty()),
                &prune.to_string_lossy(),
                effective_root,
            )
        }) && hyperedge_survives_removed_members(hyperedge, &removed_pruned_node_ids)
    };

    if let Some(max_materialized_bytes) = max_materialized_bytes {
        ensure_raw_merge_fits(
            existing,
            &new,
            &retained_node,
            &retained_edge,
            &retained_hyperedge,
            max_materialized_bytes,
        )?;
    }

    let mut merged = Extraction {
        nodes: existing
            .nodes
            .iter()
            .filter(|node| retained_node(node))
            .cloned()
            .collect(),
        edges: existing
            .links
            .iter()
            .filter(|edge| retained_edge(edge))
            .cloned()
            .collect(),
        hyperedges: existing
            .hyperedges
            .iter()
            .filter(|hyperedge| retained_hyperedge(hyperedge))
            .cloned()
            .map(|mut hyperedge| {
                prune_hyperedge_members(&mut hyperedge, &removed_pruned_node_ids);
                hyperedge
            })
            .collect(),
    };
    merged.nodes.append(&mut new.nodes);
    merged.edges.append(&mut new.edges);
    merged.hyperedges.append(&mut new.hyperedges);
    Ok(merged)
}

fn ensure_raw_merge_fits<N, E, H>(
    existing: &KnowledgeGraph,
    new: &Extraction,
    retained_node: &N,
    retained_edge: &E,
    retained_hyperedge: &H,
    max_materialized_bytes: usize,
) -> anyhow::Result<()>
where
    N: Fn(&Node) -> bool,
    E: Fn(&Edge) -> bool,
    H: Fn(&serde_json::Value) -> bool,
{
    // Count directly into a sink so preflight never allocates a second JSON
    // representation. The envelope and one delimiter per fact deliberately
    // overcount the compact extraction representation by a few bytes.
    let mut counter = CountingWriter::new(
        u64::try_from(br#"{"nodes":[],"edges":[],"hyperedges":[]}"#.len())
            .expect("extraction envelope length fits u64"),
    );
    for node in existing.nodes.iter().filter(|node| retained_node(node)) {
        counter.count_json(node)?;
    }
    for edge in existing.links.iter().filter(|edge| retained_edge(edge)) {
        counter.count_json(edge)?;
    }
    for hyperedge in existing
        .hyperedges
        .iter()
        .filter(|hyperedge| retained_hyperedge(hyperedge))
    {
        counter.count_json(hyperedge)?;
    }
    for node in &new.nodes {
        counter.count_json(node)?;
    }
    for edge in &new.edges {
        counter.count_json(edge)?;
    }
    for hyperedge in &new.hyperedges {
        counter.count_json(hyperedge)?;
    }
    let estimated_bytes = counter.bytes.saturating_mul(
        u64::try_from(INCREMENTAL_GRAPH_WORKING_SET_MULTIPLIER)
            .expect("working-set multiplier fits u64"),
    );
    let max_materialized_bytes = u64::try_from(max_materialized_bytes).unwrap_or(u64::MAX);
    anyhow::ensure!(
        estimated_bytes <= max_materialized_bytes,
        "incremental merged extraction requires an estimated {estimated_bytes}-byte working set, exceeds {max_materialized_bytes}-byte materialization limit"
    );
    Ok(())
}

struct CountingWriter {
    bytes: u64,
}

impl CountingWriter {
    const fn new(bytes: u64) -> Self {
        Self { bytes }
    }

    fn count_json(&mut self, value: &impl Serialize) -> anyhow::Result<()> {
        serde_json::to_writer(&mut *self, value).context("estimate incremental graph fact size")?;
        self.bytes = self
            .bytes
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("incremental graph fact size exceeds u64"))?;
        Ok(())
    }
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("incremental graph fact size exceeds u64"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn tier_sources(
    chunks: &[Extraction],
    root: Option<&Path>,
) -> (Vec<SourceIdentity>, Vec<SourceIdentity>) {
    let mut ast = Vec::new();
    let mut semantic = Vec::new();
    for node in chunks.iter().flat_map(|chunk| &chunk.nodes) {
        add_tiered_source(
            &mut ast,
            &mut semantic,
            &node.source_file,
            container_source(&node.extra),
            is_ast_node(node),
            root,
        );
    }
    for edge in chunks.iter().flat_map(|chunk| &chunk.edges) {
        add_tiered_source(
            &mut ast,
            &mut semantic,
            &edge.source_file,
            container_source(&edge.extra),
            is_ast_edge(edge),
            root,
        );
    }
    for hyperedge in chunks.iter().flat_map(|chunk| &chunk.hyperedges) {
        let source_file = hyperedge
            .get("source_file")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        add_tiered_source(
            &mut ast,
            &mut semantic,
            source_file,
            hyperedge
                .get(CONTAINER_SOURCE_ATTRIBUTE)
                .and_then(|value| value.as_str())
                .filter(|source| !source.is_empty()),
            is_ast_tier_value(hyperedge),
            root,
        );
    }
    (ast, semantic)
}

fn rebuilt_source_forms(rebuilt_sources: &[PathBuf], root: Option<&Path>) -> Vec<String> {
    let mut sources = Vec::new();
    for source in rebuilt_sources {
        add_source_forms(&mut sources, &source.to_string_lossy(), root);
    }
    sources
}

fn unowned_sources(
    ast: &[SourceIdentity],
    semantic: &[SourceIdentity],
    root: Option<&Path>,
) -> Vec<String> {
    let mut sources = Vec::new();
    for identity in ast.iter().chain(semantic) {
        if identity.container_source.is_some() {
            continue;
        }
        add_source_forms(&mut sources, &identity.source_file, root);
    }
    sources
}

fn identity_owned_by_rebuilt_source(
    identity: &SourceIdentity,
    rebuilt_sources: &[PathBuf],
    root: Option<&Path>,
) -> bool {
    let owner = identity
        .container_source
        .as_deref()
        .unwrap_or(&identity.source_file);
    rebuilt_sources
        .iter()
        .any(|rebuilt| same_source(owner, &rebuilt.to_string_lossy(), root))
}

fn identity_owned_by_rebuilt_provider(
    identity: &SourceIdentity,
    rebuilt_provider_sources: &[String],
) -> bool {
    let owner = identity
        .container_source
        .as_deref()
        .unwrap_or(&identity.source_file);
    rebuilt_provider_sources
        .iter()
        .any(|provider| owner == provider)
}

fn is_container_representation_root(node: &Node) -> bool {
    container_source(&node.extra).is_none()
        && matches!(
            node.extra.get("type").and_then(serde_json::Value::as_str),
            Some("container" | "format_inventory")
        )
}

fn is_container_representation_node(node: &Node) -> bool {
    container_source(&node.extra).is_none()
        && matches!(
            node.extra.get("type").and_then(serde_json::Value::as_str),
            Some("container" | "format_inventory" | "container_member")
        )
}

fn container_root_sources(
    nodes: &[Node],
    rebuilt_sources: &[PathBuf],
    root: Option<&Path>,
) -> Vec<String> {
    let mut sources = Vec::new();
    for node in nodes.iter().filter(|node| {
        is_container_representation_root(node)
            && rebuilt_sources
                .iter()
                .any(|rebuilt| same_source(&node.source_file, &rebuilt.to_string_lossy(), root))
    }) {
        add_source_forms(&mut sources, &node.source_file, root);
    }
    sources
}

fn promote_container_representation_sources(
    ast: &mut Vec<SourceIdentity>,
    semantic: &mut Vec<SourceIdentity>,
    container_sources: &[String],
    root: Option<&Path>,
) {
    let mut promoted = Vec::new();
    semantic.retain(|identity| {
        let is_direct_container_source = identity.container_source.is_none()
            && container_sources
                .iter()
                .any(|source| same_source(&identity.source_file, source, root));
        if is_direct_container_source && !promoted.contains(identity) {
            promoted.push(identity.clone());
        }
        !is_direct_container_source
    });
    for identity in promoted {
        if !ast.contains(&identity) {
            ast.push(identity);
        }
    }
}

#[derive(Debug, Default)]
struct ContainerRepresentationFamily {
    sources: Vec<String>,
    node_ids: BTreeSet<String>,
}

fn container_representation_family(
    existing: &KnowledgeGraph,
    rebuilt_sources: &[PathBuf],
    root: Option<&Path>,
) -> ContainerRepresentationFamily {
    let container_sources = container_root_sources(&existing.nodes, rebuilt_sources, root);
    let node_ids = existing
        .nodes
        .iter()
        .filter(|node| {
            is_container_representation_node(node)
                && container_sources
                    .iter()
                    .any(|source| same_source(&node.source_file, source, root))
        })
        .map(|node| node.id.clone())
        .collect();
    ContainerRepresentationFamily {
        sources: container_sources,
        node_ids,
    }
}

fn is_replaced_container_representation_edge(
    edge: &Edge,
    representation: &ContainerRepresentationFamily,
    root: Option<&Path>,
) -> bool {
    container_source(&edge.extra).is_none()
        && edge
            .extra
            .get("_origin")
            .and_then(serde_json::Value::as_str)
            != Some("semantic")
        && edge.relation == "contains"
        && representation
            .sources
            .iter()
            .any(|source| same_source(&edge.source_file, source, root))
        && (representation.node_ids.contains(&edge.source)
            || representation.node_ids.contains(&edge.target))
}

fn add_tiered_source(
    ast: &mut Vec<SourceIdentity>,
    semantic: &mut Vec<SourceIdentity>,
    source: &str,
    container_owner: Option<&str>,
    is_ast: bool,
    root: Option<&Path>,
) {
    if source.is_empty() {
        return;
    }
    let target = if is_ast { ast } else { semantic };
    let identity = SourceIdentity {
        source_file: source.to_owned(),
        container_source: container_owner.map(str::to_owned),
    };
    if !target.contains(&identity) {
        target.push(identity);
    }
    let normalized = SourceIdentity {
        source_file: normalized_source(source, root),
        container_source: container_owner.map(|owner| normalized_source(owner, root)),
    };
    if !normalized.source_file.is_empty() && !target.contains(&normalized) {
        target.push(normalized);
    }
}

fn add_source_forms(target: &mut Vec<String>, source: &str, root: Option<&Path>) {
    for value in [source.to_owned(), normalized_source(source, root)] {
        if !value.is_empty() && !target.contains(&value) {
            target.push(value);
        }
    }
}

fn is_replaced_node(
    node: &Node,
    ast: &[SourceIdentity],
    semantic: &[SourceIdentity],
    containers: &[String],
    root: Option<&Path>,
) -> bool {
    source_replaced(
        &node.source_file,
        container_source(&node.extra),
        is_ast_node(node),
        ast,
        semantic,
        containers,
        root,
    )
}

fn is_replaced_edge(
    edge: &Edge,
    ast: &[SourceIdentity],
    semantic: &[SourceIdentity],
    containers: &[String],
    root: Option<&Path>,
) -> bool {
    source_replaced(
        &edge.source_file,
        container_source(&edge.extra),
        is_ast_edge(edge),
        ast,
        semantic,
        containers,
        root,
    )
}

fn is_replaced_hyperedge(
    hyperedge: &serde_json::Value,
    ast: &[SourceIdentity],
    semantic: &[SourceIdentity],
    containers: &[String],
    root: Option<&Path>,
) -> bool {
    let source_file = hyperedge
        .get("source_file")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    source_replaced(
        source_file,
        hyperedge
            .get(CONTAINER_SOURCE_ATTRIBUTE)
            .and_then(|value| value.as_str())
            .filter(|source| !source.is_empty()),
        is_ast_tier_value(hyperedge),
        ast,
        semantic,
        containers,
        root,
    )
}

fn source_replaced(
    source: &str,
    container_owner: Option<&str>,
    is_ast: bool,
    ast: &[SourceIdentity],
    semantic: &[SourceIdentity],
    containers: &[String],
    root: Option<&Path>,
) -> bool {
    let tier = if is_ast { ast } else { semantic };
    if source_in(source, container_owner, tier, root) {
        return true;
    }
    // A container is one scanned input even though recursively dispatched
    // members may contain facts from both provenance tiers. Any newly scanned
    // unowned fact identifies its physical input, even when malformed bytes
    // now produce only a rejected inventory root. Re-extracting that outer
    // input must replace every explicitly owned old member fact so a removed
    // or renamed member cannot survive.
    container_owner.is_some_and(|owner| {
        containers
            .iter()
            .any(|container| same_source(owner, container, root))
    })
}

fn source_in(
    source: &str,
    container_owner: Option<&str>,
    candidates: &[SourceIdentity],
    root: Option<&Path>,
) -> bool {
    !source.is_empty()
        && candidates.iter().any(|candidate| {
            same_source(source, &candidate.source_file, root)
                && match (container_owner, candidate.container_source.as_deref()) {
                    (None, None) => true,
                    (Some(left), Some(right)) => same_source(left, right, root),
                    _ => false,
                }
        })
}

fn is_ast_node(node: &Node) -> bool {
    if let Some(is_ast) = node
        .extra
        .get("_origin")
        .and_then(|value| value.as_str())
        .and_then(origin_is_structural)
    {
        return is_ast;
    }
    node.source_location
        .as_deref()
        .is_some_and(ast_source_location)
}

fn is_ast_edge(edge: &Edge) -> bool {
    if let Some(is_ast) = edge
        .extra
        .get("_origin")
        .and_then(|value| value.as_str())
        .and_then(origin_is_structural)
    {
        return is_ast;
    }
    edge.extra
        .get("source_location")
        .and_then(|value| value.as_str())
        .is_some_and(ast_source_location)
}

fn ast_source_location(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next() == Some('L') && chars.next().is_some_and(|value| value.is_ascii_digit())
}

/// Classify a loose node/edge/hyperedge record by the same provenance rule used
/// for tier-scoped incremental replacement.
pub fn is_ast_tier_value(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if let Some(is_ast) = object
        .get("_origin")
        .and_then(|value| value.as_str())
        .and_then(origin_is_structural)
    {
        return is_ast;
    }
    object
        .get("source_location")
        .and_then(|value| value.as_str())
        .is_some_and(ast_source_location)
}

fn same_source(left: &str, right: &str, root: Option<&Path>) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let left_slash = left.replace('\\', "/").trim_start_matches("./").to_owned();
    let right_slash = right.replace('\\', "/").trim_start_matches("./").to_owned();
    if left_slash == right_slash {
        return true;
    }
    let Some(root) = root else {
        return false;
    };
    match (
        absolute_identity(Path::new(left), root),
        absolute_identity(Path::new(right), root),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn container_source(extra: &std::collections::BTreeMap<String, serde_json::Value>) -> Option<&str> {
    extra
        .get(CONTAINER_SOURCE_ATTRIBUTE)
        .and_then(|value| value.as_str())
        .filter(|source| !source.is_empty())
}

fn node_owned_by_prunes(node: &Node, prunes: &[&PathBuf], root: Option<&Path>) -> bool {
    prunes.iter().any(|prune| {
        same_source_or_container_prune(
            &node.source_file,
            container_source(&node.extra),
            &prune.to_string_lossy(),
            root,
        )
    })
}

fn edge_owned_by_prunes(edge: &Edge, prunes: &[&PathBuf], root: Option<&Path>) -> bool {
    prunes.iter().any(|prune| {
        same_source_or_container_prune(
            &edge.source_file,
            container_source(&edge.extra),
            &prune.to_string_lossy(),
            root,
        )
    })
}

fn hyperedge_owned_by_prunes(
    hyperedge: &serde_json::Value,
    prunes: &[&PathBuf],
    root: Option<&Path>,
) -> bool {
    let source_file = hyperedge
        .get("source_file")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    prunes.iter().any(|prune| {
        same_source_or_container_prune(
            source_file,
            hyperedge
                .get(CONTAINER_SOURCE_ATTRIBUTE)
                .and_then(serde_json::Value::as_str)
                .filter(|source| !source.is_empty()),
            &prune.to_string_lossy(),
            root,
        )
    })
}

fn edge_references_removed_node(edge: &Edge, removed_node_ids: &BTreeSet<String>) -> bool {
    removed_node_ids.contains(edge.true_source()) || removed_node_ids.contains(edge.true_target())
}

fn hyperedge_members(hyperedge: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    ["nodes", "members", "node_ids"]
        .into_iter()
        .find_map(|field| hyperedge.get(field).and_then(serde_json::Value::as_array))
}

fn hyperedge_survives_removed_members(
    hyperedge: &serde_json::Value,
    removed_node_ids: &BTreeSet<String>,
) -> bool {
    let Some(members) = hyperedge_members(hyperedge) else {
        return true;
    };
    let removed = members.iter().any(|member| {
        member
            .as_str()
            .is_some_and(|member| removed_node_ids.contains(member))
    });
    if !removed {
        return true;
    }
    !members
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|member| !removed_node_ids.contains(*member))
        .collect::<BTreeSet<_>>()
        .is_empty()
}

fn prune_hyperedge_members(hyperedge: &mut serde_json::Value, removed_node_ids: &BTreeSet<String>) {
    let Some(object) = hyperedge.as_object_mut() else {
        return;
    };
    for field in ["nodes", "members", "node_ids"] {
        if let Some(members) = object
            .get_mut(field)
            .and_then(serde_json::Value::as_array_mut)
        {
            members.retain(|member| {
                !member
                    .as_str()
                    .is_some_and(|member| removed_node_ids.contains(member))
            });
        }
    }
}

fn same_source_or_container_prune(
    source: &str,
    container_owner: Option<&str>,
    prune: &str,
    root: Option<&Path>,
) -> bool {
    match container_owner {
        Some(owner) => same_source(owner, prune, root),
        None => same_source(source, prune, root),
    }
}

fn normalized_source(value: &str, root: Option<&Path>) -> String {
    let slash = value.replace('\\', "/");
    let Some(root) = root else {
        return slash.trim_start_matches("./").to_owned();
    };
    let root_slash = root.to_string_lossy().replace('\\', "/");
    if let Some(relative) = slash.strip_prefix(&format!("{}/", root_slash.trim_end_matches('/'))) {
        relative.to_owned()
    } else {
        slash.trim_start_matches("./").to_owned()
    }
}

fn absolute_identity(path: &Path, root: &Path) -> Option<PathBuf> {
    let slash = path.to_string_lossy().replace('\\', "/");
    let normalized = Path::new(&slash);
    let absolute = if normalized.is_absolute() {
        normalized.to_path_buf()
    } else {
        root.join(normalized)
    };
    canonicalize_with_missing_tail(&absolute).ok()
}

fn canonicalize_with_missing_tail(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut tail = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|name| name.to_owned()) else {
            return Ok(path.to_path_buf());
        };
        tail.push(name);
        if !existing.pop() {
            return Ok(path.to_path_buf());
        }
    }
    let mut canonical = existing.canonicalize()?;
    for component in tail.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}
