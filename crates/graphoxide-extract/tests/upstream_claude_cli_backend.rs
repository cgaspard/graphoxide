use graphoxide_core::FileUnit;
use graphoxide_extract::llm::{
    build_claude_cli_file_request, build_claude_cli_request, build_claude_cli_request_with_options,
    builtin_provider_configs, estimate_cost, parse_claude_cli_response, resolve_claude_cli_command,
    resolve_client_options, ClaudeSchemaSupportCache,
};
use serde_json::{json, Value};
use std::{cell::Cell, collections::BTreeMap, fs};
use tempfile::TempDir;

fn env(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

fn fragment() -> Value {
    json!({
        "nodes": [
            {"id":"foo_module","label":"Foo","file_type":"document","source_file":"foo.md"},
            {"id":"foo_greet","label":"greet","file_type":"code","source_file":"foo.md"}
        ],
        "edges": [{
            "source":"foo_module","target":"foo_greet","relation":"references",
            "confidence":"EXTRACTED","confidence_score":1.0
        }],
        "hyperedges": [],
        "input_tokens": 0,
        "output_tokens": 0
    })
}

fn envelope(stop_reason: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "type":"result",
        "subtype":"success",
        "is_error":false,
        "result":fragment().to_string(),
        "stop_reason":stop_reason,
        "usage":{
            "input_tokens":6,
            "output_tokens":11,
            "cache_read_input_tokens":17837,
            "cache_creation_input_tokens":30800
        },
        "modelUsage":{"claude-opus-4-7[1m]":{"inputTokens":6,"outputTokens":11}}
    }))
    .unwrap()
}

#[test]
fn test_returns_parsed_nodes_and_edges() {
    let response = parse_claude_cli_response(0, &envelope("end_turn"), &[]).unwrap();
    assert_eq!(response.fragment["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(response.fragment["edges"].as_array().unwrap().len(), 1);
}

#[test]
fn test_token_accounting_includes_cache() {
    let response = parse_claude_cli_response(0, &envelope("end_turn"), &[]).unwrap();
    assert_eq!(response.input_tokens, 6 + 17_837 + 30_800);
    assert_eq!(response.output_tokens, 11);
    assert_eq!(response.model.as_deref(), Some("claude-opus-4-7[1m]"));
    assert_eq!(response.finish_reason, "stop");
}

#[test]
fn test_finish_reason_length_on_max_tokens() {
    let response = parse_claude_cli_response(0, &envelope("max_tokens"), &[]).unwrap();
    assert_eq!(response.finish_reason, "length");
}

#[test]
fn test_raises_when_cli_missing() {
    let error = resolve_claude_cli_command(false, |_| None).unwrap_err();
    assert!(error.to_string().contains("Claude Code CLI not found"));
}

#[test]
fn test_raises_on_nonzero_exit() {
    let error = parse_claude_cli_response(2, &[], b"auth failed").unwrap_err();
    assert!(error.to_string().contains("exited 2"));
}

#[test]
fn test_raises_on_garbage_envelope() {
    let error = parse_claude_cli_response(0, b"not json", &[]).unwrap_err();
    assert!(error.to_string().contains("unparseable JSON envelope"));
}

#[test]
fn test_extract_files_direct_dispatches_to_claude_cli() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("foo.md");
    fs::write(&source, "# Foo\n\nThe greet() helper formats a name.\n").unwrap();
    let request = build_claude_cli_file_request(
        "claude",
        &[FileUnit::Path(source)],
        temp.path(),
        None,
        true,
        &BTreeMap::new(),
    )
    .unwrap();
    assert!(request.stdin.contains("path=\"foo.md\""));
    let response = parse_claude_cli_response(0, &envelope("end_turn"), &[]).unwrap();
    assert_eq!(response.fragment["nodes"].as_array().unwrap().len(), 2);
}

#[test]
fn test_backend_registered_with_zero_cost() {
    let providers = builtin_provider_configs();
    let config = &providers["claude-cli"];
    assert_eq!(config.pricing.input, 0.0);
    assert_eq!(config.pricing.output, 0.0);
    assert_eq!(estimate_cost(config, 1_000_000, 1_000_000), 0.0);
}

#[test]
fn test_no_session_persistence_flag_in_subprocess() {
    assert!(build_claude_cli_request("dummy", None)
        .argv
        .iter()
        .any(|argument| argument == "--no-session-persistence"));
}

#[test]
fn test_no_system_prompt_flag_in_subprocess() {
    assert!(!build_claude_cli_request("dummy source", None)
        .argv
        .iter()
        .any(|argument| argument == "--system-prompt"));
}

#[test]
fn test_extraction_instructions_ride_in_user_turn() {
    let request = build_claude_cli_request("UNIQUE_SOURCE_MARKER", None);
    assert!(request.stdin.contains("graphify semantic extraction agent"));
    assert!(request.stdin.contains("output ONLY the JSON object"));
    assert!(request.stdin.contains("UNIQUE_SOURCE_MARKER"));
}

#[test]
fn test_user_turn_preserves_untrusted_source_guardrails() {
    assert!(build_claude_cli_request("dummy", None)
        .stdin
        .contains("untrusted_source"));
}

#[test]
fn test_json_schema_flag_added_when_cli_supports_it() {
    let request = build_claude_cli_request_with_options(
        "claude",
        "dummy source",
        None,
        true,
        &BTreeMap::new(),
    );
    let index = request
        .argv
        .iter()
        .position(|argument| argument == "--json-schema")
        .unwrap();
    let schema: Value = serde_json::from_str(&request.argv[index + 1]).unwrap();
    assert_eq!(schema["type"], "object");
    let required = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        required,
        std::collections::BTreeSet::from(["edges", "nodes"])
    );
}

