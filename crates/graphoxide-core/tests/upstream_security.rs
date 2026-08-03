//! Executable port of upstream `test_security.py` (60 cases).

use graphoxide_core::{
    check_graph_file_size_cap_with, decode_utf8_lossy, ensure_success_status,
    parse_max_graph_bytes, read_limited, safe_fetch, sanitize_label, sanitize_metadata,
    sanitize_metadata_string, sanitize_metadata_value, sanitize_optional_label,
    validate_graph_path, validate_graph_path_with_output_name, validate_url,
    DEFAULT_MAX_GRAPH_BYTES, METADATA_MAX_LIST_ITEMS, METADATA_MAX_VALUE_LEN,
};
use serde_json::{json, Map, Value};
use std::{fs, io::Cursor, path::Path, time::Duration};
use tempfile::tempdir;

#[test]
fn validate_url_accepts_http() {
    assert_eq!(
        validate_url("http://example.com/page").unwrap(),
        "http://example.com/page"
    );
}

#[test]
fn validate_url_accepts_https() {
    assert_eq!(
        validate_url("https://arxiv.org/abs/1706.03762").unwrap(),
        "https://arxiv.org/abs/1706.03762"
    );
}

#[test]
fn validate_url_rejects_file() {
    assert!(validate_url("file:///etc/passwd")
        .unwrap_err()
        .to_string()
        .contains("file"));
}

#[test]
fn validate_url_rejects_ftp() {
    assert!(validate_url("ftp://files.example.com/data.zip")
        .unwrap_err()
        .to_string()
        .contains("ftp"));
}

#[test]
fn validate_url_rejects_data() {
    assert!(validate_url("data:text/html,<script>alert(1)</script>")
        .unwrap_err()
        .to_string()
        .contains("data"));
}

#[test]
fn validate_url_rejects_empty_scheme() {
    assert!(validate_url("//no-scheme.example.com").is_err());
}

#[test]
fn safe_fetch_rejects_file_url() {
    assert!(safe_fetch("file:///etc/passwd", 1024, Duration::from_millis(1)).is_err());
}

#[test]
fn safe_fetch_rejects_ftp_url() {
    assert!(safe_fetch("ftp://example.com/file.zip", 1024, Duration::from_millis(1)).is_err());
}

#[test]
fn safe_fetch_returns_bytes() {
    let mut source = Cursor::new(b"hello world");
    assert_eq!(
        read_limited(&mut source, 1024, "fixture").unwrap(),
        b"hello world"
    );
}

#[test]
fn safe_fetch_raises_on_non_2xx() {
    assert!(ensure_success_status(404, "https://example.com/missing").is_err());
}

#[test]
fn safe_fetch_raises_on_size_exceeded() {
    let mut source = Cursor::new(vec![b'x'; 65_537 * 2]);
    assert!(
        read_limited(&mut source, 65_536, "https://example.com/huge")
            .unwrap_err()
            .to_string()
            .contains("size limit")
    );
}

#[test]
fn safe_fetch_text_decodes_utf8() {
    assert_eq!(decode_utf8_lossy("héllo wörld".as_bytes()), "héllo wörld");
}

#[test]
fn safe_fetch_text_replaces_bad_bytes() {
    let result = decode_utf8_lossy(b"hello \xff world");
    assert!(result.contains("hello"));
    assert!(result.contains("world"));
    assert!(!result.contains('\u{ff}'));
}

#[test]
fn validate_graph_path_allows_inside_base() {
    let tmp = tempdir().unwrap();
    let base = tmp.path().join("graphoxide-out");
    fs::create_dir(&base).unwrap();
    let graph = base.join("graph.json");
    fs::write(&graph, "{}").unwrap();
    assert_eq!(
        validate_graph_path(&graph, Some(&base)).unwrap(),
        graph.canonicalize().unwrap()
    );
}

#[test]
fn validate_graph_path_blocks_traversal() {
    let tmp = tempdir().unwrap();
    let base = tmp.path().join("graphoxide-out");
    fs::create_dir(&base).unwrap();
    let evil = base.join("..").join("etc_passwd");
    assert!(validate_graph_path(&evil, Some(&base))
        .unwrap_err()
        .to_string()
        .contains("escapes"));
}

#[test]
fn validate_graph_path_requires_base_exists() {
    let tmp = tempdir().unwrap();
    let base = tmp.path().join("graphoxide-out");
    assert!(validate_graph_path(base.join("graph.json"), Some(&base))
        .unwrap_err()
        .to_string()
        .contains("does not exist"));
}

#[test]
fn validate_graph_path_raises_if_file_missing() {
    let tmp = tempdir().unwrap();
    let base = tmp.path().join("graphoxide-out");
    fs::create_dir(&base).unwrap();
    assert!(validate_graph_path(base.join("missing.json"), Some(&base))
        .unwrap_err()
        .to_string()
        .contains("not found"));
}

