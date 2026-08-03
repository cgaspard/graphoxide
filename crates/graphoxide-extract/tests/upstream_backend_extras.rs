use graphoxide_core::FileUnit;
use graphoxide_extract::llm::{
    build_direct_extraction_plan, builtin_provider_configs, BUILTIN_PROVIDERS,
};
use std::{collections::BTreeMap, path::Path};

fn claude_environment() -> BTreeMap<String, String> {
    BTreeMap::from([("ANTHROPIC_API_KEY".into(), "test-key".into())])
}

// Graphify needed an optional Python package for Anthropic. Graphoxide links
// its HTTP implementation into the binary, so the equivalent contract is that
// the built-in provider is complete without a runtime add-on.
#[test]
fn test_anthropic_backend_is_compiled_in() {
    let providers = builtin_provider_configs();
    let claude = &providers["claude"];
    assert_eq!(claude.env_key.as_deref(), Some("ANTHROPIC_API_KEY"));
    assert_eq!(claude.base_url, "https://api.anthropic.com");
}

#[test]
fn test_anthropic_is_in_all_builtin_backends() {
    assert!(BUILTIN_PROVIDERS.contains(&"claude"));
    let unit = FileUnit::Path(Path::new("example.py").to_path_buf());
    let plan = build_direct_extraction_plan(
        &[unit],
        "claude",
        Path::new("."),
        &claude_environment(),
        false,
    )
    .unwrap();
    assert_eq!(plan.backend, "claude");
}

#[test]
fn test_backend_dependency_error_names_the_required_environment_key() {
    let unit = FileUnit::Path(Path::new("example.py").to_path_buf());
    let error =
        build_direct_extraction_plan(&[unit], "claude", Path::new("."), &BTreeMap::new(), false)
            .unwrap_err()
            .to_string();
    assert!(error.contains("ANTHROPIC_API_KEY"));
    assert!(!error.contains("pip install"));
}
