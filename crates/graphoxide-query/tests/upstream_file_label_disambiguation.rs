use graphoxide_core::{Extraction, Node};
use graphoxide_graph::{build_graph, is_file_node_label};
use graphoxide_query::{find_node, GraphIndex};
use std::collections::{BTreeMap, BTreeSet};

fn file_node(id: &str, source_file: &str) -> Node {
    Node {
        id: id.into(),
        label: source_file.rsplit('/').next().unwrap_or_default().into(),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: None,
        community: None,
        extra: BTreeMap::new(),
    }
}

#[test]
fn test_end_to_end_build_and_lookup() {
    let graph = build_graph(&[Extraction {
        nodes: vec![
            file_node("po", "supabase/functions/process-order/index.ts"),
            file_node("sr", "supabase/functions/send-receipt/index.ts"),
            file_node("main", "main.ts"),
        ],
        ..Extraction::default()
    }])
    .unwrap();

    let file_labels: BTreeSet<_> = graph
        .nodes
        .iter()
        .filter(|node| is_file_node_label(&node.label, &node.source_file))
        .map(|node| node.label.as_str())
        .collect();
    assert!(file_labels.contains("process-order/index.ts"));
    assert!(file_labels.contains("send-receipt/index.ts"));
    assert!(file_labels.contains("main.ts"));

    let index = GraphIndex::new(&graph);
    assert!(!find_node(&index, "process-order").is_empty());
    let matches = find_node(&index, "process-order/index.ts");
    assert!(!matches.is_empty());
    assert_eq!(index.node(matches[0]).label, "process-order/index.ts");
}
