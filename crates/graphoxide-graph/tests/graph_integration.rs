// One explicit integration-test binary keeps Graph's contract tests from paying
// a linker invocation per source file. Keep each module file in place so its
// fixtures and upstream provenance remain unchanged.
#[path = "raw_dedupe.rs"]
mod raw_dedupe;
#[path = "streaming.rs"]
mod streaming;
#[path = "upstream_analyze.rs"]
mod upstream_analyze;
#[path = "upstream_build.rs"]
mod upstream_build;
#[path = "upstream_build_merge_hyperedges_and_prune.rs"]
mod upstream_build_merge_hyperedges_and_prune;
#[path = "upstream_cluster.rs"]
mod upstream_cluster;
#[path = "upstream_community_hub_labels.rs"]
mod upstream_community_hub_labels;
#[path = "upstream_corrupt_graph_json.rs"]
mod upstream_corrupt_graph_json;
#[path = "upstream_dedup.rs"]
mod upstream_dedup;
#[path = "upstream_extraction_spec_ids.rs"]
mod upstream_extraction_spec_ids;
#[path = "upstream_file_label_disambiguation.rs"]
mod upstream_file_label_disambiguation;
#[path = "upstream_global_graph.rs"]
mod upstream_global_graph;
#[path = "upstream_label_retry.rs"]
mod upstream_label_retry;
#[path = "upstream_labeling.rs"]
mod upstream_labeling;
#[path = "upstream_minhash.rs"]
mod upstream_minhash;
#[path = "upstream_multigraph_diagnostics.rs"]
mod upstream_multigraph_diagnostics;
#[path = "upstream_non_string_node_ids.rs"]
mod upstream_non_string_node_ids;
#[path = "upstream_semantic_id_remap_root.rs"]
mod upstream_semantic_id_remap_root;
#[path = "upstream_semantic_similarity.rs"]
mod upstream_semantic_similarity;
#[path = "upstream_swift_builtin_noise.rs"]
mod upstream_swift_builtin_noise;
