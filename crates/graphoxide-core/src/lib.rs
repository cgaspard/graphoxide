//! Shared data model for graphoxide: the extraction schema, the knowledge graph,
//! node ID construction, and input validation.
//!
//! Mirrors the upstream Python modules `ids.py`, `validate.py`, `security.py`,
//! and the graph.json schema produced by `build.py` / consumed by `export.py`.

pub mod file_slice;
pub mod ids;
pub mod io;
pub mod jsonc;
pub mod mcp_config;
pub mod model;
pub mod reflect;
pub mod security;
pub mod semantic;
pub mod validate;

pub use file_slice::{
    bisect_slice, estimate_file_tokens, estimate_file_tokens_with, expand_oversized_files,
    is_splittable_text, is_vision_image, pack_chunks_by_tokens, partition_semantic_files,
    read_files_prompt, read_slice_text, slice_boundaries, try_pack_chunks_by_tokens, unit_path,
    FileSlice, FileUnit, CHARS_PER_TOKEN, FILE_CHAR_CAP, IMAGE_TOKEN_ESTIMATE,
    MAX_IMAGES_PER_CHUNK, PER_FILE_OVERHEAD_CHARS,
};
pub use ids::{make_id, node_id, normalize_id};
pub use io::{
    check_graph_file_size_cap, check_graph_file_size_cap_with, max_graph_bytes,
    parse_max_graph_bytes, permission_fallback, read_graph, read_graph_capped, read_graph_with_cap,
    read_json_object, read_json_object_with_cap, replace_file, write_graph_atomic,
    write_json_atomic, write_raw_extractions_atomic, write_text_atomic,
    write_text_atomic_with_replacer, CappedGraphRead, DEFAULT_MAX_GRAPH_BYTES,
};
pub use jsonc::{parse_jsonc, parse_jsonc_slice};
pub use mcp_config::{is_mcp_config_path, mcp_server_map, MCP_CONFIG_FILENAMES};
pub use model::{
    coerce_non_string_ids, normalize_graph_value, Confidence, Edge, Extraction, KnowledgeGraph,
    Node, CONTAINER_SOURCE_ATTRIBUTE,
};
pub use reflect::{
    aggregate_lessons, build_learning_overlay, lessons_fresh, load_learning_overlay,
    load_memory_docs, parse_memory_doc, reflect, render_lessons_md, save_query_result,
    write_learning_sidecar, ContestedLesson, CorrectionLesson, DeadEndLesson, LearningEntry,
    LearningProvenance, LearningSidecar, LessonAggregate, LessonBucket, MemoryDoc, OutcomeCounts,
    ReflectOptions, SaveResultOptions, SourceLesson, LEARNING_SIDECAR_NAME,
};
pub use security::{
    decode_utf8_lossy, ensure_success_status, read_limited, safe_fetch, safe_fetch_text,
    sanitize_label, sanitize_metadata, sanitize_metadata_string, sanitize_metadata_value,
    sanitize_optional_label, validate_graph_path, validate_graph_path_with_output_name,
    validate_url, METADATA_MAX_LIST_ITEMS, METADATA_MAX_VALUE_LEN,
};
pub use semantic::{
    load_validated_semantic_fragment, load_validated_semantic_fragment_with_limits,
    merge_semantic_chunk_files, parse_llm_json, sanitize_fragment_shape,
    sanitize_semantic_fragment, validate_semantic_fragment, validate_semantic_fragment_with_limits,
    ChunkMergeReport, SemanticFragmentLimits, SkippedChunk,
};
