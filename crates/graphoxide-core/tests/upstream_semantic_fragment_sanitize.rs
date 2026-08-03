use graphoxide_core::{parse_llm_json, sanitize_fragment_shape};
use serde_json::json;

#[test]
fn test_sanitize_drops_non_dict_edge_entries() {
    let output = sanitize_fragment_shape(&json!({
        "nodes": [{"id": "a"}, ["not", "a", "dict"], "bare-string", {"id": "b"}],
        "edges": [{"source": "a", "target": "b"}, ["stray", "list"], 42],
        "hyperedges": [{"id": "h"}, null]
    }));
    assert_eq!(output["nodes"], json!([{"id": "a"}, {"id": "b"}]));
    assert_eq!(output["edges"], json!([{"source": "a", "target": "b"}]));
    assert_eq!(output["hyperedges"], json!([{"id": "h"}]));
}

#[test]
fn test_sanitize_coerces_non_list_values_to_empty() {
    let output = sanitize_fragment_shape(
        &json!({"nodes": {"id": "oops"}, "edges": "nope", "hyperedges": null}),
    );
    assert_eq!(output["nodes"], json!([]));
    assert_eq!(output["edges"], json!([]));
    assert!(output["hyperedges"].is_null());
}

#[test]
fn test_parse_llm_json_sanitizes_stray_list_in_edges() {
    let raw = serde_json::to_string(&json!({
        "nodes": [{"id": "a"}],
        "edges": [{"source": "a", "target": "b"}, ["malformed"]],
        "hyperedges": []
    }))
    .unwrap();
    let parsed = parse_llm_json(&raw).unwrap();
    for bucket in ["nodes", "edges", "hyperedges"] {
        assert!(parsed[bucket]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item.is_object()));
    }
    assert_eq!(parsed["edges"], json!([{"source": "a", "target": "b"}]));
}

#[test]
fn test_parse_llm_json_fenced_response_is_sanitized() {
    let payload =
        serde_json::to_string(&json!({"nodes": [["bad"], {"id": "ok"}], "edges": []})).unwrap();
    let parsed = parse_llm_json(&format!("Here you go:\n\n```json\n{payload}\n```\n")).unwrap();
    assert_eq!(parsed["nodes"], json!([{"id": "ok"}]));
}

#[test]
fn test_merge_after_sanitize_does_not_raise_on_source_file_access() {
    let parsed = parse_llm_json(
        &serde_json::to_string(&json!({
            "nodes": [{"id": "a", "source_file": "d.md"}],
            "edges": [{"source": "a", "target": "b"}, ["oops"]]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut seen = parsed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item.get("source_file").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    seen.extend(
        parsed["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item.get("source_file").and_then(|value| value.as_str())),
    );
    assert!(seen.contains(&"d.md"));
}
