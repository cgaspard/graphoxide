use graphoxide_extract::llm::provider_configs_from_environment;
use std::collections::BTreeMap;

fn env(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

#[test]
fn test_openai_defaults_without_env() {
    let providers = provider_configs_from_environment(&BTreeMap::new());
    assert_eq!(providers["openai"].base_url, "https://api.openai.com/v1");
    assert_eq!(providers["openai"].default_model, "gpt-4.1-mini");
}

#[test]
fn test_openai_base_url_and_model_env_override() {
    let providers = provider_configs_from_environment(&env(&[
        ("OPENAI_BASE_URL", "http://localhost:8080/v1"),
        ("OPENAI_MODEL", "my-local-model"),
    ]));
    assert_eq!(providers["openai"].base_url, "http://localhost:8080/v1");
    assert_eq!(providers["openai"].default_model, "my-local-model");
}

#[test]
fn test_graphify_openai_model_wins_over_openai_model() {
    let providers = provider_configs_from_environment(&env(&[
        ("OPENAI_MODEL", "env-default-model"),
        ("GRAPHIFY_OPENAI_MODEL", "graphify-override-model"),
    ]));
    assert_eq!(providers["openai"].default_model, "graphify-override-model");
}
