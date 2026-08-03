use graphoxide_core::{validate::validate_extraction, Edge, Extraction, Node};
use graphoxide_extract::scip_ingest::{
    build_scip_metadata, ingest_scip_json, ingest_scip_json_with_defaults, make_scip_node_id,
    scip_kind_to_file_type,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

fn ingest(doc: Value) -> Extraction {
    ingest_scip_json(&doc)
}

fn ingest_with(doc: Value, source_file: &str, language: &str) -> Extraction {
    ingest_scip_json_with_defaults(&doc, source_file, language)
}

fn one_symbol_at(path: &str, symbol: Value) -> Value {
    json!({"documents": [{"relative_path": path, "symbols": [symbol]}]})
}

fn symbol_doc(symbol: &str, kind: &str, relationships: Vec<Value>) -> Value {
    one_symbol_at(
        "src/main.py",
        json!({
            "symbol": symbol,
            "kind": kind,
            "display_name": symbol.split('#').next_back().unwrap_or("").trim_matches(['(', ')']),
            "occurrences": [{"range": [10, 0, 10, 20], "symbol": symbol}],
            "relationships": relationships,
        }),
    )
}

fn metadata(node: &Node) -> &Map<String, Value> {
    node.extra["metadata"].as_object().expect("node metadata")
}

fn edge_metadata(edge: &Edge) -> &Map<String, Value> {
    edge.extra["metadata"].as_object().expect("edge metadata")
}

fn node_by_symbol<'a>(extraction: &'a Extraction, symbol: &str) -> &'a Node {
    extraction
        .nodes
        .iter()
        .find(|node| metadata(node)["scip_symbol"] == symbol)
        .unwrap_or_else(|| panic!("missing SCIP symbol {symbol}"))
}

fn assert_empty(extraction: &Extraction) {
    assert!(extraction.nodes.is_empty());
    assert!(extraction.edges.is_empty());
}

#[test]
fn test_ingest_empty_doc_returns_empty_lists() {
    assert_empty(&ingest(json!({})));
}

#[test]
fn test_ingest_dict_without_documents_key() {
    assert_empty(&ingest(json!({"metadata": "some meta"})));
}

#[test]
fn test_ingest_documents_not_a_list_is_skipped() {
    assert_empty(&ingest(json!({"documents": "not_a_list"})));
}

#[test]
fn test_ingest_documents_empty_list() {
    assert_empty(&ingest(json!({"documents": []})));
}

#[test]
fn test_ingest_single_symbol_no_relationships() {
    let result = ingest(json!({
        "documents": [{
            "relative_path": "src/main.py",
            "language": "python",
            "symbols": [{
                "symbol": "python/main.py:MainClass#",
                "kind": "class",
                "display_name": "MainClass",
                "documentation": ["The main class"],
                "relationships": [],
                "occurrences": [{"range": [5, 0, 5, 9], "symbol": "python/main.py:MainClass#"}],
            }],
        }],
    }));
    assert_eq!(result.nodes.len(), 1);
    assert!(result.edges.is_empty());
    let node = &result.nodes[0];
    assert_eq!(node.label, "MainClass");
    assert_eq!(node.file_type, "code");
    assert_eq!(node.source_file, "src/main.py");
    assert_eq!(node.source_location.as_deref(), Some("L5"));
    assert_eq!(metadata(node)["scip_symbol"], "python/main.py:MainClass#");
    assert_eq!(metadata(node)["scip_kind"], "class");
    assert_eq!(metadata(node)["scip_description"], "The main class");
}

#[test]
fn test_ingest_symbol_without_display_name_uses_suffix() {
    let result = ingest(one_symbol_at(
        "lib/helper.py",
        json!({"symbol": "python/helper.py:compute#run()", "kind": "function", "occurrences": [], "relationships": []}),
    ));
    assert_eq!(result.nodes[0].label, "run()");
}

#[test]
fn test_ingest_symbol_trailing_hash_no_display_name_has_non_empty_label() {
    let result = ingest(one_symbol_at(
        "src/Foo.java",
        json!({"symbol": "java/src/Foo.java:Foo#", "kind": "class", "occurrences": [], "relationships": []}),
    ));
    assert_eq!(result.nodes.len(), 1);
    assert!(!result.nodes[0].label.is_empty());
}

#[test]
fn test_ingest_symbol_without_hash_uses_full_symbol_as_label() {
    let result = ingest(one_symbol_at(
        "lib/helper.py",
        json!({"symbol": "SimpleFunction", "kind": "function", "occurrences": [], "relationships": []}),
    ));
    assert_eq!(result.nodes[0].label, "SimpleFunction");
}

