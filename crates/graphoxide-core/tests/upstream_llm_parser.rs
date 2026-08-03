use graphoxide_core::parse_llm_json;
use serde_json::json;

#[test]
fn test_preamble_then_fence_is_parsed() {
    let raw = "Here are the extracted entities:\n\n```json\n{\"nodes\": [{\"id\": \"a\"}], \"edges\": []}\n```";
    let result = parse_llm_json(raw).unwrap();
    assert_eq!(result["nodes"], json!([{"id": "a"}]));
    assert_eq!(result["edges"], json!([]));
}

#[test]
fn test_prose_wrapped_json_without_fence_is_parsed() {
    let raw =
        "The extracted graph is {\"nodes\": [{\"id\": \"b\"}], \"edges\": []}. Hope this helps!";
    let result = parse_llm_json(raw).unwrap();
    assert_eq!(result["nodes"], json!([{"id": "b"}]));
}

#[test]
fn test_raw_json_still_works() {
    let raw = "{\"nodes\": [], \"edges\": [], \"hyperedges\": []}";
    assert_eq!(
        parse_llm_json(raw).unwrap(),
        json!({"nodes": [], "edges": [], "hyperedges": []})
    );
}

#[test]
fn test_total_refusal_returns_empty_fragment() {
    assert_eq!(
        parse_llm_json("I cannot extract structured data from this content.").unwrap(),
        json!({"nodes": [], "edges": [], "hyperedges": []})
    );
}

#[test]
fn test_fence_with_uppercase_language_tag() {
    let result =
        parse_llm_json("```JSON\n{\"nodes\": [{\"id\": \"x\"}], \"edges\": []}\n```").unwrap();
    assert_eq!(result["nodes"], json!([{"id": "x"}]));
}

#[test]
fn test_fence_without_closing_backticks() {
    let result = parse_llm_json("```json\n{\"nodes\": [{\"id\": \"y\"}], \"edges\": []}").unwrap();
    assert_eq!(result["nodes"], json!([{"id": "y"}]));
}

#[test]
fn test_empty_response_returns_empty_fragment() {
    assert_eq!(
        parse_llm_json("").unwrap(),
        json!({"nodes": [], "edges": [], "hyperedges": []})
    );
}
