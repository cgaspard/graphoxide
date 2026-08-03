//! Tiered graph search and bounded neighborhood traversal.

use graphoxide_core::{sanitize_label, Edge, KnowledgeGraph, Node};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

pub const EXACT_MATCH_BONUS: f64 = 1_000.0;
pub const PREFIX_MATCH_BONUS: f64 = 100.0;
pub const SUBSTRING_MATCH_BONUS: f64 = 1.0;
pub const SOURCE_MATCH_BONUS: f64 = 0.5;

pub type ChineseSegmenter = dyn Fn(&str) -> Vec<String>;

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
    query_cache: Arc<GraphQueryCache>,
}

#[derive(Debug)]
pub struct TrigramIndex {
    postings: HashMap<String, Vec<usize>>,
}

#[derive(Debug, Default)]
pub struct GraphQueryCache {
    idf: Mutex<HashMap<String, f64>>,
    trigrams: OnceLock<TrigramIndex>,
}

impl<'a> GraphIndex<'a> {
    pub fn new(graph: &'a KnowledgeGraph) -> Self {
        Self::new_with_cache(graph, Arc::new(GraphQueryCache::default()))
    }

    pub fn new_with_cache(graph: &'a KnowledgeGraph, query_cache: Arc<GraphQueryCache>) -> Self {
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
            let Some((source_id, target_id)) = validated_edge_endpoints(edge) else {
                continue;
            };
            let (Some(&source), Some(&target)) =
                (positions.get(source_id), positions.get(target_id))
            else {
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
            query_cache,
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

    pub fn adjacent(&self, position: usize) -> &[usize] {
        &self.adjacent[position]
    }

    pub fn trigram_index(&self) -> &TrigramIndex {
        self.query_cache.trigrams.get_or_init(|| {
            let mut postings: HashMap<String, Vec<usize>> = HashMap::new();
            for (position, node) in self.graph.nodes.iter().enumerate() {
                for trigram in trigrams(&node_search_text(node, &node.id)) {
                    postings.entry(trigram).or_default().push(position);
                }
            }
            TrigramIndex { postings }
        })
    }

    pub fn cached_idf_terms(&self) -> Vec<String> {
        let cache = self
            .query_cache
            .idf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut terms: Vec<_> = cache.keys().cloned().collect();
        terms.sort();
        terms
    }

    pub fn query_cache(&self) -> Arc<GraphQueryCache> {
        self.query_cache.clone()
    }

    pub fn other(&self, edge: &Edge, position: usize) -> Option<usize> {
        let (source_id, target_id) = validated_edge_endpoints(edge)?;
        let source = self.position(source_id)?;
        let target = self.position(target_id)?;
        if source == position {
            Some(target)
        } else if target == position {
            Some(source)
        } else {
            None
        }
    }
}

/// Trust `_src`/`_tgt` only when they name exactly the serialized edge endpoints.
/// Hand-edited graphs occasionally contain stale markers; falling back keeps the
/// graph queryable instead of silently dropping the relationship.
fn validated_edge_endpoints(edge: &Edge) -> Option<(&str, &str)> {
    let marked = edge
        .extra
        .get("_src")
        .and_then(serde_json::Value::as_str)
        .zip(edge.extra.get("_tgt").and_then(serde_json::Value::as_str));
    if let Some((source, target)) = marked {
        if (source == edge.source && target == edge.target)
            || (source == edge.target && target == edge.source)
        {
            return Some((source, target));
        }
    }
    Some((&edge.source, &edge.target))
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

pub fn has_chinese(text: &str) -> bool {
    text.chars()
        .any(|character| ('一'..='鿿').contains(&character))
}

fn query_word_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in text.chars().flat_map(char::to_lowercase) {
        if character == '_' || character.is_alphanumeric() {
            token.push(character);
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn searchable_query_term(term: &str) -> bool {
    !term.chars().all(|character| character.is_ascii_lowercase()) || term.len() > 2
}

/// Split a natural-language question into searchable content terms.
///
/// The optional segmenter is the Rust embedding hook corresponding to upstream's
/// cached `jieba` module. Production uses the deterministic bigram fallback, while
/// embedders that already ship a Chinese segmenter can supply it without making
/// Graphoxide depend on a heavyweight dictionary.
pub fn query_terms_with_chinese_segmenter(
    question: &str,
    segmenter: Option<&ChineseSegmenter>,
) -> Vec<String> {
    let mut terms = Vec::new();
    for raw in question.split_whitespace() {
        if has_chinese(raw) {
            let text = raw.to_lowercase();
            let mut segments = if let Some(segment) = segmenter {
                segment(&text)
            } else {
                let characters: Vec<_> = text.chars().collect();
                let pairs: Vec<String> = characters
                    .windows(2)
                    .map(|pair| pair.iter().collect())
                    .collect();
                if pairs.is_empty() {
                    vec![text.clone()]
                } else {
                    pairs
                }
            };
            if text.chars().count() > 1 && !segments.contains(&text) {
                segments.push(text);
            }
            terms.extend(
                segments
                    .into_iter()
                    .map(|term| term.trim().to_owned())
                    .filter(|term| !term.is_empty() && searchable_query_term(term)),
            );
        } else {
            terms.extend(
                query_word_tokens(raw)
                    .into_iter()
                    .filter(|term| searchable_query_term(term)),
            );
        }
    }
    let content: Vec<_> = terms
        .iter()
        .filter(|term| !STOPWORDS.contains(&term.as_str()))
        .cloned()
        .collect();
    if content.is_empty() {
        terms
    } else {
        content
    }
}

pub fn query_terms(question: &str) -> Vec<String> {
    query_terms_with_chinese_segmenter(question, None)
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

fn norm_text(value: &str) -> String {
    value
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn trigrams(text: &str) -> BTreeSet<String> {
    let characters: Vec<_> = text.chars().collect();
    if characters.is_empty() {
        return BTreeSet::new();
    }
    if characters.len() < 3 {
        return [text.to_owned()].into_iter().collect();
    }
    characters
        .windows(3)
        .map(|window| window.iter().collect())
        .collect()
}

pub fn node_search_text(node: &Node, id: &str) -> String {
    [
        norm_label(node),
        search_tokens(&node.label).join(" "),
        id.to_lowercase(),
        node.source_file.to_lowercase(),
        search_tokens(&node.source_file).join(" "),
    ]
    .join("\0")
}

pub fn trigram_candidates(index: &GraphIndex<'_>, needles: &[String]) -> Option<Vec<usize>> {
    trigram_candidates_with_guard(index, needles, 0.10)
}

pub fn trigram_candidates_with_guard(
    index: &GraphIndex<'_>,
    needles: &[String],
    guard_fraction: f64,
) -> Option<Vec<usize>> {
    let trigram_index = index.trigram_index();
    let node_count = index.graph.nodes.len();
    if node_count == 0 {
        return Some(Vec::new());
    }
    let needles: Vec<_> = needles.iter().filter(|needle| !needle.is_empty()).collect();
    let threshold = (node_count as f64 * guard_fraction) as usize;
    for needle in &needles {
        let grams = trigrams(needle);
        if grams.is_empty() || grams.iter().any(|gram| gram.chars().count() < 3) {
            return None;
        }
        let present: Vec<_> = grams
            .iter()
            .filter_map(|gram| trigram_index.postings.get(gram).map(Vec::len))
            .collect();
        if present.iter().min().is_some_and(|size| *size > threshold) {
            return None;
        }
    }
    let mut candidates = HashSet::new();
    for needle in needles {
        let mut postings: Vec<&Vec<usize>> = Vec::new();
        let mut missing = false;
        for gram in trigrams(needle) {
            match trigram_index.postings.get(&gram) {
                Some(bucket) => postings.push(bucket),
                None => {
                    missing = true;
                    break;
                }
            }
        }
        if missing || postings.is_empty() {
            continue;
        }
        postings.sort_by_key(|bucket| bucket.len());
        let mut hits: HashSet<_> = postings[0].iter().copied().collect();
        for bucket in postings.into_iter().skip(1) {
            hits.retain(|position| bucket.contains(position));
            if hits.is_empty() {
                break;
            }
        }
        candidates.extend(hits);
    }
    let mut candidates: Vec<_> = candidates.into_iter().collect();
    candidates.sort_unstable();
    Some(candidates)
}

pub fn compute_idf(index: &GraphIndex<'_>, terms: &[String]) -> HashMap<String, f64> {
    let node_count = index.graph.nodes.len().max(1) as f64;
    let mut cache = index
        .query_cache
        .idf
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for term in terms {
        if cache.contains_key(term) {
            continue;
        }
        let document_frequency = index
            .graph
            .nodes
            .iter()
            .filter(|node| norm_label(node).contains(term))
            .count() as f64;
        cache.insert(
            term.clone(),
            (1.0 + node_count / (1.0 + document_frequency)).ln(),
        );
    }
    terms
        .iter()
        .map(|term| {
            (
                term.clone(),
                cache
                    .get(term)
                    .copied()
                    .unwrap_or_else(|| (1.0 + node_count).ln()),
            )
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryScores {
    pub ranked: Vec<(f64, usize)>,
    pub best_seed_by_term: BTreeMap<String, usize>,
}

pub fn score_query(
    index: &GraphIndex<'_>,
    terms: &[String],
    collect_per_term_seeds: bool,
) -> QueryScores {
    score_query_impl(index, terms, collect_per_term_seeds, true)
}

pub fn score_query_full_scan(
    index: &GraphIndex<'_>,
    terms: &[String],
    collect_per_term_seeds: bool,
) -> QueryScores {
    score_query_impl(index, terms, collect_per_term_seeds, false)
}

fn score_query_impl(
    index: &GraphIndex<'_>,
    terms: &[String],
    collect_per_term_seeds: bool,
    use_prefilter: bool,
) -> QueryScores {
    let mut normalized = Vec::new();
    for term in terms.iter().flat_map(|term| search_tokens(term)) {
        if !normalized.contains(&term) {
            normalized.push(term);
        }
    }
    if normalized.is_empty() {
        return QueryScores {
            ranked: Vec::new(),
            best_seed_by_term: BTreeMap::new(),
        };
    }
    let idf = compute_idf(index, &normalized);
    let joined = normalized.join(" ");
    let joined_weight = normalized.iter().map(|term| idf[term]).fold(1.0, f64::max);
    let mut needles = normalized.clone();
    if !joined.is_empty() {
        needles.push(joined.clone());
    }
    let candidates = use_prefilter
        .then(|| trigram_candidates(index, &needles))
        .flatten();
    let positions: Vec<_> = candidates.unwrap_or_else(|| (0..index.graph.nodes.len()).collect());
    let mut scored = Vec::new();
    let mut best_by_term: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for position in positions {
        let node = index.node(position);
        let label = norm_label(node);
        let bare = label.trim_end_matches("()");
        let label_tokens = search_tokens(&node.label).join(" ");
        let source = node.source_file.to_lowercase();
        let id = node.id.to_lowercase();
        let mut score = 0.0;
        if [label.as_str(), bare, label_tokens.as_str(), id.as_str()].contains(&joined.as_str()) {
            score += EXACT_MATCH_BONUS * 10.0 * joined_weight;
        } else if label.starts_with(&joined)
            || bare.starts_with(&joined)
            || label_tokens.starts_with(&joined)
        {
            score += PREFIX_MATCH_BONUS * 10.0 * joined_weight;
        }
        let mut matched = 0usize;
        let mut tiered = 0.0;
        for term in &normalized {
            let weight = idf[term];
            let mut tier_value = 0.0;
            let mut substring_value = 0.0;
            let mut source_value = 0.0;
            if term == &label || term == bare {
                tier_value = EXACT_MATCH_BONUS * weight;
                matched += 1;
            } else if label.starts_with(term) || bare.starts_with(term) {
                tier_value = PREFIX_MATCH_BONUS * weight;
                matched += 1;
            } else if label.contains(term) {
                substring_value = SUBSTRING_MATCH_BONUS * weight;
                score += substring_value;
                matched += 1;
            }
            if source.contains(term) {
                source_value = SOURCE_MATCH_BONUS * weight;
                score += source_value;
            }
            tiered += tier_value;
            if collect_per_term_seeds {
                let mut singleton = if [label.as_str(), bare, label_tokens.as_str(), id.as_str()]
                    .contains(&term.as_str())
                {
                    EXACT_MATCH_BONUS * 10.0 * weight
                } else if label.starts_with(term)
                    || bare.starts_with(term)
                    || label_tokens.starts_with(term)
                {
                    PREFIX_MATCH_BONUS * 10.0 * weight
                } else {
                    0.0
                };
                singleton += tier_value + substring_value + source_value;
                if singleton > 0.0 {
                    let replace = best_by_term.get(term).is_none_or(|(best_score, best)| {
                        singleton > *best_score
                            || (singleton == *best_score
                                && (index.degree(position) > index.degree(*best)
                                    || (index.degree(position) == index.degree(*best)
                                        && (node.label.chars().count()
                                            < index.node(*best).label.chars().count()
                                            || (node.label.chars().count()
                                                == index.node(*best).label.chars().count()
                                                && node.id < index.node(*best).id)))))
                    });
                    if replace {
                        best_by_term.insert(term.clone(), (singleton, position));
                    }
                }
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
                    .chars()
                    .count()
                    .cmp(&index.node(b.1).label.chars().count())
            })
            .then_with(|| index.node(a.1).id.cmp(&index.node(b.1).id))
    });
    QueryScores {
        ranked: scored,
        best_seed_by_term: best_by_term
            .into_iter()
            .map(|(term, (_, position))| (term, position))
            .collect(),
    }
}

pub fn score_nodes(index: &GraphIndex<'_>, terms: &[String]) -> Vec<(f64, usize)> {
    score_query(index, terms, false).ranked
}

pub fn pick_seeds(
    scored: &[(f64, usize)],
    max_k: usize,
    gap_ratio: f64,
    index: Option<&GraphIndex<'_>>,
    best_seed_by_term: Option<&BTreeMap<String, usize>>,
) -> Vec<usize> {
    if scored.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut labels = HashSet::new();
    for &(score, position) in scored {
        if result.len() >= max_k || (!result.is_empty() && score < scored[0].0 * gap_ratio) {
            break;
        }
        let label = index
            .map(|index| norm_label(index.node(position)))
            .unwrap_or_else(|| position.to_string());
        if labels.insert(label) {
            result.push(position);
        }
    }
    if let (Some(index), Some(best_seed_by_term)) = (index, best_seed_by_term) {
        for &position in best_seed_by_term.values() {
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
    let filtered = filter_graph_by_context(graph, relation_filters);
    traverse_graph(&filtered, question, depth, token_budget, false)
}

pub fn query_graph_dfs_filtered(
    graph: &KnowledgeGraph,
    question: &str,
    depth: usize,
    token_budget: usize,
    relation_filters: &[String],
) -> String {
    let filtered = filter_graph_by_context(graph, relation_filters);
    traverse_graph(&filtered, question, depth, token_budget, true)
}

const CONTEXT_HINTS: &[(&str, &[&str])] = &[
    (
        "call",
        &["call", "calls", "called", "invoke", "invokes", "invoked"],
    ),
    (
        "import",
        &["import", "imports", "imported", "module", "modules"],
    ),
    (
        "field",
        &[
            "field",
            "fields",
            "member",
            "members",
            "property",
            "properties",
        ],
    ),
    (
        "parameter_type",
        &[
            "parameter",
            "parameters",
            "param",
            "params",
            "argument",
            "arguments",
        ],
    ),
    ("return_type", &["return", "returns", "returned"]),
    (
        "generic_arg",
        &["generic", "generics", "template", "templates"],
    ),
];

/// Normalize explicit query contexts, otherwise infer them from question words.
pub fn resolve_context_filters(
    question: &str,
    explicit: &[String],
) -> (Vec<String>, Option<&'static str>) {
    let mut normalized = Vec::new();
    for value in explicit {
        let value = normalize_context(value);
        if !value.is_empty() && !normalized.contains(&value) {
            normalized.push(value);
        }
    }
    if !normalized.is_empty() {
        return (normalized, Some("explicit"));
    }
    let words: BTreeSet<_> = search_tokens(question).into_iter().collect();
    for (context, hints) in CONTEXT_HINTS {
        if hints.iter().any(|hint| words.contains(*hint)) {
            normalized.push((*context).to_owned());
        }
    }
    let source = (!normalized.is_empty()).then_some("heuristic");
    (normalized, source)
}

pub fn normalize_context_filters(filters: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for filter in filters {
        let filter = normalize_context(filter);
        if !filter.is_empty() && !normalized.contains(&filter) {
            normalized.push(filter);
        }
    }
    normalized
}

pub fn infer_context_filters(question: &str) -> Vec<String> {
    resolve_context_filters(question, &[]).0
}

fn normalize_context(value: &str) -> String {
    let normalized: String = value
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .trim()
        .to_owned();
    match normalized.as_str() {
        "param" | "params" | "parameter" | "parameters" | "argument" | "arguments" | "arg"
        | "args" => "parameter_type".into(),
        "return" | "returns" | "returned" => "return_type".into(),
        "generic" | "generics" | "template" | "templates" => "generic_arg".into(),
        "annotation" | "annotations" | "decorator" | "decorators" => "attribute".into(),
        "calls" | "called" | "invoke" | "invocation" => "call".into(),
        "fields" | "property" | "properties" | "member" | "members" => "field".into(),
        "imports" | "imported" | "module" | "modules" => "import".into(),
        "exports" | "exported" => "export".into(),
        _ => normalized,
    }
}

pub fn filter_graph_by_context(graph: &KnowledgeGraph, filters: &[String]) -> KnowledgeGraph {
    if filters.is_empty() {
        return graph.clone();
    }
    let mut filtered = graph.clone();
    filtered.links.retain(|edge| {
        let relation = edge.relation.to_lowercase();
        let context = edge
            .extra
            .get("context")
            .and_then(|value| value.as_str())
            .map(normalize_context);
        filters.iter().any(|filter| {
            let filter = normalize_context(filter);
            if context.as_ref() == Some(&filter) {
                return true;
            }
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

pub fn communities_from_graph(graph: &KnowledgeGraph) -> BTreeMap<i64, Vec<String>> {
    let mut communities: BTreeMap<i64, Vec<String>> = BTreeMap::new();
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

fn hub_threshold(index: &GraphIndex<'_>) -> usize {
    let mut degrees: Vec<_> = (0..index.graph.nodes.len())
        .map(|position| index.degree(position))
        .collect();
    degrees.sort_unstable();
    degrees
        .get(degrees.len() * 99 / 100)
        .copied()
        .unwrap_or(0)
        .max(50)
}

fn complete_induced_edges(
    index: &GraphIndex<'_>,
    visited: &HashSet<usize>,
    edges: &mut Vec<(usize, usize)>,
) {
    let edge_key = |source: usize, target: usize| {
        if index.graph.directed || source <= target {
            (source, target)
        } else {
            (target, source)
        }
    };
    let mut seen: HashSet<_> = edges
        .iter()
        .map(|(source, target)| edge_key(*source, *target))
        .collect();
    for edge in &index.graph.links {
        let Some((source_id, target_id)) = validated_edge_endpoints(edge) else {
            continue;
        };
        let (Some(source), Some(target)) = (index.position(source_id), index.position(target_id))
        else {
            continue;
        };
        if source == target || !visited.contains(&source) || !visited.contains(&target) {
            continue;
        }
        let key = edge_key(source, target);
        if seen.insert(key) {
            edges.push((source, target));
        }
    }
}

pub fn bfs(
    index: &GraphIndex<'_>,
    start_nodes: &[usize],
    depth: usize,
) -> (HashSet<usize>, Vec<(usize, usize)>) {
    let threshold = hub_threshold(index);
    let seeds: HashSet<_> = start_nodes.iter().copied().collect();
    let mut visited = seeds.clone();
    let mut frontier = seeds.clone();
    let mut edges = Vec::new();
    for _ in 0..depth {
        let mut next = HashSet::new();
        let mut ordered: Vec<_> = frontier.into_iter().collect();
        ordered.sort_unstable();
        for current in ordered {
            if !seeds.contains(&current) && index.degree(current) >= threshold {
                continue;
            }
            for &edge_index in index.adjacent(current) {
                if let Some(other) = index.other(&index.graph.links[edge_index], current) {
                    if !visited.contains(&other) && next.insert(other) {
                        edges.push((current, other));
                    }
                }
            }
        }
        visited.extend(&next);
        frontier = next;
    }
    complete_induced_edges(index, &visited, &mut edges);
    (visited, edges)
}

pub fn dfs(
    index: &GraphIndex<'_>,
    start_nodes: &[usize],
    depth: usize,
) -> (HashSet<usize>, Vec<(usize, usize)>) {
    let threshold = hub_threshold(index);
    let seeds: HashSet<_> = start_nodes.iter().copied().collect();
    let mut visited = HashSet::new();
    let mut edges = Vec::new();
    let mut stack: Vec<_> = start_nodes
        .iter()
        .rev()
        .map(|position| (*position, 0usize))
        .collect();
    while let Some((current, level)) = stack.pop() {
        if visited.contains(&current) || level > depth {
            continue;
        }
        visited.insert(current);
        if !seeds.contains(&current) && index.degree(current) >= threshold {
            continue;
        }
        let mut next = Vec::new();
        for &edge_index in index.adjacent(current) {
            if let Some(other) = index.other(&index.graph.links[edge_index], current) {
                if !visited.contains(&other) {
                    edges.push((current, other));
                    next.push(other);
                }
            }
        }
        for other in next.into_iter().rev() {
            stack.push((other, level + 1));
        }
    }
    complete_induced_edges(index, &visited, &mut edges);
    (visited, edges)
}

pub fn query_graph_text(
    graph: &KnowledgeGraph,
    question: &str,
    mode: &str,
    depth: usize,
    token_budget: usize,
    explicit_context_filters: &[String],
) -> String {
    query_graph_text_impl(
        graph,
        question,
        mode,
        depth,
        token_budget,
        explicit_context_filters,
        Arc::new(GraphQueryCache::default()),
        &mut || {},
    )
}

pub fn query_graph_text_with_cache(
    graph: &KnowledgeGraph,
    query_cache: Arc<GraphQueryCache>,
    question: &str,
    mode: &str,
    depth: usize,
    token_budget: usize,
    explicit_context_filters: &[String],
) -> String {
    query_graph_text_impl(
        graph,
        question,
        mode,
        depth,
        token_budget,
        explicit_context_filters,
        query_cache,
        &mut || {},
    )
}

pub fn query_graph_text_with_score_observer(
    graph: &KnowledgeGraph,
    question: &str,
    mode: &str,
    depth: usize,
    token_budget: usize,
    explicit_context_filters: &[String],
    score_observer: &mut dyn FnMut(),
) -> String {
    query_graph_text_impl(
        graph,
        question,
        mode,
        depth,
        token_budget,
        explicit_context_filters,
        Arc::new(GraphQueryCache::default()),
        score_observer,
    )
}

#[allow(clippy::too_many_arguments)]
fn query_graph_text_impl(
    graph: &KnowledgeGraph,
    question: &str,
    mode: &str,
    depth: usize,
    token_budget: usize,
    explicit_context_filters: &[String],
    query_cache: Arc<GraphQueryCache>,
    score_observer: &mut dyn FnMut(),
) -> String {
    let score_index = GraphIndex::new_with_cache(graph, query_cache);
    let terms = query_terms(question);
    score_observer();
    let scores = score_query(&score_index, &terms, true);
    let starts = pick_seeds(
        &scores.ranked,
        3,
        0.2,
        Some(&score_index),
        Some(&scores.best_seed_by_term),
    );
    if starts.is_empty() {
        return "No matching nodes found.".into();
    }
    let (contexts, context_source) = resolve_context_filters(question, explicit_context_filters);
    let traversal_graph = filter_graph_by_context(graph, &contexts);
    let traversal_index = GraphIndex::new(&traversal_graph);
    let (nodes, edges) = if mode.eq_ignore_ascii_case("dfs") {
        dfs(&traversal_index, &starts, depth)
    } else {
        bfs(&traversal_index, &starts, depth)
    };
    let start_labels = starts
        .iter()
        .map(|position| format!("'{}'", traversal_index.node(*position).label))
        .collect::<Vec<_>>()
        .join(", ");
    let mut header = format!(
        "Traversal: {} depth={depth} | Start: [{start_labels}]",
        if mode.eq_ignore_ascii_case("dfs") {
            "DFS"
        } else {
            "BFS"
        }
    );
    if !contexts.is_empty() {
        header.push_str(&format!(
            " | Context: {} ({})",
            contexts.join(", "),
            context_source.unwrap_or("explicit")
        ));
    }
    header.push_str(&format!(" | {} nodes found\n\n", nodes.len()));
    header + &subgraph_to_text(&traversal_index, &nodes, &edges, token_budget, &starts)
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
    let scores = score_query(&index, &terms, true);
    let seeds = pick_seeds(
        &scores.ranked,
        3,
        0.2,
        Some(&index),
        Some(&scores.best_seed_by_term),
    );
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
    let mut edges = Vec::new();
    for edge in &index.graph.links {
        let Some((source_id, target_id)) = validated_edge_endpoints(edge) else {
            continue;
        };
        if let (Some(source), Some(target)) = (index.position(source_id), index.position(target_id))
        {
            if source != target && visited.contains(&source) && visited.contains(&target) {
                edges.push((source, target));
            }
        }
    }
    format!(
        "{header}\n\n{}",
        subgraph_to_text(index, visited, &edges, budget, seeds)
    )
}

fn char_budget_end(text: &str, character_budget: usize) -> usize {
    text.char_indices()
        .nth(character_budget)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

fn edge_for_pair<'a>(
    index: &'a GraphIndex<'_>,
    source: usize,
    target: usize,
) -> Option<(&'a Edge, usize, usize)> {
    index.graph.links.iter().find_map(|edge| {
        let (edge_source_id, edge_target_id) = validated_edge_endpoints(edge)?;
        let edge_source = index.position(edge_source_id)?;
        let edge_target = index.position(edge_target_id)?;
        let matches = if index.graph.directed {
            edge_source == source && edge_target == target
        } else {
            (edge_source == source && edge_target == target)
                || (edge_source == target && edge_target == source)
        };
        matches.then_some((edge, edge_source, edge_target))
    })
}

pub fn subgraph_to_text(
    index: &GraphIndex<'_>,
    visited: &HashSet<usize>,
    edges: &[(usize, usize)],
    token_budget: usize,
    seeds: &[usize],
) -> String {
    let seed_hits: Vec<_> = seeds
        .iter()
        .copied()
        .filter(|seed| visited.contains(seed))
        .collect();
    let seed_set: HashSet<_> = seeds.iter().copied().collect();
    let mut distances = HashMap::new();
    let mut queue = VecDeque::new();
    for seed in &seed_hits {
        distances.insert(*seed, 0usize);
        queue.push_back(*seed);
    }
    while let Some(current) = queue.pop_front() {
        let distance = distances[&current] + 1;
        for &edge_index in index.adjacent(current) {
            if let Some(other) = index.other(&index.graph.links[edge_index], current) {
                if visited.contains(&other) && !distances.contains_key(&other) {
                    distances.insert(other, distance);
                    queue.push_back(other);
                }
            }
        }
    }
    let mut ordered = seed_hits.clone();
    let mut remainder: Vec<_> = visited
        .iter()
        .copied()
        .filter(|position| !seed_set.contains(position))
        .collect();
    remainder.sort_by(|left, right| {
        distances
            .get(left)
            .copied()
            .unwrap_or(usize::MAX)
            .cmp(&distances.get(right).copied().unwrap_or(usize::MAX))
            .then_with(|| index.degree(*right).cmp(&index.degree(*left)))
            .then_with(|| index.node(*left).id.cmp(&index.node(*right).id))
    });
    ordered.extend(remainder);

    let overlay = index
        .graph
        .extra
        .get("_learning_overlay")
        .and_then(serde_json::Value::as_object);
    let mut lines = Vec::new();
    for position in ordered {
        let node = index.node(position);
        let location = node.source_location.as_deref().unwrap_or("");
        let community = node
            .extra
            .get("community_name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| node.community.map(|community| community.to_string()))
            .unwrap_or_default();
        let learning = overlay
            .and_then(|overlay| overlay.get(&node.id))
            .and_then(serde_json::Value::as_object)
            .and_then(|entry| {
                let status = entry
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .map(sanitize_label)
                    .unwrap_or_default();
                (!status.is_empty()).then(|| {
                    format!(
                        " learning={status}{}",
                        if entry
                            .get("stale")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false)
                        {
                            ":stale"
                        } else {
                            ""
                        }
                    )
                })
            })
            .unwrap_or_default();
        lines.push(format!(
            "NODE {} [src={} loc={} community={}{}]",
            sanitize_label(&node.label),
            sanitize_label(&node.source_file),
            sanitize_label(location),
            sanitize_label(&community),
            learning
        ));
    }
    for &(source, target) in edges {
        if !visited.contains(&source) || !visited.contains(&target) || source == target {
            continue;
        }
        let Some((edge, true_source, true_target)) = edge_for_pair(index, source, target) else {
            continue;
        };
        let context = edge
            .extra
            .get("context")
            .and_then(serde_json::Value::as_str)
            .map(|context| format!(" context={}", sanitize_label(context)))
            .unwrap_or_default();
        let at = edge
            .extra
            .get("source_location")
            .and_then(serde_json::Value::as_str)
            .map(|location| {
                format!(
                    " at={}:{}",
                    sanitize_label(&edge.source_file),
                    sanitize_label(location)
                )
            })
            .unwrap_or_default();
        let confidence = serde_json::to_string(&edge.confidence).unwrap_or_default();
        lines.push(format!(
            "EDGE {} --{} [{}{}]--> {}{}",
            sanitize_label(&index.node(true_source).label),
            sanitize_label(&edge.relation),
            confidence.trim_matches('"'),
            context,
            sanitize_label(&index.node(true_target).label),
            at
        ));
    }
    let output = lines.join("\n");
    let character_budget = token_budget.saturating_mul(3);
    if output.chars().count() <= character_budget {
        return output;
    }
    let budget_end = char_budget_end(&output, character_budget);
    let mut cut_at = output[..budget_end]
        .rfind('\n')
        .filter(|cut| *cut > 0)
        .unwrap_or(budget_end);
    if !seed_hits.is_empty() {
        let seed_block_end = lines
            .iter()
            .take(seed_hits.len())
            .map(|line| line.len() + 1)
            .sum::<usize>()
            .saturating_sub(1);
        cut_at = cut_at.max(seed_block_end.min(output.len()));
    }
    let kept = &output[..cut_at];
    let total_nodes = lines
        .iter()
        .filter(|line| line.starts_with("NODE "))
        .count();
    let shown_nodes = kept
        .lines()
        .filter(|line| line.starts_with("NODE "))
        .count();
    let total_edges = lines
        .iter()
        .filter(|line| line.starts_with("EDGE "))
        .count();
    let shown_edges = kept
        .lines()
        .filter(|line| line.starts_with("EDGE "))
        .count();
    let cut_nodes = total_nodes.saturating_sub(shown_nodes);
    let omitted_lines = lines.len().saturating_sub(kept.lines().count());
    format!(
        "[!] TRUNCATED: showing {shown_nodes} of {total_nodes} nodes and {shown_edges} of {total_edges} relationships (~{token_budget}-token budget; {omitted_lines} lines omitted). The answer may be among the {cut_nodes} cut nodes — raise the token budget (CLI: --budget) or narrow the query (e.g. context_filter=['call'], or get_node for a specific symbol).\n\n{kept}\n... (truncated — {cut_nodes} more nodes cut by ~{token_budget}-token budget. Narrow with context_filter=['call'] or use get_node for a specific symbol)"
    )
}

pub fn cut_lines_to_budget(lines: &[String], token_budget: usize, narrow_hint: &str) -> String {
    let output = lines.join("\n");
    let character_budget = token_budget.saturating_mul(3);
    if output.chars().count() <= character_budget {
        return output;
    }
    let budget_end = char_budget_end(&output, character_budget);
    let cut_at = output[..budget_end]
        .rfind('\n')
        .filter(|cut| *cut > 0)
        .unwrap_or(budget_end);
    let kept = &output[..cut_at];
    let shown = kept.lines().count();
    let cut_count = lines.len().saturating_sub(shown);
    format!(
        "[!] TRUNCATED: showing {shown} of {} lines (~{token_budget}-token budget). {narrow_hint}\n\n{kept}\n... (truncated — {cut_count} more lines cut by ~{token_budget}-token budget. {narrow_hint})",
        lines.len()
    )
}

pub fn community_header(community: i64, community_name: Option<&str>) -> String {
    let base = format!("Community {community}");
    if let Some(name) = community_name {
        let clean = sanitize_label(name);
        if !clean.is_empty() && clean != base {
            return format!("{base} — {clean}");
        }
    }
    base
}

pub fn find_node(index: &GraphIndex<'_>, query: &str) -> Vec<usize> {
    find_node_impl(index, query, true)
}

pub fn find_node_full_scan(index: &GraphIndex<'_>, query: &str) -> Vec<usize> {
    find_node_impl(index, query, false)
}

fn find_node_impl(index: &GraphIndex<'_>, query: &str, use_prefilter: bool) -> Vec<usize> {
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
    let candidates = use_prefilter
        .then(|| trigram_candidates(index, &[term.clone(), norm_query.clone()]))
        .flatten();
    let positions: Vec<_> = candidates.unwrap_or_else(|| (0..index.graph.nodes.len()).collect());
    for position in positions {
        let node = index.node(position);
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
    if !tiers[0].is_empty() {
        let basename = query
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(query);
        let basename = norm_text(basename);
        let preferred: Vec<_> = tiers[0]
            .iter()
            .copied()
            .filter(|position| {
                let node = index.node(*position);
                node.source_location.as_deref() == Some("L1")
                    && (norm_text(&node.label) == basename
                        || norm_text(&node.label).ends_with(&format!("/{basename}")))
            })
            .collect();
        if preferred.len() == 1 {
            let preferred = preferred[0];
            tiers[0].retain(|position| *position != preferred);
            tiers[0].insert(0, preferred);
        }
    }
    tiers.into_iter().flatten().collect()
}

pub fn graph_file_key(path: &Path) -> anyhow::Result<(u128, u64)> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("Graph file not found: {}", path.display())
        } else {
            anyhow::anyhow!("Cannot stat graph file {}: {error}", path.display())
        }
    })?;
    let modified = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok((modified, metadata.len()))
}

pub fn load_graph(path: &Path) -> anyhow::Result<KnowledgeGraph> {
    load_graph_with_cap(path, graphoxide_core::max_graph_bytes())
}

pub fn load_graph_with_cap(path: &Path, cap: u64) -> anyhow::Result<KnowledgeGraph> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
        anyhow::bail!("Graph path must be a .json file, got: {:?}", path);
    }
    if !path.exists() {
        anyhow::bail!("Graph file not found: {}", path.display());
    }
    graphoxide_core::read_graph_with_cap(path, cap).map_err(|error| {
        let message = error.to_string();
        if message.contains("corrupted") {
            anyhow::anyhow!("graph.json is corrupted ({message}). Re-run /graphify to rebuild.")
        } else {
            error
        }
    })
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
    fn test_query_cli_explicit_context_filter() {
        let mut graph = graph();
        graph.nodes.push(Node {
            id: "build".into(),
            label: "build".into(),
            file_type: "code".into(),
            source_file: "build.py".into(),
            source_location: Some("L1".into()),
            community: Some(1),
            extra: BTreeMap::new(),
        });
        let mut import_extra = BTreeMap::new();
        import_extra.insert("context".into(), serde_json::json!("import"));
        graph.links.push(Edge {
            source: "cache".into(),
            target: "build".into(),
            relation: "imports".into(),
            confidence: Confidence::Extracted,
            source_file: "cache.py".into(),
            extra: import_extra,
        });

        let (contexts, source) = resolve_context_filters("get", &["call".into()]);
        assert_eq!(contexts, ["call"]);
        assert_eq!(source, Some("explicit"));
        let out = query_graph_filtered(&graph, "get", 2, 2000, &contexts);
        assert!(out.contains("FrontierCache"));
        assert!(!out.contains("NODE build"));
    }

    #[test]
    fn test_query_cli_heuristic_context_filter() {
        let (contexts, source) = resolve_context_filters("who calls get", &[]);
        assert_eq!(contexts, ["call"]);
        assert_eq!(source, Some("heuristic"));
        let out = query_graph_filtered(&graph(), "get", 2, 2000, &contexts);
        assert!(out.contains("FrontierCache"));
        assert!(out.contains("--calls"));
    }

    fn directional_calls_graph() -> KnowledgeGraph {
        let node = |id: &str| Node {
            id: id.into(),
            label: format!("{id}_fn"),
            file_type: "code".into(),
            source_file: format!("{id}.py"),
            source_location: Some("L1".into()),
            community: None,
            extra: BTreeMap::new(),
        };
        KnowledgeGraph {
            nodes: vec![node("caller"), node("callee")],
            links: vec![Edge {
                source: "caller".into(),
                target: "callee".into(),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                source_file: "caller.py".into(),
                extra: BTreeMap::new(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn test_query_cli_preserves_calls_direction_when_seeded_on_callee() {
        let out = query_graph(&directional_calls_graph(), "callee_fn", 2, 2000);
        assert!(out.contains("caller_fn --calls"));
        assert!(!out.contains("callee_fn --calls"));
    }

    #[test]
    fn test_query_cli_preserves_calls_direction_when_seeded_on_caller() {
        let out = query_graph(&directional_calls_graph(), "caller_fn", 2, 2000);
        assert!(out.contains("caller_fn --calls"));
        assert!(!out.contains("callee_fn --calls"));
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

    fn named_graph(names: &[&str], links: &[(&str, &str, &str, &str)]) -> KnowledgeGraph {
        let nodes = names
            .iter()
            .map(|name| Node {
                id: (*name).into(),
                label: (*name).into(),
                file_type: "code".into(),
                source_file: format!("{name}.py"),
                source_location: Some("L1".into()),
                community: None,
                extra: BTreeMap::new(),
            })
            .collect();
        let links = links
            .iter()
            .map(|(source, target, relation, context)| {
                let mut extra = BTreeMap::new();
                extra.insert("context".into(), serde_json::json!(context));
                Edge {
                    source: (*source).into(),
                    target: (*target).into(),
                    relation: (*relation).into(),
                    confidence: Confidence::Extracted,
                    source_file: String::new(),
                    extra,
                }
            })
            .collect();
        KnowledgeGraph {
            nodes,
            links,
            ..Default::default()
        }
    }

    fn edge_lines(output: &str) -> Vec<&str> {
        output
            .lines()
            .filter(|line| line.starts_with("EDGE "))
            .collect()
    }

    #[test]
    fn test_bfs_records_edge_between_two_seeds() {
        let graph = named_graph(
            &["checkout", "discounted_total"],
            &[("checkout", "discounted_total", "calls", "call")],
        );
        let out = query_graph(&graph, "checkout discounted_total", 1, 2000);
        assert_eq!(edge_lines(&out).len(), 1);
        assert!(out.contains("EDGE checkout --calls"));
    }

    #[test]
    fn test_bfs_records_cross_edge_in_a_triangle() {
        let graph = named_graph(
            &["n1", "n2", "n3"],
            &[
                ("n1", "n2", "calls", "call"),
                ("n1", "n3", "calls", "call"),
                ("n2", "n3", "calls", "call"),
            ],
        );
        assert_eq!(edge_lines(&query_graph(&graph, "n1", 2, 2000)).len(), 3);
    }

    fn visited_hubs_graph() -> KnowledgeGraph {
        let mut names = vec!["seed".to_owned(), "hub_a".to_owned(), "hub_b".to_owned()];
        let mut links = vec![
            ("seed".to_owned(), "hub_a".to_owned()),
            ("seed".to_owned(), "hub_b".to_owned()),
            ("hub_a".to_owned(), "hub_b".to_owned()),
        ];
        for index in 0..60 {
            let a = format!("a{index}");
            let b = format!("b{index}");
            names.extend([a.clone(), b.clone()]);
            links.extend([("hub_a".into(), a), ("hub_b".into(), b)]);
        }
        let name_refs: Vec<_> = names.iter().map(String::as_str).collect();
        let link_refs: Vec<_> = links
            .iter()
            .map(|(source, target)| (source.as_str(), target.as_str(), "calls", "call"))
            .collect();
        named_graph(&name_refs, &link_refs)
    }

    #[test]
    fn test_bfs_records_edge_between_two_visited_hubs() {
        let out = query_graph(&visited_hubs_graph(), "seed", 1, 2000);
        assert!(out.contains("EDGE hub_a --calls [EXTRACTED context=call]--> hub_b"));
    }

    #[test]
    fn test_dfs_records_edge_between_two_visited_hubs() {
        let out = query_graph_dfs(&visited_hubs_graph(), "seed", 1, 2000);
        assert!(out.contains("EDGE hub_a --calls [EXTRACTED context=call]--> hub_b"));
    }

    #[test]
    fn test_traversal_edges_keep_discovery_order_and_come_first() {
        let graph = named_graph(
            &["n1", "n2", "n3"],
            &[
                ("n1", "n2", "calls", "call"),
                ("n1", "n3", "calls", "call"),
                ("n2", "n3", "calls", "call"),
            ],
        );
        let out = query_graph(&graph, "n1", 2, 2000);
        let edges = edge_lines(&out);
        assert!(edges[0].contains("n1") && edges[0].contains("n2"));
        assert!(edges[1].contains("n1") && edges[1].contains("n3"));
    }

    #[test]
    fn test_no_duplicate_edges_are_returned() {
        let graph = named_graph(
            &["n1", "n2", "n3", "n4"],
            &[
                ("n1", "n2", "calls", "call"),
                ("n1", "n3", "calls", "call"),
                ("n2", "n3", "calls", "call"),
                ("n2", "n4", "calls", "call"),
                ("n3", "n4", "calls", "call"),
            ],
        );
        assert_eq!(edge_lines(&query_graph(&graph, "n1", 3, 2000)).len(), 5);
        assert_eq!(edge_lines(&query_graph_dfs(&graph, "n1", 3, 2000)).len(), 5);
    }

    #[test]
    fn test_completion_respects_the_context_filter() {
        let graph = named_graph(
            &["n1", "n2", "n3"],
            &[
                ("n1", "n2", "calls", "call"),
                ("n1", "n3", "imports", "import"),
                ("n2", "n3", "imports", "import"),
            ],
        );
        let out = query_graph_filtered(&graph, "n1 n2 n3", 1, 2000, &["call".into()]);
        assert_eq!(edge_lines(&out).len(), 1);
        assert!(out.contains("--calls"));
        assert!(!out.contains("--imports"));
    }

    #[test]
    fn test_self_loops_are_not_introduced() {
        let graph = named_graph(
            &["recurse", "caller"],
            &[
                ("recurse", "recurse", "calls", "call"),
                ("caller", "recurse", "calls", "call"),
            ],
        );
        let out = query_graph(&graph, "caller recurse", 1, 2000);
        assert_eq!(edge_lines(&out).len(), 1);
        assert!(out.contains("EDGE caller --calls"));
    }

    #[test]
    fn test_directed_graph_keeps_both_directions_of_a_mutual_edge() {
        let mut graph = named_graph(
            &["ping", "pong"],
            &[
                ("ping", "pong", "calls", "call"),
                ("pong", "ping", "calls", "call"),
            ],
        );
        graph.directed = true;
        let out = query_graph(&graph, "ping pong", 1, 2000);
        assert_eq!(edge_lines(&out).len(), 2);
        assert!(out.contains("EDGE ping --calls"));
        assert!(out.contains("EDGE pong --calls"));
    }

    #[test]
    fn test_directed_graph_renders_the_seed_to_seed_edge() {
        let mut graph = named_graph(
            &["checkout", "discounted_total"],
            &[("checkout", "discounted_total", "calls", "call")],
        );
        graph.directed = true;
        let out = query_graph(&graph, "checkout discounted_total", 1, 2000);
        assert!(out.contains("EDGE checkout --calls"));
    }

    #[test]
    fn test_query_cli_renders_the_edge_between_two_seeds() {
        let graph = named_graph(
            &["checkout", "discounted_total"],
            &[("checkout", "discounted_total", "calls", "call")],
        );
        let out = query_graph(&graph, "checkout discounted_total", 1, 2000);
        assert!(out.contains("NODE checkout"));
        assert!(out.contains("NODE discounted_total"));
        assert!(out.contains("EDGE checkout --calls"));
    }
}
