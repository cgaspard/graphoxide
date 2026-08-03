use graphoxide_core::{KnowledgeGraph, Node};
use graphoxide_graph::{label_communities_with, LabelingOptions};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};

fn node(id: &str) -> Node {
    Node {
        id: id.into(),
        label: id.into(),
        file_type: "code".into(),
        source_file: "sample.py".into(),
        source_location: None,
        community: None,
        extra: BTreeMap::new(),
    }
}

#[test]
fn test_label_batch_recovers_via_split_on_invalid_json() {
    let community_ids = [42, 99, 137, 201];
    let graph = KnowledgeGraph {
        nodes: community_ids
            .iter()
            .map(|community| node(&format!("node_{community}")))
            .collect(),
        ..Default::default()
    };
    let communities = community_ids
        .iter()
        .map(|community| (*community, vec![format!("node_{community}")]))
        .collect::<BTreeMap<_, _>>();
    let calls = AtomicUsize::new(0);
    let mut options = LabelingOptions::new("gemini");
    options.max_concurrency = 1;

    let (labels, _) = label_communities_with(&graph, &communities, &[], &options, |request| {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok("{this is not valid json, missing quotes".into());
        }
        let answer = request
            .prompt
            .lines()
            .filter_map(|line| line.strip_prefix("Community "))
            .filter_map(|line| line.split_once(':'))
            .filter_map(|(id, _)| id.parse::<i64>().ok())
            .map(|id| (id.to_string(), json!(format!("Label {id}"))))
            .collect::<serde_json::Map<_, _>>();
        Ok(serde_json::Value::Object(answer).to_string().into())
    })
    .unwrap();

    assert_eq!(
        labels,
        community_ids
            .iter()
            .map(|community| (*community, format!("Label {community}")))
            .collect()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}
