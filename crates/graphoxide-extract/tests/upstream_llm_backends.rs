use graphoxide_core::{expand_oversized_files, FileUnit, FILE_CHAR_CAP};
use graphoxide_extract::llm::{
    build_azure_request_plan, build_direct_extraction_plan, build_direct_extraction_plan_paths,
    build_openai_request_plan, builtin_provider_configs, detect_backend, estimate_cost,
    extraction_system_prompt, get_provider_api_key, model_requires_default_temperature,
    parse_claude_cli_output, parse_response_fragment, provider_configs_from_environment,
    resolve_client_options, resolve_corpus_concurrency, resolve_max_retries,
    resolve_ollama_base_url, resolve_output_cap, resolve_temperature, response_is_hollow,
};
use graphoxide_extract::semantic_pipeline::{
    extract_with_adaptive_retry, looks_like_context_exceeded, SemanticChunkResult,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};
use tempfile::TempDir;

fn env(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

#[test]
fn test_resolve_ollama_base_url_prefers_base_url() {
    let environment = env(&[
        ("OLLAMA_BASE_URL", "custom-base-url"),
        ("OLLAMA_HOST", "ignored-host:11434"),
    ]);
    assert_eq!(
        resolve_ollama_base_url("default-url", &environment),
        "custom-base-url"
    );
}

#[test]
fn test_resolve_ollama_base_url_normalizes_host_without_scheme() {
    assert_eq!(
        resolve_ollama_base_url("default-url", &env(&[("OLLAMA_HOST", "myhost:11434")])),
        "http://myhost:11434/v1"
    );
}

#[test]
fn test_resolve_ollama_base_url_preserves_normalized_host() {
    assert_eq!(
        resolve_ollama_base_url(
            "default-url",
            &env(&[("OLLAMA_HOST", "https://myhost:11434/v1")])
        ),
        "https://myhost:11434/v1"
    );
}

#[test]
fn test_resolve_ollama_base_url_returns_default_without_env() {
    assert_eq!(
        resolve_ollama_base_url("default-url", &BTreeMap::new()),
        "default-url"
    );
}

#[test]
fn test_gemini_accepts_gemini_api_key() {
    let providers = builtin_provider_configs();
    let environment = env(&[("GEMINI_API_KEY", "gemini-key")]);
    assert_eq!(
        detect_backend(&providers, &environment).as_deref(),
        Some("gemini")
    );
    assert_eq!(
        get_provider_api_key("gemini", &providers, &environment),
        Some("gemini-key")
    );
}

#[test]
fn test_gemini_accepts_google_api_key() {
    let providers = builtin_provider_configs();
    let environment = env(&[("GOOGLE_API_KEY", "google-key")]);
    assert_eq!(
        detect_backend(&providers, &environment).as_deref(),
        Some("gemini")
    );
    assert_eq!(
        get_provider_api_key("gemini", &providers, &environment),
        Some("google-key")
    );
}

#[test]
fn test_backend_detection_prefers_gemini() {
    let providers = builtin_provider_configs();
    let environment = env(&[
        ("OPENAI_API_KEY", "openai-key"),
        ("ANTHROPIC_API_KEY", "anthropic-key"),
        ("MOONSHOT_API_KEY", "moonshot-key"),
        ("GEMINI_API_KEY", "gemini-key"),
    ]);
    assert_eq!(
        detect_backend(&providers, &environment).as_deref(),
        Some("gemini")
    );
}

#[test]
fn test_openai_backend_detected() {
    let providers = builtin_provider_configs();
    let environment = env(&[("OPENAI_API_KEY", "openai-key")]);
    assert_eq!(
        detect_backend(&providers, &environment).as_deref(),
        Some("openai")
    );
    assert_eq!(
        get_provider_api_key("openai", &providers, &environment),
        Some("openai-key")
    );
}

#[test]
fn test_extract_files_direct_routes_gemini_through_openai_compat() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("note.md");
    fs::write(&source, "# Architecture\n\nThe runner emits a snapshot.\n").unwrap();
    let plan = build_direct_extraction_plan_paths(
        std::slice::from_ref(&source),
        "gemini",
        temp.path(),
        &env(&[("GOOGLE_API_KEY", "google-key")]),
        false,
    )
    .unwrap();
    assert_eq!(
        plan.base_url,
        "https://generativelanguage.googleapis.com/v1beta/openai/"
    );
    assert_eq!(plan.api_key, "google-key");
    assert_eq!(plan.request.model, "gemini-3-flash-preview");
    assert!(plan
        .request
        .user
        .contains("<untrusted_source path=\"note.md\" sha256="));
    assert!(plan
        .request
        .user
        .contains("# Architecture\n\nThe runner emits a snapshot."));
    assert!(plan.request.user.ends_with("</untrusted_source>"));
    assert_eq!(plan.request.temperature, Some(0.0));
    assert_eq!(plan.request.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(plan.request.max_completion_tokens, 16_384);
}

#[test]
fn test_openai_compat_backends_resolve_full_output_cap() {
    let providers = builtin_provider_configs();
    for backend in ["ollama", "deepseek", "openai", "kimi"] {
        assert_eq!(
            resolve_output_cap(&providers[backend], &BTreeMap::new()),
            16_384
        );
    }
}

#[test]
fn test_gemini_model_can_be_overridden_by_env() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("note.md");
    fs::write(&source, "# Architecture\n").unwrap();
    let plan = build_direct_extraction_plan_paths(
        &[source],
        "gemini",
        temp.path(),
        &env(&[
            ("GOOGLE_API_KEY", "google-key"),
            ("GRAPHIFY_GEMINI_MODEL", "gemini-3.1-pro-preview"),
        ]),
        false,
    )
    .unwrap();
    assert_eq!(plan.request.model, "gemini-3.1-pro-preview");
}

