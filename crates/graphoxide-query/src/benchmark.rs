//! Token-reduction benchmark compatible with upstream `graphify.benchmark`.

use crate::query::query_terms;
use graphoxide_core::{check_graph_file_size_cap_with, read_graph, KnowledgeGraph};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::path::Path;

const CHARS_PER_TOKEN: usize = 4;

pub const SAMPLE_QUESTIONS: &[&str] = &[
    "how does authentication work",
    "what is the main entry point",
    "how are errors handled",
    "what connects the data layer to the api",
    "what are the core abstractions",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkQuestion {
    pub question: String,
    pub query_tokens: usize,
    pub reduction: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub corpus_tokens: usize,
    pub corpus_words: usize,
    pub nodes: usize,
    pub edges: usize,
    pub avg_query_tokens: usize,
    pub reduction_ratio: f64,
    pub per_question: Vec<BenchmarkQuestion>,
}

impl BenchmarkResult {
    fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            corpus_tokens: 0,
            corpus_words: 0,
            nodes: 0,
            edges: 0,
            avg_query_tokens: 0,
            reduction_ratio: 0.0,
            per_question: Vec::new(),
        }
    }
}

fn rounded_tenth(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

pub fn query_subgraph_tokens(graph: &KnowledgeGraph, question: &str, depth: usize) -> usize {
    let terms = query_terms(question);
    let mut scored: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(position, node)| {
            let label = node.label.to_lowercase();
            let score = terms.iter().filter(|term| label.contains(*term)).count();
            (score > 0).then_some((score, position))
        })
        .collect();
    scored.sort_by(|(score_a, position_a), (score_b, position_b)| {
        score_b.cmp(score_a).then_with(|| {
            graph.nodes[*position_a]
                .id
                .cmp(&graph.nodes[*position_b].id)
        })
    });
    let starts: Vec<_> = scored
        .into_iter()
        .take(3)
        .map(|(_, position)| position)
        .collect();
    if starts.is_empty() {
        return 0;
    }
    let positions: std::collections::HashMap<_, _> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(position, node)| (node.id.as_str(), position))
        .collect();
    let mut visited: HashSet<_> = starts.iter().copied().collect();
    let mut queue: VecDeque<_> = starts
        .into_iter()
        .map(|position| (position, 0usize))
        .collect();
    let mut discovery_edges = Vec::new();
    while let Some((current, current_depth)) = queue.pop_front() {
        if current_depth >= depth {
            continue;
        }
        for (edge_index, edge) in graph.links.iter().enumerate() {
            let (Some(&source), Some(&target)) = (
                positions.get(edge.true_source()),
                positions.get(edge.true_target()),
            ) else {
                continue;
            };
            let other = if source == current {
                Some(target)
            } else if target == current {
                Some(source)
            } else {
                None
            };
            let Some(other) = other else { continue };
            if visited.insert(other) {
                discovery_edges.push(edge_index);
                queue.push_back((other, current_depth + 1));
            }
        }
    }
    let mut ordered: Vec<_> = visited.into_iter().collect();
    ordered.sort_unstable();
    let mut lines = Vec::new();
    for position in ordered {
        let node = &graph.nodes[position];
        lines.push(format!(
            "NODE {} src={} loc={}",
            node.label,
            node.source_file,
            node.source_location.as_deref().unwrap_or("")
        ));
    }
    for edge_index in discovery_edges {
        let edge = &graph.links[edge_index];
        let (Some(&source), Some(&target)) = (
            positions.get(edge.true_source()),
            positions.get(edge.true_target()),
        ) else {
            continue;
        };
        lines.push(format!(
            "EDGE {} --{}--> {}",
            graph.nodes[source].label, edge.relation, graph.nodes[target].label
        ));
    }
    (lines.join("\n").len() / CHARS_PER_TOKEN).max(1)
}

pub fn benchmark_graph(
    graph: &KnowledgeGraph,
    corpus_words: Option<usize>,
    questions: Option<&[String]>,
) -> BenchmarkResult {
    let corpus_words = corpus_words.unwrap_or(graph.nodes.len() * 50);
    let corpus_tokens = corpus_words * 100 / 75;
    let defaults: Vec<_> = SAMPLE_QUESTIONS
        .iter()
        .map(|value| (*value).to_owned())
        .collect();
    let questions = questions.unwrap_or(&defaults);
    let mut per_question = Vec::new();
    for question in questions {
        let query_tokens = query_subgraph_tokens(graph, question, 3);
        if query_tokens > 0 {
            per_question.push(BenchmarkQuestion {
                question: question.clone(),
                query_tokens,
                reduction: rounded_tenth(corpus_tokens as f64 / query_tokens as f64),
            });
        }
    }
    if per_question.is_empty() {
        return BenchmarkResult::error(
            "No matching nodes found for sample questions. Build the graph first.",
        );
    }
    let avg_query_tokens = per_question
        .iter()
        .map(|question| question.query_tokens)
        .sum::<usize>()
        / per_question.len();
    BenchmarkResult {
        error: None,
        corpus_tokens,
        corpus_words,
        nodes: graph.nodes.len(),
        edges: graph.links.len(),
        avg_query_tokens,
        reduction_ratio: if avg_query_tokens > 0 {
            rounded_tenth(corpus_tokens as f64 / avg_query_tokens as f64)
        } else {
            0.0
        },
        per_question,
    }
}

