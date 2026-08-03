use graphoxide_core::{normalize_graph_value, validate};
use serde_json::{json, Value};

fn valid() -> Value {
    json!({
        "nodes": [
            {"id": "n1", "label": "Foo", "file_type": "code", "source_file": "foo.py"},
            {"id": "n2", "label": "Bar", "file_type": "document", "source_file": "bar.md"}
        ],
        "edges": [{
            "source": "n1", "target": "n2", "relation": "references",
            "confidence": "EXTRACTED", "source_file": "foo.py", "weight": 1.0
        }]
    })
}

#[test]
fn test_valid_passes() {
    assert!(validate::validate_extraction_json(&valid()).is_empty());
}

#[test]
fn test_missing_nodes_key() {
    let errors = validate::validate_extraction_json(&json!({"edges": []}));
    assert!(errors.iter().any(|error| error.contains("nodes")));
}

#[test]
fn test_missing_edges_key() {
    let errors = validate::validate_extraction_json(&json!({"nodes": []}));
    assert!(errors.iter().any(|error| error.contains("edges")));
}

#[test]
fn test_not_a_dict() {
    assert_eq!(validate::validate_extraction_json(&json!([])).len(), 1);
}

#[test]
fn test_invalid_file_type() {
    let errors = validate::validate_extraction_json(&json!({
        "nodes": [{"id": "n1", "label": "X", "file_type": "video", "source_file": "x.mp4"}],
        "edges": []
    }));
    assert!(errors.iter().any(|error| error.contains("file_type")));
}

#[test]
fn test_invalid_confidence() {
    let errors = validate::validate_extraction_json(&json!({
        "nodes": [
            {"id": "n1", "label": "A", "file_type": "code", "source_file": "a.py"},
            {"id": "n2", "label": "B", "file_type": "code", "source_file": "b.py"}
        ],
        "edges": [{"source": "n1", "target": "n2", "relation": "calls", "confidence": "CERTAIN", "source_file": "a.py"}]
    }));
    assert!(errors.iter().any(|error| error.contains("confidence")));
}

#[test]
fn test_dangling_edge_source() {
    let errors = validate::validate_extraction_json(&json!({
        "nodes": [{"id": "n1", "label": "A", "file_type": "code", "source_file": "a.py"}],
        "edges": [{"source": "missing_id", "target": "n1", "relation": "calls", "confidence": "EXTRACTED", "source_file": "a.py"}]
    }));
    assert!(errors
        .iter()
        .any(|error| error.contains("source") && error.contains("missing_id")));
}

#[test]
fn test_dangling_edge_target() {
    let errors = validate::validate_extraction_json(&json!({
        "nodes": [{"id": "n1", "label": "A", "file_type": "code", "source_file": "a.py"}],
        "edges": [{"source": "n1", "target": "ghost", "relation": "calls", "confidence": "EXTRACTED", "source_file": "a.py"}]
    }));
    assert!(errors
        .iter()
        .any(|error| error.contains("target") && error.contains("ghost")));
}

#[test]
fn test_missing_node_field() {
    let errors = validate::validate_extraction_json(&json!({
        "nodes": [{"id": "n1", "label": "A", "source_file": "a.py"}],
        "edges": []
    }));
    assert!(errors.iter().any(|error| error.contains("file_type")));
}

#[test]
fn test_assert_valid_raises_on_errors() {
    let error = validate::assert_valid_json(&json!({"nodes": "bad", "edges": [], "oops": true}))
        .unwrap_err();
    assert!(error.to_string().contains("error"));
}

#[test]
fn test_assert_valid_passes_silently() {
    validate::assert_valid_json(&valid()).unwrap();
}

#[test]
fn test_legacy_aliases_valid_after_build_canonicalization() {
    let mut data = json!({
        "nodes": [
            {"id": "n1", "name": "Foo", "path": "a/b.md", "file_type": "concept"},
            {"id": "n2", "label": "Bar", "file_type": "code", "source_file": "bar.py"}
        ],
        "edges": [{
            "source": "n1", "target": "n2", "type": "references",
            "confidence_score": 0.9, "source_file": "a/b.md"
        }]
    });
    assert!(validate::validate_extraction_json(&data)
        .iter()
        .any(|error| error.contains("missing required field")));
    normalize_graph_value(&mut data);
    assert!(validate::validate_extraction_json(&data).is_empty());
}

#[test]
fn test_non_hashable_node_id_reported_not_raised() {
    let errors = validate::validate_extraction_json(&json!({
        "nodes": [
            {"id": "n1", "label": "A", "file_type": "code", "source_file": "a.py"},
            {"id": ["x", "y"], "label": "B", "file_type": "code", "source_file": "b.py"}
        ],
        "edges": []
    }));
    assert!(errors.iter().any(|error| error.contains("non-hashable id")));
}

#[test]
fn test_non_hashable_edge_endpoint_reported_not_raised() {
    let errors = validate::validate_extraction_json(&json!({
        "nodes": [
            {"id": "n1", "label": "A", "file_type": "code", "source_file": "a.py"},
            {"id": "n2", "label": "B", "file_type": "code", "source_file": "b.py"}
        ],
        "edges": [{"source": "n1", "target": ["n2", "n3"], "relation": "calls", "confidence": "INFERRED", "source_file": "a.py"}]
    }));
    assert!(errors
        .iter()
        .any(|error| error.contains("target") && error.contains("non-hashable")));
}

#[test]
fn test_non_hashable_node_id_does_not_mask_valid_ids() {
    let errors = validate::validate_extraction_json(&json!({
        "nodes": [
            {"id": "n1", "label": "A", "file_type": "code", "source_file": "a.py"},
            {"id": {"oops": 1}, "label": "B", "file_type": "code", "source_file": "b.py"}
        ],
        "edges": [{"source": "n1", "target": "ghost", "relation": "calls", "confidence": "EXTRACTED", "source_file": "a.py"}]
    }));
    assert!(errors.iter().any(|error| error.contains("non-hashable id")));
    assert!(errors
        .iter()
        .any(|error| error.contains("target") && error.contains("ghost")));
}
