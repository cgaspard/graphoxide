use graphoxide_core::{
    load_validated_semantic_fragment, load_validated_semantic_fragment_with_limits,
    sanitize_semantic_fragment, validate_semantic_fragment, validate_semantic_fragment_with_limits,
    SemanticFragmentLimits,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs};
use tempfile::tempdir;

fn valid_fragment() -> Value {
    json!({
        "nodes": [{"id": "module_func", "label": "func", "file_type": "code"}],
        "edges": [{"source": "module_func", "target": "other_node"}],
        "hyperedges": []
    })
}

#[test]
fn test_validate_semantic_fragment_accepts_valid() {
    assert!(validate_semantic_fragment(&valid_fragment()).is_empty());
}

#[test]
fn test_validate_semantic_fragment_rejects_non_object() {
    assert!(validate_semantic_fragment(&json!(["not", "an", "object"]))
        .iter()
        .any(|error| error.to_lowercase().contains("object")));
}

#[test]
fn test_validate_semantic_fragment_rejects_oversize_payload() {
    let mut fragment = valid_fragment();
    fragment["nodes"][0]["label"] = Value::String("x".repeat(128));
    let limits = SemanticFragmentLimits {
        bytes: 64,
        ..SemanticFragmentLimits::default()
    };
    assert!(validate_semantic_fragment_with_limits(&fragment, limits)
        .iter()
        .any(|error| error.to_lowercase().contains("payload")));
}

