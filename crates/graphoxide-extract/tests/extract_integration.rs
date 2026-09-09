// One explicit integration-test binary keeps Extract's contract tests from
// paying a linker invocation per source file. Keep each module file in place
// so its fixtures and upstream provenance remain unchanged.
#[path = "coverage_report.rs"]
mod coverage_report;
#[path = "cross_language_identity.rs"]
mod cross_language_identity;
#[path = "deferred_project_manifest.rs"]
mod deferred_project_manifest;
#[path = "dot_semantic.rs"]
mod dot_semantic;
#[path = "dotnet_path_portability.rs"]
mod dotnet_path_portability;
#[path = "evidence_locators.rs"]
mod evidence_locators;
#[path = "format_capability_truth.rs"]
mod format_capability_truth;
#[path = "json_config_gating.rs"]
mod json_config_gating;
#[path = "language_parity.rs"]
mod language_parity;
#[path = "managed_output_dir.rs"]
mod managed_output_dir;
#[path = "mcp_config_resilience.rs"]
mod mcp_config_resilience;
#[path = "office_semantic.rs"]
mod office_semantic;
#[path = "pdf_semantic.rs"]
mod pdf_semantic;
#[path = "registry_state.rs"]
mod registry_state;
#[path = "registry_v1.rs"]
mod registry_v1;
#[path = "resolution_path_classification.rs"]
mod resolution_path_classification;
#[path = "upstream_adversarial_resolution.rs"]
mod upstream_adversarial_resolution;
#[path = "upstream_anthropic_custom_endpoint.rs"]
mod upstream_anthropic_custom_endpoint;
#[path = "upstream_backend_extras.rs"]
mod upstream_backend_extras;
#[path = "upstream_cache.rs"]
mod upstream_cache;
#[path = "upstream_cargo_introspect.rs"]
mod upstream_cargo_introspect;
#[path = "upstream_charmap_encoding.rs"]
mod upstream_charmap_encoding;
#[path = "upstream_chunking.rs"]
mod upstream_chunking;
#[path = "upstream_claude_cli_backend.rs"]
mod upstream_claude_cli_backend;
#[path = "upstream_cpp_objc_cross_file_calls.rs"]
mod upstream_cpp_objc_cross_file_calls;
#[path = "upstream_cpp_preprocess.rs"]
mod upstream_cpp_preprocess;
#[path = "upstream_cross_extension_reexport_self_cycle.rs"]
mod upstream_cross_extension_reexport_self_cycle;
#[path = "upstream_cross_language_call_resolution.rs"]
mod upstream_cross_language_call_resolution;
#[path = "upstream_csharp_member_calls.rs"]
mod upstream_csharp_member_calls;
#[path = "upstream_csharp_partial_classes.rs"]
mod upstream_csharp_partial_classes;
#[path = "upstream_csharp_type_resolution.rs"]
mod upstream_csharp_type_resolution;
#[path = "upstream_dart.rs"]
mod upstream_dart;
#[path = "upstream_detect.rs"]
mod upstream_detect;
#[path = "upstream_dotnet.rs"]
mod upstream_dotnet;
#[path = "upstream_evidence_binding.rs"]
mod upstream_evidence_binding;
#[path = "upstream_extract.rs"]
mod upstream_extract;
#[path = "upstream_extract_cache_location.rs"]
mod upstream_extract_cache_location;
#[path = "upstream_file_node_id_spec.rs"]
mod upstream_file_node_id_spec;
#[path = "upstream_image_vision.rs"]
mod upstream_image_vision;
#[path = "upstream_import_extension_resolution.rs"]
mod upstream_import_extension_resolution;
#[path = "upstream_indirect_dispatch.rs"]
mod upstream_indirect_dispatch;
#[path = "upstream_java_resolution.rs"]
mod upstream_java_resolution;
#[path = "upstream_js_import_resolution.rs"]
mod upstream_js_import_resolution;
#[path = "upstream_js_language_resolution.rs"]
mod upstream_js_language_resolution;
#[path = "upstream_jsconfig_baseurl.rs"]
mod upstream_jsconfig_baseurl;
#[path = "upstream_kotlin_object_literal.rs"]
mod upstream_kotlin_object_literal;
#[path = "upstream_language_mapping.rs"]
mod upstream_language_mapping;
#[path = "upstream_llm_backends.rs"]
mod upstream_llm_backends;
#[path = "upstream_llm_parser_cli.rs"]
mod upstream_llm_parser_cli;
#[path = "upstream_long_path_hashing.rs"]
mod upstream_long_path_hashing;
#[path = "upstream_manifest_ingest.rs"]
mod upstream_manifest_ingest;
#[path = "upstream_module_extensions.rs"]
mod upstream_module_extensions;
#[path = "upstream_multilang.rs"]
mod upstream_multilang;
#[path = "upstream_node_id_canonical.rs"]
mod upstream_node_id_canonical;
#[path = "upstream_office_incremental.rs"]
mod upstream_office_incremental;
#[path = "upstream_office_limits.rs"]
mod upstream_office_limits;
#[path = "upstream_ollama.rs"]
mod upstream_ollama;
#[path = "upstream_ollama_retry_cap.rs"]
mod upstream_ollama_retry_cap;
#[path = "upstream_openai_custom_endpoint.rs"]
mod upstream_openai_custom_endpoint;
#[path = "upstream_partial_cache.rs"]
mod upstream_partial_cache;
#[path = "upstream_pascal.rs"]
mod upstream_pascal;
#[path = "upstream_pascal_call_scoping.rs"]
mod upstream_pascal_call_scoping;
#[path = "upstream_pascal_resolution.rs"]
mod upstream_pascal_resolution;
#[path = "upstream_pg_introspect.rs"]
mod upstream_pg_introspect;
#[path = "upstream_php_type_resolution.rs"]
mod upstream_php_type_resolution;
#[path = "upstream_provider_registry.rs"]
mod upstream_provider_registry;
#[path = "upstream_python_decorators.rs"]
mod upstream_python_decorators;
#[path = "upstream_python_import_resolution.rs"]
mod upstream_python_import_resolution;
#[path = "upstream_rationale.rs"]
mod upstream_rationale;
#[path = "upstream_registry_terraform.rs"]
mod upstream_registry_terraform;
#[path = "upstream_ruby_resolution.rs"]
mod upstream_ruby_resolution;
#[path = "upstream_scala_self_type.rs"]
mod upstream_scala_self_type;
#[path = "upstream_scip_ingest.rs"]
mod upstream_scip_ingest;
#[path = "upstream_semantic_cache_out_root.rs"]
mod upstream_semantic_cache_out_root;
#[path = "upstream_sfc_extraction.rs"]
mod upstream_sfc_extraction;
#[path = "upstream_stale_prune.rs"]
mod upstream_stale_prune;
#[path = "upstream_stat_index_portability.rs"]
mod upstream_stat_index_portability;
#[path = "upstream_static_module_imports.rs"]
mod upstream_static_module_imports;
#[path = "upstream_swift_resolution.rs"]
mod upstream_swift_resolution;
#[path = "upstream_symbol_resolution.rs"]
mod upstream_symbol_resolution;
#[path = "upstream_ts_language_features.rs"]
mod upstream_ts_language_features;
#[path = "upstream_zero_node_no_cache.rs"]
mod upstream_zero_node_no_cache;
