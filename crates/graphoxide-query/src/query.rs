//! Tiered graph search and bounded neighborhood traversal.

use graphoxide_core::{sanitize_label, Edge, KnowledgeGraph, Node};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

const STOPWORDS: &[&str] = &[
    "how",
    "what",
    "why",
    "when",
    "where",
    "which",
    "who",
    "whom",
    "whose",
    "does",
    "did",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "can",
    "could",
    "should",
    "would",
    "will",
    "shall",
    "may",
    "might",
    "must",
    "has",
    "have",
    "had",
    "the",
    "and",
    "but",
    "not",
    "for",
    "from",
    "with",
    "without",
    "into",
    "onto",
    "off",
    "that",
    "this",
    "these",
    "those",
    "there",
    "here",
    "its",
    "their",
    "them",
    "they",
    "about",
    "any",
    "all",
    "some",
    "work",
    "works",
    "working",
    "der",
    "die",
    "das",
    "den",
    "dem",
    "ein",
    "eine",
    "und",
    "oder",
    "nicht",
    "wie",
    "wer",
    "wann",
    "wo",
    "warum",
    "wieso",
    "welche",
    "welcher",
    "welches",
    "ist",
    "sind",
    "wird",
    "wurde",
    "hat",
    "haben",
    "kann",
    "koennen",
    "können",
    "soll",
    "muss",
    "sich",
    "bei",
    "mit",
    "von",
    "fuer",
    "für",
    "ueber",
    "über",
    "nach",
    "aus",
    "gibt",
    "es",
    "funktioniert",
    "geaendert",
    "geändert",
    "aendert",
    "ändert",
    "pourquoi",
    "quand",
    "quel",
    "quelle",
    "quels",
    "quelles",
    "quoi",
    "qui",
    "que",
    "est",
    "sont",
    "fonctionne",
    "cette",
    "dans",
    "avec",
    "où",
    "cómo",
    "como",
    "qué",
    "cuál",
    "cuáles",
    "cuándo",
    "dónde",
    "donde",
    "porque",
    "por",
    "para",
    "funciona",
    "está",
    "están",
    "hay",
    "qual",
    "quais",
    "quando",
    "onde",
    "são",
    "estão",
    "tem",
    "uma",
    "não",
    "perché",
    "cosa",
    "quale",
    "quali",
    "dove",
    "funziona",
    "sono",
    "che",
    "della",
];

pub struct GraphIndex<'a> {
    pub graph: &'a KnowledgeGraph,
    positions: HashMap<&'a str, usize>,
    adjacent: Vec<Vec<usize>>,
    incoming: Vec<Vec<usize>>,
    outgoing: Vec<Vec<usize>>,
}

impl<'a> GraphIndex<'a> {
    pub fn new(graph: &'a KnowledgeGraph) -> Self {
        let positions: HashMap<&'a str, usize> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        let mut adjacent = vec![Vec::new(); graph.nodes.len()];
        let mut incoming = vec![Vec::new(); graph.nodes.len()];
        let mut outgoing = vec![Vec::new(); graph.nodes.len()];
        for (edge_index, edge) in graph.links.iter().enumerate() {
            let (Some(&source), Some(&target)) = (
                positions.get(edge.true_source()),
                positions.get(edge.true_target()),
            ) else {
                continue;
            };
            outgoing[source].push(edge_index);
            incoming[target].push(edge_index);
            adjacent[source].push(edge_index);
            if source != target {
                adjacent[target].push(edge_index);
            }
        }
        Self {
            graph,
            positions,
            adjacent,
            incoming,
            outgoing,
        }
    }