#[test]
fn test_ingest_symbol_without_occurrences_has_empty_source_location() {
    let result = ingest(one_symbol_at(
        "lib/a.py",
        json!({"symbol": "python/lib/a.py:Foo#", "kind": "class", "occurrences": [], "relationships": []}),
    ));
    assert_eq!(result.nodes[0].source_location.as_deref(), Some(""));
}

#[test]
fn test_ingest_symbol_without_occurrences_key() {
    let result = ingest(one_symbol_at(
        "lib/a.py",
        json!({"symbol": "python/lib/a.py:Foo#", "kind": "class", "relationships": []}),
    ));
    assert_eq!(result.nodes[0].source_location.as_deref(), Some(""));
}

#[test]
fn test_ingest_multiple_symbols_in_one_document() {
    let result = ingest(
        json!({"documents": [{"relative_path": "src/mod.py", "symbols": [
            {"symbol": "python/mod.py:A#", "kind": "class", "display_name": "A", "occurrences": [], "relationships": []},
            {"symbol": "python/mod.py:B#", "kind": "function", "display_name": "B", "occurrences": [], "relationships": []},
            {"symbol": "python/mod.py:C#", "kind": "variable", "display_name": "C", "occurrences": [], "relationships": []}
        ]}]}),
    );
    assert_eq!(result.nodes.len(), 3);
    assert_eq!(
        result
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["A", "B", "C"])
    );
}

#[test]
fn test_ingest_multiple_documents() {
    let result = ingest(json!({"documents": [
        {"relative_path": "a.py", "symbols": [{"symbol": "A#", "kind": "class", "occurrences": [], "relationships": []}]},
        {"relative_path": "b.py", "symbols": [{"symbol": "B#", "kind": "function", "occurrences": [], "relationships": []}]}
    ]}));
    assert_eq!(result.nodes.len(), 2);
    assert_eq!(
        result
            .nodes
            .iter()
            .map(|node| node.source_file.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["a.py", "b.py"])
    );
}

#[test]
fn test_ingest_is_reference_emits_scip_ref_edge() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![json!({"symbol": "python/main.py:Helper#help()", "is_reference": true})],
    ));
    assert_eq!(result.edges.len(), 1);
    assert_eq!(result.edges[0].relation, "scip_ref");
}

#[test]
fn test_ingest_is_definition_emits_scip_def_edge() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![json!({"symbol": "python/main.py:Base#run()", "is_definition": true})],
    ));
    assert_eq!(result.edges[0].relation, "scip_def");
}

#[test]
fn test_ingest_is_implementation_emits_scip_impl_edge() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![
            json!({"symbol": "python/main.py:Base#run()", "is_implementation": true, "is_definition": true}),
        ],
    ));
    assert_eq!(result.edges[0].relation, "scip_impl");
}

#[test]
fn test_ingest_is_type_definition_emits_scip_typed_edge() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![json!({"symbol": "python/main.py:Base#run()", "is_type_definition": true})],
    ));
    assert_eq!(result.edges[0].relation, "scip_typed");
}

#[test]
fn test_ingest_relationship_priority_order() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![json!({
            "symbol": "python/main.py:Base#run()",
            "is_implementation": true,
            "is_type_definition": true,
            "is_definition": true,
            "is_reference": true
        })],
    ));
    assert_eq!(result.edges[0].relation, "scip_impl");
}

#[test]
fn test_ingest_relationship_no_boolean_flags_defaults_to_ref() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![json!({"symbol": "python/main.py:Other#"})],
    ));
    assert_eq!(result.edges[0].relation, "scip_ref");
}

#[test]
fn test_ingest_multiple_relationships_on_one_symbol() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![
            json!({"symbol": "python/main.py:Base#run()", "is_definition": true}),
            json!({"symbol": "python/main.py:Helper#help()", "is_reference": true}),
        ],
    ));
    assert_eq!(result.edges.len(), 2);
    assert_eq!(
        result
            .edges
            .iter()
            .map(|edge| edge.relation.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["scip_def", "scip_ref"])
    );
}

#[test]
fn test_ingest_relationship_without_target_symbol_is_skipped() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![
            json!({"symbol": "", "is_reference": true}),
            json!({"is_reference": true}),
        ],
    ));
    assert!(result.edges.is_empty());
}

#[test]
fn test_ingest_duplicate_edges_are_deduplicated() {
    let relationship = json!({"symbol": "python/main.py:Helper#help()", "is_reference": true});
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![relationship.clone(), relationship],
    ));
    assert_eq!(result.edges.len(), 1);
}

