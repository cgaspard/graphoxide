//! Graph construction and analysis.
//!
//! Port of upstream `build.py` (merge extractions into one graph), `dedup.py` /
//! `_minhash.py` (near-duplicate node folding), `cluster.py` (community
//! detection), and `analyze.py` (god nodes, surprises, questions).

pub mod analyze;
pub mod build;
pub mod cluster;
pub mod dedup;
pub mod diagnostics;
pub mod enrichment;
pub mod global_graph;
pub mod incremental;
pub mod labeling;
pub mod merge_repos;
pub mod minhash;
pub mod multigraph_compat;
mod provenance;
pub mod raw;
pub mod streaming;

pub use analyze::{
    analyze, file_category, find_import_cycles, god_nodes, graph_diff, is_concept_node,
    is_json_key_node, suggest_questions, surprise_score, surprising_connections, Analysis,
    DiffEdge, DiffNode, GodNode, GraphDiff, ImportCycle, SuggestedQuestion, Surprise,
};
pub use build::{
    attach_hyperedges, build_graph, build_graph_with_options, build_graph_with_options_and_root,
    build_graph_with_report, build_graph_with_report_and_options,
    build_graph_with_report_and_options_and_root,
    build_graph_with_report_and_options_and_root_with_callback, build_graph_with_report_and_root,
    build_graph_with_root, disambiguate_file_labels_in_extractions,
    disambiguate_file_labels_in_nodes, graph_has_legacy_ids, is_file_node_label, semantic_id_remap,
    shortest_unique_suffix, source_file_stem, BuildOptions, BuildReport, BuildSubStage,
    BuildSubStageCallback, EdgeDropReason,
    EdgeRepairReason, HyperedgeDropReason, HyperedgeRepairReason, NodeDropReason, NodeMergeReason,
};
pub use cluster::{
    cluster, cohesion_score, communities, community_member_sigs, label_communities_by_hub,
    remap_communities_to_previous, remap_community_map, score_all,
};
pub use dedup::{
    deduplicate, deduplicate_entities, defines_id, is_variant_pair, label_entropy,
    normalized_label, numeric_tokens_differ, shingles, short_label_blocked, DedupDiagnostic,
    DedupDiagnosticLevel, EntityDeduplicationReport,
};
pub use diagnostics::{
    diagnose_extraction, diagnose_file, diagnose_file_with_cap, format_diagnostic_json,
    format_diagnostic_report, scan_producer_suppression_sites, DiagnosticOptions,
    MultigraphDiagnosticSummary, ProducerSuppression, SameEndpointExample, SuppressionSite,
};
pub use enrichment::{
    apply_media_transcript_summaries, is_media_inventory_node, media_transcript_summary_id,
    EnrichmentApplyError, EnrichmentApplyReport, MediaTranscriptSummaryRecord,
    ENRICHMENT_DATA_BOUNDARY, ENRICHMENT_SCHEMA_VERSION, MAX_ENRICHMENT_MODEL_BYTES,
    MAX_ENRICHMENT_SOURCE_BYTES, MAX_ENRICHMENT_SUMMARY_BYTES, MAX_ENRICHMENT_TOPICS,
    MAX_ENRICHMENT_TOPIC_BYTES, MEDIA_TRANSCRIPT_SUMMARY_PROFILE, REDACTION_VERSION,
};
pub use global_graph::{
    prefix_graph_for_global, prune_repo_from_graph, GlobalAddResult, GlobalGraphStore,
    GlobalRepoRecord,
};
pub use incremental::{
    build_merge, build_merge_with_options, infer_merge_root, is_ast_tier_value,
    merge_raw_extraction, IncrementalOptions,
};
pub use labeling::{
    community_label_lines, generate_community_labels_with, label_communities_with,
    parse_label_response, placeholder_community_labels, GeneratedLabels, LabelRequest,
    LabelResponse, LabelSource, LabelUsage, LabelingError, LabelingOptions, DEFAULT_BATCH_SIZE,
    DEFAULT_TOP_K,
};
pub use merge_repos::{distinct_repo_tags, merge_repository_graphs};
pub use minhash::{optimal_lsh_params, MinHash, MinHashLsh};
pub use multigraph_compat::{
    probe_multigraph_capabilities, require_multigraph_capabilities, CapabilityCheck,
    MultigraphCapabilityResult,
};
pub use provenance::origin_is_structural;
pub use raw::{
    build_graph_from_value, canonicalize_extraction, dedupe_raw_edges, dedupe_raw_extractions,
    dedupe_raw_nodes, edge_data, edge_datas, IngestReport,
};
pub use streaming::{
    build_graph_from_fact_batches, build_graph_from_fact_batches_with_root, sort_fact_batches,
    ClusterResourceError, ClusterResourceLimits, FactBatch, FactBatchError, FactBatchKey,
    FactBatchLimits, FactBatchMergeLimits, FactBatchOrderError, FactBatchRunBuilder,
    FactBatchRunError, FactBatchRunLimits, FactBatchRunStore, FactBatchRunStoreError, FactKind,
    MergedFactBatchRuns, OrderedFactBatchRun, StagedGraphOutput, DEFAULT_FACT_BATCH_MAX_BYTES,
    DEFAULT_FACT_BATCH_MAX_FACTS, DEFAULT_FACT_MATERIALIZATION_MAX_BYTES,
    DEFAULT_FACT_RUN_MAX_BATCHES, DEFAULT_FACT_RUN_MAX_BYTES,
};