    pub fn position(&self, id: &str) -> Option<usize> {
        self.positions.get(id).copied()
    }
    pub fn node(&self, position: usize) -> &'a Node {
        &self.graph.nodes[position]
    }
    pub fn degree(&self, position: usize) -> usize {
        self.adjacent[position].len()
    }
    pub fn incoming(&self, position: usize) -> &[usize] {
        &self.incoming[position]
    }
    pub fn outgoing(&self, position: usize) -> &[usize] {
        &self.outgoing[position]
    }

    pub fn other(&self, edge: &Edge, position: usize) -> Option<usize> {
        let source = self.position(edge.true_source())?;
        let target = self.position(edge.true_target())?;
        if source == position {
            Some(target)
        } else if target == position {
            Some(source)
        } else {
            None
        }
    }
}

pub fn search_tokens(text: &str) -> Vec<String> {
    let stripped: String = text
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect();
    let mut tokens = Vec::new();
    let mut token = String::new();
    for ch in stripped.chars() {
        if ch == '_' || ch.is_alphanumeric() {
            token.push(ch);
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

pub fn query_terms(question: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for token in search_tokens(question) {
        if token.chars().any(|c| ('一'..='鿿').contains(&c)) && token.chars().count() > 2 {
            let chars: Vec<_> = token.chars().collect();
            terms.extend(chars.windows(2).map(|w| w.iter().collect()));
            terms.push(token);
        } else if !token.chars().all(|c| c.is_ascii_lowercase()) || token.len() > 2 {
            terms.push(token);
        }
    }
    let content: Vec<_> = terms
        .iter()
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .cloned()
        .collect();
    if content.is_empty() {
        terms
    } else {
        content
    }
}

fn norm_label(node: &Node) -> String {
    node.extra
        .get("norm_label")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            node.label
                .nfkd()
                .filter(|c| !is_combining_mark(*c))
                .flat_map(char::to_lowercase)
                .collect()
        })
}

pub fn score_nodes(index: &GraphIndex<'_>, terms: &[String]) -> Vec<(f64, usize)> {
    let mut normalized = Vec::new();
    for term in terms.iter().flat_map(|term| search_tokens(term)) {
        if !normalized.contains(&term) {
            normalized.push(term);
        }
    }
    if normalized.is_empty() {
        return Vec::new();
    }
    let n = index.graph.nodes.len().max(1) as f64;
    let idf: HashMap<_, _> = normalized
        .iter()
        .map(|term| {
            let df = index
                .graph
                .nodes
                .iter()
                .filter(|node| norm_label(node).contains(term))
                .count() as f64;
            (term.as_str(), (1.0 + n / (1.0 + df)).ln())
        })
        .collect();
    let joined = normalized.join(" ");
    let joined_weight = normalized
        .iter()
        .map(|t| idf[t.as_str()])
        .fold(1.0, f64::max);
    let mut scored = Vec::new();
    for (position, node) in index.graph.nodes.iter().enumerate() {
        let label = norm_label(node);
        let bare = label.trim_end_matches("()");
        let label_tokens = search_tokens(&node.label).join(" ");
        let source = node.source_file.to_lowercase();
        let id = node.id.to_lowercase();
        let mut score = 0.0;
        if [label.as_str(), bare, label_tokens.as_str(), id.as_str()].contains(&joined.as_str()) {
            score += 10_000.0 * joined_weight;
        } else if label.starts_with(&joined)
            || bare.starts_with(&joined)
            || label_tokens.starts_with(&joined)
        {
            score += 1_000.0 * joined_weight;
        }
        let mut matched = 0usize;
        let mut tiered = 0.0;
        for term in &normalized {
            let weight = idf[term.as_str()];
            if term == &label || term == bare {
                tiered += 1_000.0 * weight;
                matched += 1;
            } else if label.starts_with(term) || bare.starts_with(term) {
                tiered += 100.0 * weight;
                matched += 1;
            } else if label.contains(term) {
                score += weight;
                matched += 1;
            }
            if source.contains(term) {
                score += 0.5 * weight;
            }
        }
        if tiered > 0.0 {
            score += tiered * (matched as f64 / normalized.len() as f64).powi(2);
        }
        if score > 0.0 {
            scored.push((score, position));
        }
    }
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| {
                index
                    .node(a.1)
                    .label
                    .len()
                    .cmp(&index.node(b.1).label.len())
            })
            .then_with(|| index.node(a.1).id.cmp(&index.node(b.1).id))
    });
    scored
}