#[test]
fn test_missing_gemini_key_names_both_supported_env_vars() {
    let error = build_direct_extraction_plan_paths(
        &["missing.md"],
        "gemini",
        std::path::Path::new("."),
        &BTreeMap::new(),
        false,
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("GEMINI_API_KEY or GOOGLE_API_KEY"));
}

#[test]
fn test_extract_files_direct_accepts_str_paths() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("note.md");
    fs::write(&source, "# Architecture\n").unwrap();
    let source_string = source.to_string_lossy().into_owned();
    let plan = build_direct_extraction_plan_paths(
        &[source_string.as_str()],
        "gemini",
        temp.path(),
        &env(&[("GOOGLE_API_KEY", "google-key")]),
        false,
    )
    .unwrap();
    assert_eq!(plan.files, [source]);
}

#[test]
fn test_extract_corpus_parallel_accepts_str_and_mixed_paths() {
    let temp = TempDir::new().unwrap();
    let first = temp.path().join("a.md");
    let second = temp.path().join("b.md");
    fs::write(&first, "# A\n").unwrap();
    fs::write(&second, "# B\n").unwrap();
    let units = vec![
        FileUnit::Path(first.clone()),
        FileUnit::Path(second.clone()),
    ];
    let plan = build_direct_extraction_plan(
        &units,
        "gemini",
        temp.path(),
        &env(&[("GOOGLE_API_KEY", "google-key")]),
        false,
    )
    .unwrap();
    assert_eq!(plan.files, [first, second]);
    assert!(plan.request.user.contains("path=\"a.md\""));
    assert!(plan.request.user.contains("path=\"b.md\""));
}

#[test]
fn test_corpus_parallel_oversized_markdown_does_not_crash_on_fileslice() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("big.md");
    fs::write(&source, "# Section\n\n".repeat(FILE_CHAR_CAP / 4)).unwrap();
    let units = expand_oversized_files(&[source], FILE_CHAR_CAP);
    assert!(units.len() > 1);
    assert!(units.iter().all(|unit| matches!(unit, FileUnit::Slice(_))));
    let plan = build_direct_extraction_plan(
        &units,
        "gemini",
        temp.path(),
        &env(&[("GOOGLE_API_KEY", "google-key")]),
        false,
    )
    .unwrap();
    assert_eq!(plan.files.len(), units.len());
    assert!(plan.request.user.contains("# Section"));
}

#[test]
fn test_str_path_entry_points_handle_edge_cases() {
    let plan = build_direct_extraction_plan_paths::<&str>(
        &[],
        "gemini",
        std::path::Path::new("."),
        &env(&[("GOOGLE_API_KEY", "google-key")]),
        false,
    )
    .unwrap();
    assert!(plan.files.is_empty());
    assert!(plan.request.user.is_empty());
}