pub fn run_benchmark(
    graph_path: impl AsRef<Path>,
    corpus_words: Option<usize>,
    questions: Option<&[String]>,
    max_bytes: u64,
) -> anyhow::Result<BenchmarkResult> {
    let graph_path = graph_path.as_ref();
    check_graph_file_size_cap_with(graph_path, max_bytes)?;
    let graph = read_graph(graph_path)?;
    Ok(benchmark_graph(&graph, corpus_words, questions))
}

pub fn render_benchmark(result: &BenchmarkResult, unicode: bool) -> String {
    if let Some(error) = &result.error {
        return format!("Benchmark error: {error}");
    }
    let rule = if unicode { "─" } else { "-" }.repeat(50);
    let arrow = if unicode { "→" } else { "->" };
    let mut lines = vec![
        "graphoxide token reduction benchmark".to_owned(),
        rule,
        format!(
            "  Corpus:          {} words {arrow} ~{} tokens (naive)",
            result.corpus_words, result.corpus_tokens
        ),
        format!(
            "  Graph:           {} nodes, {} edges",
            result.nodes, result.edges
        ),
        format!("  Avg query cost:  ~{} tokens", result.avg_query_tokens),
        format!(
            "  Reduction:       {}x fewer tokens per query",
            result.reduction_ratio
        ),
        "".into(),
        "  Per question:".into(),
    ];
    lines.extend(
        result
            .per_question
            .iter()
            .map(|question| format!("    [{}x] {}", question.reduction, question.question)),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_core::{Confidence, Edge, Node};
    use std::collections::BTreeMap;

    fn graph() -> KnowledgeGraph {
        let node = |id: &str, label: &str, source: &str| Node {
            id: id.into(),
            label: label.into(),
            file_type: "code".into(),
            source_file: source.into(),
            source_location: Some("L1".into()),
            community: None,
            extra: BTreeMap::new(),
        };
        let edge = |source: &str, target: &str, relation: &str| Edge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            confidence: Confidence::Extracted,
            source_file: String::new(),
            extra: BTreeMap::new(),
        };
        KnowledgeGraph {
            nodes: vec![
                node("n1", "authentication", "auth.py"),
                node("n2", "api_handler", "api.py"),
                node("n3", "main_entry", "main.py"),
                node("n4", "error_handler", "errors.py"),
                node("n5", "database_layer", "db.py"),
            ],
            links: vec![
                edge("n1", "n2", "calls"),
                edge("n2", "n3", "imports"),
                edge("n3", "n4", "uses"),
                edge("n5", "n2", "provides"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_query_returns_positive_for_matching_question() {
        assert!(query_subgraph_tokens(&graph(), "how does authentication work", 3) > 0);
    }

    #[test]
    fn test_query_returns_zero_for_no_match() {
        assert_eq!(query_subgraph_tokens(&graph(), "xyzzy plugh zorkmid", 3), 0);
    }

    #[test]
    fn test_query_bfs_expands_neighbors() {
        let graph = graph();
        assert!(
            query_subgraph_tokens(&graph, "authentication", 3)
                >= query_subgraph_tokens(&graph, "authentication", 1)
        );
    }

    #[test]
    fn test_query_keeps_short_non_english_terms() {
        let graph = KnowledgeGraph {
            nodes: vec![Node {
                id: "frontend".into(),
                label: "前端".into(),
                file_type: "document".into(),
                source_file: "docs/前端.md".into(),
                source_location: Some("L1".into()),
                community: None,
                extra: BTreeMap::new(),
            }],
            ..Default::default()
        };
        assert!(query_subgraph_tokens(&graph, "前端", 1) > 0);
    }

    #[test]
    fn test_run_benchmark_returns_reduction() {
        let result = benchmark_graph(&graph(), Some(10_000), None);
        assert!(result.error.is_none());
        assert!(result.reduction_ratio > 1.0);
    }

    #[test]
    fn test_run_benchmark_corpus_tokens_proportional() {
        let graph = graph();
        let first = benchmark_graph(&graph, Some(1_000), None);
        let second = benchmark_graph(&graph, Some(10_000), None);
        assert!(second.corpus_tokens.abs_diff(first.corpus_tokens * 10) <= first.corpus_tokens);
    }

    #[test]
    fn test_run_benchmark_per_question_list() {
        let questions = vec![
            "how does authentication work".into(),
            "what is the main entry".into(),
        ];
        let result = benchmark_graph(&graph(), Some(5_000), Some(&questions));
        assert!(!result.per_question.is_empty());
        assert!(result
            .per_question
            .iter()
            .all(|entry| !entry.question.is_empty()
                && entry.query_tokens > 0
                && entry.reduction > 0.0));
    }

    #[test]
    fn test_run_benchmark_estimates_corpus_if_no_words() {
        assert!(benchmark_graph(&graph(), None, None).corpus_words > 0);
    }

    #[test]
    fn test_run_benchmark_error_on_empty_graph() {
        assert!(
            benchmark_graph(&KnowledgeGraph::default(), Some(1_000), None)
                .error
                .is_some()
        );
    }

    #[test]
    fn test_run_benchmark_includes_node_edge_counts() {
        let result = benchmark_graph(&graph(), Some(5_000), None);
        assert_eq!(result.nodes, 5);
        assert_eq!(result.edges, 4);
    }

    #[test]
    fn test_print_benchmark_no_crash() {
        let output = render_benchmark(&benchmark_graph(&graph(), Some(5_000), None), true);
        assert!(output.to_lowercase().contains("reduction"));
        assert!(output.contains('x'));
    }

    #[test]
    fn test_print_benchmark_error_message() {
        assert!(
            render_benchmark(&BenchmarkResult::error("test error message"), true)
                .contains("test error message")
        );
    }

    #[test]
    fn test_safe_returns_unicode_when_encodable() {
        let output = render_benchmark(&benchmark_graph(&graph(), Some(5_000), None), true);
        assert!(output.contains('→'));
        assert!(output.contains('─'));
    }

    #[test]
    fn test_safe_falls_back_when_unencodable() {
        let output = render_benchmark(&benchmark_graph(&graph(), Some(5_000), None), false);
        assert!(output.contains("->"));
        assert!(output.contains("-----"));
        assert!(!output.contains(['→', '─']));
    }

    #[test]
    fn test_print_benchmark_survives_cp1252_stdout() {
        let output = render_benchmark(&benchmark_graph(&graph(), Some(5_000), None), false);
        assert!(output.is_ascii());
        assert!(output.to_lowercase().contains("reduction"));
    }

    #[test]
    fn test_run_benchmark_rejects_oversized_graph() {
        let path = std::env::temp_dir().join(format!(
            "graphoxide-oversized-benchmark-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, serde_json::to_vec(&graph()).unwrap()).expect("write graph fixture");
        let error = run_benchmark(&path, None, None, 8).expect_err("size cap must reject graph");
        std::fs::remove_file(&path).expect("remove graph fixture");
        assert!(error.to_string().contains("exceeds"));
    }

    fn graph_payload(edge_key: &str) -> serde_json::Value {
        let mut value = serde_json::json!({
            "nodes": [
                {"id":"auth_flow", "label":"authentication flow", "source_file":"auth.py", "source_location":"L1"},
                {"id":"login_handler", "label":"user login authentication handler", "source_file":"auth.py", "source_location":"L10"},
                {"id":"main_entry", "label":"main entry point", "source_file":"main.py", "source_location":"L1"}
            ],
            "hyperedges": [],
            "input_tokens": 0,
            "output_tokens": 0
        });
        value.as_object_mut().unwrap().insert(edge_key.into(), serde_json::json!([
            {"source":"auth_flow", "target":"login_handler", "relation":"calls", "confidence":"EXTRACTED"},
            {"source":"login_handler", "target":"main_entry", "relation":"used_by", "confidence":"EXTRACTED"}
        ]));
        value
    }

    fn parsed_payload(edge_key: &str) -> KnowledgeGraph {
        serde_json::from_value(graph_payload(edge_key)).expect("raw or clustered graph payload")
    }

    #[test]
    fn test_run_benchmark_raw_edges_keyed_graph() {
        let result = benchmark_graph(&parsed_payload("edges"), Some(5_000), None);
        assert!(result.error.is_none());
        assert_eq!((result.nodes, result.edges), (3, 2));
        assert!(result.reduction_ratio > 0.0);
        assert!(result
            .per_question
            .iter()
            .any(|entry| entry.question.contains("authentication")));
    }

    #[test]
    fn test_run_benchmark_links_keyed_graph() {
        let result = benchmark_graph(&parsed_payload("links"), Some(5_000), None);
        assert!(result.error.is_none());
        assert_eq!((result.nodes, result.edges), (3, 2));
        assert!(result.reduction_ratio > 0.0);
    }

    #[test]
    fn test_raw_and_links_graphs_benchmark_identically() {
        assert_eq!(
            benchmark_graph(&parsed_payload("edges"), Some(5_000), None),
            benchmark_graph(&parsed_payload("links"), Some(5_000), None)
        );
    }
}
