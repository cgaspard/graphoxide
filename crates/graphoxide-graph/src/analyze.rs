//! Deterministic architectural analysis.

use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GodNode {
    pub id: String,
    pub label: String,
    pub degree: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Surprise {
    pub source: String,
    pub target: String,
    pub source_files: [String; 2],
    pub confidence: Confidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence_score: Option<f64>,
    pub relation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Analysis {
    pub god_nodes: Vec<GodNode>,
    pub surprising_connections: Vec<Surprise>,
    pub suggested_questions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffNode {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GraphDiff {
    pub new_nodes: Vec<DiffNode>,
    pub removed_nodes: Vec<DiffNode>,
    pub new_edges: Vec<DiffEdge>,
    pub removed_edges: Vec<DiffEdge>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportCycle {
    pub cycle: Vec<String>,
    pub length: usize,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SuggestedQuestion {
    #[serde(rename = "type")]
    pub kind: String,
    pub question: Option<String>,
    pub why: String,
}

pub fn analyze(graph: &KnowledgeGraph) -> anyhow::Result<Analysis> {
    let communities = communities_from_nodes(graph);
    let questions = suggest_questions(graph, &communities, &BTreeMap::new(), 10)
        .into_iter()
        .filter_map(|question| question.question)
        .collect();
    Ok(Analysis {
        god_nodes: god_nodes(graph, 10),
        surprising_connections: surprising_connections(graph, &communities, 5),
        suggested_questions: questions,
    })
}

fn node_positions(graph: &KnowledgeGraph) -> HashMap<&str, usize> {
    graph
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect()
}

fn degrees(graph: &KnowledgeGraph) -> BTreeMap<String, usize> {
    let mut degree: BTreeMap<_, _> = graph
        .nodes
        .iter()
        .map(|node| (node.id.clone(), 0))
        .collect();
    for edge in &graph.links {
        let source = edge.true_source();
        let target = edge.true_target();
        if degree.contains_key(source) && degree.contains_key(target) {
            *degree.get_mut(source).expect("source was checked") += 1;
            if source != target {
                *degree.get_mut(target).expect("target was checked") += 1;
            }
        }
    }
    degree
}

fn is_file_node(node: &Node, degree: usize) -> bool {
    if node.label.is_empty() {
        return false;
    }
    if !node.source_file.is_empty() {
        let source = node.source_file.replace('\\', "/");
        let basename = source.rsplit('/').next().unwrap_or(&source);
        if node.label == basename
            || (node.label.contains('/')
                && (source == node.label || source.ends_with(&format!("/{}", node.label))))
        {
            return true;
        }
    }
    (node.label.starts_with('.') && node.label.ends_with("()"))
        || (node.label.ends_with("()") && degree <= 1)
}

pub fn is_concept_node(node: &Node) -> bool {
    node.source_file.is_empty()
        || !node
            .source_file
            .replace('\\', "/")
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .contains('.')
}

pub fn is_json_key_node(node: &Node) -> bool {
    const NOISE: &[&str] = &[
        "start",
        "end",
        "name",
        "id",
        "type",
        "properties",
        "value",
        "key",
        "data",
        "items",
        "title",
        "description",
        "version",
        "dependencies",
        "devdependencies",
        "peerdependencies",
        "optionaldependencies",
        "bundleddependencies",
        "bundledependencies",
    ];
    node.source_file.to_lowercase().ends_with(".json")
        && NOISE.contains(&node.label.trim().to_lowercase().as_str())
}

pub fn god_nodes(graph: &KnowledgeGraph, top_n: usize) -> Vec<GodNode> {
    const BUILTIN_NOISE: &[&str] = &[
        "str",
        "int",
        "float",
        "bool",
        "bytes",
        "bytearray",
        "complex",
        "object",
        "True",
        "False",
        "MagicMock",
        "Mock",
        "AsyncMock",
        "NonCallableMock",
        "NonCallableMagicMock",
        "PropertyMock",
        "patch",
        "sentinel",
        "Path",
        "Any",
        "Optional",
        "List",
        "Dict",
        "Set",
        "Tuple",
        "Union",
        "Callable",
        "Type",
        "ClassVar",
        "Final",
        "Literal",
        "Protocol",
        "Counter",
        "defaultdict",
        "OrderedDict",
        "datetime",
        "Enum",
        "os",
        "sys",
        "re",
        "json",
        "io",
        "abc",
        "typing",
        "Foundation",
        "SwiftUI",
        "UIKit",
        "AppKit",
        "Combine",
        "String",
        "Int",
        "Double",
        "Float",
        "Bool",
        "Data",
        "URL",
        "Date",
        "UUID",
        "Sendable",
        "Codable",
        "Decodable",
        "Encodable",
        "Equatable",
        "Hashable",
        "Identifiable",
        "Comparable",
        "AnyObject",
        "Error",
        "LocalizedError",
        "NSObject",
        "NSString",
        "NSError",
        "NSLock",
        "View",
        "Color",
        "Font",
        "DispatchQueue",
    ];
    let degree = degrees(graph);
    let mut ranked: Vec<_> = graph
        .nodes
        .iter()
        .filter_map(|node| {
            let node_degree = degree.get(&node.id).copied().unwrap_or_default();
            (!is_file_node(node, node_degree)
                && !is_concept_node(node)
                && !is_json_key_node(node)
                && !BUILTIN_NOISE.contains(&node.label.as_str()))
            .then(|| GodNode {
                id: node.id.clone(),
                label: if node.label.is_empty() {
                    node.id.clone()
                } else {
                    node.label.clone()
                },
                degree: node_degree,
            })
        })
        .collect();
    // Python's degree mapping preserves node insertion order, and `sorted` is
    // stable. Keep that same tie behavior instead of introducing an ID sort.
    ranked.sort_by_key(|node| Reverse(node.degree));
    ranked.truncate(top_n);
    ranked
}

pub fn file_category(path: &str) -> &'static str {
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "py" | "pyw" | "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" | "ejs"
        | "ets" | "go" | "rs" | "java" | "groovy" | "gradle" | "cpp" | "cc" | "cxx" | "c" | "h"
        | "hpp" | "cu" | "cuh" | "metal" | "rb" | "rake" | "swift" | "kt" | "kts" | "cs"
        | "scala" | "php" | "lua" | "luau" | "toc" | "zig" | "ps1" | "psm1" | "psd1" | "ex"
        | "exs" | "m" | "mm" | "jl" | "vue" | "svelte" | "astro" | "dart" | "v" | "sv" | "svh"
        | "sql" | "r" | "f" | "f90" | "f95" | "f03" | "f08" | "pas" | "pp" | "dpr" | "dpk"
        | "lpr" | "inc" | "dfm" | "lfm" | "lpk" | "sh" | "bash" | "json" | "tf" | "tfvars"
        | "hcl" | "dm" | "dme" | "dmi" | "dmm" | "dmf" | "sln" | "slnx" | "csproj" | "fsproj"
        | "vbproj" | "xaml" | "razor" | "cshtml" | "cls" | "trigger" => "code",
        "pdf" => "paper",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => "image",
        _ => "doc",
    }
}

fn language_family(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "py" | "pyw" => "python",
        "js" | "jsx" | "mjs" | "cjs" | "ejs" | "ts" | "tsx" | "mts" | "cts" | "vue" | "svelte" => {
            "js"
        }
        "go" => "go",
        "rs" => "rust",
        "java" | "kt" | "kts" | "scala" => "jvm",
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" => "c",
        "rb" | "rake" => "ruby",
        "swift" => "swift",
        "cs" => "dotnet",
        "php" => "php",
        "r" => "r",
        _ => return None,
    })
}

fn cross_language(left: &str, right: &str) -> bool {
    matches!(
        (language_family(left), language_family(right)),
        (Some(left), Some(right)) if left != right
    )
}

fn top_level_dir(path: &str) -> &str {
    path.split('/').next().unwrap_or(path)
}

#[allow(clippy::too_many_arguments)]
pub fn surprise_score(
    graph: &KnowledgeGraph,
    source_id: &str,
    target_id: &str,
    edge: &Edge,
    node_community: &BTreeMap<String, i64>,
    source_file: &str,
    target_file: &str,
    precomputed_degrees: Option<&BTreeMap<String, usize>>,
) -> (i64, Vec<String>) {
    let positions = node_positions(graph);
    let category_source = file_category(source_file);
    let category_target = file_category(target_file);
    let suppress_structural = edge.confidence == Confidence::Inferred
        && matches!(edge.relation.as_str(), "calls" | "uses")
        && (cross_language(source_file, target_file)
            || BTreeSet::from([category_source, category_target])
                == BTreeSet::from(["code", "doc"]));
    let mut score = if suppress_structural {
        0
    } else {
        match edge.confidence {
            Confidence::Ambiguous => 3,
            Confidence::Inferred => 2,
            Confidence::Extracted => 1,
        }
    };
    let mut reasons = Vec::new();
    if edge.confidence != Confidence::Extracted {
        reasons.push(format!(
            "{} connection - not explicitly stated in source",
            match edge.confidence {
                Confidence::Ambiguous => "ambiguous",
                Confidence::Inferred => "inferred",
                Confidence::Extracted => unreachable!(),
            }
        ));
    }
    if category_source != category_target && !suppress_structural {
        score += 2;
        reasons.push(format!(
            "crosses file types ({category_source} ↔ {category_target})"
        ));
    }
    if top_level_dir(source_file) != top_level_dir(target_file) && !suppress_structural {
        score += 2;
        reasons.push("connects across different repos/directories".into());
    }
    let source_community = node_community.get(source_id);
    let target_community = node_community.get(target_id);
    if source_community.is_some()
        && target_community.is_some()
        && source_community != target_community
        && !suppress_structural
    {
        score += 1;
        reasons.push("bridges separate communities".into());
    }
    if edge.relation == "semantically_similar_to" {
        score = (score as f64 * 1.5) as i64;
        reasons.push("semantically similar concepts with no structural link".into());
    }
    let owned_degrees;
    let degree = if let Some(degree) = precomputed_degrees {
        degree
    } else {
        owned_degrees = degrees(graph);
        &owned_degrees
    };
    let source_degree = degree.get(source_id).copied().unwrap_or_default();
    let target_degree = degree.get(target_id).copied().unwrap_or_default();
    if source_degree.min(target_degree) <= 2 && source_degree.max(target_degree) >= 5 {
        score += 1;
        let source = positions.get(source_id).map(|index| &graph.nodes[*index]);
        let target = positions.get(target_id).map(|index| &graph.nodes[*index]);
        let peripheral = if source_degree <= 2 { source } else { target };
        let hub = if source_degree <= 2 { target } else { source };
        reasons.push(format!(
            "peripheral node `{}` unexpectedly reaches hub `{}`",
            peripheral.map_or(source_id, |node| node.label.as_str()),
            hub.map_or(target_id, |node| node.label.as_str())
        ));
    }
    (score, reasons)
}

fn communities_from_nodes(graph: &KnowledgeGraph) -> BTreeMap<i64, Vec<String>> {
    let mut communities = BTreeMap::<i64, Vec<String>>::new();
    for node in &graph.nodes {
        if let Some(community) = node.community {
            communities
                .entry(community)
                .or_default()
                .push(node.id.clone());
        }
    }
    communities
}

fn node_community_map(communities: &BTreeMap<i64, Vec<String>>) -> BTreeMap<String, i64> {
    communities
        .iter()
        .flat_map(|(community, nodes)| {
            nodes
                .iter()
                .map(|node| (node.clone(), *community))
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn surprising_connections(
    graph: &KnowledgeGraph,
    communities: &BTreeMap<i64, Vec<String>>,
    top_n: usize,
) -> Vec<Surprise> {
    let sources: BTreeSet<_> = graph
        .nodes
        .iter()
        .map(|node| node.source_file.as_str())
        .filter(|source| !source.is_empty())
        .collect();
    if sources.len() > 1 {
        let cross_file = cross_file_surprises(graph, communities, top_n);
        if !cross_file.is_empty() {
            return cross_file;
        }
    }
    cross_community_surprises(graph, communities, top_n)
}

fn cross_file_surprises(
    graph: &KnowledgeGraph,
    communities: &BTreeMap<i64, Vec<String>>,
    top_n: usize,
) -> Vec<Surprise> {
    let positions = node_positions(graph);
    let node_community = node_community_map(communities);
    let degree = degrees(graph);
    let mut candidates = Vec::new();
    for edge in &graph.links {
        if matches!(
            edge.relation.as_str(),
            "imports" | "imports_from" | "contains" | "method"
        ) {
            continue;
        }
        let (Some(&source_index), Some(&target_index)) = (
            positions.get(edge.true_source()),
            positions.get(edge.true_target()),
        ) else {
            continue;
        };
        let endpoint_a = &graph.nodes[source_index];
        let endpoint_b = &graph.nodes[target_index];
        if is_concept_node(endpoint_a)
            || is_concept_node(endpoint_b)
            || is_file_node(endpoint_a, degree[&endpoint_a.id])
            || is_file_node(endpoint_b, degree[&endpoint_b.id])
            || endpoint_a.source_file == endpoint_b.source_file
        {
            continue;
        }
        let (score, reasons) = surprise_score(
            graph,
            &endpoint_a.id,
            &endpoint_b.id,
            edge,
            &node_community,
            &endpoint_a.source_file,
            &endpoint_b.source_file,
            Some(&degree),
        );
        candidates.push((
            score,
            Surprise {
                source: endpoint_a.label.clone(),
                target: endpoint_b.label.clone(),
                source_files: [
                    endpoint_a.source_file.clone(),
                    endpoint_b.source_file.clone(),
                ],
                confidence: edge.confidence,
                confidence_score: edge
                    .extra
                    .get("confidence_score")
                    .and_then(serde_json::Value::as_f64),
                relation: edge.relation.clone(),
                why: Some(if reasons.is_empty() {
                    "cross-file semantic connection".into()
                } else {
                    reasons.join("; ")
                }),
                note: None,
            },
        ));
    }
    candidates.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.source.cmp(&right.1.source))
    });
    candidates
        .into_iter()
        .take(top_n)
        .map(|(_, surprise)| surprise)
        .collect()
}

fn cross_community_surprises(
    graph: &KnowledgeGraph,
    communities: &BTreeMap<i64, Vec<String>>,
    top_n: usize,
) -> Vec<Surprise> {
    let positions = node_positions(graph);
    let degree = degrees(graph);
    let node_community = node_community_map(communities);
    if communities.is_empty() {
        return graph
            .links
            .iter()
            .filter_map(|edge| {
                let source = &graph.nodes[*positions.get(edge.true_source())?];
                let target = &graph.nodes[*positions.get(edge.true_target())?];
                Some(Surprise {
                    source: source.label.clone(),
                    target: target.label.clone(),
                    source_files: [source.source_file.clone(), target.source_file.clone()],
                    confidence: edge.confidence,
                    confidence_score: None,
                    relation: edge.relation.clone(),
                    why: None,
                    note: Some("Bridges graph structure".into()),
                })
            })
            .take(top_n)
            .collect();
    }
    let mut candidates = Vec::new();
    for edge in &graph.links {
        let (Some(&source_index), Some(&target_index)) = (
            positions.get(edge.true_source()),
            positions.get(edge.true_target()),
        ) else {
            continue;
        };
        let source = &graph.nodes[source_index];
        let target = &graph.nodes[target_index];
        let (Some(source_community), Some(target_community)) = (
            node_community.get(&source.id),
            node_community.get(&target.id),
        ) else {
            continue;
        };
        if source_community == target_community
            || is_file_node(source, degree[&source.id])
            || is_file_node(target, degree[&target.id])
            || matches!(
                edge.relation.as_str(),
                "imports" | "imports_from" | "contains" | "method"
            )
        {
            continue;
        }
        let pair = if source_community <= target_community {
            (*source_community, *target_community)
        } else {
            (*target_community, *source_community)
        };
        candidates.push((
            confidence_order(edge.confidence),
            pair,
            Surprise {
                source: source.label.clone(),
                target: target.label.clone(),
                source_files: [source.source_file.clone(), target.source_file.clone()],
                confidence: edge.confidence,
                confidence_score: None,
                relation: edge.relation.clone(),
                why: None,
                note: Some(format!(
                    "Bridges community {source_community} → community {target_community}"
                )),
            },
        ));
    }
    candidates.sort_by_key(|(order, _, _)| *order);
    let mut seen_pairs = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|(_, pair, _)| seen_pairs.insert(*pair))
        .take(top_n)
        .map(|(_, _, surprise)| surprise)
        .collect()
}

fn confidence_order(confidence: Confidence) -> usize {
    match confidence {
        Confidence::Ambiguous => 0,
        Confidence::Inferred => 1,
        Confidence::Extracted => 2,
    }
}

pub fn graph_diff(old: &KnowledgeGraph, new: &KnowledgeGraph) -> GraphDiff {
    let old_nodes: BTreeMap<_, _> = old
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let new_nodes: BTreeMap<_, _> = new
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let added_ids: BTreeSet<_> = new_nodes
        .keys()
        .filter(|id| !old_nodes.contains_key(**id))
        .copied()
        .collect();
    let removed_ids: BTreeSet<_> = old_nodes
        .keys()
        .filter(|id| !new_nodes.contains_key(**id))
        .copied()
        .collect();
    let new_nodes_list: Vec<DiffNode> = added_ids
        .into_iter()
        .map(|id| DiffNode {
            id: id.into(),
            label: new_nodes[id].label.clone(),
        })
        .collect();
    let removed_nodes_list: Vec<DiffNode> = removed_ids
        .into_iter()
        .map(|id| DiffNode {
            id: id.into(),
            label: old_nodes[id].label.clone(),
        })
        .collect();
    let old_keys: BTreeSet<_> = old.links.iter().map(|edge| edge_key(old, edge)).collect();
    let new_keys: BTreeSet<_> = new.links.iter().map(|edge| edge_key(new, edge)).collect();
    let added_edges: BTreeSet<_> = new_keys.difference(&old_keys).cloned().collect();
    let removed_edges: BTreeSet<_> = old_keys.difference(&new_keys).cloned().collect();
    let new_edges_list: Vec<DiffEdge> = new
        .links
        .iter()
        .filter(|edge| added_edges.contains(&edge_key(new, edge)))
        .map(diff_edge)
        .collect();
    let removed_edges_list: Vec<DiffEdge> = old
        .links
        .iter()
        .filter(|edge| removed_edges.contains(&edge_key(old, edge)))
        .map(diff_edge)
        .collect();
    let mut parts = Vec::new();
    push_summary(&mut parts, new_nodes_list.len(), "new node", "new nodes");
    push_summary(&mut parts, new_edges_list.len(), "new edge", "new edges");
    push_summary_suffix(
        &mut parts,
        removed_nodes_list.len(),
        "node removed",
        "nodes removed",
    );
    push_summary_suffix(
        &mut parts,
        removed_edges_list.len(),
        "edge removed",
        "edges removed",
    );
    GraphDiff {
        new_nodes: new_nodes_list,
        removed_nodes: removed_nodes_list,
        new_edges: new_edges_list,
        removed_edges: removed_edges_list,
        summary: if parts.is_empty() {
            "no changes".into()
        } else {
            parts.join(", ")
        },
    }
}

fn edge_key(graph: &KnowledgeGraph, edge: &Edge) -> (String, String, String) {
    let source = edge.true_source();
    let target = edge.true_target();
    if graph.directed || source <= target {
        (source.into(), target.into(), edge.relation.clone())
    } else {
        (target.into(), source.into(), edge.relation.clone())
    }
}

fn diff_edge(edge: &Edge) -> DiffEdge {
    DiffEdge {
        source: edge.true_source().into(),
        target: edge.true_target().into(),
        relation: edge.relation.clone(),
        confidence: edge.confidence,
    }
}

fn push_summary(parts: &mut Vec<String>, count: usize, one: &str, many: &str) {
    if count > 0 {
        parts.push(format!("{count} {}", if count == 1 { one } else { many }));
    }
}

fn push_summary_suffix(parts: &mut Vec<String>, count: usize, one: &str, many: &str) {
    push_summary(parts, count, one, many);
}

pub fn suggest_questions(
    graph: &KnowledgeGraph,
    _communities: &BTreeMap<i64, Vec<String>>,
    _community_labels: &BTreeMap<i64, String>,
    top_n: usize,
) -> Vec<SuggestedQuestion> {
    let degree = degrees(graph);
    let mut questions = Vec::new();
    let positions = node_positions(graph);
    for edge in &graph.links {
        if edge.confidence != Confidence::Ambiguous {
            continue;
        }
        let (Some(&source), Some(&target)) = (
            positions.get(edge.true_source()),
            positions.get(edge.true_target()),
        ) else {
            continue;
        };
        questions.push(SuggestedQuestion {
            kind: "ambiguous_edge".into(),
            question: Some(format!(
                "What is the exact relationship between `{}` and `{}`?",
                graph.nodes[source].label, graph.nodes[target].label
            )),
            why: format!(
                "Edge tagged AMBIGUOUS (relation: {}) - confidence is low.",
                edge.relation
            ),
        });
    }
    let isolated: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| {
            let node_degree = degree.get(&node.id).copied().unwrap_or_default();
            node_degree <= 1
                && !is_file_node(node, node_degree)
                && !is_concept_node(node)
                && node.file_type != "rationale"
        })
        .collect();
    if !isolated.is_empty() {
        let labels = isolated
            .iter()
            .take(3)
            .map(|node| format!("`{}`", node.label))
            .collect::<Vec<_>>()
            .join(", ");
        questions.push(SuggestedQuestion {
            kind: "isolated_nodes".into(),
            question: Some(format!("What connects {labels} to the rest of the system?")),
            why: format!(
                "{} weakly-connected node{} found - possible documentation gaps or missing edges.",
                isolated.len(),
                if isolated.len() == 1 { "" } else { "s" }
            ),
        });
    }
    if questions.is_empty() {
        questions.push(SuggestedQuestion {
            kind: "no_signal".into(),
            question: None,
            why: "Not enough signal to generate questions.".into(),
        });
    }
    questions.truncate(top_n);
    questions
}