#[test]
fn test_ingest_edge_structure_complete() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![json!({"symbol": "python/main.py:Helper#help()", "is_reference": true})],
    ));
    let edge = &result.edges[0];
    assert_eq!(edge.confidence, graphoxide_core::Confidence::Extracted);
    assert_eq!(edge.extra["confidence_score"], 1.0);
    assert_eq!(edge.extra["weight"], 1.0);
    assert_eq!(edge.extra["context"], "scip");
    assert_eq!(edge.source_file, "src/main.py");
    assert_eq!(edge.extra["source_location"], "L10");
    assert!(edge_metadata(edge).contains_key("scip_relationship"));
}

#[test]
fn test_ingest_edge_source_location_from_first_occurrence() {
    let result = ingest(one_symbol_at(
        "src/mod.py",
        json!({
            "symbol": "python/mod.py:Foo#bar()",
            "kind": "function",
            "occurrences": [
                {"range": [42, 0, 42, 10], "symbol": "python/mod.py:Foo#bar()"},
                {"range": [99, 0, 99, 10], "symbol": "python/mod.py:Foo#bar()"}
            ],
            "relationships": [{"symbol": "python/mod.py:Baz#", "is_reference": true}]
        }),
    ));
    assert_eq!(result.edges[0].extra["source_location"], "L42");
    assert_eq!(result.nodes[0].source_location.as_deref(), Some("L42"));
}

#[test]
fn test_ingest_node_id_contains_source_file_and_symbol_suffix() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![],
    ));
    assert!(result.nodes[0].id.starts_with("scip_"));
    assert!(result.nodes[0].id.contains("run"));
}

#[test]
fn test_ingest_node_id_is_deterministic() {
    let doc = symbol_doc("python/main.py:MyClass#run()", "function", vec![]);
    assert_eq!(ingest(doc.clone()).nodes[0].id, ingest(doc).nodes[0].id);
}

#[test]
fn test_ingest_node_id_differs_by_source_file() {
    let symbol = json!({"symbol": "F#", "kind": "class", "occurrences": [], "relationships": []});
    assert_ne!(
        ingest(one_symbol_at("a.py", symbol.clone())).nodes[0].id,
        ingest(one_symbol_at("b.py", symbol)).nodes[0].id
    );
}

#[test]
fn test_ingest_duplicate_symbols_in_same_file_are_deduplicated() {
    let symbol = json!({"symbol": "F#", "kind": "class", "occurrences": [], "relationships": []});
    let result = ingest(
        json!({"documents": [{"relative_path": "src/main.py", "symbols": [symbol.clone(), symbol]}]}),
    );
    assert_eq!(result.nodes.len(), 1);
}

macro_rules! non_object_case {
    ($name:ident, $value:expr) => {
        #[test]
        fn $name() {
            assert_empty(&ingest($value));
        }
    };
}

non_object_case!(test_ingest_non_dict_input_returns_empty_none, Value::Null);
non_object_case!(
    test_ingest_non_dict_input_returns_empty_a_string,
    json!("a string")
);
non_object_case!(test_ingest_non_dict_input_returns_empty_42, json!(42));
non_object_case!(
    test_ingest_non_dict_input_returns_empty_3_14,
    Value::Number("3.14".parse().expect("valid JSON number"))
);
non_object_case!(test_ingest_non_dict_input_returns_empty_true, json!(true));
non_object_case!(
    test_ingest_non_dict_input_returns_empty_bad_input5,
    json!([])
);
non_object_case!(
    test_ingest_non_dict_input_returns_empty_bad_input6,
    json!([1, 2, 3])
);

#[test]
fn test_ingest_document_item_not_a_dict_is_skipped() {
    let result = ingest(json!({"documents": ["not_a_dict", 123, null, {
        "relative_path": "valid.py",
        "symbols": [{"symbol": "F#", "kind": "class", "occurrences": [], "relationships": []}]
    }]}));
    assert_eq!(result.nodes.len(), 1);
}

#[test]
fn test_ingest_symbol_item_not_a_dict_is_skipped() {
    let result = ingest(
        json!({"documents": [{"relative_path": "src/main.py", "symbols": [
            "not_a_dict", 42, null,
            {"symbol": "python/main.py:Valid#", "kind": "class", "display_name": "Valid", "occurrences": [], "relationships": []}
        ]}]}),
    );
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(result.nodes[0].label, "Valid");
}

