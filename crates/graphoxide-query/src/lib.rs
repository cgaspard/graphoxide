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
pub mod paths;
pub mod query;

pub use affected::affected;
pub use paths::{explain, shortest_path};
pub use query::{god_nodes, query_graph, query_graph_dfs, GraphIndex};