pub fn find_import_cycles(
    graph: &KnowledgeGraph,
    max_cycle_length: usize,
    top_n: usize,
) -> Vec<ImportCycle> {
    if max_cycle_length == 0 || top_n == 0 {
        return Vec::new();
    }
    let positions = node_positions(graph);
    let mut adjacency = BTreeMap::<String, BTreeSet<String>>::new();
    for edge in &graph.links {
        if !matches!(edge.relation.as_str(), "imports_from" | "re_exports")
            || edge
                .extra
                .get("deferred")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            || edge.source_file.is_empty()
        {
            continue;
        }
        let Some(&source_position) = positions.get(edge.true_source()) else {
            continue;
        };
        let Some(&target_position) = positions.get(edge.true_target()) else {
            continue;
        };
        let left_file = &graph.nodes[source_position].source_file;
        let right_file = &graph.nodes[target_position].source_file;
        let target_file = if *left_file == edge.source_file {
            right_file
        } else if *right_file == edge.source_file {
            left_file
        } else if !right_file.is_empty() && *right_file != edge.source_file {
            right_file
        } else {
            left_file
        };
        if target_file.is_empty() {
            continue;
        }
        adjacency
            .entry(edge.source_file.clone())
            .or_default()
            .insert(target_file.clone());
    }
    if adjacency.is_empty() {
        return Vec::new();
    }
    let starts: BTreeSet<String> = adjacency
        .keys()
        .cloned()
        .chain(adjacency.values().flatten().cloned())
        .collect();
    let mut normalized_cycles = BTreeSet::<Vec<String>>::new();
    for start in starts {
        let mut path = vec![start.clone()];
        let mut visited = BTreeSet::from([start.clone()]);
        enumerate_cycles(
            &start,
            &start,
            &adjacency,
            max_cycle_length,
            &mut path,
            &mut visited,
            &mut normalized_cycles,
            top_n.saturating_mul(10),
        );
    }
    let mut cycles: Vec<_> = normalized_cycles.into_iter().collect();
    cycles.sort_by_key(|cycle| (cycle.len(), cycle.clone()));
    cycles
        .into_iter()
        .take(top_n)
        .map(|cycle| ImportCycle {
            length: cycle.len(),
            cycle,
            why: "circular dependency".into(),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn enumerate_cycles(
    start: &str,
    current: &str,
    adjacency: &BTreeMap<String, BTreeSet<String>>,
    max_cycle_length: usize,
    path: &mut Vec<String>,
    visited: &mut BTreeSet<String>,
    cycles: &mut BTreeSet<Vec<String>>,
    limit: usize,
) {
    if cycles.len() >= limit {
        return;
    }
    let Some(neighbors) = adjacency.get(current) else {
        return;
    };
    for neighbor in neighbors {
        if neighbor == start {
            if path.len() <= max_cycle_length {
                cycles.insert(normalize_cycle(path));
            }
            continue;
        }
        if path.len() >= max_cycle_length || !visited.insert(neighbor.clone()) {
            continue;
        }
        path.push(neighbor.clone());
        enumerate_cycles(
            start,
            neighbor,
            adjacency,
            max_cycle_length,
            path,
            visited,
            cycles,
            limit,
        );
        path.pop();
        visited.remove(neighbor);
    }
}

fn normalize_cycle(cycle: &[String]) -> Vec<String> {
    let minimum = cycle
        .iter()
        .enumerate()
        .min_by_key(|(_, value)| *value)
        .map(|(index, _)| index)
        .unwrap_or_default();
    cycle[minimum..]
        .iter()
        .chain(&cycle[..minimum])
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_cover_newer_languages() {
        assert_eq!(file_category("app.swift"), "code");
        assert_eq!(file_category("plugin.lua"), "code");
        assert_eq!(file_category("paper.pdf"), "paper");
    }
}
