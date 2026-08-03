//! One-to-one port of pinned Graphify `tests/test_pascal_resolution.py`.

use graphoxide_core::{Confidence, Edge, Extraction};
use graphoxide_extract::{extract, extract_files};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/upstream/pascal_cross_file"
);

fn fixture(name: &str) -> PathBuf {
    Path::new(FIXTURES).join(name)
}

fn combine(extractions: Vec<Extraction>) -> Extraction {
    Extraction {
        nodes: extractions
            .iter()
            .flat_map(|extraction| extraction.nodes.iter().cloned())
            .collect(),
        edges: extractions
            .iter()
            .flat_map(|extraction| extraction.edges.iter().cloned())
            .collect(),
        hyperedges: Vec::new(),
    }
}

fn extract_corpus(names: &[&str]) -> Extraction {
    let cache = TempDir::new().expect("temporary Pascal cache");
    let paths: Vec<_> = names.iter().map(|name| fixture(name)).collect();
    combine(
        extract_files(&paths, Some(cache.path()), true)
            .expect("extract Pascal corpus")
            .extractions,
    )
}

fn call_edge<'a>(
    result: &'a Extraction,
    source_label: &str,
    target_label: &str,
) -> Option<&'a Edge> {
    let labels: HashMap<_, _> = result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect();
    result.edges.iter().find(|edge| {
        edge.relation == "calls"
            && labels.get(edge.true_source()) == Some(&source_label)
            && labels.get(edge.true_target()) == Some(&target_label)
    })
}

#[test]
fn test_single_file_extraction_reports_unresolved_inherited_call() {
    let result = extract(&fixture("DerivedGadget.pas")).expect("extract derived Pascal unit");
    let run = result
        .nodes
        .iter()
        .find(|node| node.label == "Run()")
        .expect("Run method");
    assert!(call_edge(&result, "Run()", "Prepare()").is_none());
    let raw = result.edges.iter().find(|edge| {
        edge.relation == "__pascal_raw_call"
            && edge.true_source() == run.id
            && edge.extra.get("callee").and_then(|value| value.as_str()) == Some("prepare")
    });
    assert!(
        raw.is_some(),
        "unresolved inherited call was silently dropped"
    );
}

#[test]
fn test_calls_resolve_across_files_via_inherits_chain() {
    let result = extract_corpus(&["BaseGadget.pas", "DerivedGadget.pas"]);
    let edge = call_edge(&result, "Run()", "Prepare()").expect("resolved inherited call");
    assert_eq!(edge.confidence, Confidence::Extracted);
    assert_eq!(
        edge.extra.get("context").and_then(|value| value.as_str()),
        Some("call")
    );
    // Graphify v0.9.32 reports L17 by adding a body-relative offset to the
    // declaration line. The call is physically on L18; keep the corrected span.
    assert_eq!(
        edge.extra
            .get("source_location")
            .and_then(|value| value.as_str()),
        Some("L18")
    );
}

#[test]
fn test_cross_file_calls_do_not_cross_unrelated_classes() {
    let result = extract_corpus(&["BaseGadget.pas", "OtherGadget.pas", "DerivedGadget.pas"]);
    let edge = call_edge(&result, "Run()", "Prepare()").expect("resolved inherited call");
    let target = result
        .nodes
        .iter()
        .find(|node| node.id == edge.true_target())
        .expect("resolved target node");
    assert!(target.source_file.contains("BaseGadget.pas"));
    assert!(!target.source_file.contains("OtherGadget.pas"));
}

#[test]
fn test_pascal_resolver_registered() {
    let result = extract_corpus(&["BaseGadget.pas", "DerivedGadget.pas"]);
    let edge = call_edge(&result, "Run()", "Prepare()").expect("registered Pascal resolver");
    assert_eq!(
        edge.extra
            .get("metadata")
            .and_then(|value| value.get("resolver"))
            .and_then(|value| value.as_str()),
        Some("pascal_inherited_calls")
    );
    assert!(
        result
            .edges
            .iter()
            .all(|edge| edge.relation != "__pascal_raw_call"),
        "registered corpus resolver must consume private raw-call facts"
    );
}
