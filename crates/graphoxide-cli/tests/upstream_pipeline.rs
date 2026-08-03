use graphoxide_core::{Confidence, Extraction, KnowledgeGraph};
use graphoxide_export::{DetectionSummary, ReportOptions, TokenCost, VaultOptions};
use graphoxide_graph::{GodNode, Surprise};
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::TempDir;

struct PipelineResult {
    detection: graphoxide_extract::detect::DetectResult,
    extraction: Extraction,
    graph: KnowledgeGraph,
    communities: BTreeMap<i64, Vec<String>>,
    cohesion: BTreeMap<i64, f64>,
    gods: Vec<GodNode>,
    surprises: Vec<Surprise>,
    questions: Vec<graphoxide_graph::SuggestedQuestion>,
    report: String,
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn corpus(root: &Path) {
    write(
        &root.join("app.py"),
        r#"from util import helper

class Engine:
    def start(self):
        return helper()

def run():
    engine = Engine()
    return engine.start()
"#,
    );
    write(
        &root.join("util.py"),
        "def helper():\n    return 42\n\ndef format_result(value):\n    return str(value)\n",
    );
    write(
        &root.join("README.md"),
        "# Architecture\n\nThe application delegates work to the utility module.\n",
    );
}

fn flatten(extractions: Vec<Extraction>) -> Extraction {
    let mut output = Extraction::default();
    for extraction in extractions {
        output.nodes.extend(extraction.nodes);
        output.edges.extend(extraction.edges);
        output.hyperedges.extend(extraction.hyperedges);
    }
    output
}

fn run_pipeline(root: &Path, output: &Path) -> PipelineResult {
    corpus(root);

    let detection = graphoxide_extract::detect::detect(
        root,
        &graphoxide_extract::detect::DetectOptions::default(),
    )
    .unwrap();
    assert!(detection.total_files > 0);
    let code_files = detection
        .files
        .get("code")
        .into_iter()
        .flatten()
        .map(std::path::PathBuf::from)
        .collect::<Vec<_>>();
    assert!(!code_files.is_empty());

    let extracted = graphoxide_extract::extract_files(&code_files, Some(root), true).unwrap();
    let extraction = flatten(extracted.extractions);
    assert!(!extraction.nodes.is_empty());
    assert!(!extraction.edges.is_empty());

    let mut graph = graphoxide_graph::build_graph(std::slice::from_ref(&extraction)).unwrap();
    assert!(!graph.nodes.is_empty());
    assert!(!graph.links.is_empty());
    graphoxide_graph::cluster(&mut graph).unwrap();
    let communities = graphoxide_graph::communities(&graph);
    assert!(!communities.is_empty());
    let cohesion = graphoxide_graph::score_all(&graph, &communities);
    assert_eq!(cohesion.len(), communities.len());
    assert!(cohesion.values().all(|score| (0.0..=1.0).contains(score)));

    let gods = graphoxide_graph::god_nodes(&graph, 10);
    assert!(!gods.is_empty());
    let surprises = graphoxide_graph::surprising_connections(&graph, &communities, 10);
    let labels = communities
        .keys()
        .map(|community| (*community, format!("Group {community}")))
        .collect::<BTreeMap<_, _>>();
    let questions = graphoxide_graph::suggest_questions(&graph, &communities, &labels, 10);
    let analysis = graphoxide_graph::Analysis {
        god_nodes: gods.clone(),
        surprising_connections: surprises.clone(),
        suggested_questions: questions
            .iter()
            .filter_map(|question| question.question.clone())
            .collect(),
    };
    let report = graphoxide_export::render_report_with_options(
        &graph,
        &analysis,
        &ReportOptions {
            root: root.to_string_lossy().into_owned(),
            detection: DetectionSummary {
                total_files: detection.total_files,
                total_words: detection.total_words,
                warning: detection.warning.clone(),
            },
            tokens: TokenCost::default(),
            cohesion: cohesion.clone(),
            ..ReportOptions::default()
        },
    );
    assert!(report.contains("God Nodes"));
    assert!(report.contains("Communities"));
    assert!(report.len() > 100);

    fs::create_dir_all(output).unwrap();
    let json_path = output.join("graph.json");
    graphoxide_core::write_graph_atomic(&json_path, &graph, true).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&json_path).unwrap()).unwrap();
    assert!(json["nodes"].is_array() && json["links"].is_array());
    assert!(json["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|node| node.get("community").is_some()));

    let html_path = output.join("graph.html");
    let html = graphoxide_export::render_html(&graph).unwrap();
    graphoxide_core::write_text_atomic(&html_path, &html).unwrap();
    assert!(html.contains("vis-network"));
    assert!(html.contains("RAW_NODES"));

    let vault = output.join("obsidian");
    let notes = graphoxide_export::export_vault_with_options(
        &graph,
        &communities,
        &vault,
        &VaultOptions {
            community_labels: labels,
            cohesion: cohesion.clone(),
        },
    )
    .unwrap();
    assert!(notes > 0);
    assert!(vault.join(".obsidian/graph.json").is_file());
    assert!(fs::read_dir(&vault)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| { entry.path().extension().and_then(|value| value.to_str()) == Some("md") }));

    PipelineResult {
        detection,
        extraction,
        graph,
        communities,
        cohesion,
        gods,
        surprises,
        questions,
        report,
    }
}

