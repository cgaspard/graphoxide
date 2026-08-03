//! Executable port of upstream `test_id_normalization_contract.py` (34 cases).

use graphoxide_core::{make_id, node_id, normalize_id};
use proptest::prelude::*;

macro_rules! normalization_case {
    ($name:ident, $raw:expr) => {
        #[test]
        fn $name() {
            let raw = $raw;
            assert_eq!(make_id(&[raw]), normalize_id(raw));
        }
    };
}

normalization_case!(make_matches_session_validate_token, "Session_ValidateToken");
normalization_case!(make_matches_punctuation, "session.validate-token");
normalization_case!(make_matches_repeated_separators, "foo__bar..baz");
normalization_case!(make_matches_leading_trailing, "  Leading_Trailing__  ");
normalization_case!(make_matches_path_separators, "A/B\\C");
normalization_case!(make_matches_mixed_case, "MixedCASE");
normalization_case!(make_matches_composed_accent, "café");
normalization_case!(make_matches_decomposed_accent, "cafe\u{301}");
normalization_case!(make_matches_cjk, "日本語クラス");
normalization_case!(make_matches_cyrillic, "Кириллица");
normalization_case!(make_matches_mixed_accented_latin, "naïve_Über");
normalization_case!(make_matches_chunk_like_suffix, "x_c1");
normalization_case!(make_matches_dunder, "__dunder__");
normalization_case!(make_matches_whitespace_runs, "tab\tnewline\nspace ");

macro_rules! idempotence_case {
    ($name:ident, $raw:expr) => {
        #[test]
        fn $name() {
            let once = normalize_id($raw);
            assert_eq!(normalize_id(&once), once);
        }
    };
}

idempotence_case!(idempotent_session_validate_token, "Session_ValidateToken");
idempotence_case!(idempotent_punctuation, "session.validate-token");
idempotence_case!(idempotent_repeated_separators, "foo__bar..baz");
idempotence_case!(idempotent_leading_trailing, "  Leading_Trailing__  ");
idempotence_case!(idempotent_path_separators, "A/B\\C");
idempotence_case!(idempotent_mixed_case, "MixedCASE");
idempotence_case!(idempotent_composed_accent, "café");
idempotence_case!(idempotent_decomposed_accent, "cafe\u{301}");
idempotence_case!(idempotent_cjk, "日本語クラス");
idempotence_case!(idempotent_cyrillic, "Кириллица");
idempotence_case!(idempotent_mixed_accented_latin, "naïve_Über");
idempotence_case!(idempotent_chunk_like_suffix, "x_c1");
idempotence_case!(idempotent_dunder, "__dunder__");
idempotence_case!(idempotent_whitespace_runs, "tab\tnewline\nspace ");

#[test]
fn make_id_joins_then_normalizes() {
    let parts = ["auth", "session.py", "ValidateToken"];
    assert_eq!(
        make_id(&parts),
        normalize_id("auth_session.py_ValidateToken")
    );
    assert_eq!(
        make_id(&["auth", "session", "ValidateToken"]),
        "auth_session_validatetoken"
    );
}

#[test]
fn unicode_identifiers_do_not_collapse_to_empty() {
    let a = normalize_id("クラスА");
    let b = normalize_id("クラスB");
    assert!(!a.is_empty());
    assert!(!b.is_empty());
    assert_ne!(a, b);
}

#[test]
fn normalized_ids_are_safe_node_ids() {
    let cases = [
        "Session_ValidateToken",
        "session.validate-token",
        "foo__bar..baz",
        "  Leading_Trailing__  ",
        "A/B\\C",
        "MixedCASE",
        "café",
        "cafe\u{301}",
        "日本語クラス",
        "Кириллица",
        "naïve_Über",
        "x_c1",
        "__dunder__",
        "tab\tnewline\nspace ",
    ];
    for raw in cases {
        let output = normalize_id(raw);
        assert!(!output.chars().any(char::is_uppercase), "{output:?}");
        assert!(!output
            .chars()
            .any(|ch| matches!(ch, '.' | '/' | '\\') || ch.is_whitespace()));
        assert!(!output.starts_with('_'));
        assert!(!output.ends_with('_'));
    }
}

#[test]
fn all_rust_callers_share_one_implementation() {
    assert_eq!(make_id(&["Foo.Bar"]), normalize_id("Foo.Bar"));
    assert_eq!(node_id("Foo.Bar", "baz"), make_id(&["Foo.Bar", "baz"]));
    assert_eq!(node_id("Ångström", "Ⅳ"), make_id(&["Ångström", "Ⅳ"]));
}

proptest! {
    #[test]
    fn property_make_id_equals_normalize_id(value in any::<String>()) {
        prop_assert_eq!(make_id(&[&value]), normalize_id(&value));
    }

    #[test]
    fn property_normalize_id_idempotent(value in any::<String>()) {
        let once = normalize_id(&value);
        prop_assert_eq!(normalize_id(&once), once);
    }
}