#[test]
fn validate_graph_path_default_base_discovers_output_dir() {
    let tmp = tempdir().unwrap();
    let base = tmp.path().join("graphoxide-out");
    fs::create_dir(&base).unwrap();
    let graph = base.join("graph.json");
    fs::write(&graph, "{}").unwrap();
    assert_eq!(
        validate_graph_path(&graph, None).unwrap(),
        graph.canonicalize().unwrap()
    );
}

#[test]
fn validate_graph_path_default_base_honours_output_override() {
    let tmp = tempdir().unwrap();
    let base = tmp.path().join("custom-out");
    fs::create_dir(&base).unwrap();
    let graph = base.join("graph.json");
    fs::write(&graph, "{}").unwrap();
    assert_eq!(
        validate_graph_path_with_output_name(&graph, None, "custom-out").unwrap(),
        graph.canonicalize().unwrap()
    );
}

#[test]
fn sanitize_label_passthrough_html_chars() {
    assert_eq!(sanitize_label("<script>"), "<script>");
    assert_eq!(sanitize_label("foo & bar"), "foo & bar");
}

#[test]
fn sanitize_label_strips_control_chars() {
    assert_eq!(sanitize_label("hello\0\u{1f}world"), "helloworld");
}

#[test]
fn sanitize_label_caps_at_256() {
    assert!(sanitize_label(&"a".repeat(300)).chars().count() <= 256);
}

#[test]
fn sanitize_label_safe_passthrough() {
    assert_eq!(sanitize_label("MyClass"), "MyClass");
    assert_eq!(sanitize_label("extract_python"), "extract_python");
}

#[test]
fn sanitize_label_none_returns_empty() {
    assert_eq!(sanitize_optional_label(None), "");
}

#[test]
fn graph_size_cap_default_is_512_mib() {
    assert_eq!(DEFAULT_MAX_GRAPH_BYTES, 512 * 1024 * 1024);
}

#[test]
fn max_graph_bytes_default_when_unset() {
    assert_eq!(parse_max_graph_bytes(None), DEFAULT_MAX_GRAPH_BYTES);
}

#[test]
fn max_graph_bytes_default_when_blank() {
    assert_eq!(parse_max_graph_bytes(Some("   ")), DEFAULT_MAX_GRAPH_BYTES);
}

#[test]
fn max_graph_bytes_plain_integer() {
    assert_eq!(parse_max_graph_bytes(Some("671088640")), 671_088_640);
}

#[test]
fn max_graph_bytes_mb_suffix_is_binary() {
    assert_eq!(parse_max_graph_bytes(Some("640MB")), 640 * 1024 * 1024);
}

#[test]
fn max_graph_bytes_gb_suffix_is_binary() {
    assert_eq!(parse_max_graph_bytes(Some("2GB")), 2 * 1024 * 1024 * 1024);
}

#[test]
fn max_graph_bytes_suffix_is_case_insensitive() {
    assert_eq!(parse_max_graph_bytes(Some("3gb")), 3 * 1024 * 1024 * 1024);
}

#[test]
fn max_graph_bytes_tolerates_space_before_suffix() {
    assert_eq!(parse_max_graph_bytes(Some("5 GB")), 5 * 1024 * 1024 * 1024);
}

macro_rules! bad_cap {
    ($name:ident, $value:expr) => {
        #[test]
        fn $name() {
            assert_eq!(parse_max_graph_bytes(Some($value)), DEFAULT_MAX_GRAPH_BYTES);
        }
    };
}

bad_cap!(max_graph_bytes_rejects_not_a_number, "not-a-number");
bad_cap!(max_graph_bytes_rejects_fraction, "1.5GB");
bad_cap!(max_graph_bytes_rejects_hex, "0x10");
bad_cap!(max_graph_bytes_rejects_kb, "640KB");
bad_cap!(max_graph_bytes_rejects_zero, "0");
bad_cap!(max_graph_bytes_rejects_negative_one, "-1");
bad_cap!(max_graph_bytes_rejects_negative_gb, "-4GB");

#[test]
fn graph_size_cap_under_limit_returns_none() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, r#"{"nodes": [], "links": []}"#).unwrap();
    check_graph_file_size_cap_with(&path, 1024).unwrap();
}

#[test]
fn graph_size_cap_over_limit_raises() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, "x".repeat(50)).unwrap();
    assert!(check_graph_file_size_cap_with(&path, 16).is_err());
}

#[test]
fn graph_size_cap_error_message_includes_size_and_cap() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, "A".repeat(16)).unwrap();
    let message = check_graph_file_size_cap_with(&path, 8)
        .unwrap_err()
        .to_string();
    assert!(message.contains("16"));
    assert!(message.contains('8'));
    assert!(message.to_lowercase().contains("byte"));
}

#[test]
fn graph_size_cap_at_boundary_passes() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    fs::write(&path, "A".repeat(32)).unwrap();
    check_graph_file_size_cap_with(&path, 32).unwrap();
    assert!(check_graph_file_size_cap_with(&path, 31).is_err());
}

#[test]
fn graph_size_cap_missing_file_silently_returns() {
    let tmp = tempdir().unwrap();
    check_graph_file_size_cap_with(&tmp.path().join("does_not_exist.json"), 1).unwrap();
}

