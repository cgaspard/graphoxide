use graphoxide_core::make_id;
use graphoxide_graph::source_file_stem;
use std::path::Path;

fn ast_symbol_id(path: &str, entity: &str) -> String {
    make_id(&[&source_file_stem(Path::new(path)), entity])
}

#[test]
fn test_spec_node_id_examples_match_ast_extractor() {
    for (path, entity, expected) in [
        (
            "src/auth/session.py",
            "ValidateToken",
            "src_auth_session_validatetoken",
        ),
        (
            "lib/utils/helpers.py",
            "parse_url",
            "lib_utils_helpers_parse_url",
        ),
        ("tests/test_foo.py", "_helper", "tests_test_foo_helper"),
        (
            "docs/v1/api/README.md",
            "getUser",
            "docs_v1_api_readme_getuser",
        ),
    ] {
        assert_eq!(ast_symbol_id(path, entity), expected, "{path} + {entity}");
    }
}

#[test]
fn test_cautionary_wrong_forms_are_actually_wrong() {
    let correct = ast_symbol_id("src/auth/session.py", "ValidateToken");
    assert_eq!(correct, "src_auth_session_validatetoken");
    assert_ne!(make_id(&["session", "ValidateToken"]), correct);
    assert_ne!(make_id(&["auth", "session", "ValidateToken"]), correct);
}