#[test]
fn test_ingest_symbol_without_symbol_id_is_skipped() {
    let result = ingest(
        json!({"documents": [{"relative_path": "src/main.py", "symbols": [
            {"kind": "class", "occurrences": [], "relationships": []},
            {"symbol": "", "kind": "class", "occurrences": [], "relationships": []}
        ]}]}),
    );
    assert!(result.nodes.is_empty());
}

#[test]
fn test_ingest_relationship_item_not_a_dict_is_skipped() {
    let result = ingest(symbol_doc(
        "python/main.py:MyClass#run()",
        "function",
        vec![
            json!("not_a_dict"),
            json!(42),
            Value::Null,
            json!({"symbol": "python/main.py:Helper#help()", "is_reference": true}),
        ],
    ));
    assert_eq!(result.edges.len(), 1);
}

#[test]
fn test_ingest_document_without_symbols_key() {
    assert_empty(&ingest(
        json!({"documents": [{"relative_path": "src/main.py", "language": "python"}]}),
    ));
}

#[test]
fn test_ingest_document_with_symbols_not_a_list() {
    assert_empty(&ingest(
        json!({"documents": [{"relative_path": "src/main.py", "symbols": "not_a_list"}]}),
    ));
}

#[test]
fn test_ingest_symbol_without_kind_defaults_to_unknown() {
    let result = ingest(one_symbol_at(
        "src/main.py",
        json!({"symbol": "F#", "occurrences": [], "relationships": []}),
    ));
    assert_eq!(metadata(&result.nodes[0])["scip_kind"], "unknown");
}

#[test]
fn test_ingest_default_source_file_is_empty_string() {
    let result = ingest(
        json!({"documents": [{"symbols": [{"symbol": "F#", "kind": "class", "occurrences": [], "relationships": []}]}]}),
    );
    assert_eq!(result.nodes[0].source_file, "");
}

#[test]
fn test_ingest_source_file_falls_back_to_function_param() {
    let result = ingest_with(
        json!({"documents": [{"symbols": [{"symbol": "F#", "kind": "class", "occurrences": [], "relationships": []}]}]}),
        "fallback.scip",
        "python",
    );
    assert_eq!(result.nodes[0].source_file, "fallback.scip");
}

#[test]
fn test_ingest_document_relative_path_overrides_source_file_param() {
    let result = ingest_with(
        one_symbol_at(
            "explicit.py",
            json!({"symbol": "F#", "kind": "class", "occurrences": [], "relationships": []}),
        ),
        "fallback.scip",
        "python",
    );
    assert_eq!(result.nodes[0].source_file, "explicit.py");
}

#[test]
fn test_ingest_document_without_language_defaults_to_function_param() {
    let result = ingest_with(
        one_symbol_at(
            "src/main.ts",
            json!({"symbol": "F#", "kind": "class", "occurrences": [], "relationships": []}),
        ),
        "",
        "typescript",
    );
    assert_eq!(result.nodes.len(), 1);
}

#[test]
fn test_ingest_symbol_with_short_range_uses_first_element_as_line() {
    let result = ingest(one_symbol_at(
        "src/mod.py",
        json!({"symbol": "python/mod.py:F#", "kind": "class", "occurrences": [{"range": [7, 0], "symbol": "python/mod.py:F#"}], "relationships": []}),
    ));
    assert_eq!(result.nodes[0].source_location.as_deref(), Some("L7"));
}

#[test]
fn test_ingest_symbol_with_non_dict_occurrence_is_skipped() {
    let result = ingest(one_symbol_at(
        "src/mod.py",
        json!({"symbol": "python/mod.py:F#", "kind": "class", "occurrences": ["bad", 123, null, {"range": [15, 0, 15, 5]}], "relationships": []}),
    ));
    assert_eq!(result.nodes[0].source_location.as_deref(), Some(""));
}

#[test]
fn test_ingest_symbol_with_non_list_range_falls_back_to_zero() {
    let result = ingest(one_symbol_at(
        "src/mod.py",
        json!({"symbol": "F#", "kind": "class", "occurrences": [{"range": "not_a_list", "symbol": "F#"}], "relationships": []}),
    ));
    assert_eq!(result.nodes[0].source_location.as_deref(), Some(""));
}

#[test]
fn test_ingest_symbol_with_documentation_becomes_description() {
    let result = ingest(one_symbol_at(
        "src/mod.py",
        json!({"symbol": "F#", "kind": "class", "documentation": ["First line", "Second line"], "occurrences": [], "relationships": []}),
    ));
    assert_eq!(metadata(&result.nodes[0])["scip_description"], "First line");
}

