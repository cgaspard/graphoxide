use graphoxide_graph::{
    diagnose_extraction, diagnose_file, diagnose_file_with_cap, format_diagnostic_json,
    format_diagnostic_report, scan_producer_suppression_sites, DiagnosticOptions,
};
use serde_json::{json, Value};
use std::fs;
use tempfile::tempdir;

fn diagnostic_fixture() -> Value {
    json!({
        "nodes": [
            {"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"},
            {"id": "b", "label": "B", "file_type": "code", "source_file": "b.py"},
            {"id": "c", "label": "C", "file_type": "code", "source_file": "c.py"}
        ],
        "edges": [
            {
                "source": "a", "target": "b", "relation": "calls",
                "confidence": "EXTRACTED", "source_file": "a.py",
                "source_location": "L1", "context": "call"
            },
            {
                "source": "a", "target": "b", "relation": "imports",
                "confidence": "EXTRACTED", "source_file": "a.py",
                "source_location": "L2", "context": "import"
            },
            {
                "source": "a", "target": "b", "relation": "calls",
                "confidence": "INFERRED", "source_file": "a.py",
                "source_location": "L3", "context": "call"
            },
            {
                "source": "a", "target": "b", "relation": "calls",
                "confidence": "EXTRACTED", "source_file": "a.py",
                "source_location": "L1", "context": "call"
            },
            {
                "source": "a", "target": "missing", "relation": "calls",
                "confidence": "EXTRACTED", "source_file": "a.py"
            },
            {
                "source": "a", "relation": "calls", "confidence": "EXTRACTED",
                "source_file": "a.py"
            },
            {
                "source": "c", "target": "c", "relation": "references",
                "confidence": "EXTRACTED", "source_file": "c.py"
            }
        ]
    })
}

fn options(directed: bool, max_examples: usize) -> DiagnosticOptions {
    DiagnosticOptions {
        directed,
        max_examples,
        extract_path: None,
    }
}

#[test]
fn test_diagnose_extraction_categorizes_same_endpoint_collapse() {
    let summary = diagnose_extraction(&diagnostic_fixture(), &options(true, 5));
    assert_eq!(summary.node_count, 3);
    assert_eq!(summary.raw_edge_count, 7);
    assert_eq!(summary.valid_candidate_edges, 5);
    assert_eq!(summary.missing_endpoint_edges, 1);
    assert_eq!(summary.dangling_endpoint_edges, 1);
    assert_eq!(summary.self_loop_edges, 1);
    assert_eq!(summary.exact_duplicate_edges, 1);
    assert_eq!(summary.directed_unique_endpoint_pairs, 2);
    assert_eq!(summary.directed_same_endpoint_collapsed_edges, 3);
    assert_eq!(summary.same_endpoint_group_count, 1);
    assert_eq!(summary.relation_variant_groups, 1);
    assert_eq!(summary.source_location_variant_groups, 1);
    assert_eq!(summary.post_build_graph_type, "DiGraph");
    assert_eq!(summary.post_build_edge_count, Some(2));
}

#[test]
fn test_diagnose_extraction_accepts_node_link_links_key() {
    let mut fixture = diagnostic_fixture();
    let object = fixture.as_object_mut().unwrap();
    let edges = object.remove("edges").unwrap();
    object.insert("links".into(), edges);
    let summary = diagnose_extraction(&fixture, &options(true, 5));
    assert_eq!(summary.raw_edge_count, 7);
    assert_eq!(summary.directed_same_endpoint_collapsed_edges, 3);
}

#[test]
fn test_diagnose_extraction_does_not_mutate_input() {
    let fixture = diagnostic_fixture();
    let original = fixture.clone();
    let _ = diagnose_extraction(&fixture, &options(true, 5));
    assert_eq!(fixture, original);
}

#[test]
fn test_diagnose_extraction_handles_malformed_shapes_without_crashing() {
    let fixture = json!({
        "nodes": [
            {"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"},
            ["not", "a", "node"],
            {"id": "b", "label": "B", "file_type": "code", "source_file": "b.py"}
        ],
        "edges": [
            null,
            ["not", "an", "edge"],
            {"from": "a", "to": "b", "relation": "legacy_from_to"},
            {"source": "a", "target": {"unhashable": "target"}, "relation": "bad-target"},
            {"source": "a", "target": "missing", "relation": "dangling"},
            {"source": "", "target": "b", "relation": "missing-source"}
        ]
    });
    let summary = diagnose_extraction(&fixture, &options(true, 5));
    assert_eq!(summary.node_count, 2);
    assert_eq!(summary.raw_edge_count, 6);
    assert_eq!(summary.non_object_edges, 2);
    assert_eq!(summary.missing_endpoint_edges, 1);
    assert_eq!(summary.dangling_endpoint_edges, 2);
    assert_eq!(summary.valid_candidate_edges, 1);
    assert!(summary.post_build_error.starts_with("TypeError:"));
}

#[test]
fn test_diagnose_extraction_handles_non_list_nodes_and_edges() {
    let summary = diagnose_extraction(
        &json!({"nodes": {"id": "a"}, "edges": {"source": "a", "target": "b"}}),
        &options(true, 5),
    );
    assert_eq!(summary.node_count, 0);
    assert_eq!(summary.raw_edge_count, 0);
    assert_eq!(summary.valid_candidate_edges, 0);
}

#[test]
fn test_diagnose_extraction_bounds_examples() {
    let summary = diagnose_extraction(&diagnostic_fixture(), &options(true, 0));
    assert_eq!(summary.directed_same_endpoint_collapsed_edges, 3);
    assert!(summary.examples.is_empty());
}

