//! One-to-one executable port of the nine parametrized god-node cases in
//! pinned Graphify `tests/test_swift_builtin_noise.py`.

use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_graph::god_nodes;
use std::collections::BTreeMap;

fn node(id: &str, label: &str, source_file: &str) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: (!source_file.is_empty()).then(|| "L1".into()),
        community: None,
        extra: BTreeMap::new(),
    }
}

fn edge(source: &str, target: &str, source_file: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "references".into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: BTreeMap::from([
            ("weight".into(), 1.0.into()),
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
        ]),
    }
}

fn assert_swift_builtin_filtered(builtin_label: &str) {
    let mut graph = KnowledgeGraph {
        directed: false,
        multigraph: false,
        nodes: vec![
            node("real_node", "AudioStreamer", "Sources/AudioStreamer.swift"),
            node("builtin_node", builtin_label, ""),
        ],
        links: Vec::new(),
        hyperedges: Vec::new(),
        extra: BTreeMap::new(),
    };
    for index in 0..20 {
        let id = format!("user_type_{index}");
        let source = format!("Sources/Feature{index}.swift");
        graph
            .nodes
            .push(node(&id, &format!("Feature{index}"), &source));
        graph.links.push(edge(&id, "builtin_node", &source));
    }
    graph.links.push(Edge {
        relation: "calls".into(),
        ..edge("real_node", "user_type_0", "Sources/AudioStreamer.swift")
    });

    let ids: Vec<_> = god_nodes(&graph, 10)
        .into_iter()
        .map(|result| result.id)
        .collect();
    assert!(
        !ids.iter().any(|id| id == "builtin_node"),
        "Swift builtin {builtin_label:?} appeared in god_nodes(): {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id == "real_node"),
        "project abstraction was displaced by {builtin_label:?}: {ids:?}"
    );
}

macro_rules! builtin_case {
    ($name:ident, $label:literal) => {
        #[test]
        fn $name() {
            assert_swift_builtin_filtered($label);
        }
    };
}

builtin_case!(
    test_god_nodes_excludes_swift_builtin_labels_foundation,
    "Foundation"
);
builtin_case!(
    test_god_nodes_excludes_swift_builtin_labels_swiftui,
    "SwiftUI"
);
builtin_case!(
    test_god_nodes_excludes_swift_builtin_labels_nslock,
    "NSLock"
);
builtin_case!(test_god_nodes_excludes_swift_builtin_labels_data, "Data");
builtin_case!(test_god_nodes_excludes_swift_builtin_labels_view, "View");
builtin_case!(
    test_god_nodes_excludes_swift_builtin_labels_sendable,
    "Sendable"
);
builtin_case!(
    test_god_nodes_excludes_swift_builtin_labels_codable,
    "Codable"
);
builtin_case!(
    test_god_nodes_excludes_swift_builtin_labels_dispatchqueue,
    "DispatchQueue"
);
builtin_case!(test_god_nodes_excludes_swift_builtin_labels_color, "Color");
