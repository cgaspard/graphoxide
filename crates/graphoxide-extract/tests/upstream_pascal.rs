//! One-to-one executable port of pinned Graphify `tests/test_pascal.py`.

use graphoxide_core::{Confidence, Edge, Extraction};
use graphoxide_extract::{detect, extract};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/upstream");

fn fixture(name: &str) -> PathBuf {
    Path::new(FIXTURES).join(name)
}

fn extracted(name: &str) -> Extraction {
    extract(&fixture(name)).unwrap_or_else(|error| panic!("extract {name}: {error:#}"))
}

fn labels(result: &Extraction) -> Vec<&str> {
    result
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect()
}

fn relations(result: &Extraction) -> HashSet<&str> {
    result
        .edges
        .iter()
        .map(|edge| edge.relation.as_str())
        .collect()
}

fn edge_context(edge: &Edge) -> Option<&str> {
    edge.extra.get("context").and_then(|value| value.as_str())
}

fn assert_labels(result: &Extraction, expected: &[&str]) {
    let actual = labels(result);
    for expected in expected {
        assert!(
            actual.iter().any(|label| label.contains(expected)),
            "missing label containing {expected:?}; labels={actual:?}"
        );
    }
}

fn assert_context(result: &Extraction, relation: &str, context: &str) {
    let edges: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| edge.relation == relation)
        .collect();
    assert!(!edges.is_empty(), "missing {relation:?} edges");
    assert!(
        edges.iter().all(|edge| edge_context(edge) == Some(context)),
        "not all {relation:?} edges carry context={context:?}: {edges:?}"
    );
}

fn assert_no_dangling(result: &Extraction, relations: Option<&[&str]>) {
    let ids: HashSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    for edge in &result.edges {
        if relations.is_none_or(|relations| relations.contains(&edge.relation.as_str())) {
            assert!(
                ids.contains(edge.true_source()),
                "dangling edge source: {edge:?}"
            );
            assert!(
                ids.contains(edge.true_target()),
                "dangling edge target: {edge:?}"
            );
        }
    }
}

fn duplicate_edges(result: &Extraction) -> Vec<(String, String, String)> {
    let mut counts = HashMap::new();
    for edge in &result.edges {
        *counts
            .entry((
                edge.true_source().to_owned(),
                edge.true_target().to_owned(),
                edge.relation.clone(),
            ))
            .or_insert(0_usize) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(edge, count)| (count > 1).then_some(edge))
        .collect()
}

#[test]
fn test_pascal_no_error() {
    let result = extracted("sample.pas");
    assert!(!result.nodes.is_empty());
}

#[test]
fn test_pascal_finds_unit() {
    assert_labels(&extracted("sample.pas"), &["SampleUnit"]);
}

#[test]
fn test_pascal_finds_classes() {
    assert_labels(
        &extracted("sample.pas"),
        &["TBaseProcessor", "TDataProcessor"],
    );
}

#[test]
fn test_pascal_finds_interface() {
    assert_labels(&extracted("sample.pas"), &["IProcessor"]);
}

#[test]
fn test_pascal_finds_methods() {
    assert_labels(
        &extracted("sample.pas"),
        &["Process", "Initialize", "GetCount", "Reset"],
    );
}

#[test]
fn test_pascal_finds_imports() {
    assert!(relations(&extracted("sample.pas")).contains("imports"));
}

#[test]
fn test_pascal_import_edges_have_import_context() {
    assert_context(&extracted("sample.pas"), "imports", "import");
}

#[test]
fn test_pascal_finds_inherits() {
    assert!(relations(&extracted("sample.pas")).contains("inherits"));
}

#[test]
fn test_pascal_inherits_from_base() {
    let result = extracted("sample.pas");
    let labels: HashMap<_, _> = result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect();
    assert!(result.edges.iter().any(|edge| {
        edge.relation == "inherits"
            && labels
                .get(edge.true_source())
                .is_some_and(|label| label.contains("TDataProcessor"))
    }));
}

#[test]
fn test_pascal_finds_calls() {
    assert!(relations(&extracted("sample.pas")).contains("calls"));
}

#[test]
fn test_pascal_call_edges_have_call_context() {
    assert_context(&extracted("sample.pas"), "calls", "call");
}

#[test]
fn test_pascal_all_edges_extracted() {
    let structural = ["contains", "method", "inherits", "imports"];
    for edge in extracted("sample.pas")
        .edges
        .into_iter()
        .filter(|edge| structural.contains(&edge.relation.as_str()))
    {
        assert_eq!(edge.confidence, Confidence::Extracted, "edge={edge:?}");
    }
}

#[test]
fn test_pascal_no_dangling_edges() {
    assert_no_dangling(
        &extracted("sample.pas"),
        Some(&["contains", "method", "inherits", "calls"]),
    );
}

