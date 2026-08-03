//! Executable port of upstream `test_confidence.py` (7 cases).

use graphoxide_core::{read_graph, write_graph_atomic, Confidence, Extraction};
use graphoxide_export::render_report;
use graphoxide_graph::{analyze, build_graph};
use serde_json::json;
use tempfile::tempdir;

fn extraction() -> Extraction {
    serde_json::from_value(json!({
        "nodes": [
            {"id": "n_a", "label": "A", "file_type": "code", "source_file": "a.py"},
            {"id": "n_b", "label": "B", "file_type": "code", "source_file": "b.py"},
            {"id": "n_c", "label": "C", "file_type": "document", "source_file": "c.md"},
            {"id": "n_d", "label": "D", "file_type": "document", "source_file": "d.md"}
        ],
        "edges": [
            {"source": "n_a", "target": "n_b", "relation": "calls", "confidence": "EXTRACTED", "confidence_score": 1.0, "source_file": "a.py", "weight": 1.0},
            {"source": "n_b", "target": "n_c", "relation": "implements", "confidence": "INFERRED", "confidence_score": 0.75, "source_file": "b.py", "weight": 0.8},
            {"source": "n_c", "target": "n_d", "relation": "references", "confidence": "AMBIGUOUS", "confidence_score": 0.2, "source_file": "c.md", "weight": 0.5}
        ]
    })).unwrap()
}

#[test]
fn extracted_edges_have_score_1() {
    let graph = build_graph(&[extraction()]).unwrap();
    for edge in graph
        .links
        .iter()
        .filter(|edge| edge.confidence == Confidence::Extracted)
    {
        assert_eq!(edge.extra["confidence_score"], 1.0);
    }
}

#[test]
fn inferred_edges_score_in_range() {
    let graph = build_graph(&[extraction()]).unwrap();
    let edge = graph
        .links
        .iter()
        .find(|edge| edge.confidence == Confidence::Inferred)
        .unwrap();
    let score = edge.extra["confidence_score"].as_f64().unwrap();
    assert!((0.0..=1.0).contains(&score));
}

#[test]
fn ambiguous_edges_score_at_most_04() {
    let graph = build_graph(&[extraction()]).unwrap();
    let edge = graph
        .links
        .iter()
        .find(|edge| edge.confidence == Confidence::Ambiguous)
        .unwrap();
    assert!(edge.extra["confidence_score"].as_f64().unwrap() <= 0.4);
}

#[test]
fn confidence_score_round_trip() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    let graph = build_graph(&[extraction()]).unwrap();
    write_graph_atomic(&path, &graph, true).unwrap();
    let loaded = read_graph(path).unwrap();
    assert!(!loaded.links.is_empty());
    for edge in loaded.links {
        let score = edge.extra["confidence_score"].as_f64().unwrap();
        assert!((0.0..=1.0).contains(&score));
    }
}

#[test]
fn to_json_defaults_missing_confidence_score() {
    let extraction: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id": "n_x", "label": "X", "file_type": "code", "source_file": "x.py"},
            {"id": "n_y", "label": "Y", "file_type": "code", "source_file": "y.py"},
            {"id": "n_z", "label": "Z", "file_type": "code", "source_file": "z.py"}
        ],
        "edges": [
            {"source": "n_x", "target": "n_y", "relation": "calls", "confidence": "EXTRACTED", "source_file": "x.py"},
            {"source": "n_y", "target": "n_z", "relation": "depends_on", "confidence": "INFERRED", "source_file": "y.py"}
        ]
    })).unwrap();
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.json");
    write_graph_atomic(&path, &build_graph(&[extraction]).unwrap(), true).unwrap();
    let graph = read_graph(path).unwrap();
    assert_eq!(
        graph
            .links
            .iter()
            .find(|edge| edge.confidence == Confidence::Extracted)
            .unwrap()
            .extra["confidence_score"],
        1.0
    );
    assert_eq!(
        graph
            .links
            .iter()
            .find(|edge| edge.confidence == Confidence::Inferred)
            .unwrap()
            .extra["confidence_score"],
        0.5
    );
}

#[test]
fn report_shows_avg_confidence_for_inferred() {
    let graph = build_graph(&[extraction()]).unwrap();
    let report = render_report(&graph, &analyze(&graph).unwrap());
    assert!(report.contains("avg confidence"));
    assert!(report.contains("0.75"));
}

#[test]
fn report_inferred_tag_with_score() {
    let extraction: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id": "n_p", "label": "Parser", "file_type": "code", "source_file": "parser.py"},
            {"id": "n_q", "label": "Renderer", "file_type": "code", "source_file": "renderer.py"}
        ],
        "edges": [{"source": "n_p", "target": "n_q", "relation": "feeds", "confidence": "INFERRED", "confidence_score": 0.82, "source_file": "parser.py"}]
    })).unwrap();
    let graph = build_graph(&[extraction]).unwrap();
    let report = render_report(&graph, &analyze(&graph).unwrap());
    assert!(report.contains("INFERRED 0.82"), "{report}");
}
