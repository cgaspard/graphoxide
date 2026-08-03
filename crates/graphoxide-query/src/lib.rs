//! Read-side graph operations against graph.json.
//!
//! Port of the upstream `query`, `path`, `explain`, `affected`, `god-nodes`
//! CLI commands and `benchmark.py`. These are the hot paths agents hit on
//! every question, so they get the most performance attention.
//!
//! NOTE: upstream does NOT use rapidfuzz here — seed scoring is hand-tiered
//! (exact/prefix/substring x IDF) over a trigram inverted index. See
//! HANDOFF.md § "Query engine" for the exact constants and algorithm.

pub mod affected;
pub mod benchmark;
pub mod path_classification;
pub mod paths;
pub mod prs;
pub mod query;
pub mod querylog;

pub use affected::{affected, resolve_seed};
pub use benchmark::{
    benchmark_graph, query_subgraph_tokens, render_benchmark, run_benchmark, BenchmarkQuestion,
    BenchmarkResult, SAMPLE_QUESTIONS,
};
pub use path_classification::{disambiguate_ambiguous_candidates, is_test_path};
pub use paths::{explain, explain_with_overlay, shortest_path};
pub use query::{
    bfs, communities_from_graph, community_header, compute_idf, cut_lines_to_budget, dfs,
    filter_graph_by_context, find_node, find_node_full_scan, god_nodes, graph_file_key,
    has_chinese, infer_context_filters, load_graph, load_graph_with_cap, node_search_text,
    normalize_context_filters, pick_seeds, query_graph, query_graph_dfs, query_graph_dfs_filtered,
    query_graph_filtered, query_graph_text, query_graph_text_with_cache,
    query_graph_text_with_score_observer, query_terms, query_terms_with_chinese_segmenter,
    resolve_context_filters, score_nodes, score_query, score_query_full_scan, search_tokens,
    subgraph_to_text, trigram_candidates, trigram_candidates_with_guard, trigrams,
    ChineseSegmenter, GraphIndex, GraphQueryCache, QueryScores, TrigramIndex, EXACT_MATCH_BONUS,
    PREFIX_MATCH_BONUS, SOURCE_MATCH_BONUS, SUBSTRING_MATCH_BONUS,
};
pub use querylog::{
    config_from_env as query_log_config_from_env,
    config_from_values as query_log_config_from_values, log_query, log_query_from_env,
    nodes_from_result, QueryLogConfig, QueryLogRecord,
};