fn seeds(index: &GraphIndex<'_>, terms: &[String], scored: &[(f64, usize)]) -> Vec<usize> {
    if scored.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut labels = HashSet::new();
    for &(score, position) in scored {
        if result.len() >= 3 || (!result.is_empty() && score < scored[0].0 * 0.2) {
            break;
        }
        if labels.insert(norm_label(index.node(position))) {
            result.push(position);
        }
    }
    for term in BTreeSet::from_iter(terms.iter()) {
        let term_scored = score_nodes(index, std::slice::from_ref(term));
        if let Some(&(top_score, _)) = term_scored.first() {
            let mut tied: Vec<_> = term_scored
                .iter()
                .take_while(|(score, _)| *score == top_score)
                .map(|(_, position)| *position)
                .collect();
            tied.sort_by(|a, b| {
                index
                    .degree(*b)
                    .cmp(&index.degree(*a))
                    .then_with(|| index.node(*a).label.len().cmp(&index.node(*b).label.len()))
                    .then_with(|| index.node(*a).id.cmp(&index.node(*b).id))
            });
            let position = tied[0];
            let label = norm_label(index.node(position));
            if !result.contains(&position) && labels.insert(label) {
                result.push(position);
            }
        }
    }
    result
}

pub fn query_graph(
    graph: &KnowledgeGraph,
    question: &str,
    depth: usize,
    token_budget: usize,
) -> String {
    traverse_graph(graph, question, depth, token_budget, false)
}

pub fn query_graph_dfs(
    graph: &KnowledgeGraph,
    question: &str,
    depth: usize,
    token_budget: usize,
) -> String {
    traverse_graph(graph, question, depth, token_budget, true)
}

pub fn query_graph_filtered(
    graph: &KnowledgeGraph,
    question: &str,
    depth: usize,
    token_budget: usize,
    relation_filters: &[String],
) -> String {
    let filtered = graph_with_relations(graph, relation_filters);
    traverse_graph(&filtered, question, depth, token_budget, false)
}

pub fn query_graph_dfs_filtered(
    graph: &KnowledgeGraph,
    question: &str,
    depth: usize,
    token_budget: usize,
    relation_filters: &[String],
) -> String {
    let filtered = graph_with_relations(graph, relation_filters);
    traverse_graph(&filtered, question, depth, token_budget, true)
}

fn graph_with_relations(graph: &KnowledgeGraph, filters: &[String]) -> KnowledgeGraph {
    if filters.is_empty() {
        return graph.clone();
    }
    let mut filtered = graph.clone();
    filtered.links.retain(|edge| {
        let relation = edge.relation.to_lowercase();
        filters.iter().any(|filter| {
            let filter = filter.trim().to_lowercase();
            match filter.as_str() {
                "call" | "calls" | "call_flow" | "call-flow" => {
                    matches!(relation.as_str(), "calls" | "indirect_call")
                }
                "import" | "imports" => {
                    matches!(relation.as_str(), "imports" | "imports_from" | "re_exports")
                }
                "type" | "types" => matches!(
                    relation.as_str(),
                    "references" | "inherits" | "implements" | "type_ref"
                ),
                "structure" | "contain" | "contains" => {
                    matches!(relation.as_str(), "contains" | "method" | "case_of")
                }
                _ => !filter.is_empty() && relation.contains(&filter),
            }
        })
    });
    filtered
}

