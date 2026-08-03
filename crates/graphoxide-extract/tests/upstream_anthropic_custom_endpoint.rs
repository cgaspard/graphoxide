use graphoxide_extract::llm::provider_configs_from_environment;
use std::collections::BTreeMap;

fn env(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

#[test]
fn test_claude_defaults_without_env() {
    let providers = provider_configs_from_environment(&BTreeMap::new());
    assert_eq!(providers["claude"].base_url, "https://api.anthropic.com");
    assert_eq!(providers["claude"].default_model, "claude-sonnet-4-6");
}

#[test]
fn test_claude_base_url_and_model_env_override() {
    let providers = provider_configs_from_environment(&env(&[
        ("ANTHROPIC_BASE_URL", "http://localhost:4000"),
        ("ANTHROPIC_MODEL", "my-proxied-model"),
    ]));
    assert_eq!(providers["claude"].base_url, "http://localhost:4000");
    assert_eq!(providers["claude"].default_model, "my-proxied-model");
}
