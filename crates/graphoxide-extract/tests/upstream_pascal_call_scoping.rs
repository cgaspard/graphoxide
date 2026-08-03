//! One-to-one port of pinned Graphify `tests/test_pascal_call_scoping.py`.

use graphoxide_core::Extraction;
use graphoxide_extract::extract;
use std::{collections::HashMap, path::Path};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/upstream/sample_scoped_calls.pas"
);

fn scoped_calls() -> Extraction {
    extract(Path::new(FIXTURE)).expect("extract scoped Pascal fixture")
}

fn method_id(result: &Extraction, class_label: &str, method_label: &str) -> String {
    let classes: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| node.label == class_label)
        .map(|node| node.id.as_str())
        .collect();
    assert_eq!(
        classes.len(),
        1,
        "expected one {class_label:?} class node; got {classes:?}"
    );
    let nodes: HashMap<_, _> = result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    result
        .edges
        .iter()
        .find(|edge| {
            edge.relation == "method"
                && edge.true_source() == classes[0]
                && nodes
                    .get(edge.true_target())
                    .is_some_and(|node| node.label == method_label)
        })
        .map(|edge| edge.true_target().to_owned())
        .unwrap_or_else(|| panic!("missing {class_label}.{method_label}"))
}

fn has_call(result: &Extraction, source: &str, target: &str) -> bool {
    result.edges.iter().any(|edge| {
        edge.relation == "calls" && edge.true_source() == source && edge.true_target() == target
    })
}

fn assert_calls_scoped_to_own_class() {
    let result = scoped_calls();
    let configure = method_id(&result, "TFirstWidget", "Configure()");
    let reset = method_id(&result, "TFirstWidget", "Reset()");
    assert!(has_call(&result, &configure, &reset));
}

fn assert_calls_do_not_cross_unrelated_classes() {
    let result = scoped_calls();
    let configure = method_id(&result, "TFirstWidget", "Configure()");
    let unrelated = method_id(&result, "TSecondWidget", "Reset()");
    assert!(!has_call(&result, &configure, &unrelated));
}

fn assert_calls_scoped_other_direction() {
    let result = scoped_calls();
    let configure = method_id(&result, "TSecondWidget", "Configure()");
    let own_reset = method_id(&result, "TSecondWidget", "Reset()");
    let unrelated = method_id(&result, "TFirstWidget", "Reset()");
    assert!(has_call(&result, &configure, &own_reset));
    assert!(!has_call(&result, &configure, &unrelated));
}

fn assert_calls_resolve_via_ancestor_chain() {
    let result = scoped_calls();
    let run = method_id(&result, "TDerivedWidget", "Run()");
    let prepare = method_id(&result, "TBaseWidget", "Prepare()");
    assert!(has_call(&result, &run, &prepare));
}

#[test]
fn test_calls_scoped_to_own_class_tree_sitter() {
    assert_calls_scoped_to_own_class();
}

#[test]
fn test_calls_scoped_to_own_class_regex_fallback() {
    assert_calls_scoped_to_own_class();
}

#[test]
fn test_calls_do_not_cross_unrelated_classes_tree_sitter() {
    assert_calls_do_not_cross_unrelated_classes();
}

#[test]
fn test_calls_do_not_cross_unrelated_classes_regex_fallback() {
    assert_calls_do_not_cross_unrelated_classes();
}

#[test]
fn test_calls_scoped_other_direction_tree_sitter() {
    assert_calls_scoped_other_direction();
}

#[test]
fn test_calls_scoped_other_direction_regex_fallback() {
    assert_calls_scoped_other_direction();
}

#[test]
fn test_calls_resolve_via_ancestor_chain_tree_sitter() {
    assert_calls_resolve_via_ancestor_chain();
}

#[test]
fn test_calls_resolve_via_ancestor_chain_regex_fallback() {
    assert_calls_resolve_via_ancestor_chain();
}
