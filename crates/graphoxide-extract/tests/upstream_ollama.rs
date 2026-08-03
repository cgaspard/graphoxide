use graphoxide_extract::llm::{
    build_direct_extraction_plan_paths, builtin_provider_configs, detect_backend,
    validate_ollama_base_url_with_resolver,
};
use std::{collections::BTreeMap, fs, net::IpAddr};
use tempfile::TempDir;

fn env(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

#[test]
fn test_ollama_blocks_link_local_and_metadata() {
    for url in [
        "http://169.254.169.254/v1",
        "http://169.254.1.5:11434/v1",
        "http://metadata.google.internal/v1",
        "http://0.0.0.0:11434/v1",
    ] {
        assert!(
            validate_ollama_base_url_with_resolver(url, true, |_, _| Vec::new()).is_err(),
            "{url}"
        );
    }
}

#[test]
fn test_ollama_loopback_and_lan_do_not_raise() {
    let local =
        validate_ollama_base_url_with_resolver("http://localhost:11434/v1", true, |_, _| {
            vec!["127.0.0.1".parse().unwrap()]
        })
        .unwrap();
    assert!(local.warning.is_none());
    let lan =
        validate_ollama_base_url_with_resolver("http://192.168.1.50:11434/v1", true, |_, _| {
            Vec::new()
        })
        .unwrap();
    assert!(lan.warning.unwrap().contains("non-loopback"));
}

#[test]
fn test_ollama_alias_resolving_to_link_local_blocked() {
    let error =
        validate_ollama_base_url_with_resolver("http://innocent-looking-host/v1", true, |_, _| {
            vec![IpAddr::V4("169.254.169.254".parse().unwrap())]
        });
    assert!(error.is_err());
}

#[test]
fn test_ollama_warn_false_still_hard_blocks_but_stays_quiet() {
    let lan =
        validate_ollama_base_url_with_resolver("http://192.168.1.50:11434/v1", false, |_, _| {
            Vec::new()
        })
        .unwrap();
    assert!(lan.warning.is_none());
    assert!(
        validate_ollama_base_url_with_resolver(
            "http://169.254.169.254/v1",
            false,
            |_, _| Vec::new()
        )
        .is_err()
    );
}

#[test]
fn test_ollama_in_backends() {
    let providers = builtin_provider_configs();
    let ollama = &providers["ollama"];
    assert_eq!(ollama.pricing.input, 0.0);
    assert_eq!(ollama.pricing.output, 0.0);
    assert!(ollama.extra.contains_key("max_tokens"));
}

#[test]
fn test_detect_backend_ollama() {
    assert_eq!(
        detect_backend(
            &builtin_provider_configs(),
            &env(&[("OLLAMA_BASE_URL", "http://localhost:11434/v1")])
        )
        .as_deref(),
        Some("ollama")
    );
}

#[test]
fn test_detect_backend_kimi_beats_ollama() {
    assert_eq!(
        detect_backend(
            &builtin_provider_configs(),
            &env(&[
                ("MOONSHOT_API_KEY", "test-key"),
                ("OLLAMA_BASE_URL", "http://localhost:11434/v1"),
            ])
        )
        .as_deref(),
        Some("kimi")
    );
}

#[test]
fn test_detect_backend_claude_beats_ollama() {
    assert_eq!(
        detect_backend(
            &builtin_provider_configs(),
            &env(&[
                ("OLLAMA_BASE_URL", "http://localhost:11434/v1"),
                ("ANTHROPIC_API_KEY", "sk-test"),
            ])
        )
        .as_deref(),
        Some("claude")
    );
}

#[test]
fn test_detect_backend_none_without_envvars() {
    assert_eq!(
        detect_backend(&builtin_provider_configs(), &BTreeMap::new()),
        None
    );
}

#[test]
fn test_ollama_api_key_sentinel() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("sample.py");
    fs::write(&source, "x = 1\n").unwrap();
    let plan = build_direct_extraction_plan_paths(
        &[source],
        "ollama",
        temp.path(),
        &BTreeMap::new(),
        false,
    )
    .unwrap();
    assert_eq!(plan.api_key, "ollama");
}