fn traverse_graph(
    graph: &KnowledgeGraph,
    question: &str,
    depth: usize,
    token_budget: usize,
    dfs: bool,
) -> String {
    let index = GraphIndex::new(graph);
    let terms = query_terms(question);
    let scored = score_nodes(&index, &terms);
    let seeds = seeds(&index, &terms, &scored);
    if seeds.is_empty() {
        let terms = terms
            .iter()
            .map(|term| format!("'{}'", sanitize_label(term)))
            .collect::<Vec<_>>()
            .join(", ");
        return format!(
            "No graph nodes matched the content terms [{terms}]. This means Graphoxide could not choose a starting symbol; it does not prove the concept is absent. Try an exact symbol, filename, or domain term, or inspect the project overview."
        );
    }
    let mut degrees: Vec<_> = (0..graph.nodes.len()).map(|n| index.degree(n)).collect();
    degrees.sort_unstable();
    let hub_threshold = degrees
        .get(degrees.len() * 99 / 100)
        .copied()
        .unwrap_or(0)
        .max(50);
    let seed_set: HashSet<_> = seeds.iter().copied().collect();
    let mut visited = seed_set.clone();
    if dfs {
        let mut stack: Vec<_> = seeds.iter().rev().map(|p| (*p, 0usize)).collect();
        while let Some((current, level)) = stack.pop() {
            if level >= depth
                || (!seed_set.contains(&current) && index.degree(current) >= hub_threshold)
            {
                continue;
            }
            let mut next = Vec::new();
            for &edge_index in &index.adjacent[current] {
                if let Some(other) = index.other(&graph.links[edge_index], current) {
                    if visited.insert(other) {
                        next.push(other);
                    }
                }
            }
            next.sort_unstable_by(|a, b| b.cmp(a));
            stack.extend(next.into_iter().map(|p| (p, level + 1)));
        }
    } else {
        let mut frontier = seed_set.clone();
        for _ in 0..depth {
            let mut next = HashSet::new();
            let mut ordered: Vec<_> = frontier.into_iter().collect();
            ordered.sort_unstable();
            for current in ordered {
                if !seed_set.contains(&current) && index.degree(current) >= hub_threshold {
                    continue;
                }
                for &edge_index in &index.adjacent[current] {
                    if let Some(other) = index.other(&graph.links[edge_index], current) {
                        if !visited.contains(&other) {
                            next.insert(other);
                        }
                    }
                }
            }
            visited.extend(&next);
            frontier = next;
        }
    }
    render(&index, &visited, &seeds, token_budget, depth, dfs)
}

