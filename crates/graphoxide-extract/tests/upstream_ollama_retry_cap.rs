use graphoxide_extract::llm::build_openai_request_plan;
use std::collections::BTreeMap;

fn env(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

fn plan(
    environment: &BTreeMap<String, String>,
    backend: &str,
) -> graphoxide_extract::llm::OpenAiRequestPlan {
    build_openai_request_plan(
        if backend == "ollama" {
            "http://localhost:11434/v1"
        } else {
            "https://api.moonshot.cn/v1"
        },
        "m",
        "def f(): pass",
        Some(0.0),
        None,
        8_192,
        backend,
        false,
        None,
        environment,
    )
}

#[test]
fn test_ollama_defaults_to_zero_sdk_retries() {
    assert_eq!(plan(&BTreeMap::new(), "ollama").max_retries, 0);
}

#[test]
fn test_ollama_honors_explicit_max_retries() {
    assert_eq!(
        plan(&env(&[("GRAPHIFY_MAX_RETRIES", "3")]), "ollama").max_retries,
        3
    );
}

#[test]
fn test_cloud_backend_keeps_default_retries() {
    assert_eq!(plan(&BTreeMap::new(), "kimi").max_retries, 6);
}

#[test]
fn test_api_timeout_is_passed_to_client() {
    assert_eq!(
        plan(&env(&[("GRAPHIFY_API_TIMEOUT", "180")]), "ollama").timeout_seconds,
        180.0
    );
}