#[test]
fn test_pascal_dispatch_registered() {
    let temp = TempDir::new().expect("temporary Pascal dispatch fixture");
    let source = fs::read_to_string(fixture("sample.pas")).expect("read Pascal fixture");
    for extension in ["pas", "pp", "dpr", "dpk", "lpr", "inc"] {
        let path = temp.path().join(format!("sample.{extension}"));
        fs::write(&path, &source).expect("write Pascal dispatch fixture");
        assert!(
            !extract(&path)
                .expect("dispatch Pascal source")
                .nodes
                .is_empty(),
            "missing dispatch for .{extension}"
        );
    }
    let lfm = temp.path().join("sample.lfm");
    fs::copy(fixture("sample.lfm"), &lfm).expect("copy LFM fixture");
    assert!(!extract(&lfm).expect("dispatch LFM").nodes.is_empty());
    let lpk = temp.path().join("sample.lpk");
    fs::copy(fixture("sample.lpk"), &lpk).expect("copy LPK fixture");
    assert!(!extract(&lpk).expect("dispatch LPK").nodes.is_empty());
}

#[test]
fn test_pascal_detect_extensions_registered() {
    for extension in ["pas", "pp", "dpr", "lpr", "lfm", "lpk"] {
        assert_eq!(
            detect::classify_file(Path::new(&format!("sample.{extension}"))),
            Some(detect::FileType::Code),
            "missing detector registration for .{extension}"
        );
    }
}

#[test]
fn test_lfm_no_error() {
    assert!(!extracted("sample.lfm").nodes.is_empty());
}

#[test]
fn test_lfm_finds_root_form_class() {
    assert_labels(&extracted("sample.lfm"), &["TSampleForm"]);
}

#[test]
fn test_lfm_finds_component_classes() {
    assert_labels(
        &extracted("sample.lfm"),
        &["TPanel", "TButton", "TLabel", "TTimer"],
    );
}

#[test]
fn test_lfm_finds_event_handlers() {
    assert_labels(
        &extracted("sample.lfm"),
        &["ButtonOKClick", "TimerRefreshTimer"],
    );
}

#[test]
fn test_lfm_event_edges_have_event_context() {
    assert_context(&extracted("sample.lfm"), "references", "event");
}

#[test]
fn test_lfm_contains_edges_form_hierarchy() {
    assert!(relations(&extracted("sample.lfm")).contains("contains"));
}

#[test]
fn test_lfm_no_dangling_edges() {
    assert_no_dangling(&extracted("sample.lfm"), None);
}

#[test]
fn test_lpk_no_error() {
    assert!(!extracted("sample.lpk").nodes.is_empty());
}

#[test]
fn test_lpk_finds_package_name() {
    assert_labels(&extracted("sample.lpk"), &["SamplePackage"]);
}

#[test]
fn test_lpk_finds_required_packages() {
    assert_labels(&extracted("sample.lpk"), &["FCL", "LCL"]);
}

#[test]
fn test_lpk_imports_edges_have_import_context() {
    assert_context(&extracted("sample.lpk"), "imports", "import");
}

#[test]
fn test_lpk_contains_listed_units() {
    assert_labels(&extracted("sample.lpk"), &["sample", "sampleutils"]);
}

#[test]
fn test_lpk_no_dangling_edges() {
    assert_no_dangling(&extracted("sample.lpk"), None);
}

#[test]
fn test_dfm_no_error() {
    assert!(!extracted("sample.dfm").nodes.is_empty());
}

#[test]
fn test_dfm_finds_root_form_class() {
    assert_labels(&extracted("sample.dfm"), &["TMainForm"]);
}

#[test]
fn test_dfm_finds_component_classes() {
    assert_labels(
        &extracted("sample.dfm"),
        &["TPanel", "TButton", "TMemo", "TStatusBar"],
    );
}

#[test]
fn test_dfm_finds_event_handlers() {
    assert_labels(&extracted("sample.dfm"), &["FormCreate", "ButtonOKClick"]);
}

#[test]
fn test_dfm_event_edges_have_event_context() {
    assert_context(&extracted("sample.dfm"), "references", "event");
}

#[test]
fn test_dfm_contains_edges_form_hierarchy() {
    assert!(relations(&extracted("sample.dfm")).contains("contains"));
}

#[test]
fn test_dfm_no_dangling_edges() {
    assert_no_dangling(&extracted("sample.dfm"), None);
}

#[test]
fn test_dfm_binary_returns_empty_not_crash() {
    let fixture = TempDir::new().expect("temporary binary DFM fixture");
    let path = fixture.path().join("binary.dfm");
    fs::write(&path, b"\xff\x0a\x00\x00some binary data").expect("write binary DFM");
    let result = extract(&path).expect("binary DFM is a supported no-op");
    assert!(result.nodes.is_empty());
    assert!(result.edges.is_empty());
}

#[test]
fn test_dfm_dispatch_registered() {
    assert!(!extracted("sample.dfm").nodes.is_empty());
}

#[test]
fn test_dfm_detect_extension_registered() {
    assert_eq!(
        detect::classify_file(Path::new("sample.dfm")),
        Some(detect::FileType::Code)
    );
}

#[test]
fn test_pascal_no_duplicate_method_edges_tree_sitter() {
    assert_eq!(duplicate_edges(&extracted("sample.pas")), Vec::new());
}

#[test]
fn test_pascal_no_duplicate_method_edges_regex() {
    // Graphoxide's production Pascal pass is the deterministic regex parser;
    // exercise it independently under the upstream fallback test identity.
    assert_eq!(duplicate_edges(&extracted("sample.pas")), Vec::new());
}