#[test]
fn test_ingest_symbol_with_empty_documentation_skips_description() {
    let result = ingest(one_symbol_at(
        "src/mod.py",
        json!({"symbol": "F#", "kind": "class", "documentation": [""], "occurrences": [], "relationships": []}),
    ));
    assert!(!metadata(&result.nodes[0]).contains_key("scip_description"));
}

#[test]
fn test_ingest_symbol_without_documentation_omits_description() {
    let result = ingest(one_symbol_at(
        "src/mod.py",
        json!({"symbol": "F#", "kind": "class", "occurrences": [], "relationships": []}),
    ));
    assert!(!metadata(&result.nodes[0]).contains_key("scip_description"));
}

#[test]
fn test_ingest_symbol_without_relationships_key_still_creates_node() {
    let result = ingest(one_symbol_at(
        "src/mod.py",
        json!({"symbol": "F#", "kind": "class", "occurrences": []}),
    ));
    assert_eq!(result.nodes.len(), 1);
    assert!(result.edges.is_empty());
}

#[test]
fn test_make_scip_node_id_with_hash_separator() {
    let node_id = make_scip_node_id("python/main.py:MyClass#run()", "src/main.py");
    assert!(node_id.starts_with("scip_"));
    assert!(node_id.contains("run"));
    assert!(!node_id.contains(['(', ')']));
}

#[test]
fn test_make_scip_node_id_without_hash() {
    let node_id = make_scip_node_id("SimpleSymbol", "src/mod.py");
    assert!(node_id.starts_with("scip_"));
    assert!(node_id.to_lowercase().contains("simplesymbol"));
}

#[test]
fn test_make_scip_node_id_special_characters_are_sanitised() {
    assert!(make_scip_node_id("foo.bar#baz!@qux", "test.py").contains("scip_baz__qux"));
}

#[test]
fn test_make_scip_node_id_deterministic() {
    let first = make_scip_node_id("python/main.py:Foo#bar", "src/a.py");
    let second = make_scip_node_id("python/main.py:Foo#bar", "src/a.py");
    assert_eq!(first, second);
    assert_eq!(first, "scip_bar_7944c0ff3539");
}

#[test]
fn test_make_scip_node_id_source_file_affects_hash() {
    assert_ne!(
        make_scip_node_id("F#", "a.py"),
        make_scip_node_id("F#", "b.py")
    );
}

#[test]
fn test_make_scip_node_id_symbol_affects_hash() {
    assert_ne!(
        make_scip_node_id("A#", "f.py"),
        make_scip_node_id("B#", "f.py")
    );
}

#[test]
fn test_make_scip_node_id_empty_after_sanitisation_falls_back() {
    let node_id = make_scip_node_id("#", "src/f.py");
    assert_eq!(node_id.len(), "scip_".len() + 12);
    assert!(node_id.starts_with("scip_"));
    assert!(node_id["scip_".len()..]
        .chars()
        .all(|character| character.is_ascii_hexdigit()));
}

#[test]
fn test_scip_kind_to_file_type_always_code() {
    for kind in ["class", "function", "variable", "", "arbitrary_string"] {
        assert_eq!(scip_kind_to_file_type(kind), "code");
    }
}

#[test]
fn test_build_scip_metadata_with_description() {
    assert_eq!(
        build_scip_metadata("sym_id", "class", "A sample description"),
        Map::from_iter([
            ("scip_symbol".into(), json!("sym_id")),
            ("scip_kind".into(), json!("class")),
            ("scip_description".into(), json!("A sample description")),
        ])
    );
}

#[test]
fn test_build_scip_metadata_without_description() {
    let metadata = build_scip_metadata("sym_id", "class", "");
    assert_eq!(
        metadata,
        Map::from_iter([
            ("scip_symbol".into(), json!("sym_id")),
            ("scip_kind".into(), json!("class")),
        ])
    );
    assert!(!metadata.contains_key("scip_description"));
}

#[test]
fn test_ingest_many_symbols() {
    let symbols: Vec<_> = (0..100)
        .map(|index| json!({"symbol": format!("S{index}#"), "kind": "class", "occurrences": [], "relationships": []}))
        .collect();
    let result = ingest(json!({"documents": [{"relative_path": "big.py", "symbols": symbols}]}));
    assert_eq!(result.nodes.len(), 100);
    assert!(result.edges.is_empty());
}

#[test]
fn test_ingest_edge_with_zero_sourceline_has_empty_location() {
    let result = ingest(one_symbol_at(
        "src/mod.py",
        json!({"symbol": "A#", "kind": "class", "occurrences": [], "relationships": [{"symbol": "B#", "is_reference": true}]}),
    ));
    assert_eq!(result.edges[0].extra["source_location"], "");
}

