//! Graph construction and analysis.
//!
//! Port of upstream `build.py` (merge extractions into one graph), `dedup.py` /
//! `_minhash.py` (near-duplicate node folding), `cluster.py` (community
//! detection), and `analyze.py` (god nodes, surprises, questions).

pub mod analyze;
pub mod build;
pub mod cluster;
pub mod dedup;

pub use analyze::{analyze, Analysis};
pub use build::build_graph;
pub use cluster::cluster;
pub use dedup::deduplicate;
