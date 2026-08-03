//! Incremental graph merge with source replacement and deletion pruning.

use crate::provenance::origin_is_structural;
use crate::{build_graph_with_options, build_graph_with_options_and_root, BuildOptions};
use anyhow::Context;
use graphoxide_core::{read_graph, read_graph_with_cap, Edge, Extraction, KnowledgeGraph, Node};
use std::{
    fs,
    path::{Path, PathBuf},
};

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

/// Best-effort scan root for a graph when an incremental caller omitted it.
pub fn infer_merge_root(graph_path: impl AsRef<Path>) -> Option<PathBuf> {
    let graph_path = graph_path.as_ref();
    for marker_name in [".graphoxide_root", ".graphify_root"] {
        let marker = graph_path.parent()?.join(marker_name);
        if let Ok(recorded) = fs::read_to_string(marker) {
            let recorded = recorded.trim();
            if !recorded.is_empty() {
                if let Ok(root) = canonicalize_with_missing_tail(Path::new(recorded)) {
                    return Some(root);
                }
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
    let new_sources: Vec<String> = new_ast_sources
        .iter()
        .chain(&new_semantic_sources)
        .cloned()
        .collect();

    let mut chunks = Vec::new();
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
                effective_root.as_deref(),
            )
        });
        carried.edges.retain(|edge| {
            !is_replaced_edge(
                edge,
                &new_ast_sources,
                &new_semantic_sources,
                effective_root.as_deref(),
            )
        });
        carried.hyperedges.retain(|hyperedge| {
            !is_replaced_hyperedge(
                hyperedge,
                &new_ast_sources,
                &new_semantic_sources,
                effective_root.as_deref(),
            )
        });
        chunks.push(carried);
    }
    chunks.extend_from_slice(new_chunks);

    // Replacement wins over a contradictory deletion for the same source.
    let effective_prunes: Vec<&PathBuf> = prune_sources
        .iter()
        .filter(|prune| {
            !new_sources.iter().any(|source| {
                same_source(&prune.to_string_lossy(), source, effective_root.as_deref())
            })
        })
        .collect();
    for chunk in &mut chunks {
        chunk.nodes.retain(|node| {
            !effective_prunes.iter().any(|prune| {
                same_source(
                    &node.source_file,
                    &prune.to_string_lossy(),
                    effective_root.as_deref(),
                )
            })
        });
        chunk.edges.retain(|edge| {
            !effective_prunes.iter().any(|prune| {
                same_source(
                    &edge.source_file,
                    &prune.to_string_lossy(),
                    effective_root.as_deref(),
                )
            })
        });
        chunk.hyperedges.retain(|hyperedge| {
            let source_file = hyperedge
                .get("source_file")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            source_file.is_empty()
                || !effective_prunes.iter().any(|prune| {
                    same_source(
                        source_file,
                        &prune.to_string_lossy(),
                        effective_root.as_deref(),
                    )
                })
        });
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
    let (new_ast_sources, new_semantic_sources) =
        tier_sources(std::slice::from_ref(new), effective_root.as_deref());
    let new_sources: Vec<String> = new_ast_sources
        .iter()
        .chain(&new_semantic_sources)
        .cloned()
        .collect();
    let effective_prunes: Vec<&PathBuf> = prune_sources
        .iter()
        .filter(|prune| {
            !new_sources.iter().any(|source| {
                same_source(&prune.to_string_lossy(), source, effective_root.as_deref())
            })
        })
        .collect();
    let pruned = |source_file: &str| {
        effective_prunes.iter().any(|prune| {
            same_source(
                source_file,
                &prune.to_string_lossy(),
                effective_root.as_deref(),
            )
        })
    };
    let mut merged = Extraction {
        nodes: existing
            .nodes
            .into_iter()
            .filter(|node| {
                !is_replaced_node(
                    node,
                    &new_ast_sources,
                    &new_semantic_sources,
                    effective_root.as_deref(),
                ) && !pruned(&node.source_file)
            })
            .collect(),
        edges: existing
            .links
            .into_iter()
            .filter(|edge| {
                !is_replaced_edge(
                    edge,
                    &new_ast_sources,
                    &new_semantic_sources,
                    effective_root.as_deref(),
                ) && !pruned(&edge.source_file)
            })
            .collect(),
        hyperedges: existing
            .hyperedges
            .into_iter()
            .filter(|hyperedge| {
                let source_file = hyperedge
                    .get("source_file")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                !is_replaced_hyperedge(
                    hyperedge,
                    &new_ast_sources,
                    &new_semantic_sources,
                    effective_root.as_deref(),
                ) && !pruned(source_file)
            })
            .collect(),
    };
    merged.nodes.extend(new.nodes.clone());
    merged.edges.extend(new.edges.clone());
    merged.hyperedges.extend(new.hyperedges.clone());
    Ok(merged)
}

fn tier_sources(chunks: &[Extraction], root: Option<&Path>) -> (Vec<String>, Vec<String>) {
    let mut ast = Vec::new();
    let mut semantic = Vec::new();
    for node in chunks.iter().flat_map(|chunk| &chunk.nodes) {
        add_tiered_source(
            &mut ast,
            &mut semantic,
            &node.source_file,
            is_ast_node(node),
            root,
        );
    }
    for edge in chunks.iter().flat_map(|chunk| &chunk.edges) {
        add_tiered_source(
            &mut ast,
            &mut semantic,
            &edge.source_file,
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
            is_ast_tier_value(hyperedge),
            root,
        );
    }
    (ast, semantic)
}

fn add_tiered_source(
    ast: &mut Vec<String>,
    semantic: &mut Vec<String>,
    source: &str,
    is_ast: bool,
    root: Option<&Path>,
) {
    if source.is_empty() {
        return;
    }
    add_source_forms(if is_ast { ast } else { semantic }, source, root);
}

fn add_source_forms(target: &mut Vec<String>, source: &str, root: Option<&Path>) {
    for value in [source.to_owned(), normalized_source(source, root)] {
        if !value.is_empty() && !target.contains(&value) {
            target.push(value);
        }
    }
}

fn is_replaced_node(node: &Node, ast: &[String], semantic: &[String], root: Option<&Path>) -> bool {
    source_in(
        &node.source_file,
        if is_ast_node(node) { ast } else { semantic },
        root,
    )
}

fn is_replaced_edge(edge: &Edge, ast: &[String], semantic: &[String], root: Option<&Path>) -> bool {
    source_in(
        &edge.source_file,
        if is_ast_edge(edge) { ast } else { semantic },
        root,
    )
}

fn is_replaced_hyperedge(
    hyperedge: &serde_json::Value,
    ast: &[String],
    semantic: &[String],
    root: Option<&Path>,
) -> bool {
    let source_file = hyperedge
        .get("source_file")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    source_in(
        source_file,
        if is_ast_tier_value(hyperedge) {
            ast
        } else {
            semantic
        },
        root,
    )
}

fn source_in(source: &str, candidates: &[String], root: Option<&Path>) -> bool {
    !source.is_empty()
        && candidates
            .iter()
            .any(|candidate| same_source(source, candidate, root))
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
