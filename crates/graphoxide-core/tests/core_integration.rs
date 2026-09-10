// One explicit integration-test binary keeps Core's contract tests from paying
// a linker invocation per source file. Keep each module file in place so its
// fixtures and upstream provenance remain unchanged.
#[path = "upstream_atomic_writes.rs"]
mod upstream_atomic_writes;
#[path = "upstream_file_slice.rs"]
mod upstream_file_slice;
#[path = "upstream_id_normalization_contract.rs"]
mod upstream_id_normalization_contract;
#[path = "upstream_ingest.rs"]
mod upstream_ingest;
#[path = "upstream_llm_parser.rs"]
mod upstream_llm_parser;
#[path = "upstream_merge_chunks_validation.rs"]
mod upstream_merge_chunks_validation;
#[path = "upstream_reflect.rs"]
mod upstream_reflect;
#[path = "upstream_security.rs"]
mod upstream_security;
#[path = "upstream_semantic_cleanup.rs"]
mod upstream_semantic_cleanup;
#[path = "upstream_semantic_fragment_sanitize.rs"]
mod upstream_semantic_fragment_sanitize;
#[path = "upstream_validate.rs"]
mod upstream_validate;
