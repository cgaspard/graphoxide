// One explicit integration-test binary keeps Export's contract tests from paying
// a linker invocation per source file. Keep each module file in place so its
// fixtures and upstream provenance remain unchanged.
#[path = "direct_taxonomy.rs"]
mod direct_taxonomy;
#[path = "upstream_callflow_html.rs"]
mod upstream_callflow_html;
#[path = "upstream_confidence.rs"]
mod upstream_confidence;
#[path = "upstream_export.rs"]
mod upstream_export;
#[path = "upstream_falkordb_integration.rs"]
mod upstream_falkordb_integration;
#[path = "upstream_graph_community_markdown.rs"]
mod upstream_graph_community_markdown;
#[path = "upstream_graph_community_topics.rs"]
mod upstream_graph_community_topics;
#[path = "upstream_hypergraph.rs"]
mod upstream_hypergraph;
#[path = "upstream_obsidian_dangling_member.rs"]
mod upstream_obsidian_dangling_member;
#[path = "upstream_obsidian_filename_cap.rs"]
mod upstream_obsidian_filename_cap;
#[path = "upstream_report.rs"]
mod upstream_report;
#[path = "upstream_semantic_similarity.rs"]
mod upstream_semantic_similarity;