#[test]
fn test_relationship_target_in_same_document_resolves_via_index() {
    let result = ingest(
        json!({"documents": [{"relative_path": "src/mod.py", "symbols": [
            {"symbol": "Caller#", "kind": "function", "relationships": [{"symbol": "Callee#", "is_reference": true}]},
            {"symbol": "Callee#", "kind": "function"}
        ]}]}),
    );
    let ids = result
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(result.edges.len(), 1);
    assert!(ids.contains(result.edges[0].source.as_str()));
    assert!(ids.contains(result.edges[0].target.as_str()));
}

#[test]
fn test_relationship_target_across_documents_resolves_via_index() {
    let result = ingest(json!({"documents": [
        {"relative_path": "src/a.py", "symbols": [{"symbol": "Caller#", "kind": "function", "relationships": [{"symbol": "Callee#", "is_reference": true}]}]},
        {"relative_path": "src/b.py", "symbols": [{"symbol": "Callee#", "kind": "function"}]}
    ]}));
    let caller = node_by_symbol(&result, "Caller#");
    let callee = node_by_symbol(&result, "Callee#");
    assert_eq!(result.edges[0].source, caller.id);
    assert_eq!(result.edges[0].target, callee.id);
    assert_eq!(callee.source_file, "src/b.py");
}

#[test]
fn test_relationship_target_unknown_emits_stub_node() {
    let result = ingest(one_symbol_at(
        "src/a.py",
        json!({"symbol": "Caller#", "kind": "function", "relationships": [{"symbol": "ExternalLib#fn", "is_reference": true}]}),
    ));
    let stub = node_by_symbol(&result, "ExternalLib#fn");
    assert_eq!(metadata(stub)["scip_kind"], "external");
    let ids = result
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    assert!(ids.contains(result.edges[0].source.as_str()));
    assert!(ids.contains(result.edges[0].target.as_str()));
}

#[test]
fn test_relationship_edges_survive_validate_extraction_and_build() {
    let result = ingest(
        json!({"documents": [{"relative_path": "src/a.py", "symbols": [
            {
                "symbol": "Caller#",
                "kind": "function",
                "occurrences": [{"range": [10, 0, 10, 6]}],
                "relationships": [
                    {"symbol": "Callee#", "is_reference": true},
                    {"symbol": "External#fn", "is_implementation": true}
                ]
            },
            {"symbol": "Callee#", "kind": "function"}
        ]}]}),
    );
    validate_extraction(&result).expect("valid SCIP extraction");
    let graph = graphoxide_graph::build_graph(&[result]).expect("build SCIP graph");
    assert_eq!(graph.links.len(), 2);
}

#[test]
fn test_non_string_relative_path_falls_back_to_default() {
    let result = ingest_with(
        json!({"documents": [{"relative_path": ["unexpected", "list"], "symbols": [{"symbol": "Foo#", "kind": "function"}]}]}),
        "fallback.py",
        "python",
    );
    assert_eq!(result.nodes[0].source_file, "fallback.py");
}

#[test]
fn test_non_string_language_falls_back() {
    let result = ingest(
        json!({"documents": [{"relative_path": "src/a.py", "language": 42, "symbols": [{"symbol": "Foo#", "kind": "function"}]}]}),
    );
    assert_eq!(result.nodes.len(), 1);
}

#[test]
fn test_non_string_symbol_id_is_skipped() {
    let result = ingest(
        json!({"documents": [{"relative_path": "src/a.py", "symbols": [
            {"symbol": 123, "kind": "function"},
            {"symbol": "Valid#", "kind": "function"}
        ]}]}),
    );
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(metadata(&result.nodes[0])["scip_symbol"], "Valid#");
}

#[test]
fn test_relationships_none_is_treated_as_empty() {
    let result = ingest(one_symbol_at(
        "src/a.py",
        json!({"symbol": "Foo#", "kind": "function", "relationships": null}),
    ));
    assert_eq!(result.nodes.len(), 1);
    assert!(result.edges.is_empty());
}

#[test]
fn test_relationship_symbol_non_string_is_skipped() {
    let result = ingest(one_symbol_at(
        "src/a.py",
        json!({"symbol": "Foo#", "kind": "function", "relationships": [
            {"symbol": 123, "is_reference": true},
            {"symbol": "RealTarget#", "is_reference": true}
        ]}),
    ));
    assert_eq!(result.edges.len(), 1);
    assert_eq!(
        edge_metadata(&result.edges[0])["scip_relationship"]["symbol"],
        "RealTarget#"
    );
}

