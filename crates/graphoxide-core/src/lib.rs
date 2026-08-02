//! Shared data model for graphoxide: the extraction schema, the knowledge graph,
//! node ID construction, and input validation.
//!
//! Mirrors the upstream Python modules `ids.py`, `validate.py`, `security.py`,
//! and the graph.json schema produced by `build.py` / consumed by `export.py`.

pub mod ids;
pub mod io;
pub mod model;
pub mod security;
pub mod validate;

pub use ids::{make_id, node_id, normalize_id};
pub use io::{read_graph, replace_file, write_graph_atomic, write_raw_extractions_atomic};
pub use model::{Confidence, Edge, Extraction, KnowledgeGraph, Node};
pub use security::sanitize_label;