#[test]
fn graph_size_cap_unreadable_path_silently_returns() {
    // Rust does not monkey-patch `Path::metadata`; a missing parent exercises the
    // same `io::Error` branch used for permission-denied metadata failures.
    check_graph_file_size_cap_with(Path::new("/definitely/missing/graph.json"), 1).unwrap();
}

#[test]
fn sanitize_metadata_string_strips_control_chars() {
    assert_eq!(sanitize_metadata_string("hello\0\u{1f}world"), "helloworld");
}

#[test]
fn sanitize_metadata_string_escapes_html() {
    let result = sanitize_metadata_string("<script>alert('x')</script>");
    assert!(result.contains("&lt;"));
    assert!(result.contains("&gt;"));
    assert!(!result.contains("<script>"));
}

#[test]
fn sanitize_metadata_string_escapes_quotes() {
    let result = sanitize_metadata_string("a\"b'c");
    assert!(result.contains("&quot;"));
    assert!(result.contains("&#x27;"));
}

#[test]
fn sanitize_metadata_string_caps_length() {
    assert!(
        sanitize_metadata_string("a".repeat(METADATA_MAX_VALUE_LEN + 100))
            .chars()
            .count()
            <= METADATA_MAX_VALUE_LEN
    );
}

#[test]
fn sanitize_metadata_string_coerces_non_string() {
    struct Custom;
    impl std::fmt::Display for Custom {
        fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            output.write_str("custom-repr")
        }
    }
    assert_eq!(sanitize_metadata_string(Custom), "custom-repr");
}

#[test]
#[allow(clippy::approx_constant)] // exact upstream test vector
fn sanitize_metadata_value_preserves_simple_types() {
    for value in [
        json!(42),
        json!(3.14),
        json!(true),
        json!(false),
        Value::Null,
    ] {
        assert_eq!(sanitize_metadata_value(&value), value);
    }
}

#[test]
fn sanitize_metadata_value_recurses_into_dict() {
    let output = sanitize_metadata_value(&json!({"k": "<script>x</script>"}));
    assert!(output["k"].as_str().unwrap().contains("&lt;"));
}

#[test]
fn sanitize_metadata_value_recurses_into_list() {
    let output = sanitize_metadata_value(&json!(["<a>", "<b>", "<c>"]));
    assert!(output
        .as_array()
        .unwrap()
        .iter()
        .all(|value| value.as_str().unwrap().contains("&lt;")));
}

#[test]
fn sanitize_metadata_value_caps_list_length() {
    let input = Value::Array((0..METADATA_MAX_LIST_ITEMS * 3).map(Value::from).collect());
    assert_eq!(
        sanitize_metadata_value(&input).as_array().unwrap().len(),
        METADATA_MAX_LIST_ITEMS
    );
}

#[test]
fn sanitize_metadata_value_converts_tuple_to_list() {
    // serde_json has one sequence representation for Rust tuples and vectors.
    let input = serde_json::to_value(("a", "b")).unwrap();
    assert_eq!(sanitize_metadata_value(&input), json!(["a", "b"]));
}

#[test]
fn sanitize_metadata_none_returns_empty_dict() {
    assert!(sanitize_metadata(None).is_empty());
}

#[test]
fn sanitize_metadata_drops_empty_key() {
    let input = Map::from_iter([("\0".into(), json!("v")), ("k".into(), json!("v2"))]);
    let output = sanitize_metadata(Some(&input));
    assert_eq!(output.len(), 1);
    assert_eq!(output["k"], "v2");
}

#[test]
fn sanitize_metadata_sanitizes_keys() {
    let input = Map::from_iter([("<bad>".into(), json!("v"))]);
    let output = sanitize_metadata(Some(&input));
    assert!(!output.contains_key("<bad>"));
    assert!(output.keys().any(|key| key.contains("&lt;")));
}

#[test]
fn sanitize_metadata_recursive_nested() {
    let input = json!({
        "outer": {"inner": "<script>x</script>", "list": ["a", "<b>", 99, null, true]},
        "scalar": 42
    });
    let output = sanitize_metadata(input.as_object());
    assert!(output["outer"]["inner"].as_str().unwrap().contains("&lt;"));
    assert_eq!(output["outer"]["list"][0], "a");
    assert!(output["outer"]["list"][1]
        .as_str()
        .unwrap()
        .contains("&lt;"));
    assert_eq!(output["outer"]["list"][2], 99);
    assert!(output["outer"]["list"][3].is_null());
    assert_eq!(output["outer"]["list"][4], true);
    assert_eq!(output["scalar"], 42);
}

#[test]
fn sanitize_metadata_bool_not_coerced_to_int() {
    let input = json!({"flag_t": true, "flag_f": false, "num": 1});
    let output = sanitize_metadata(input.as_object());
    assert_eq!(output["flag_t"], true);
    assert_eq!(output["flag_f"], false);
    assert_eq!(output["num"], 1);
}