#[test]
fn test_non_string_kind_falls_back_to_unknown() {
    let result = ingest(one_symbol_at(
        "src/a.py",
        json!({"symbol": "Foo#", "kind": ["not", "a", "string"]}),
    ));
    assert_eq!(metadata(&result.nodes[0])["scip_kind"], "unknown");
}

#[test]
fn test_non_string_display_name_falls_back() {
    let result = ingest(one_symbol_at(
        "src/a.py",
        json!({"symbol": "Foo#bar", "kind": "function", "display_name": 42}),
    ));
    assert_eq!(result.nodes[0].label, "bar");
}

#[test]
fn test_documentation_with_non_string_entries_is_ignored() {
    let result = ingest(one_symbol_at(
        "src/a.py",
        json!({"symbol": "Foo#", "kind": "function", "documentation": [42, "later"]}),
    ));
    assert!(!metadata(&result.nodes[0]).contains_key("scip_description"));
}

#[test]
fn test_unrecognized_top_level_structure_returns_empty() {
    for doc in [json!("not a dict"), json!([{"documents": []}]), Value::Null] {
        assert_empty(&ingest(doc));
    }
}

#[test]
fn test_documents_field_non_list_returns_empty() {
    assert_empty(&ingest(json!({"documents": "not a list"})));
}

#[test]
fn test_document_entry_non_dict_is_skipped() {
    let result = ingest(json!({"documents": [
        "not a dict",
        {"relative_path": "src/a.py", "symbols": [{"symbol": "Foo#", "kind": "function"}]}
    ]}));
    assert_eq!(result.nodes.len(), 1);
}

#[test]
fn test_occurrence_negative_line_falls_back_to_zero() {
    let result = ingest(one_symbol_at(
        "src/a.py",
        json!({"symbol": "Foo#", "kind": "function", "occurrences": [{"range": [-1, 0, -1, 6]}]}),
    ));
    assert_eq!(result.nodes[0].source_location.as_deref(), Some(""));
}

#[test]
fn test_duplicate_local_symbol_resolves_to_same_document() {
    let result = ingest(json!({"documents": [
        {"relative_path": "a.py", "symbols": [{"symbol": "F#", "kind": "function"}]},
        {"relative_path": "b.py", "symbols": [{"symbol": "F#", "kind": "function", "relationships": [{"symbol": "F#", "is_reference": true}]}]}
    ]}));
    let f_nodes = result
        .nodes
        .iter()
        .filter(|node| metadata(node)["scip_symbol"] == "F#")
        .collect::<Vec<_>>();
    assert_eq!(f_nodes.len(), 2);
    let a = f_nodes
        .iter()
        .find(|node| node.source_file == "a.py")
        .unwrap();
    let b = f_nodes
        .iter()
        .find(|node| node.source_file == "b.py")
        .unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!(result.edges[0].source, b.id);
    assert_eq!(result.edges[0].target, b.id);
}

#[test]
fn test_unique_cross_document_symbol_still_resolves() {
    let result = ingest(json!({"documents": [
        {"relative_path": "src/a.py", "symbols": [{"symbol": "Caller#", "kind": "function", "relationships": [{"symbol": "UniqueCallee#", "is_reference": true}]}]},
        {"relative_path": "src/b.py", "symbols": [{"symbol": "UniqueCallee#", "kind": "function"}]}
    ]}));
    let callee = node_by_symbol(&result, "UniqueCallee#");
    assert_eq!(result.edges[0].target, callee.id);
    assert_eq!(callee.source_file, "src/b.py");
}

#[test]
fn test_ambiguous_duplicate_target_across_docs_creates_stub() {
    let result = ingest(json!({"documents": [
        {"relative_path": "a.py", "symbols": [{"symbol": "Shared#", "kind": "function"}]},
        {"relative_path": "b.py", "symbols": [{"symbol": "Shared#", "kind": "function"}]},
        {"relative_path": "c.py", "symbols": [{"symbol": "Caller#", "kind": "function", "relationships": [{"symbol": "Shared#", "is_reference": true}]}]}
    ]}));
    let shared_in_c = result
        .nodes
        .iter()
        .filter(|node| metadata(node)["scip_symbol"] == "Shared#" && node.source_file == "c.py")
        .collect::<Vec<_>>();
    assert_eq!(shared_in_c.len(), 1);
    assert_eq!(metadata(shared_in_c[0])["scip_kind"], "external");
    assert_eq!(result.edges[0].target, shared_in_c[0].id);
}