#[test]
fn test_looks_like_context_exceeded_matches_common_messages() {
    for message in [
        "Error code: 400 - {'error': 'Context size has been exceeded.'}",
        "n_keep: 22374 >= n_ctx: 4096",
        "context_length_exceeded: This model's maximum context length is 8192 tokens",
        "exceeds the available context size",
        "The prompt is too long for this model.",
    ] {
        assert!(
            looks_like_context_exceeded(&anyhow::anyhow!(message)),
            "{message}"
        );
    }
}

#[test]
fn test_looks_like_context_exceeded_ignores_unrelated_errors() {
    for message in [
        "timeout",
        "rate limit",
        "401 unauthorized",
        "connection refused",
    ] {
        assert!(
            !looks_like_context_exceeded(&anyhow::anyhow!(message)),
            "{message}"
        );
    }
}

fn file_units(temp: &TempDir, count: usize) -> Vec<FileUnit> {
    (0..count)
        .map(|index| {
            let path = temp.path().join(format!("f{index}.md"));
            fs::write(&path, "hello").unwrap();
            FileUnit::Path(path)
        })
        .collect()
}

fn successful_chunk(units: &[FileUnit]) -> SemanticChunkResult {
    SemanticChunkResult {
        nodes: units
            .iter()
            .map(|unit| {
                json!({"id": graphoxide_core::unit_path(unit).file_stem().unwrap().to_string_lossy()})
            })
            .collect(),
        finish_reason: "stop".into(),
        ..SemanticChunkResult::default()
    }
}

