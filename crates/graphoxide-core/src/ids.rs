//! Node ID construction and normalization.
//!
//! Derived from upstream Graphify's `ids.py`. IDs must be constructed identically so
//! that graphs built by the Rust and Python implementations merge cleanly.

use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

/// Normalize an ID using the recipe in upstream Graphify's `normalize_id`.
pub fn normalize_id(value: &str) -> String {
    let mut normalized = String::new();
    let mut pending_separator = false;

    for ch in value.nfkc() {
        if ch.is_alphanumeric() {
            if pending_separator && !normalized.is_empty() {
                normalized.push('_');
            }
            for folded in ch.to_lowercase() {
                normalized.push(folded);
            }
            pending_separator = false;
        } else {
            pending_separator = true;
        }
    }
    normalized.trim_matches('_').case_fold().collect()
}

/// Join name parts and return their canonical graph ID.
pub fn make_id(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .filter(|part| !part.is_empty())
        .map(|part| part.trim_matches(['_', '.']))
        .collect::<Vec<_>>()
        .join("_");
    normalize_id(&joined)
}

/// Construct the conventional `<source_file>_<symbol>` node ID.
pub fn node_id(source_file: &str, symbol: &str) -> String {
    make_id(&[source_file, symbol])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_upstream_vectors() {
        let vectors = [
            ("Hello, World!", "hello_world"),
            ("__docs/v1.api.py__", "docs_v1_api_py"),
            ("Ｆｏｏ １２", "foo_12"),
            ("中文/解析", "中文_解析"),
            ("Straße", "strasse"),
            ("e\u{301}", "é"),
        ];
        for (input, expected) in vectors {
            assert_eq!(normalize_id(input), expected, "input={input:?}");
            assert_eq!(normalize_id(&normalize_id(input)), expected);
        }
        assert_eq!(
            make_id(&["docs/v1_api.py", "parse"]),
            "docs_v1_api_py_parse"
        );
        assert_eq!(make_id(&["extractors/__init__"]), "extractors_init");
    }
}
