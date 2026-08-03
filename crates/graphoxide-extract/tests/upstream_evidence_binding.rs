use graphoxide_core::{validate::validate_extraction_json, FileSlice, FileUnit};
use graphoxide_extract::semantic_pipeline::{
    bind_node_evidence, extract_corpus, label_identifiers, SemanticChunkResult,
    SemanticCorpusOptions,
};
use graphoxide_graph::{diagnose_extraction, format_diagnostic_report, DiagnosticOptions};
use serde_json::{json, Value};
use std::fs;
use tempfile::tempdir;

const SOURCE: &str = "def real_function():\n    return PaymentProcessor().charge_card()\n\nclass PaymentProcessor:\n    def charge_card(self):\n        pass\n";

fn labels(nodes: &[Value]) -> std::collections::BTreeMap<&str, &serde_json::Map<String, Value>> {
    nodes
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|node| Some((node.get("label")?.as_str()?, node)))
        .collect()
}

#[test]
fn test_fabricated_code_symbol_is_downgraded() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("mod.py");
    fs::write(&source, SOURCE).unwrap();
    let mut nodes = vec![
        json!({"id":"a","label":"real_function()","file_type":"code","source_file":"mod.py"}),
        json!({"id":"b","label":"totally_fabricated_symbol()","file_type":"code","source_file":"mod.py"}),
        json!({"id":"c","label":"Payments Overview","file_type":"concept","source_file":"mod.py"}),
    ];
    assert_eq!(
        bind_node_evidence(&mut nodes, &[FileUnit::Path(source)], temporary.path()),
        1
    );
    let by_label = labels(&nodes);
    assert_eq!(
        by_label["totally_fabricated_symbol()"]["verification"],
        "unverified"
    );
    assert!(!by_label["real_function()"].contains_key("verification"));
    assert!(!by_label["Payments Overview"].contains_key("verification"));
}

#[test]
fn test_qualified_and_prettified_labels_do_not_false_positive() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("mod.py");
    fs::write(&source, SOURCE).unwrap();
    let mut nodes = vec![
        json!({"id":"a","label":"PaymentProcessor.charge_card()","file_type":"code","source_file":"mod.py"}),
        json!({"id":"b","label":"charge_card(amount, token)","file_type":"code","source_file":"mod.py"}),
    ];
    assert_eq!(
        bind_node_evidence(&mut nodes, &[FileUnit::Path(source)], temporary.path()),
        0
    );
    assert!(nodes.iter().all(|node| node.get("verification").is_none()));
}

#[test]
fn test_document_and_sourceless_nodes_are_never_flagged() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("mod.py");
    fs::write(&source, SOURCE).unwrap();
    let mut nodes = vec![
        json!({"id":"a","label":"Nonexistent Heading","file_type":"document","source_file":"mod.py"}),
        json!({"id":"b","label":"orphan_symbol()","file_type":"code"}),
    ];
    assert_eq!(
        bind_node_evidence(&mut nodes, &[FileUnit::Path(source)], temporary.path()),
        0
    );
    assert!(nodes.iter().all(|node| node.get("verification").is_none()));
}

#[test]
fn test_node_attributed_to_undispatched_file_is_left_to_out_of_scope() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("mod.py");
    fs::write(&source, SOURCE).unwrap();
    fs::write(
        temporary.path().join("other.py"),
        "def elsewhere():\n pass\n",
    )
    .unwrap();
    let mut nodes =
        vec![json!({"id":"a","label":"ghost_func()","file_type":"code","source_file":"other.py"})];
    assert_eq!(
        bind_node_evidence(&mut nodes, &[FileUnit::Path(source)], temporary.path()),
        0
    );
    assert!(nodes[0].get("verification").is_none());
}

#[test]
fn test_uncheckable_short_label_is_not_flagged() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("mod.py");
    fs::write(&source, SOURCE).unwrap();
    let mut nodes = vec![json!({
        "id":"a","label":"id()","file_type":"code","source_file":"mod.py"
    })];
    assert_eq!(
        bind_node_evidence(&mut nodes, &[FileUnit::Path(source)], temporary.path()),
        0
    );
    assert!(nodes[0].get("verification").is_none());
}