fn render(
    index: &GraphIndex<'_>,
    visited: &HashSet<usize>,
    seeds: &[usize],
    budget: usize,
    depth: usize,
    dfs: bool,
) -> String {
    let mut distance = HashMap::new();
    let mut queue = VecDeque::new();
    for &seed in seeds {
        distance.insert(seed, 0usize);
        queue.push_back(seed);
    }
    while let Some(current) = queue.pop_front() {
        let next_depth = distance[&current] + 1;
        for &edge_index in &index.adjacent[current] {
            if let Some(other) = index.other(&index.graph.links[edge_index], current) {
                if visited.contains(&other) && !distance.contains_key(&other) {
                    distance.insert(other, next_depth);
                    queue.push_back(other);
                }
            }
        }
    }
    let mut ordered: Vec<_> = visited.iter().copied().collect();
    ordered.sort_by_key(|p| {
        (
            if seeds.contains(p) { 0 } else { 1 },
            distance.get(p).copied().unwrap_or(usize::MAX),
            usize::MAX - index.degree(*p),
            index.node(*p).id.clone(),
        )
    });
    let starts = format!(
        "[{}]",
        seeds
            .iter()
            .map(|p| format!(
                "'{}'",
                index
                    .node(*p)
                    .label
                    .replace('\\', "\\\\")
                    .replace('\'', "\\'")
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let header = format!(
        "Traversal: {} depth={depth} | Start: {starts} | {} nodes found",
        if dfs { "DFS" } else { "BFS" },
        visited.len()
    );
    let mut lines = Vec::new();
    for position in ordered {
        let node = index.node(position);
        let location = node.source_location.as_deref().unwrap_or("");
        let community = node
            .extra
            .get("community_name")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .or_else(|| node.community.map(|v| v.to_string()))
            .unwrap_or_default();
        lines.push(format!(
            "NODE {} [src={} loc={} community={}]",
            sanitize_label(&node.label),
            sanitize_label(&node.source_file),
            sanitize_label(location),
            sanitize_label(&community)
        ));
    }
    for edge in &index.graph.links {
        let (Some(source), Some(target)) = (
            index.position(edge.true_source()),
            index.position(edge.true_target()),
        ) else {
            continue;
        };
        if !visited.contains(&source) || !visited.contains(&target) || source == target {
            continue;
        }
        let context = edge
            .extra
            .get("context")
            .and_then(|v| v.as_str())
            .map(|v| format!(" context={}", sanitize_label(v)))
            .unwrap_or_default();
        let at = edge
            .extra
            .get("source_location")
            .and_then(|v| v.as_str())
            .map(|loc| {
                format!(
                    " at={}:{}",
                    sanitize_label(&edge.source_file),
                    sanitize_label(loc)
                )
            })
            .unwrap_or_default();
        lines.push(format!(
            "EDGE {} --{} [{}{}]--> {}{}",
            sanitize_label(&index.node(source).label),
            sanitize_label(&edge.relation),
            serde_json::to_string(&edge.confidence)
                .unwrap()
                .trim_matches('"'),
            context,
            sanitize_label(&index.node(target).label),
            at
        ));
    }
    format!("{header}\n\n{}", budget_lines(lines, budget))
}

fn budget_lines(lines: Vec<String>, budget: usize) -> String {
    let full = lines.join("\n");
    let limit = budget.saturating_mul(3);
    if full.len() <= limit {
        return full;
    }
    let boundary = full
        .char_indices()
        .take_while(|(i, _)| *i <= limit)
        .filter(|(_, c)| *c == '\n')
        .map(|(i, _)| i)
        .last()
        .unwrap_or(limit.min(full.len()));
    let visible = &full[..boundary];
    let total_nodes = lines
        .iter()
        .filter(|line| line.starts_with("NODE "))
        .count();
    let total_edges = lines
        .iter()
        .filter(|line| line.starts_with("EDGE "))
        .count();
    let shown_nodes = visible
        .lines()
        .filter(|line| line.starts_with("NODE "))
        .count();
    let shown_edges = visible
        .lines()
        .filter(|line| line.starts_with("EDGE "))
        .count();
    let omitted_lines = lines.len().saturating_sub(visible.lines().count());
    format!(
        "[!] TRUNCATED: showing {shown_nodes}/{total_nodes} nodes and {shown_edges}/{total_edges} relationships within the ~{budget}-token budget ({omitted_lines} lines omitted). Raise token_budget or narrow the query with context_filter=['call'] or get_node.\n\n{visible}\n... (truncated — {omitted_lines} lines omitted)"
    )
}

pub fn find_node(index: &GraphIndex<'_>, query: &str) -> Vec<usize> {
    let term = search_tokens(query).join(" ");
    if term.is_empty() {
        return Vec::new();
    }
    let norm_query = query
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let mut tiers: [Vec<usize>; 4] = Default::default();
    for (position, node) in index.graph.nodes.iter().enumerate() {
        let label = norm_label(node);
        let bare = label.trim_end_matches("()");
        let label_tokens = search_tokens(&node.label).join(" ");
        let source_tokens = search_tokens(&node.source_file).join(" ");
        let id = node.id.to_lowercase();
        if term == source_tokens {
            tiers[0].push(position);
        } else if [label.as_str(), bare, label_tokens.as_str(), id.as_str()]
            .contains(&term.as_str())
            || norm_query == label
            || norm_query == bare
        {
            tiers[1].push(position);
        } else if label.starts_with(&term)
            || bare.starts_with(&term)
            || label_tokens.starts_with(&term)
            || id.starts_with(&term)
            || label.starts_with(&norm_query)
        {
            tiers[2].push(position);
        } else if label.contains(&term)
            || label_tokens.contains(&term)
            || label.contains(&norm_query)
        {
            tiers[3].push(position);
        }
    }
    tiers.into_iter().flatten().collect()
}

pub fn god_nodes(graph: &KnowledgeGraph, top: usize) -> Vec<(String, String, usize)> {
    let index = GraphIndex::new(graph);
    let noise = [
        "str",
        "int",
        "float",
        "bool",
        "object",
        "Path",
        "Any",
        "Optional",
        "List",
        "Dict",
        "Set",
        "Tuple",
        "Union",
        "Foundation",
        "SwiftUI",
        "String",
        "Int",
        "Data",
    ];
    let mut ranked: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.file_type != "concept" && !noise.contains(&n.label.as_str()))
        .filter(|(_, n)| {
            let basename = n.source_file.rsplit('/').next().unwrap_or("");
            n.label != basename && !(n.label.starts_with('.') && n.label.ends_with("()"))
        })
        .map(|(p, n)| (n.id.clone(), n.label.clone(), index.degree(p)))
        .collect();
    ranked.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(top);
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_core::{Confidence, Extraction};
    use std::collections::BTreeMap;
    fn graph() -> KnowledgeGraph {
        let node = |id: &str, label: &str| Node {
            id: id.into(),
            label: label.into(),
            file_type: "code".into(),
            source_file: format!("{id}.rs"),
            source_location: Some("L1".into()),
            community: None,
            extra: BTreeMap::new(),
        };
        KnowledgeGraph {
            nodes: vec![node("cache", "FrontierCache"), node("get", "get()")],
            links: vec![Edge {
                source: "get".into(),
                target: "cache".into(),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                source_file: "get.rs".into(),
                extra: BTreeMap::new(),
            }],
            ..Default::default()
        }
    }
    #[test]
    fn query_finds_content_after_stopword_filter() {
        let out = query_graph(&graph(), "how does frontier cache work", 2, 2000);
        assert!(out.contains("FrontierCache"));
        assert!(out.contains("EDGE get() --calls [EXTRACTED]--> FrontierCache"));
    }
    #[test]
    fn relation_filters_keep_call_flow_focused() {
        let mut graph = graph();
        graph.nodes.push(Node {
            id: "file".into(),
            label: "cache.rs".into(),
            file_type: "code".into(),
            source_file: "cache.rs".into(),
            source_location: Some("L1".into()),
            community: None,
            extra: BTreeMap::new(),
        });
        graph.links.push(Edge {
            source: "file".into(),
            target: "get".into(),
            relation: "contains".into(),
            confidence: Confidence::Extracted,
            source_file: "cache.rs".into(),
            extra: BTreeMap::new(),
        });

        let out = query_graph_filtered(&graph, "get", 2, 2000, &["call".into()]);
        assert!(out.contains("--calls"));
        assert!(!out.contains("--contains"));
        assert!(!out.contains("NODE cache.rs"));
    }
    #[test]
    fn no_match_explains_the_limit_of_the_result() {
        let out = query_graph(&graph(), "authentication", 2, 2000);
        assert!(out.contains("does not prove the concept is absent"));
        assert!(out.contains("'authentication'"));
    }
    #[test]
    fn truncation_reports_nodes_and_relationships_separately() {
        let out = query_graph(&graph(), "get", 2, 10);
        assert!(out.contains("TRUNCATED"));
        assert!(out.contains("nodes and"));
        assert!(out.contains("relationships"));
        assert!(out.contains("lines omitted"));
    }
    #[test]
    fn raw_extraction_type_remains_available() {
        let _ = Extraction::default();
    }
}
