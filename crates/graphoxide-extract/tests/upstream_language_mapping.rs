use std::collections::BTreeSet;

const MAPPING: &str = include_str!("../../../parity/source-maps/test_languages.mapping.json");
const LANGUAGE_PARITY_SOURCE: &str = include_str!("language_parity.rs");
const FALLBACK_SOURCE: &str = include_str!("../src/fallback.rs");

#[test]
fn all_331_upstream_language_cases_have_unique_executable_rust_mappings() {
    let mapping: serde_json::Value = serde_json::from_str(MAPPING).expect("valid mapping JSON");
    assert_eq!(mapping["schema_version"], 1);
    assert_eq!(mapping["upstream"]["collected_cases"], 331);
    assert_eq!(mapping["upstream"]["executed_cases"], 318);
    assert_eq!(mapping["upstream"]["skipped_optional_dm_cases"], 13);
    assert_eq!(mapping["rust"]["mapped_cases"], 331);
    assert_eq!(mapping["rust"]["language_parity_target_cases"], 321);
    assert_eq!(mapping["rust"]["library_markdown_cases"], 10);

    let cases = mapping["cases"].as_array().expect("mapping cases array");
    assert_eq!(cases.len(), 331);
    let mut upstream = BTreeSet::new();
    let mut rust = BTreeSet::new();
    for (index, case) in cases.iter().enumerate() {
        assert_eq!(case["ordinal"].as_u64(), Some((index + 1) as u64));
        let upstream_nodeid = case["upstream_nodeid"]
            .as_str()
            .expect("upstream pytest node ID");
        let rust_test = case["rust_test"].as_str().expect("Rust test ID");
        let target = case["rust_target"].as_str().expect("Rust test target");
        assert!(upstream.insert(upstream_nodeid));
        assert!(rust.insert((target, rust_test)));

        let upstream_name = upstream_nodeid
            .rsplit("::")
            .next()
            .expect("pytest function name");
        let rust_name = rust_test
            .rsplit("::")
            .next()
            .expect("Rust function name")
            .strip_prefix("upstream_")
            .unwrap_or_else(|| rust_test.rsplit("::").next().unwrap());
        assert_eq!(rust_name, upstream_name);
        match target {
            "language_parity" => assert!(
                LANGUAGE_PARITY_SOURCE.contains(upstream_name),
                "mapped integration test is absent from language_parity.rs: {rust_test}"
            ),
            "lib" => assert!(
                FALLBACK_SOURCE.contains(
                    rust_test
                        .rsplit("::")
                        .next()
                        .expect("Markdown unit-test name")
                ),
                "mapped Markdown test is absent from fallback.rs: {rust_test}"
            ),
            other => panic!("unknown Rust test target {other:?}"),
        }
    }
    assert_eq!(upstream.len(), 331);
    assert_eq!(rust.len(), 331);
}