#[test]
fn test_existing_lower_confidence_is_not_overwritten() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("mod.py");
    fs::write(&source, SOURCE).unwrap();
    let mut nodes = vec![json!({
        "id":"a","label":"made_up()","file_type":"code","source_file":"mod.py","confidence":"INFERRED"
    })];
    assert_eq!(
        bind_node_evidence(&mut nodes, &[FileUnit::Path(source)], temporary.path()),
        0
    );
    assert_eq!(nodes[0]["confidence"], "INFERRED");
    assert!(nodes[0].get("verification").is_none());
}

#[test]
fn test_label_identifiers_helper() {
    assert_eq!(label_identifiers("foo()"), ["foo"]);
    assert_eq!(label_identifiers("Cls.method(x)"), ["Cls", "method"]);
    assert!(label_identifiers("id()").is_empty());
    assert!(label_identifiers("").is_empty());
}

#[test]
fn test_bind_node_evidence_returns_downgrade_count() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("mod.py");
    fs::write(&source, SOURCE).unwrap();
    let mut nodes = vec![
        json!({"id":"a","label":"real_function()","file_type":"code","source_file":"mod.py"}),
        json!({"id":"b","label":"fake_one()","file_type":"code","source_file":"mod.py"}),
        json!({"id":"c","label":"fake_two()","file_type":"code","source_file":"mod.py"}),
    ];
    assert_eq!(
        bind_node_evidence(&mut nodes, &[FileUnit::Path(source)], temporary.path()),
        2
    );
}

#[test]
fn test_evidence_binding_handles_file_slice() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("big.md");
    let text = format!("intro\n{SOURCE}\ntail\n");
    fs::write(&source, &text).unwrap();
    let unit = FileUnit::Slice(FileSlice {
        path: source,
        start: 0,
        end: text.len(),
        index: 0,
        total: 1,
    });
    let mut nodes = vec![
        json!({"id":"a","label":"real_function()","file_type":"code","source_file":"big.md"}),
        json!({"id":"b","label":"ghost_symbol()","file_type":"code","source_file":"big.md"}),
    ];
    assert_eq!(bind_node_evidence(&mut nodes, &[unit], temporary.path()), 1);
    let by_label = labels(&nodes);
    assert!(!by_label["real_function()"].contains_key("verification"));
    assert_eq!(by_label["ghost_symbol()"]["verification"], "unverified");
}

#[test]
fn test_evidence_binding_handles_absolute_source_file() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("mod.py");
    fs::write(&source, SOURCE).unwrap();
    let mut nodes = vec![json!({
        "id":"a","label":"ghost_symbol()","file_type":"code",
        "source_file":source.canonicalize().unwrap().display().to_string()
    })];
    assert_eq!(
        bind_node_evidence(&mut nodes, &[FileUnit::Path(source)], temporary.path()),
        1
    );
    assert_eq!(nodes[0]["verification"], "unverified");
}

#[test]
fn test_downgrade_emits_stderr_summary() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("mod.py");
    fs::write(&source, SOURCE).unwrap();
    let files = vec![source];
    let options = SemanticCorpusOptions {
        token_budget: None,
        chunk_size: 10,
        max_concurrency: 1,
        checkpoint: false,
        ..SemanticCorpusOptions::default()
    };
    let result = extract_corpus(
        &files,
        temporary.path(),
        &options,
        &|_| {
            Ok(SemanticChunkResult {
                nodes: vec![json!({
                    "id":"b","label":"totally_made_up_symbol()","file_type":"code","source_file":"mod.py"
                })],
                finish_reason: "stop".into(),
                ..SemanticChunkResult::default()
            })
        },
        None,
    )
    .unwrap();
    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.contains("unverified")));
}

#[test]
fn test_unverified_flag_does_not_fail_validation() {
    let errors = validate_extraction_json(&json!({
        "nodes":[{"id":"n1","label":"foo","file_type":"code","source_file":"a.md","verification":"unverified"}],
        "edges":[]
    }));
    assert!(errors
        .iter()
        .all(|error| !error.to_lowercase().contains("verification")));
}

#[test]
fn test_diagnostics_reports_unverified_node_count() {
    let extraction = json!({
        "nodes":[
            {"id":"n1","label":"ghost","file_type":"code","source_file":"a.md","verification":"unverified"},
            {"id":"n2","label":"real","file_type":"code","source_file":"a.md"}
        ],
        "edges":[]
    });
    let summary = diagnose_extraction(&extraction, &DiagnosticOptions::default());
    assert_eq!(summary.unverified_node_count, 1);
    assert!(format_diagnostic_report(&summary).contains("unverified_code_nodes: 1"));
}
