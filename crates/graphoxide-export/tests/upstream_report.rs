use graphoxide_core::{Extraction, KnowledgeGraph};
use graphoxide_export::{render_report_with_options, DetectionSummary, ReportOptions, TokenCost};
use graphoxide_graph::{analyze, build_graph, Analysis};
use serde_json::json;
use std::collections::BTreeMap;

fn make_inputs() -> (KnowledgeGraph, Analysis, ReportOptions) {
    let extraction: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id": "n_transformer", "label": "Transformer", "file_type": "code", "source_file": "model.py", "source_location": "L1"},
            {"id": "n_attention", "label": "MultiHeadAttention", "file_type": "code", "source_file": "model.py", "source_location": "L10"},
            {"id": "n_layernorm", "label": "LayerNorm", "file_type": "code", "source_file": "model.py", "source_location": "L20"},
            {"id": "n_concept_attn", "label": "attention mechanism", "file_type": "document", "source_file": "paper.md", "source_location": "§3.1"}
        ],
        "edges": [
            {"source": "n_transformer", "target": "n_attention", "relation": "contains", "confidence": "EXTRACTED", "source_file": "model.py", "weight": 1.0},
            {"source": "n_transformer", "target": "n_layernorm", "relation": "contains", "confidence": "EXTRACTED", "source_file": "model.py", "weight": 1.0},
            {"source": "n_attention", "target": "n_concept_attn", "relation": "implements", "confidence": "INFERRED", "source_file": "model.py", "weight": 0.8},
            {"source": "n_layernorm", "target": "n_concept_attn", "relation": "referenced", "confidence": "AMBIGUOUS", "source_file": "paper.md", "weight": 0.5}
        ]
    }))
    .unwrap();
    let mut graph = build_graph(&[extraction]).unwrap();
    for node in &mut graph.nodes {
        node.community = Some(0);
        node.extra
            .insert("community_name".into(), "Community 0".into());
    }
    let analysis = analyze(&graph).unwrap();
    let options = ReportOptions {
        root: "./project".into(),
        detection: DetectionSummary {
            total_files: 4,
            total_words: 62_400,
            warning: None,
        },
        tokens: TokenCost {
            input: 1_200,
            output: 340,
        },
        cohesion: BTreeMap::from([(0, 0.5)]),
        ..ReportOptions::default()
    };
    (graph, analysis, options)
}

fn report() -> String {
    let (graph, analysis, options) = make_inputs();
    render_report_with_options(&graph, &analysis, &options)
}

#[test]
fn test_report_contains_header() {
    assert!(report().contains("# Graph Report"));
}

#[test]
fn test_report_contains_corpus_check() {
    assert!(report().contains("## Corpus Check"));
}

#[test]
fn test_report_contains_god_nodes() {
    assert!(report().contains("## God Nodes"));
}

#[test]
fn test_report_contains_surprising_connections() {
    assert!(report().contains("## Surprising Connections"));
}

#[test]
fn test_report_contains_communities() {
    assert!(report().contains("## Communities"));
}

#[test]
fn test_report_contains_ambiguous_section() {
    assert!(report().contains("## Ambiguous Edges"));
}

#[test]
fn test_report_shows_token_cost() {
    let report = report();
    assert!(report.contains("Token cost"));
    assert!(report.contains("1,200"));
}

#[test]
fn test_report_shows_raw_cohesion_scores() {
    let (graph, analysis, mut options) = make_inputs();
    options.min_community_size = 1;
    let report = render_report_with_options(&graph, &analysis, &options);
    assert!(report.contains("Cohesion:"));
    assert!(!report.contains('✓'));
    assert!(!report.contains('⚠'));
}

#[test]
fn test_report_work_memory_section_present_with_overlay_and_dead_ends() {
    let (graph, analysis, mut options) = make_inputs();
    options.learning = Some(json!({
        "overlay": {
            "auth_login": {"status": "preferred", "uses": 3, "score": 2.4, "label": "login()", "stale": false},
            "redis": {"status": "tentative", "uses": 1, "score": 0.5, "label": "RedisClient", "stale": false}
        },
        "dead_ends": [{"question": "does it use websockets?", "nodes": ["WSServer"], "date": "2026-05-01"}]
    }));
    let report = render_report_with_options(&graph, &analysis, &options);
    assert!(report.contains("## Work-memory lessons"));
    assert!(report.contains("**Preferred sources**"));
    assert!(report.contains("`login()`"));
    assert!(!report.contains("RedisClient"));
    assert!(report.contains("**Known dead ends**"));
    assert!(report.contains("does it use websockets?"));
    assert!(report.contains("`WSServer`"));
}

#[test]
fn test_report_work_memory_section_absent_without_overlay() {
    let (graph, analysis, mut options) = make_inputs();
    let before = render_report_with_options(&graph, &analysis, &options);
    assert!(!before.contains("## Work-memory lessons"));
    options.learning = Some(json!({"overlay": {}, "dead_ends": []}));
    let empty = render_report_with_options(&graph, &analysis, &options);
    assert!(!empty.contains("## Work-memory lessons"));
    assert_eq!(before, empty);
}

#[test]
fn test_import_cycles_section_present_for_code_corpus() {
    assert!(report().contains("## Import Cycles"));
}

#[test]
fn test_import_cycles_section_absent_for_documents_only_corpus() {
    let extraction: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id": "d1", "label": "intro.md", "file_type": "document", "source_file": "intro.md"},
            {"id": "d2", "label": "guide.md", "file_type": "document", "source_file": "guide.md"}
        ],
        "edges": [{"source": "d1", "target": "d2", "relation": "references", "confidence": "EXTRACTED", "source_file": "intro.md"}]
    }))
    .unwrap();
    let mut graph = build_graph(&[extraction]).unwrap();
    for node in &mut graph.nodes {
        node.community = Some(0);
    }
    let analysis = analyze(&graph).unwrap();
    let report = render_report_with_options(&graph, &analysis, &ReportOptions::default());
    assert!(!report.contains("## Import Cycles"));
}

#[test]
fn test_report_hubs_are_plain_text_by_default() {
    let (mut graph, analysis, mut options) = make_inputs();
    options.min_community_size = 1;
    for node in &mut graph.nodes {
        node.extra
            .insert("community_name".into(), "Widget 0".into());
    }
    let report = render_report_with_options(&graph, &analysis, &options);
    assert!(report.contains("## Community Hubs (Navigation)"));
    assert!(!report.contains("[[_COMMUNITY_"));
    assert!(report.contains("- Widget 0"));
}

#[test]
fn test_report_hubs_use_wikilinks_when_obsidian() {
    let (mut graph, analysis, mut options) = make_inputs();
    options.min_community_size = 1;
    options.obsidian = true;
    for node in &mut graph.nodes {
        node.extra
            .insert("community_name".into(), "Widget 0".into());
    }
    let report = render_report_with_options(&graph, &analysis, &options);
    assert!(report.contains("[[_COMMUNITY_"));
}