#[test]
fn test_adaptive_retry_splits_on_context_exceeded() {
    let temp = TempDir::new().unwrap();
    let units = file_units(&temp, 4);
    let calls = AtomicUsize::new(0);
    let result = extract_with_adaptive_retry(&units, 3, &|chunk| {
        calls.fetch_add(1, Ordering::SeqCst);
        if chunk.len() == 4 {
            anyhow::bail!("Error 400: Context size has been exceeded.");
        }
        Ok(successful_chunk(chunk))
    })
    .unwrap();
    assert_eq!(result.nodes.len(), 4);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn test_adaptive_retry_gives_up_on_single_file_overflow() {
    let temp = TempDir::new().unwrap();
    let units = file_units(&temp, 1);
    let result =
        extract_with_adaptive_retry(&units, 3, &|_| anyhow::bail!("context_length_exceeded"))
            .unwrap();
    assert!(result.nodes.is_empty());
    assert!(result.edges.is_empty());
    assert_eq!(result.finish_reason, "stop");
}

#[test]
fn test_adaptive_retry_re_raises_unrelated_errors() {
    let temp = TempDir::new().unwrap();
    let units = file_units(&temp, 1);
    let error =
        extract_with_adaptive_retry(&units, 3, &|_| anyhow::bail!("rate limit hit")).unwrap_err();
    assert!(error.to_string().contains("rate limit"));
}

#[test]
fn test_response_is_hollow_flags_empty_string() {
    assert!(response_is_hollow(
        Some(""),
        &json!({"nodes": [], "edges": [], "hyperedges": []})
    ));
}

#[test]
fn test_response_is_hollow_flags_none_content() {
    assert!(response_is_hollow(
        None,
        &json!({"nodes": [], "edges": [], "hyperedges": []})
    ));
}

#[test]
fn test_response_is_hollow_flags_whitespace_only() {
    assert!(response_is_hollow(
        Some("   \n\t  "),
        &json!({"nodes": [], "edges": [], "hyperedges": []})
    ));
}

#[test]
fn test_response_is_hollow_flags_parsed_but_no_nodes_or_edges() {
    assert!(response_is_hollow(
        Some(r#"{"sorry":"I cannot"}"#),
        &json!({})
    ));
    assert!(response_is_hollow(
        Some("{}"),
        &json!({"nodes": [], "edges": [], "hyperedges": []})
    ));
}

#[test]
fn test_response_is_hollow_accepts_real_extraction() {
    assert!(!response_is_hollow(
        Some(r#"{"nodes":[{"id":"x"}]}"#),
        &json!({"nodes": [{"id": "x"}], "edges": [], "hyperedges": []})
    ));
    assert!(!response_is_hollow(
        Some(r#"{"edges":[...]}"#),
        &json!({"nodes": [], "edges": [{"source": "a", "target": "b"}], "hyperedges": []})
    ));
}

#[test]
fn test_call_openai_compat_relabels_empty_content_as_length() {
    assert_eq!(
        parse_response_fragment(Some(""), "stop").unwrap().1,
        "length"
    );
}

#[test]
fn test_call_openai_compat_relabels_none_content_as_length() {
    assert_eq!(parse_response_fragment(None, "stop").unwrap().1, "length");
}

#[test]
fn test_call_openai_compat_relabels_unparseable_json_as_length() {
    assert_eq!(
        parse_response_fragment(Some(r#"{"nodes":[{"id":"#), "stop")
            .unwrap()
            .1,
        "length"
    );
}

#[test]
fn test_call_openai_compat_preserves_real_finish_reason() {
    let (fragment, finish) = parse_response_fragment(
        Some(r#"{"nodes":[{"id":"a"}],"edges":[],"hyperedges":[]}"#),
        "stop",
    )
    .unwrap();
    assert_eq!(finish, "stop");
    assert_eq!(fragment["nodes"], json!([{"id": "a"}]));
}

#[test]
fn test_model_requires_default_temperature_vectors() {
    for model in [
        "o1",
        "o1-preview",
        "o1-mini",
        "o3",
        "o3-mini",
        "o4-mini",
        "gpt-5",
        "gpt-5-mini",
        "openai/o3-mini",
    ] {
        assert!(model_requires_default_temperature(model), "{model}");
    }
    for model in [
        "gpt-4.1-mini",
        "gpt-4o",
        "gpt-4.1",
        "kimi-k2.6",
        "deepseek-v4-flash",
        "",
        "o1x",
        "go3",
    ] {
        assert!(!model_requires_default_temperature(model), "{model}");
    }
}

#[test]
fn test_resolve_temperature_default_for_normal_model() {
    assert_eq!(
        resolve_temperature(Some(0.0), "gpt-4.1-mini", None),
        Some(0.0)
    );
}

#[test]
fn test_resolve_temperature_omitted_for_reasoning_model() {
    assert_eq!(resolve_temperature(Some(0.0), "o3-mini", None), None);
    assert_eq!(resolve_temperature(Some(0.0), "gpt-5", None), None);
}

#[test]
fn test_resolve_temperature_env_var_numeric_overrides() {
    assert_eq!(
        resolve_temperature(Some(0.0), "gpt-4.1-mini", Some("0.7")),
        Some(0.7)
    );
    assert_eq!(
        resolve_temperature(Some(0.0), "o3-mini", Some("0.7")),
        Some(0.7)
    );
}

#[test]
fn test_resolve_temperature_env_var_none_omits() {
    assert_eq!(
        resolve_temperature(Some(0.0), "gpt-4.1-mini", Some("none")),
        None
    );
}

#[test]
fn test_resolve_temperature_env_var_invalid_falls_back() {
    assert_eq!(
        resolve_temperature(Some(0.0), "gpt-4.1-mini", Some("hot")),
        Some(0.0)
    );
    assert_eq!(resolve_temperature(Some(0.0), "o3-mini", Some("hot")), None);
}

#[test]
fn test_openai_compat_omits_temperature_for_o3_model() {
    let plan = build_openai_request_plan(
        "https://api.openai.com/v1",
        "o3-mini",
        "u",
        Some(0.0),
        None,
        8_192,
        "openai",
        false,
        None,
        &BTreeMap::new(),
    );
    assert_eq!(plan.model, "o3-mini");
    assert_eq!(plan.temperature, None);
}

#[test]
fn test_openai_compat_sends_temperature_for_normal_model() {
    let plan = build_openai_request_plan(
        "https://api.openai.com/v1",
        "gpt-4.1-mini",
        "u",
        Some(0.0),
        None,
        8_192,
        "openai",
        false,
        None,
        &BTreeMap::new(),
    );
    assert_eq!(plan.temperature, Some(0.0));
}

#[test]
fn test_openai_compat_env_var_temperature_applied() {
    let plan = build_openai_request_plan(
        "https://api.openai.com/v1",
        "gpt-4.1-mini",
        "u",
        Some(0.0),
        None,
        8_192,
        "openai",
        false,
        None,
        &env(&[("GRAPHIFY_LLM_TEMPERATURE", "0.3")]),
    );
    assert_eq!(plan.temperature, Some(0.3));
}

#[test]
fn test_native_extraction_prompt_requests_hyperedges() {
    for deep in [false, true] {
        let prompt = extraction_system_prompt(deep);
        assert!(prompt.to_ascii_lowercase().contains("hyperedge"));
        assert!(prompt.contains("3 or more nodes"));
        assert!(!prompt.contains(r#""hyperedges":[]"#));
        assert!(prompt.contains(r#""nodes":["node_id1""#));
    }
}

#[test]
fn test_native_extraction_prompt_matches_skill_spec_on_hyperedges() {
    assert!(
        extraction_system_prompt(false).contains("3 or more nodes clearly participate together")
    );
}

#[test]
fn test_base_url_env_overrides() {
    for (backend, key, override_url) in [
        ("kimi", "KIMI_BASE_URL", "https://proxy.example/kimi/v1"),
        ("gemini", "GEMINI_BASE_URL", "https://proxy.example/gemini"),
        (
            "deepseek",
            "DEEPSEEK_BASE_URL",
            "https://proxy.example/deepseek",
        ),
    ] {
        let providers = provider_configs_from_environment(&env(&[(key, override_url)]));
        assert_eq!(providers[backend].base_url, override_url);
    }
}

#[test]
fn test_base_url_defaults_without_env() {
    let providers = provider_configs_from_environment(&BTreeMap::new());
    assert_eq!(providers["kimi"].base_url, "https://api.moonshot.ai/v1");
    assert_eq!(
        providers["gemini"].base_url,
        "https://generativelanguage.googleapis.com/v1beta/openai/"
    );
    assert_eq!(providers["deepseek"].base_url, "https://api.deepseek.com");
}

#[test]
fn test_resolve_max_retries_default_and_env() {
    assert!(resolve_max_retries(6, None) >= 5);
    assert_eq!(resolve_max_retries(6, Some("10")), 10);
    assert_eq!(resolve_max_retries(6, Some("0")), 0);
    assert!(resolve_max_retries(6, Some("bogus")) >= 5);
}

#[test]
fn test_ollama_extra_body_sets_num_ctx_and_keep_alive() {
    let plan = build_openai_request_plan(
        "http://localhost:11434/v1",
        "qwen2.5-coder:7b",
        "user msg",
        Some(0.0),
        None,
        8_192,
        "ollama",
        false,
        None,
        &BTreeMap::new(),
    );
    assert!(!plan.stream);
    assert!(
        plan.extra_body.as_ref().unwrap()["options"]["num_ctx"]
            .as_u64()
            .unwrap()
            >= 8_192
    );
    assert_eq!(plan.extra_body.as_ref().unwrap()["keep_alive"], "30m");
    assert_eq!(plan.max_retries, 0);
}

#[test]
fn test_openai_compat_forces_non_streaming_response() {
    let plan = build_openai_request_plan(
        "https://gateway.example/v1",
        "gpt-4.1-mini",
        "u",
        Some(0.0),
        None,
        8_192,
        "openai",
        false,
        None,
        &BTreeMap::new(),
    );
    assert!(!plan.stream);
}

#[test]
fn test_ollama_num_ctx_scales_with_small_token_budget() {
    let plan = build_openai_request_plan(
        "http://localhost:11434/v1",
        "qwen2.5-coder:7b",
        &"x".repeat(32_000),
        Some(0.0),
        None,
        16_384,
        "ollama",
        false,
        None,
        &BTreeMap::new(),
    );
    let num_ctx = plan.extra_body.unwrap()["options"]["num_ctx"]
        .as_u64()
        .unwrap();
    assert!((8_192..131_072).contains(&num_ctx));
}

#[test]
fn test_ollama_num_ctx_env_override() {
    let plan = build_openai_request_plan(
        "http://localhost:11434/v1",
        "m",
        "u",
        Some(0.0),
        None,
        8_192,
        "ollama",
        false,
        None,
        &env(&[("GRAPHIFY_OLLAMA_NUM_CTX", "65536")]),
    );
    assert_eq!(plan.extra_body.unwrap()["options"]["num_ctx"], 65_536);
}

#[test]
fn test_non_ollama_backend_gets_no_num_ctx_extra_body() {
    let plan = build_openai_request_plan(
        "https://api.openai.com/v1",
        "gpt-4.1-mini",
        "u",
        Some(0.0),
        None,
        8_192,
        "openai",
        false,
        None,
        &BTreeMap::new(),
    );
    assert!(plan.extra_body.is_none());
}

#[test]
fn test_explicit_extra_body_precedence() {
    let explicit = json!({"thinking": {"type": "enabled"}});
    let plan = build_openai_request_plan(
        "https://api.moonshot.ai/v1",
        "kimi",
        "u",
        Some(0.0),
        None,
        8_192,
        "kimi",
        false,
        Some(explicit.clone()),
        &env(&[("GRAPHIFY_DISABLE_THINKING", "1")]),
    );
    assert_eq!(plan.extra_body, Some(explicit));
}

#[test]
fn test_call_openai_compat_uses_explicit_extra_body() {
    let explicit = json!({"chat_template_kwargs": {"enable_thinking": false}});
    let plan = build_openai_request_plan(
        "https://kitor.example/vllm/v1",
        "Qwen3.6-27B",
        "u",
        Some(0.0),
        None,
        8_192,
        "kitor-vllm",
        false,
        Some(explicit.clone()),
        &BTreeMap::new(),
    );
    assert_eq!(plan.extra_body, Some(explicit));
}

#[test]
fn test_call_openai_compat_extra_body_wins_over_moonshot_default() {
    let explicit = json!({"thinking": {"type": "enabled"}});
    let plan = build_openai_request_plan(
        "https://api.moonshot.ai/v1",
        "kimi-k2-thinking",
        "u",
        Some(0.0),
        None,
        8_192,
        "kimi",
        false,
        Some(explicit.clone()),
        &BTreeMap::new(),
    );
    assert_eq!(plan.extra_body, Some(explicit));
}

#[test]
fn test_deepseek_thinking_on_by_default() {
    let plan = build_openai_request_plan(
        "https://api.deepseek.com",
        "deepseek-v4-flash",
        "u",
        Some(0.0),
        None,
        8_192,
        "deepseek",
        false,
        None,
        &BTreeMap::new(),
    );
    assert!(plan
        .extra_body
        .is_none_or(|body| body.get("thinking").is_none()));
}

#[test]
fn test_deepseek_thinking_disabled_via_env() {
    let plan = build_openai_request_plan(
        "https://api.deepseek.com",
        "deepseek-v4-flash",
        "u",
        Some(0.0),
        None,
        8_192,
        "deepseek",
        false,
        None,
        &env(&[("GRAPHIFY_DISABLE_THINKING", "1")]),
    );
    assert_eq!(
        plan.extra_body,
        Some(json!({"thinking": {"type": "disabled"}}))
    );
}

#[test]
fn test_explicit_extra_body_wins_over_thinking_env() {
    let explicit = json!({"thinking": {"type": "enabled"}});
    let plan = build_openai_request_plan(
        "https://api.deepseek.com",
        "deepseek-v4-flash",
        "u",
        Some(0.0),
        None,
        8_192,
        "deepseek",
        false,
        Some(explicit.clone()),
        &env(&[("GRAPHIFY_DISABLE_THINKING", "1")]),
    );
    assert_eq!(plan.extra_body, Some(explicit));
}

#[test]
fn test_call_openai_compat_explicit_extra_body_skips_ollama_auto_derive() {
    let explicit = json!({"options": {"num_ctx": 4096}});
    let plan = build_openai_request_plan(
        "http://localhost:11434/v1",
        "qwen2.5-coder:7b",
        "u",
        Some(0.0),
        None,
        8_192,
        "ollama",
        false,
        Some(explicit.clone()),
        &BTreeMap::new(),
    );
    assert_eq!(plan.extra_body, Some(explicit));
}

#[test]
fn test_extract_corpus_parallel_ollama_runs_serially() {
    assert_eq!(resolve_corpus_concurrency("ollama", 4, &BTreeMap::new()), 1);
}

#[test]
fn test_extract_corpus_parallel_ollama_parallel_env_restores_concurrency() {
    assert_eq!(
        resolve_corpus_concurrency("ollama", 4, &env(&[("GRAPHIFY_OLLAMA_PARALLEL", "1")])),
        4
    );
}

#[test]
fn test_adaptive_retry_bisects_on_hollow_ollama_response() {
    let temp = TempDir::new().unwrap();
    let units = file_units(&temp, 4);
    let calls = AtomicUsize::new(0);
    let result = extract_with_adaptive_retry(&units, 3, &|chunk| {
        calls.fetch_add(1, Ordering::SeqCst);
        if chunk.len() == 4 {
            Ok(SemanticChunkResult {
                input_tokens: 100,
                finish_reason: "length".into(),
                ..SemanticChunkResult::default()
            })
        } else {
            Ok(successful_chunk(chunk))
        }
    })
    .unwrap();
    assert_eq!(result.nodes.len(), 4);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn test_deepseek_thinking_toggle() {
    let build = |environment: &BTreeMap<String, String>| {
        build_openai_request_plan(
            "https://api.deepseek.com",
            "deepseek-v4-flash",
            "u",
            Some(0.0),
            None,
            8_192,
            "deepseek",
            false,
            None,
            environment,
        )
    };
    assert!(build(&BTreeMap::new()).extra_body.is_none());
    assert_eq!(
        build(&env(&[("GRAPHIFY_DISABLE_THINKING", "1")])).extra_body,
        Some(json!({"thinking": {"type": "disabled"}}))
    );
}

#[test]
fn test_detect_backend_azure_and_cost() {
    let providers = builtin_provider_configs();
    let both = env(&[
        ("AZURE_OPENAI_API_KEY", "azure-key"),
        (
            "AZURE_OPENAI_ENDPOINT",
            "https://my-resource.openai.azure.com/",
        ),
    ]);
    assert_eq!(detect_backend(&providers, &both).as_deref(), Some("azure"));
    assert_ne!(
        detect_backend(&providers, &env(&[("AZURE_OPENAI_API_KEY", "azure-key")])).as_deref(),
        Some("azure")
    );
    assert_eq!(estimate_cost(&providers["azure"], 1_000_000, 500_000), 7.5);
}

#[test]
fn test_call_azure_uses_correct_client_params_and_max_completion_tokens() {
    let plan = build_azure_request_plan(
        "https://my-resource.openai.azure.com/",
        "gpt-4o",
        "test",
        16_384,
        &env(&[("AZURE_OPENAI_API_VERSION", "2024-08-01-preview")]),
    );
    assert_eq!(plan.endpoint, "https://my-resource.openai.azure.com/");
    assert_eq!(plan.api_version, "2024-08-01-preview");
    assert_eq!(plan.max_completion_tokens, 16_384);
}

#[test]
fn test_detect_backend_returns_azure_when_both_vars_set() {
    let providers = builtin_provider_configs();
    let environment = env(&[
        ("AZURE_OPENAI_API_KEY", "azure-key"),
        (
            "AZURE_OPENAI_ENDPOINT",
            "https://my-resource.openai.azure.com/",
        ),
    ]);
    assert_eq!(
        detect_backend(&providers, &environment).as_deref(),
        Some("azure")
    );
    assert_eq!(
        get_provider_api_key("azure", &providers, &environment),
        Some("azure-key")
    );
}

#[test]
fn test_detect_backend_azure_requires_endpoint_not_just_key() {
    let providers = builtin_provider_configs();
    assert_ne!(
        detect_backend(&providers, &env(&[("AZURE_OPENAI_API_KEY", "azure-key")])).as_deref(),
        Some("azure")
    );
}

#[test]
fn test_estimate_cost_azure_no_keyerror() {
    let providers = builtin_provider_configs();
    assert_eq!(estimate_cost(&providers["azure"], 1_000_000, 500_000), 7.5);
}

#[test]
fn test_call_claude_cli_passes_errors_replace_to_subprocess() {
    let envelope = br#"{"type":"result","result":"{\"nodes\":[],\"edges\":[],\"hyperedges\":[]}"}"#;
    let parsed = parse_claude_cli_output(0, envelope, &[]).unwrap();
    assert_eq!(parsed["nodes"], json!([]));
}

#[test]
fn test_call_claude_cli_tolerates_non_utf8_in_stderr() {
    let error = parse_claude_cli_output(1, &[], b"GBK error: \xff\xfe")
        .unwrap_err()
        .to_string();
    assert!(error.contains("claude -p exited 1"));
    assert!(error.contains('\u{fffd}'));
}

#[test]
fn test_openai_compat_client_built_with_retries() {
    assert!(resolve_client_options(&BTreeMap::new()).max_retries >= 5);
}

#[test]
fn test_call_llm_claude_client_built_with_timeout_and_retries() {
    let options = resolve_client_options(&env(&[("GRAPHIFY_API_TIMEOUT", "1")]));
    assert_eq!(options.timeout_seconds, 1.0);
    assert!(options.max_retries >= 5);
}

#[test]
fn test_call_llm_openai_compat_client_built_with_timeout_and_retries() {
    let options = resolve_client_options(&env(&[("GRAPHIFY_API_TIMEOUT", "1")]));
    assert_eq!(options.timeout_seconds, 1.0);
    assert!(options.max_retries >= 5);
}