#[test]
fn test_validate_semantic_fragment_rejects_too_many_nodes() {
    let mut fragment = valid_fragment();
    fragment["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "extra", "label": "extra", "file_type": "code"}));
    let limits = SemanticFragmentLimits {
        nodes: 1,
        ..SemanticFragmentLimits::default()
    };
    assert!(validate_semantic_fragment_with_limits(&fragment, limits)
        .iter()
        .any(|error| error.to_lowercase().contains("nodes")));
}

#[test]
fn test_validate_semantic_fragment_rejects_too_many_edges() {
    let limits = SemanticFragmentLimits {
        edges: 0,
        ..SemanticFragmentLimits::default()
    };
    assert!(
        validate_semantic_fragment_with_limits(&valid_fragment(), limits)
            .iter()
            .any(|error| error.to_lowercase().contains("edges"))
    );
}

#[test]
fn test_validate_semantic_fragment_rejects_path_separator_in_id() {
    let mut fragment = valid_fragment();
    fragment["nodes"][0]["id"] = json!("../etc/passwd");
    assert!(validate_semantic_fragment(&fragment)
        .iter()
        .any(|error| error.contains("nodes[0].id")));
}

#[test]
fn test_validate_semantic_fragment_accepts_unknown_file_type() {
    let mut fragment = valid_fragment();
    fragment["nodes"][0]["file_type"] = json!("executable");
    assert!(!validate_semantic_fragment(&fragment)
        .iter()
        .any(|error| error.contains("file_type")));
}

#[test]
fn test_validate_semantic_fragment_accepts_rationale_file_type() {
    let mut fragment = valid_fragment();
    fragment["nodes"][0]["file_type"] = json!("rationale");
    assert!(!validate_semantic_fragment(&fragment)
        .iter()
        .any(|error| error.contains("file_type")));
}

#[test]
fn test_validate_semantic_fragment_accepts_concept_file_type() {
    let mut fragment = valid_fragment();
    fragment["nodes"][0]["file_type"] = json!("concept");
    assert!(!validate_semantic_fragment(&fragment)
        .iter()
        .any(|error| error.contains("file_type")));
}

#[test]
fn test_load_validated_semantic_fragment_accepts_valid() {
    let temp = tempdir().unwrap();
    let chunk = temp.path().join(".graphoxide_chunk_00.json");
    fs::write(&chunk, serde_json::to_vec(&valid_fragment()).unwrap()).unwrap();
    assert_eq!(
        load_validated_semantic_fragment(&chunk).unwrap(),
        valid_fragment()
    );
}

#[test]
fn test_load_validated_semantic_fragment_rejects_oversize_before_parse() {
    let temp = tempdir().unwrap();
    let chunk = temp.path().join(".graphoxide_chunk_99.json");
    fs::write(&chunk, format!("[{}]", vec!["\"x\""; 50].join(","))).unwrap();
    let limits = SemanticFragmentLimits {
        bytes: 64,
        ..SemanticFragmentLimits::default()
    };
    assert!(load_validated_semantic_fragment_with_limits(&chunk, limits)
        .unwrap_err()
        .iter()
        .any(|error| error.to_lowercase().contains("payload")));
}

#[test]
fn test_load_validated_semantic_fragment_rejects_invalid_json() {
    let temp = tempdir().unwrap();
    let chunk = temp.path().join(".graphoxide_chunk_bad.json");
    fs::write(&chunk, "{not valid json").unwrap();
    assert!(load_validated_semantic_fragment(&chunk)
        .unwrap_err()
        .iter()
        .any(|error| error.to_lowercase().contains("invalid json")));
}

#[test]
fn test_validate_hyperedge_rejects_bad_id() {
    let mut fragment = valid_fragment();
    fragment["hyperedges"] =
        json!([{"id": "../escape", "label": "x", "nodes": ["module_func", "module_func"]}]);
    assert!(validate_semantic_fragment(&fragment)
        .iter()
        .any(|error| error.contains("hyperedges[0].id")));
}

#[test]
fn test_validate_hyperedge_rejects_bad_node_ref() {
    let mut fragment = valid_fragment();
    fragment["hyperedges"] =
        json!([{"id": "valid_he", "label": "x", "nodes": ["module_func", "../bad_ref"]}]);
    assert!(validate_semantic_fragment(&fragment)
        .iter()
        .any(|error| error.contains("hyperedges[0].nodes[1]")));
}

#[test]
fn test_validate_hyperedge_requires_list() {
    let mut fragment = valid_fragment();
    fragment["hyperedges"] = json!([{"id": "valid_he", "label": "x", "nodes": "not a list"}]);
    assert!(validate_semantic_fragment(&fragment)
        .iter()
        .any(|error| error.contains("hyperedges[0].nodes")));
}

#[test]
fn test_validate_hyperedge_caps_count() {
    let mut fragment = valid_fragment();
    fragment["hyperedges"] = json!([
        {"id": "he_0", "nodes": ["module_func", "module_func"]},
        {"id": "he_1", "nodes": ["module_func", "module_func"]},
        {"id": "he_2", "nodes": ["module_func", "module_func"]}
    ]);
    let limits = SemanticFragmentLimits {
        hyperedges: 1,
        ..SemanticFragmentLimits::default()
    };
    assert!(validate_semantic_fragment_with_limits(&fragment, limits)
        .iter()
        .any(|error| error.contains("hyperedges has 3")));
}

#[test]
fn test_sanitize_drops_rationale_filetype_node() {
    let output = sanitize_semantic_fragment(&json!({
        "nodes": [
            {"id": "real_node", "label": "Real", "file_type": "code"},
            {"id": "garbage", "label": "junk", "file_type": "rationale"}
        ], "edges": [], "hyperedges": []
    }));
    let ids = output["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert!(ids.contains("real_node") && !ids.contains("garbage"));
}

#[test]
fn test_sanitize_converts_sentence_rationale_node_to_attribute() {
    let output = sanitize_semantic_fragment(&json!({
        "nodes": [
            {"id": "real_node", "label": "Real", "file_type": "code"},
            {"id": "why_node", "label": "We chose tree-sitter because the deterministic parser is faster than regex-based extraction.", "file_type": "rationale"}
        ],
        "edges": [{"source": "why_node", "target": "real_node", "relation": "rationale_for"}],
        "hyperedges": []
    }));
    assert_eq!(output["nodes"].as_array().unwrap().len(), 1);
    assert!(output["nodes"][0]["rationale"]
        .as_str()
        .unwrap()
        .contains("tree-sitter"));
}

#[test]
fn test_sanitize_converts_allowed_filetype_sentence_via_rationale_for_edge() {
    let output = sanitize_semantic_fragment(&json!({
        "nodes": [
            {"id": "real_node", "label": "Real", "file_type": "code"},
            {"id": "sentence_node", "label": "Decision: this node has sentence-like rationale text but uses an allowed file_type, so it should not survive as a standalone graph node.", "file_type": "document"}
        ],
        "edges": [{"source": "sentence_node", "target": "real_node", "relation": "rationale_for"}],
        "hyperedges": []
    }));
    assert_eq!(output["nodes"].as_array().unwrap().len(), 1);
    assert!(output["nodes"][0]["rationale"]
        .as_str()
        .unwrap()
        .contains("Decision"));
}

#[test]
fn test_sanitize_keeps_short_concept_named_node_with_punctuation() {
    let output = sanitize_semantic_fragment(&json!({
        "nodes": [
            {"id": "a_b", "label": "a.b.c", "file_type": "document"},
            {"id": "anchor", "label": "Anchor", "file_type": "code"}
        ],
        "edges": [{"source": "a_b", "target": "anchor", "relation": "rationale_for"}],
        "hyperedges": []
    }));
    assert_eq!(output["nodes"].as_array().unwrap().len(), 2);
}

#[test]
fn test_sanitize_filters_hyperedges_after_node_removal() {
    let output = sanitize_semantic_fragment(&json!({
        "nodes": [
            {"id": "real_node", "label": "Real", "file_type": "code"},
            {"id": "other", "label": "Other", "file_type": "code"},
            {"id": "garbage", "label": "junk", "file_type": "rationale"}
        ],
        "edges": [],
        "hyperedges": [
            {"id": "group_a", "nodes": ["garbage", "real_node", "other"]},
            {"id": "group_b", "nodes": ["garbage", "real_node"]}
        ]
    }));
    assert_eq!(output["hyperedges"].as_array().unwrap().len(), 1);
    assert_eq!(output["hyperedges"][0]["id"], "group_a");
    assert_eq!(
        output["hyperedges"][0]["nodes"],
        json!(["real_node", "other"])
    );
}

#[test]
fn test_sanitize_drops_hyperedge_with_only_unknown_refs() {
    let output = sanitize_semantic_fragment(&json!({
        "nodes": [{"id": "real_node", "label": "Real", "file_type": "code"}],
        "edges": [],
        "hyperedges": [{"id": "phantom", "nodes": ["ghost1", "ghost2"]}]
    }));
    assert_eq!(output["hyperedges"], json!([]));
}

#[test]
fn test_sanitize_boundary_sentence_threshold() {
    let output = sanitize_semantic_fragment(&json!({
        "nodes": [
            {"id": "anchor", "label": "Anchor", "file_type": "code"},
            {"id": "n1", "label": "Note: alpha beta gamma delta epsilon zeta eta", "file_type": "rationale"}
        ],
        "edges": [{"source": "n1", "target": "anchor", "relation": "rationale_for"}],
        "hyperedges": []
    }));
    assert!(output["nodes"][0]["rationale"]
        .as_str()
        .unwrap()
        .contains("alpha"));
    let short = sanitize_semantic_fragment(&json!({
        "nodes": [
            {"id": "anchor", "label": "Anchor", "file_type": "code"},
            {"id": "n2", "label": "alpha beta gamma delta epsilon zeta eta", "file_type": "rationale"}
        ], "edges": [], "hyperedges": []
    }));
    assert!(short["nodes"][0].get("rationale").is_none());
}

#[test]
fn test_sanitize_rationale_only_propagates_through_rationale_for_edges() {
    let output = sanitize_semantic_fragment(&json!({
        "nodes": [
            {"id": "rationale_target", "label": "Rationale Target", "file_type": "code"},
            {"id": "unrelated_target", "label": "Unrelated Target", "file_type": "code"},
            {"id": "why_node", "label": "Decision: we chose tree-sitter because the deterministic parser is faster than regex-based extraction.", "file_type": "rationale"}
        ],
        "edges": [
            {"source": "why_node", "target": "rationale_target", "relation": "rationale_for"},
            {"source": "why_node", "target": "unrelated_target", "relation": "references"}
        ], "hyperedges": []
    }));
    let nodes = output["nodes"].as_array().unwrap();
    let rationale = nodes
        .iter()
        .find(|node| node["id"] == "rationale_target")
        .unwrap();
    let unrelated = nodes
        .iter()
        .find(|node| node["id"] == "unrelated_target")
        .unwrap();
    assert!(rationale["rationale"]
        .as_str()
        .unwrap()
        .contains("tree-sitter"));
    assert!(unrelated.get("rationale").is_none());
}

#[test]
fn test_sanitize_keeps_members_keyed_hyperedge() {
    let output = sanitize_semantic_fragment(&json!({
        "nodes": [
            {"id": "real_a", "label": "A", "file_type": "code"},
            {"id": "real_b", "label": "B", "file_type": "code"}
        ], "edges": [],
        "hyperedges": [{"id": "grp", "label": "Group", "members": ["real_a", "real_b"]}]
    }));
    assert_eq!(output["hyperedges"].as_array().unwrap().len(), 1);
    assert_eq!(
        output["hyperedges"][0]["nodes"],
        json!(["real_a", "real_b"])
    );
    assert!(output["hyperedges"][0].get("members").is_none());
}

#[test]
fn test_validate_accepts_node_ids_keyed_hyperedge() {
    let mut fragment = valid_fragment();
    fragment["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "second", "label": "Second", "file_type": "code"}));
    fragment["hyperedges"] =
        json!([{"id": "grp", "label": "G", "node_ids": ["module_func", "second"]}]);
    assert!(validate_semantic_fragment(&fragment).is_empty());
}