#[test]
fn test_diagnose_extraction_stops_examples_at_requested_limit() {
    let mut fixture = diagnostic_fixture();
    fixture["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "d", "label": "D", "file_type": "code", "source_file": "d.py"}));
    fixture["edges"].as_array_mut().unwrap().extend([
        json!({"source": "b", "target": "d", "relation": "imports", "source_file": "b.py"}),
        json!({"source": "b", "target": "d", "relation": "calls", "source_file": "b.py"}),
    ]);
    let summary = diagnose_extraction(&fixture, &options(true, 1));
    assert_eq!(summary.same_endpoint_group_count, 2);
    assert_eq!(summary.examples.len(), 1);
}

#[test]
fn test_diagnose_extraction_defaults_raw_inputs_to_directed() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("raw-extraction.json");
    fs::write(&path, diagnostic_fixture().to_string()).unwrap();
    let summary = diagnose_file(&path, None, 5, None).unwrap();
    assert_eq!(summary.effective_directed, Some(true));
    assert_eq!(summary.post_build_graph_type, "DiGraph");
}

#[test]
fn test_diagnose_file_reads_json_and_formats_report() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("graph.json");
    fs::write(&path, diagnostic_fixture().to_string()).unwrap();
    let summary = diagnose_file(&path, Some(true), 2, None).unwrap();
    let report = format_diagnostic_report(&summary);
    assert_eq!(summary.input_path.as_deref(), path.to_str());
    assert!(report.contains("[graphoxide] MultiDiGraph edge-collapse diagnostic"));
    assert!(report.contains("directed_same_endpoint_collapsed_edges: 3"));
    assert!(report.contains("relation_variant_groups: 1"));
    assert!(report.contains("producer_suppression_sites:"));
    assert!(report.contains("examples:"));
    assert!(report.contains("a -> b"));
}

#[test]
fn test_format_diagnostic_report_includes_build_and_suppression_errors() {
    let temporary = tempdir().unwrap();
    let missing = temporary.path().join("missing-extract.rs");
    let fixture = json!({
        "nodes": [
            {"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"},
            ["not", "a", "node"]
        ],
        "edges": []
    });
    let mut diagnostic_options = options(true, 5);
    diagnostic_options.extract_path = Some(missing);
    let summary = diagnose_extraction(&fixture, &diagnostic_options);
    let report = format_diagnostic_report(&summary);
    assert!(report.contains("post_build_error: TypeError:"));
    assert!(report.contains("producer_suppression_error: file not found"));
}

#[test]
fn test_diagnostic_json_report_is_serializable() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("graph.json");
    fs::write(&path, diagnostic_fixture().to_string()).unwrap();
    let summary = diagnose_file(&path, Some(true), 5, None).unwrap();
    let payload = format_diagnostic_json(&summary);
    assert_eq!(payload["schema_version"], 1);
    assert_eq!(payload["summary"]["raw_edge_count"], 7);
    assert!(payload.get("producer_suppression").is_some());
    serde_json::to_string(&payload).unwrap();
}

#[test]
fn test_scan_producer_suppression_sites_finds_seen_sets() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("extract.rs");
    fs::write(
        &path,
        "seen_call_pairs: set[tuple[str, str]] = set()\n\
         seen_static_ref_pairs: set[tuple[str, str, str]] = set()\n\
         other = set()\n",
    )
    .unwrap();
    let result = scan_producer_suppression_sites(path);
    assert_eq!(result.total_sites, 2);
    assert_eq!(result.sites[0].name, "seen_call_pairs");
    assert_eq!(result.sites[0].tuple_arity, 2);
    assert_eq!(result.sites[1].tuple_arity, 3);
}

#[test]
fn test_scan_producer_suppression_sites_handles_unknown_tuple_arity() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("extract.rs");
    fs::write(&path, "seen_blank: set[tuple[ ]] = set()\n").unwrap();
    let result = scan_producer_suppression_sites(path);
    assert_eq!(result.total_sites, 1);
    assert_eq!(result.sites[0].tuple_arity, 0);
}

#[test]
fn test_diagnose_file_rejects_oversized_graph() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("graph.json");
    fs::write(&path, diagnostic_fixture().to_string()).unwrap();
    let error = diagnose_file_with_cap(&path, None, 5, None, 16).unwrap_err();
    assert!(error.to_string().contains("exceeds"));
}

#[test]
fn test_diagnose_file_rejects_non_object_json() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("graph.json");
    fs::write(&path, "[]").unwrap();
    let error = diagnose_file(&path, None, 5, None).unwrap_err();
    assert!(error.to_string().contains("JSON object"));
}

#[test]
fn test_diagnose_file_defaults_to_json_directed_flag() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("graph.json");
    let mut fixture = diagnostic_fixture();
    fixture["directed"] = json!(false);
    fs::write(&path, fixture.to_string()).unwrap();
    let summary = diagnose_file(&path, None, 5, None).unwrap();
    assert_eq!(summary.effective_directed, Some(false));
    assert_eq!(summary.post_build_graph_type, "Graph");
}

#[test]
fn test_diagnose_file_explicit_directed_override() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("graph.json");
    let mut fixture = diagnostic_fixture();
    fixture["directed"] = json!(false);
    fs::write(&path, fixture.to_string()).unwrap();
    let summary = diagnose_file(&path, Some(true), 5, None).unwrap();
    assert_eq!(summary.effective_directed, Some(true));
    assert_eq!(summary.post_build_graph_type, "DiGraph");
}

#[test]
fn test_scan_producer_suppression_sites_reports_missing_file() {
    let temporary = tempdir().unwrap();
    let result = scan_producer_suppression_sites(temporary.path().join("missing-extract.rs"));
    assert_eq!(result.total_sites, 0);
    assert!(result.sites.is_empty());
    assert_eq!(result.error, "file not found");
}
