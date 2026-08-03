use graphoxide_core::{Confidence, KnowledgeGraph};
use graphoxide_export::report::render_report;
use graphoxide_graph::{Analysis, Surprise};

fn report(relation: &str) -> String {
    let analysis = Analysis {
        surprising_connections: vec![Surprise {
            source: "validate_input".into(),
            target: "check_input".into(),
            source_files: ["auth/validators.py".into(), "api/checks.py".into()],
            confidence: Confidence::Inferred,
            confidence_score: Some(0.82),
            relation: relation.into(),
            why: Some("semantically similar concepts with no structural link".into()),
            note: None,
        }],
        ..Analysis::default()
    };
    render_report(&KnowledgeGraph::default(), &analysis)
}

#[test]
fn test_report_renders_semantically_similar_tag() {
    assert!(report("semantically_similar_to").contains("[semantically similar]"));
}

#[test]
fn test_report_semantic_tag_on_correct_line() {
    let report = report("semantically_similar_to");
    let line = report
        .lines()
        .find(|line| line.contains("semantically_similar_to"))
        .expect("semantic relation line");
    assert!(line.contains("[semantically similar]"));
}

#[test]
fn test_report_no_semantic_tag_for_other_relations() {
    assert!(!report("references").contains("[semantically similar]"));
}