#[test]
fn test_pipeline_runs_end_to_end() {
    let fixture = TempDir::new().unwrap();
    let result = run_pipeline(
        fixture.path(),
        &fixture.path().join("graphoxide-out/pipeline-artifacts"),
    );
    assert!(!result.graph.nodes.is_empty());
    assert_eq!(result.cohesion.len(), result.communities.len());
    assert!(result.surprises.len() <= result.graph.links.len());
    assert!(result.questions.len() <= 10);
}

#[test]
fn test_pipeline_graph_has_edges() {
    let fixture = TempDir::new().unwrap();
    let result = run_pipeline(
        fixture.path(),
        &fixture.path().join("graphoxide-out/pipeline-artifacts"),
    );
    assert!(!result.graph.links.is_empty());
}

#[test]
fn test_pipeline_all_nodes_have_community() {
    let fixture = TempDir::new().unwrap();
    let result = run_pipeline(
        fixture.path(),
        &fixture.path().join("graphoxide-out/pipeline-artifacts"),
    );
    let assigned = result
        .communities
        .values()
        .flatten()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(result
        .graph
        .nodes
        .iter()
        .all(|node| assigned.contains(node.id.as_str())));
}

#[test]
fn test_pipeline_report_mentions_top_god_node() {
    let fixture = TempDir::new().unwrap();
    let result = run_pipeline(
        fixture.path(),
        &fixture.path().join("graphoxide-out/pipeline-artifacts"),
    );
    assert!(result.report.contains(&result.gods[0].label));
}

#[test]
fn test_pipeline_detection_finds_code_and_docs() {
    let fixture = TempDir::new().unwrap();
    let result = run_pipeline(
        fixture.path(),
        &fixture.path().join("graphoxide-out/pipeline-artifacts"),
    );
    assert!(result
        .detection
        .files
        .get("code")
        .is_some_and(|files| !files.is_empty()));
    assert!(result
        .detection
        .files
        .get("document")
        .is_some_and(|files| !files.is_empty()));
}

#[test]
fn test_pipeline_incremental_update() {
    let fixture = TempDir::new().unwrap();
    let first = run_pipeline(fixture.path(), &fixture.path().join("graphoxide-out/first"));
    let second = run_pipeline(
        fixture.path(),
        &fixture.path().join("graphoxide-out/second"),
    );
    assert_eq!(first.graph.nodes.len(), second.graph.nodes.len());
    assert_eq!(first.graph.links.len(), second.graph.links.len());
}

#[test]
fn test_pipeline_extraction_confidence_labels() {
    let fixture = TempDir::new().unwrap();
    let result = run_pipeline(
        fixture.path(),
        &fixture.path().join("graphoxide-out/pipeline-artifacts"),
    );
    for edge in result.extraction.edges {
        assert!(matches!(
            edge.confidence,
            Confidence::Extracted | Confidence::Inferred | Confidence::Ambiguous
        ));
        let serialized = serde_json::to_value(edge.confidence).unwrap();
        assert!(matches!(
            serialized.as_str(),
            Some("EXTRACTED" | "INFERRED" | "AMBIGUOUS")
        ));
    }
}

#[test]
fn test_pipeline_no_self_loops() {
    let fixture = TempDir::new().unwrap();
    let result = run_pipeline(
        fixture.path(),
        &fixture.path().join("graphoxide-out/pipeline-artifacts"),
    );
    assert!(result
        .graph
        .links
        .iter()
        .all(|edge| edge.true_source() != edge.true_target()));
}
