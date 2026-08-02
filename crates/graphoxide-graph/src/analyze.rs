//! Deterministic architectural analysis.

use graphoxide_core::{Confidence, KnowledgeGraph};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Serialize)]
pub struct GodNode {
    pub id: String,
    pub label: String,
    pub degree: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct Surprise {
    pub source: String,
    pub target: String,
    pub source_files: [String; 2],
    pub confidence: Confidence,
    pub relation: String,
    pub why: String,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct Analysis {
    pub god_nodes: Vec<GodNode>,
    pub surprising_connections: Vec<Surprise>,
    pub suggested_questions: Vec<String>,
}

pub fn analyze(graph: &KnowledgeGraph) -> anyhow::Result<Analysis> {
    let positions: HashMap<_, _> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    let mut degree = vec![0usize; graph.nodes.len()];
    for edge in &graph.links {
        if let (Some(&a), Some(&b)) = (
            positions.get(edge.true_source()),
            positions.get(edge.true_target()),
        ) {
            degree[a] += 1;
            if a != b {
                degree[b] += 1
            }
        }
    }
    let noise = [
        "str", "int", "float", "bool", "bytes", "object", "Path", "Any", "Optional", "List",
        "Dict", "Set", "Tuple", "Union", "Callable", "String", "Int", "Data",
    ];
    let mut gods: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            n.file_type != "concept"
                && !matches!(
                    n.extra.get("type").and_then(|v| v.as_str()),
                    Some("file" | "json_key")
                )
                && !noise.contains(&n.label.as_str())
                && !n.source_file.is_empty()
        })
        .map(|(i, n)| GodNode {
            id: n.id.clone(),
            label: n.label.clone(),
            degree: degree[i],
        })
        .collect();
    gods.sort_by(|a, b| b.degree.cmp(&a.degree).then_with(|| a.id.cmp(&b.id)));
    gods.truncate(10);
    let mut surprises = Vec::new();
    let mut sorted_degree = degree.clone();
    sorted_degree.sort_unstable();
    let hub_degree = sorted_degree
        .get(sorted_degree.len().saturating_mul(9) / 10)
        .copied()
        .unwrap_or(0);
    for edge in &graph.links {
        if matches!(
            edge.relation.as_str(),
            "imports" | "imports_from" | "contains" | "method"
        ) {
            continue;
        }
        let (Some(&a), Some(&b)) = (
            positions.get(edge.true_source()),
            positions.get(edge.true_target()),
        ) else {
            continue;
        };
        let na = &graph.nodes[a];
        let nb = &graph.nodes[b];
        if na.source_file.is_empty()
            || nb.source_file.is_empty()
            || na.source_file == nb.source_file
        {
            continue;
        }
        if edge.confidence != Confidence::Extracted
            && language_family(&na.source_file).is_some()
            && language_family(&nb.source_file).is_some()
            && language_family(&na.source_file) != language_family(&nb.source_file)
        {
            continue;
        }
        let mut reasons = Vec::new();
        if edge.confidence != Confidence::Extracted {
            reasons.push(format!(
                "{} connection - not explicitly stated in source",
                format!("{:?}", edge.confidence).to_lowercase()
            ))
        }
        if na.source_file.split('/').next() != nb.source_file.split('/').next() {
            reasons.push("connects across different repos/directories".into())
        }
        if na.community != nb.community {
            reasons.push("bridges separate communities".into())
        }
        let cross_topdir = na.source_file.split('/').next() != nb.source_file.split('/').next();
        let mut score = match edge.confidence {
            Confidence::Ambiguous => 3,
            Confidence::Inferred => 2,
            Confidence::Extracted => 1,
        };
        if na.file_type != nb.file_type {
            score += 2;
            reasons.push("connects different content categories".into());
        }
        if cross_topdir {
            score += 2;
        }
        if na.community != nb.community {
            score += 1;
        }
        if (degree[a] <= 1 && degree[b] >= hub_degree)
            || (degree[b] <= 1 && degree[a] >= hub_degree)
        {
            score += 1;
            reasons.push("links a peripheral node to an architectural hub".into());
        }
        surprises.push((
            score,
            Surprise {
                source: na.label.clone(),
                target: nb.label.clone(),
                source_files: [na.source_file.clone(), nb.source_file.clone()],
                confidence: edge.confidence,
                relation: edge.relation.clone(),
                why: if reasons.is_empty() {
                    "cross-file semantic connection".into()
                } else {
                    reasons.join("; ")
                },
            },
        ));
    }
    surprises.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.source.cmp(&b.1.source)));
    let surprising_connections = surprises.into_iter().take(5).map(|(_, s)| s).collect();
    let mut questions = Vec::new();
    for god in gods.iter().take(3) {
        questions.push(format!(
            "How does {} coordinate the surrounding components?",
            god.label.trim_end_matches("()")
        ));
    }
    for surprise in surprises_for_questions(graph).into_iter().take(2) {
        questions.push(format!(
            "Why does {} connect to {}?",
            surprise.0, surprise.1
        ));
    }
    let mut community_hubs = BTreeMap::<i64, (&str, usize)>::new();
    for (index, node) in graph.nodes.iter().enumerate() {
        if let Some(cid) = node.community {
            let candidate = (node.label.as_str(), degree[index]);
            community_hubs
                .entry(cid)
                .and_modify(|current| {
                    if candidate.1 > current.1
                        || (candidate.1 == current.1 && candidate.0 < current.0)
                    {
                        *current = candidate;
                    }
                })
                .or_insert(candidate);
        }
    }
    for (cid, (hub, _)) in community_hubs.into_iter().take(2) {
        questions.push(format!(
            "What responsibility does community {cid} around {hub} own?"
        ));
    }
    if let Some(edge) = graph
        .links
        .iter()
        .find(|e| e.confidence == Confidence::Ambiguous)
    {
        let labels: BTreeMap<_, _> = graph
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.label.as_str()))
            .collect();
        if let (Some(a), Some(b)) = (
            labels.get(edge.true_source()),
            labels.get(edge.true_target()),
        ) {
            questions.push(format!(
                "Should the ambiguous relationship between {a} and {b} be made explicit?"
            ));
        }
    }
    if let Some((_, node)) = graph
        .nodes
        .iter()
        .enumerate()
        .find(|(i, _)| degree[*i] == 0)
    {
        questions.push(format!(
            "Why is {} isolated from the rest of the graph?",
            node.label
        ));
    }
    questions.truncate(10);
    Ok(Analysis {
        god_nodes: gods,
        surprising_connections,
        suggested_questions: questions,
    })
}

fn language_family(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => "js",
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" => "c",
        "py" | "pyi" => "python",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "cs" => "csharp",
        "rb" => "ruby",
        _ => return None,
    })
}
fn surprises_for_questions(graph: &KnowledgeGraph) -> Vec<(String, String)> {
    let labels: BTreeMap<_, _> = graph
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.label.as_str()))
        .collect();
    graph
        .links
        .iter()
        .filter(|e| e.confidence != Confidence::Extracted)
        .filter_map(|e| {
            Some((
                labels.get(e.true_source())?.to_string(),
                labels.get(e.true_target())?.to_string(),
            ))
        })
        .collect()
}
