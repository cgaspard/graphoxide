use graphoxide_extract::llm::{
    detect_backend, load_custom_providers, provider_base_url_ok, provider_base_url_verdict,
    ProviderConfig,
};
use serde_json::json;
use std::{collections::BTreeMap, fs};
use tempfile::TempDir;

fn write(path: &std::path::Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

#[test]
fn test_custom_provider_add_list_show_remove() {
    let temp = TempDir::new().unwrap();
    let global = temp.path().join("providers.json");
    write(
        &global,
        json!({"nvidia": {"base_url": "https://integrate.api.nvidia.com/v1", "default_model": "minimaxai/minimax-m2.7", "env_key": "NVIDIA_API_KEY", "pricing": {"input": 0.0, "output": 0.0}, "temperature": 0}}),
    );
    let loaded = load_custom_providers(&global, &temp.path().join("local.json"), false);
    assert_eq!(
        loaded.providers["nvidia"].base_url,
        "https://integrate.api.nvidia.com/v1"
    );
}

#[test]
fn test_custom_provider_pricing_defaults_to_zero() {
    let temp = TempDir::new().unwrap();
    let global = temp.path().join("providers.json");
    write(
        &global,
        json!({"mymodel": {"base_url": "http://localhost:8080/v1", "default_model": "llama3", "env_key": "MY_API_KEY"}}),
    );
    let loaded = load_custom_providers(&global, &temp.path().join("local.json"), false);
    assert_eq!(loaded.providers["mymodel"].pricing.input, 0.0);
    assert_eq!(loaded.providers["mymodel"].pricing.output, 0.0);
}

#[test]
fn test_custom_provider_cannot_shadow_builtin() {
    let temp = TempDir::new().unwrap();
    let global = temp.path().join("providers.json");
    write(
        &global,
        json!({"claude": {"base_url": "http://evil.example.com/v1", "default_model": "evil-model", "env_key": "EVIL_KEY"}}),
    );
    assert!(
        !load_custom_providers(&global, &temp.path().join("local.json"), false)
            .providers
            .contains_key("claude")
    );
}

#[test]
fn test_project_local_providers_ignored_without_optin() {
    let temp = TempDir::new().unwrap();
    let local = temp.path().join("local.json");
    write(
        &local,
        json!({"evil": {"base_url": "https://attacker.example/v1", "default_model": "m", "env_key": "K"}}),
    );
    let loaded = load_custom_providers(&temp.path().join("global.json"), &local, false);
    assert!(!loaded.providers.contains_key("evil"));
    assert!(loaded
        .warnings
        .iter()
        .any(|warning| warning.contains("ignoring project-local")));
}

#[test]
fn test_project_local_providers_loaded_with_optin() {
    let temp = TempDir::new().unwrap();
    let local = temp.path().join("local.json");
    write(
        &local,
        json!({"lab": {"base_url": "https://lab.internal/v1", "default_model": "m", "env_key": "K"}}),
    );
    assert!(
        load_custom_providers(&temp.path().join("global.json"), &local, true)
            .providers
            .contains_key("lab")
    );
}

#[test]
fn test_non_http_provider_base_url_rejected() {
    let temp = TempDir::new().unwrap();
    let global = temp.path().join("providers.json");
    write(
        &global,
        json!({"sneaky": {"base_url": "file:///etc/passwd", "default_model": "m", "env_key": "K"}}),
    );
    assert!(
        !load_custom_providers(&global, &temp.path().join("local.json"), false)
            .providers
            .contains_key("sneaky")
    );
}

#[test]
fn test_provider_base_url_ok_scheme_and_warnings() {
    assert!(provider_base_url_ok("https://api.example/v1", "ok"));
    assert!(provider_base_url_ok("http://localhost:11434/v1", "local"));
    assert!(!provider_base_url_ok("file:///etc/passwd", "bad"));
    assert!(!provider_base_url_ok("gopher://x/", "bad2"));
    let verdict = provider_base_url_verdict("http://example.com/v1", "plain");
    assert!(verdict.allowed);
    assert!(verdict
        .warning
        .is_some_and(|warning| warning.contains("plaintext")));
}

#[test]
fn test_detect_backend_custom_provider_after_builtins() {
    let providers = BTreeMap::from([(
        "myprovider".into(),
        ProviderConfig {
            base_url: "http://example.com/v1".into(),
            default_model: "mymodel".into(),
            env_key: Some("MY_CUSTOM_KEY".into()),
            ..ProviderConfig::default()
        },
    )]);
    let environment = BTreeMap::from([("MY_CUSTOM_KEY".into(), "test-key".into())]);
    assert_eq!(
        detect_backend(&providers, &environment).as_deref(),
        Some("myprovider")
    );
}
