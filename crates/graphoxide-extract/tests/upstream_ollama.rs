use graphoxide_extract::llm::{
    build_direct_extraction_plan_paths, builtin_provider_configs, detect_backend,
    plan_ollama_connection_with_resolver, validate_ollama_base_url_with_resolver,
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
        "http://2852039166:11434/v1",
        "http://0xa9fea9fe:11434/v1",
        "http://169.254.1.5:11434/v1",
        "http://metadata.google.internal/v1",
        "http://metadata.google.internal./v1",
        "http://metadata.google.internal../v1",
        "http://0.0.0.0:11434/v1",
        "http://[::ffff:169.254.169.254]:11434/v1",
        "http://[::ffff:0.0.0.0]:11434/v1",
        "http://[::169.254.169.254]:11434/v1",
        "http://[64:ff9b::169.254.169.254]:11434/v1",
        "http://[fd00:ec2::254]:11434/v1",
        "ftp://192.168.1.50/ollama",
        "http://secret@192.168.1.50:11434/v1",
        "http://@192.168.1.50:11434/v1",
        "http://:@192.168.1.50:11434/v1",
        "http:@192.168.1.50:11434/v1",
        r"http:\@192.168.1.50:11434/v1",
        "http://192.168.1.50:11434/v1?key=secret",
        "http://192.168.1.50:11434/v1#secret",
    ] {
        assert!(
            validate_ollama_base_url_with_resolver(url, true, |_, _| Vec::new()).is_err(),
            "{url}"
        );
    }
    let oversized = format!("http://192.168.1.50:11434/{}", "x".repeat(2048));
    assert!(validate_ollama_base_url_with_resolver(&oversized, true, |_, _| Vec::new()).is_err());
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
fn test_ollama_dns_resolution_is_fail_closed_and_canonicalized() {
    assert!(
        validate_ollama_base_url_with_resolver("http://ollama.lan:11434/v1", true, |_, _| {
            Vec::new()
        })
        .is_err()
    );
    let validated =
        plan_ollama_connection_with_resolver("http://OLLAMA.LAN..:11434/v1", true, |host, port| {
            assert_eq!(host, "ollama.lan");
            assert_eq!(port, 11434);
            vec![
                "192.168.10.11".parse().unwrap(),
                "192.168.10.10".parse().unwrap(),
                "192.168.10.11".parse().unwrap(),
            ]
        })
        .unwrap();
    assert_eq!(validated.canonical_host, "ollama.lan");
    assert_eq!(
        validated.resolved_addresses,
        vec![
            "192.168.10.11".parse::<IpAddr>().unwrap(),
            "192.168.10.10".parse::<IpAddr>().unwrap(),
        ]
    );
}

#[test]
fn test_ollama_userinfo_check_does_not_reject_path_at_signs() {
    let validated = plan_ollama_connection_with_resolver(
        "http://192.168.10.10:11434/api/@scope",
        false,
        |_, _| Vec::new(),
    )
    .unwrap();
    assert_eq!(validated.canonical_host, "192.168.10.10");
}

#[test]
fn test_ollama_alias_resolving_to_link_local_blocked() {
    let error =
        validate_ollama_base_url_with_resolver("http://innocent-looking-host/v1", true, |_, _| {
            vec![IpAddr::V4("169.254.169.254".parse().unwrap())]
        });
    assert!(error.is_err());
    let mixed =
        plan_ollama_connection_with_resolver("http://mixed-answer-host/v1", true, |_, _| {
            vec![
                "192.168.10.10".parse().unwrap(),
                "169.254.169.254".parse().unwrap(),
            ]
        });
    assert!(mixed.is_err());
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