#[test]
fn test_json_schema_flag_absent_when_cli_lacks_it() {
    let request = build_claude_cli_request_with_options(
        "claude",
        "dummy source",
        None,
        false,
        &BTreeMap::new(),
    );
    assert!(!request
        .argv
        .iter()
        .any(|argument| argument == "--json-schema"));
    assert_eq!(
        parse_claude_cli_response(0, &envelope("end_turn"), &[])
            .unwrap()
            .fragment["nodes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn test_supports_json_schema_detects_flag_in_help() {
    let mut cache = ClaudeSchemaSupportCache::default();
    assert!(cache.supports("/fake/claude-new", |_| {
        Ok("Options:\n --json-schema <schema> structured output".into())
    }));
}

#[test]
fn test_supports_json_schema_false_when_flag_absent() {
    let mut cache = ClaudeSchemaSupportCache::default();
    assert!(!cache.supports("/fake/claude-old", |_| {
        Ok("Options:\n --output-format <format> text|json".into())
    }));
}

#[test]
fn test_supports_json_schema_false_and_cached_on_probe_error() {
    let mut cache = ClaudeSchemaSupportCache::default();
    let calls = Cell::new(0);
    assert!(
        !cache.supports("/fake/claude-broken", |_: &str| -> anyhow::Result<String> {
            calls.set(calls.get() + 1);
            anyhow::bail!("boom")
        })
    );
    assert!(!cache.supports("/fake/claude-broken", |_| {
        calls.set(calls.get() + 1);
        Ok("--json-schema".into())
    }));
    assert_eq!(calls.get(), 1);
}

#[test]
fn test_windows_prefers_claude_cmd_over_bare_claude() {
    let command = resolve_claude_cli_command(true, |name| match name {
        "claude" => Some(r"C:\Users\u\AppData\Roaming\npm\claude.ps1".into()),
        "claude.cmd" => Some(r"C:\Users\u\AppData\Roaming\npm\claude.cmd".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(command, r"C:\Users\u\AppData\Roaming\npm\claude.cmd");
}

#[test]
fn test_windows_falls_back_to_bare_claude_when_cmd_missing() {
    let command = resolve_claude_cli_command(true, |name| {
        (name == "claude").then(|| "/usr/local/bin/claude".into())
    })
    .unwrap();
    assert_eq!(command, "claude");
}

#[test]
fn test_windows_raises_when_neither_cmd_nor_bare_claude_present() {
    assert!(resolve_claude_cli_command(true, |_| None)
        .unwrap_err()
        .to_string()
        .contains("Claude Code CLI not found"));
}

#[test]
fn test_non_windows_uses_bare_claude() {
    let command = resolve_claude_cli_command(false, |name| {
        (name == "claude").then(|| "/usr/local/bin/claude".into())
    })
    .unwrap();
    assert_eq!(command, "claude");
}

#[test]
fn test_resolve_api_timeout_default() {
    assert_eq!(
        resolve_client_options(&BTreeMap::new()).timeout_seconds,
        600.0
    );
}

#[test]
fn test_resolve_api_timeout_env_override() {
    assert_eq!(
        resolve_client_options(&env(&[("GRAPHIFY_API_TIMEOUT", "45")])).timeout_seconds,
        45.0
    );
}

#[test]
fn test_resolve_api_timeout_ignores_invalid() {
    assert_eq!(
        resolve_client_options(&env(&[("GRAPHIFY_API_TIMEOUT", "not-a-number")])).timeout_seconds,
        600.0
    );
}

#[test]
fn test_resolve_api_timeout_ignores_nonpositive() {
    assert_eq!(
        resolve_client_options(&env(&[("GRAPHIFY_API_TIMEOUT", "0")])).timeout_seconds,
        600.0
    );
}

#[test]
fn test_claude_cli_extraction_honours_timeout() {
    let request = build_claude_cli_request_with_options(
        "claude",
        "dummy",
        None,
        false,
        &env(&[("GRAPHIFY_API_TIMEOUT", "30")]),
    );
    assert_eq!(request.timeout_seconds, 30.0);
}

#[test]
fn test_call_llm_claude_cli_branch_honours_timeout() {
    let request = build_claude_cli_request_with_options(
        "claude",
        "x",
        None,
        false,
        &env(&[("GRAPHIFY_API_TIMEOUT", "30")]),
    );
    assert_eq!(request.timeout_seconds, 30.0);
}

#[test]
fn test_simple_completion_resolves_cmd_shim_on_windows() {
    let command = resolve_claude_cli_command(true, |name| {
        Some(if name == "claude.cmd" {
            r"C:\npm\claude.cmd".into()
        } else {
            r"C:\npm\claude".into()
        })
    })
    .unwrap();
    assert_eq!(command, r"C:\npm\claude.cmd");
}

#[test]
fn test_prefers_structured_output_over_prose_result() {
    let bytes = serde_json::to_vec(&json!({
        "type":"result",
        "subtype":"success",
        "is_error":false,
        "result":"Knowledge graph extracted successfully: 2 nodes, 1 edge.",
        "structured_output":fragment(),
        "stop_reason":"end_turn",
        "usage":{"input_tokens":6,"output_tokens":11},
        "modelUsage":{"claude-opus-4-7[1m]":{}}
    }))
    .unwrap();
    let response = parse_claude_cli_response(0, &bytes, &[]).unwrap();
    assert_eq!(response.fragment["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(response.fragment["edges"].as_array().unwrap().len(), 1);
    assert_eq!(response.finish_reason, "stop");
}
