use graphoxide_core::{read_graph, write_graph_atomic, Confidence, Edge, Extraction, Node};
use graphoxide_graph::{
    build_graph, build_graph_from_fact_batches, build_graph_from_fact_batches_with_root,
    build_graph_with_options_and_root, build_merge, merge_raw_extraction, sort_fact_batches,
    BuildOptions, FactBatch, FactBatchKey, FactBatchLimits, FactBatchOrderError, StagedGraphOutput,
};
use serde_json::json;

fn node(id: &str) -> Node {
    Node {
        id: id.into(),
        label: id.into(),
        file_type: "code".into(),
        source_file: format!("{id}.rs"),
        source_location: Some("L1".into()),
        community: None,
        extra: Default::default(),
    }
}

fn edge(source: &str, target: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "calls".into(),
        confidence: Confidence::Extracted,
        source_file: format!("{source}.rs"),
        extra: Default::default(),
    }
}

#[test]
fn split_batches_preserve_native_extraction_order() {
    let extraction = Extraction {
        nodes: vec![node("a"), node("b")],
        edges: vec![edge("a", "b")],
        hyperedges: vec![json!({"id":"h","nodes":["a", "b"]})],
    };
    let batches = FactBatch::split_extraction(
        7,
        extraction,
        FactBatchLimits {
            max_facts: 2,
            max_estimated_bytes: 16 * 1024,
        },
    )
    .unwrap();

    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].key(), FactBatchKey::new(7, 0));
    assert_eq!(batches[1].key(), FactBatchKey::new(7, 1));
    assert_eq!(batches[0].extraction().nodes.len(), 2);
    assert_eq!(batches[1].extraction().edges.len(), 1);
    assert_eq!(batches[1].extraction().hyperedges.len(), 1);
}

#[test]
fn batch_builder_matches_existing_graph_build_for_out_of_order_completion() {
    let first = Extraction {
        nodes: vec![node("a")],
        edges: vec![],
        hyperedges: vec![],
    };
    let second = Extraction {
        nodes: vec![node("b")],
        edges: vec![edge("a", "b")],
        hyperedges: vec![],
    };
    let expected = build_graph(&[first.clone(), second.clone()]).unwrap();
    let limits = FactBatchLimits::default();
    let mut batches = FactBatch::split_extraction(1, second, limits).unwrap();
    batches.extend(FactBatch::split_extraction(0, first, limits).unwrap());

    let (actual, report) = build_graph_from_fact_batches(batches, BuildOptions::default()).unwrap();
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert!(report.nodes_accounted_for());
    assert!(report.edges_accounted_for());
}

#[test]
fn root_aware_fact_batches_match_root_aware_compatibility_build() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = root.join("src/lib.rs");
    let extraction = Extraction {
        nodes: vec![Node {
            source_file: source.display().to_string(),
            ..node("rooted")
        }],
        edges: vec![],
        hyperedges: vec![],
    };
    let expected = build_graph_with_options_and_root(
        std::slice::from_ref(&extraction),
        root,
        BuildOptions::default(),
    )
    .unwrap();
    let actual = build_graph_from_fact_batches_with_root(
        FactBatch::split_extraction(0, extraction, FactBatchLimits::default()).unwrap(),
        BuildOptions::default(),
        Some(root),
    )
    .unwrap()
    .0;
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn staged_incremental_raw_merge_matches_compatibility_incremental_build() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let graph_path = root.join("graph.json");
    let initial = vec![
        Extraction {
            nodes: vec![Node {
                label: "old_a".into(),
                ..node("a")
            }],
            edges: vec![edge("a", "b")],
            hyperedges: vec![],
        },
        Extraction {
            nodes: vec![node("b")],
            edges: vec![],
            hyperedges: vec![],
        },
    ];
    let existing = build_graph(&initial).unwrap();
    assert!(write_graph_atomic(&graph_path, &existing, true).unwrap());
    let replacement = Extraction {
        nodes: vec![Node {
            label: "new_a".into(),
            ..node("a")
        }],
        edges: vec![edge("a", "b")],
        hyperedges: vec![],
    };
    let expected = build_merge(
        std::slice::from_ref(&replacement),
        &graph_path,
        &[],
        Some(root),
    )
    .unwrap();
    let merged = merge_raw_extraction(&replacement, &graph_path, &[], Some(root)).unwrap();
    let actual = build_graph_from_fact_batches_with_root(
        FactBatch::split_extraction(0, merged, FactBatchLimits::default()).unwrap(),
        BuildOptions::default(),
        Some(root),
    )
    .unwrap()
    .0;
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn duplicate_batch_key_is_rejected() {
    let limits = FactBatchLimits::default();
    let first = FactBatch::try_new(
        FactBatchKey::new(0, 0),
        Extraction {
            nodes: vec![node("a")],
            ..Extraction::default()
        },
        limits,
    )
    .unwrap();
    let second = FactBatch::try_new(
        FactBatchKey::new(0, 0),
        Extraction {
            nodes: vec![node("b")],
            ..Extraction::default()
        },
        limits,
    )
    .unwrap();
    let mut batches = vec![second, first];
    assert_eq!(
        sort_fact_batches(&mut batches),
        Err(FactBatchOrderError::DuplicateKey(FactBatchKey::new(0, 0)))
    );
}

#[test]
fn oversized_single_fact_is_not_admitted() {
    let error = FactBatch::split_extraction(
        0,
        Extraction {
            nodes: vec![Node {
                label: "x".repeat(128),
                ..node("a")
            }],
            ..Extraction::default()
        },
        FactBatchLimits {
            max_facts: 1,
            max_estimated_bytes: 32,
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("exceeding"));
}

#[test]
fn staged_output_uses_existing_atomic_graph_writer() {
    let batch = FactBatch::try_new(
        FactBatchKey::new(0, 0),
        Extraction {
            nodes: vec![node("a")],
            ..Extraction::default()
        },
        FactBatchLimits::default(),
    )
    .unwrap();
    let staged = StagedGraphOutput::from_fact_batches([batch], BuildOptions::default()).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph.json");

    assert!(staged.commit_atomic(&path, true).unwrap());
    assert_eq!(read_graph(path).unwrap().nodes[0].id, "a");
}