#[test]
fn test_relationship_truthy_string_flag_is_ignored() {
    let result = ingest(one_symbol_at(
        "a.py",
        json!({"symbol": "Foo#", "kind": "function", "relationships": [{
            "symbol": "B#", "is_implementation": "false", "is_reference": true
        }]}),
    ));
    assert_eq!(result.edges[0].relation, "scip_ref");
}

#[test]
fn test_relationship_int_flag_is_ignored() {
    let result = ingest(one_symbol_at(
        "a.py",
        json!({"symbol": "Foo#", "kind": "function", "relationships": [{
            "symbol": "B#", "is_implementation": 1, "is_reference": true
        }]}),
    ));
    assert_eq!(result.edges[0].relation, "scip_ref");
}

#[test]
fn test_relationship_boolean_true_routes_correctly() {
    for (flag, relation) in [
        ("is_implementation", "scip_impl"),
        ("is_type_definition", "scip_typed"),
        ("is_definition", "scip_def"),
        ("is_reference", "scip_ref"),
    ] {
        let mut relationship = Map::new();
        relationship.insert("symbol".into(), json!("B#"));
        relationship.insert(flag.into(), json!(true));
        let result = ingest(one_symbol_at(
            "a.py",
            json!({"symbol": "Foo#", "kind": "function", "relationships": [relationship]}),
        ));
        assert_eq!(result.edges[0].relation, relation, "flag={flag}");
    }
}

#[test]
fn test_occurrence_bool_line_falls_back_to_zero() {
    let result = ingest(one_symbol_at(
        "a.py",
        json!({"symbol": "Foo#", "kind": "function", "occurrences": [{"range": [true, 0, true, 1]}]}),
    ));
    assert_eq!(result.nodes[0].source_location.as_deref(), Some(""));
}

#[test]
fn test_duplicate_same_document_definition_does_not_create_false_ambiguity() {
    let result = ingest(json!({"documents": [
        {"relative_path": "a.py", "symbols": [
            {"symbol": "Helper#", "kind": "function"},
            {"symbol": "Helper#", "kind": "function"}
        ]},
        {"relative_path": "b.py", "symbols": [{"symbol": "Caller#", "kind": "function", "relationships": [{"symbol": "Helper#", "is_reference": true}]}]}
    ]}));
    let helpers = result
        .nodes
        .iter()
        .filter(|node| metadata(node)["scip_symbol"] == "Helper#")
        .collect::<Vec<_>>();
    assert_eq!(helpers.len(), 1);
    assert_eq!(helpers[0].source_file, "a.py");
    assert_eq!(metadata(helpers[0])["scip_kind"], "function");
    assert_eq!(result.edges[0].target, helpers[0].id);
}

#[test]
fn test_ingest_node_metadata_html_escaped() {
    let result = ingest(one_symbol_at(
        "src/x.py",
        json!({
            "symbol": "python/x.py:Evil#",
            "kind": "class",
            "display_name": "Evil",
            "documentation": ["<script>alert('xss')</script>"],
            "occurrences": [{"range": [1, 0, 1, 5]}]
        }),
    ));
    let description = metadata(&result.nodes[0])["scip_description"]
        .as_str()
        .unwrap();
    assert!(!description.contains("<script>"));
    assert!(description.contains("&lt;script&gt;"));
}

#[test]
fn test_ingest_node_metadata_control_chars_stripped() {
    let result = ingest(one_symbol_at(
        "src/x.py",
        json!({
            "symbol": "python/x.py:Func#",
            "kind": "function",
            "display_name": "Func",
            "documentation": ["before\u{0000}mid\u{001f}after"],
            "occurrences": [{"range": [1, 0, 1, 5]}]
        }),
    ));
    let description = metadata(&result.nodes[0])["scip_description"]
        .as_str()
        .unwrap();
    assert!(!description.contains(['\u{0000}', '\u{001f}']));
    assert!(description.contains("beforemidafter"));
}

#[test]
fn test_ingest_relationship_metadata_sanitized() {
    let result = ingest(one_symbol_at(
        "src/a.py",
        json!({
            "symbol": "python/a.py:Caller#",
            "kind": "function",
            "display_name": "Caller",
            "occurrences": [{"range": [1, 0, 1, 5]}],
            "relationships": [{
                "symbol": "python/a.py:Helper#",
                "is_reference": true,
                "label": "<img src=x onerror=alert(1)>"
            }]
        }),
    ));
    let relationship = edge_metadata(&result.edges[0])["scip_relationship"]
        .as_object()
        .unwrap();
    let label = relationship["label"].as_str().unwrap();
    assert!(!label.contains("<img"));
    assert!(label.contains("&lt;img"));
}
