//! Executable ports of pinned Graphify `tests/test_labeling.py` library cases.

use graphoxide_core::{KnowledgeGraph, Node};
use graphoxide_graph::{
    generate_community_labels_with, label_communities_with, GodNode, LabelResponse, LabelSource,
    LabelUsage, LabelingOptions,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn node(id: &str, label: &str) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: "sample.py".into(),
        source_location: None,
        community: None,
        extra: BTreeMap::new(),
    }
}

fn graph() -> (KnowledgeGraph, BTreeMap<i64, Vec<String>>) {
    (
        KnowledgeGraph {
            nodes: vec![
                node("order_place", "place_order"),
                node("order_repo", "OrderRepository"),
                node("pay_charge", "charge_card"),
                node("pay_stripe", "StripeClient"),
            ],
            ..Default::default()
        },
        BTreeMap::from([
            (0, vec!["order_place".into(), "order_repo".into()]),
            (1, vec!["pay_charge".into(), "pay_stripe".into()]),
        ]),
    )
}

fn wide_graph(
    count: usize,
    nodes_per_community: usize,
) -> (KnowledgeGraph, BTreeMap<i64, Vec<String>>) {
    let mut graph = KnowledgeGraph::default();
    let mut communities = BTreeMap::new();
    for community in 0..count as i64 {
        let mut members = Vec::new();
        for suffix in 0..nodes_per_community {
            let id = format!("c{community}_{suffix}");
            graph
                .nodes
                .push(node(&id, &format!("node_{community}_{suffix}")));
            members.push(id);
        }
        communities.insert(community, members);
    }
    (graph, communities)
}

fn answer_prompt(prompt: &str, prefix: &str) -> String {
    let labels = prompt
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Community ")?;
            let community = rest.split_once(':')?.0.parse::<i64>().ok()?;
            Some((community.to_string(), json!(format!("{prefix}{community}"))))
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::Value::Object(labels).to_string()
}

#[test]
fn test_label_communities_happy_path() {
    let (graph, communities) = graph();
    let captured = Mutex::new(None);
    let options = LabelingOptions::new("gemini");
    let (labels, _) = label_communities_with(&graph, &communities, &[], &options, |request| {
        *captured.lock().unwrap() = Some((request.prompt.clone(), request.backend.clone()));
        Ok(r#"{"0":"Order Management","1":"Payment Flow"}"#.into())
    })
    .unwrap();
    assert_eq!(
        labels,
        BTreeMap::from([(0, "Order Management".into()), (1, "Payment Flow".into())])
    );
    let (prompt, backend) = captured.into_inner().unwrap().unwrap();
    assert!(prompt.contains("place_order"));
    assert!(prompt.contains("StripeClient"));
    assert_eq!(backend, "gemini");
}

#[test]
fn test_label_communities_passes_model_override() {
    let (graph, communities) = graph();
    let captured = Mutex::new(None);
    let mut options = LabelingOptions::new("gemini");
    options.model = Some("gemini-3.1-flash-lite".into());
    let (labels, _) = label_communities_with(&graph, &communities, &[], &options, |request| {
        *captured.lock().unwrap() = Some((request.backend.clone(), request.model.clone()));
        Ok(r#"{"0":"Order Management","1":"Payment Flow"}"#.into())
    })
    .unwrap();
    assert_eq!(labels[&0], "Order Management");
    assert_eq!(labels[&1], "Payment Flow");
    assert_eq!(
        captured.into_inner().unwrap(),
        Some(("gemini".into(), Some("gemini-3.1-flash-lite".into())))
    );
}

#[test]
fn test_label_communities_partial_reply_fills_placeholder() {
    let (graph, communities) = graph();
    let options = LabelingOptions::new("gemini");
    let (labels, _) = label_communities_with(&graph, &communities, &[], &options, |_| {
        Ok(r#"{"0":"Order Management"}"#.into())
    })
    .unwrap();
    assert_eq!(labels[&0], "Order Management");
    assert_eq!(labels[&1], "Community 1");
}

#[test]
fn test_label_communities_strips_code_fences() {
    let (graph, communities) = graph();
    let options = LabelingOptions::new("gemini");
    let (labels, _) = label_communities_with(&graph, &communities, &[], &options, |_| {
        Ok("```json\n{\"0\":\"Orders\",\"1\":\"Pay\"}\n```".into())
    })
    .unwrap();
    assert_eq!(
        labels,
        BTreeMap::from([(0, "Orders".into()), (1, "Pay".into())])
    );
}

#[test]
fn test_label_communities_malformed_raises() {
    let (graph, communities) = graph();
    let options = LabelingOptions::new("gemini");
    let error = label_communities_with(&graph, &communities, &[], &options, |_| {
        Ok("sorry, I cannot help".into())
    })
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("all 1 community-label batches failed"));
}

#[test]
fn test_generate_community_labels_degrades_on_error() {
    let (graph, communities) = graph();
    let options = LabelingOptions::new("gemini");
    let result = generate_community_labels_with(&graph, &communities, &[], Some(&options), |_| {
        Ok("not json".into())
    });
    assert_eq!(result.source, LabelSource::Placeholder);
    assert_eq!(
        result.labels,
        BTreeMap::from([(0, "Community 0".into()), (1, "Community 1".into())])
    );
}

#[test]
fn test_generate_community_labels_no_backend() {
    let (graph, communities) = graph();
    let result = generate_community_labels_with(&graph, &communities, &[], None, |_| {
        panic!("a missing backend must not invoke the transport")
    });
    assert_eq!(result.source, LabelSource::Placeholder);
    assert_eq!(result.labels[&0], "Community 0");
    assert_eq!(result.labels[&1], "Community 1");
}

#[test]
fn test_generate_community_labels_success() {
    let (graph, communities) = graph();
    let options = LabelingOptions::new("gemini");
    let result = generate_community_labels_with(&graph, &communities, &[], Some(&options), |_| {
        Ok(r#"{"0":"Orders","1":"Payments"}"#.into())
    });
    assert_eq!(result.source, LabelSource::Llm);
    assert_eq!(
        result.labels,
        BTreeMap::from([(0, "Orders".into()), (1, "Payments".into())])
    );
}

#[test]
fn test_gods_as_dicts_do_not_crash() {
    let (graph, communities) = graph();
    let gods = vec![GodNode {
        id: "order_repo".into(),
        label: "OrderRepository".into(),
        degree: 10,
    }];
    let options = LabelingOptions::new("gemini");
    let captured = Mutex::new(String::new());
    let (labels, _) = label_communities_with(&graph, &communities, &gods, &options, |request| {
        captured.lock().unwrap().clone_from(&request.prompt);
        Ok(r#"{"0":"Orders","1":"Pay"}"#.into())
    })
    .unwrap();
    assert!(captured
        .into_inner()
        .unwrap()
        .contains("Community 0: OrderRepository, place_order"));
    assert_eq!(labels[&0], "Orders");
    assert_eq!(labels[&1], "Pay");
}

#[test]
fn test_empty_communities_returns_placeholders() {
    let graph = KnowledgeGraph::default();
    let communities = BTreeMap::from([(0, Vec::new())]);
    let calls = AtomicUsize::new(0);
    let options = LabelingOptions::new("gemini");
    let (labels, _) = label_communities_with(&graph, &communities, &[], &options, |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok("{}".into())
    })
    .unwrap();
    assert_eq!(labels, BTreeMap::from([(0, "Community 0".into())]));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn test_label_communities_batches_when_over_batch_size() {
    let (graph, communities) = wide_graph(250, 2);
    let calls = Mutex::new(Vec::new());
    let mut options = LabelingOptions::new("gemini");
    options.batch_size = 100;
    options.max_concurrency = 1;
    let (labels, _) = label_communities_with(&graph, &communities, &[], &options, |request| {
        let count = request
            .prompt
            .lines()
            .filter(|line| line.starts_with("Community "))
            .count();
        calls.lock().unwrap().push(count);
        Ok(answer_prompt(&request.prompt, "Cluster ").into())
    })
    .unwrap();
    assert_eq!(calls.into_inner().unwrap(), vec![100, 100, 50]);
    assert_eq!(labels.len(), 250);
    assert!(labels.values().all(|label| label.starts_with("Cluster ")));
}

#[test]
fn test_label_communities_partial_batch_failure_keeps_successful_batches() {
    let (graph, communities) = wide_graph(150, 2);
    let calls = AtomicUsize::new(0);
    let mut options = LabelingOptions::new("gemini");
    options.batch_size = 50;
    options.max_concurrency = 1;
    let (labels, _) = label_communities_with(&graph, &communities, &[], &options, |request| {
        if calls.fetch_add(1, Ordering::SeqCst) == 1 {
            anyhow::bail!("simulated transient backend failure")
        }
        Ok(answer_prompt(&request.prompt, "Named ").into())
    })
    .unwrap();
    assert_eq!(
        labels
            .values()
            .filter(|label| label.starts_with("Named "))
            .count(),
        100
    );
    assert_eq!(
        labels
            .values()
            .filter(|label| label.starts_with("Community "))
            .count(),
        50
    );
}

#[test]
fn test_label_communities_all_batches_fail_raises() {
    let (graph, communities) = wide_graph(150, 2);
    let mut options = LabelingOptions::new("gemini");
    options.batch_size = 50;
    options.max_concurrency = 1;
    let error = label_communities_with(&graph, &communities, &[], &options, |_| {
        anyhow::bail!("backend down")
    })
    .unwrap_err();
    assert!(error.to_string().contains("backend down"));
}

#[test]
fn test_label_communities_max_communities_caps_total() {
    let (graph, communities) = wide_graph(150, 2);
    let seen = AtomicUsize::new(0);
    let mut options = LabelingOptions::new("gemini");
    options.max_communities = Some(40);
    options.batch_size = 100;
    let _ = label_communities_with(&graph, &communities, &[], &options, |request| {
        seen.fetch_add(
            request
                .prompt
                .lines()
                .filter(|line| line.starts_with("Community "))
                .count(),
            Ordering::SeqCst,
        );
        Ok(answer_prompt(&request.prompt, "X").into())
    })
    .unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 40);
}

#[test]
fn test_label_communities_parallel_matches_sequential() {
    let (graph, communities) = wide_graph(6, 1);
    let mut sequential = LabelingOptions::new("gemini");
    sequential.batch_size = 1;
    sequential.max_concurrency = 1;
    let mut parallel = sequential.clone();
    parallel.max_concurrency = 4;
    let call = |request: &graphoxide_graph::LabelRequest| {
        Ok(answer_prompt(&request.prompt, "name-").into())
    };
    let (sequential, _) =
        label_communities_with(&graph, &communities, &[], &sequential, call).unwrap();
    let (parallel, _) = label_communities_with(&graph, &communities, &[], &parallel, call).unwrap();
    assert_eq!(sequential, parallel);
    assert_eq!(
        sequential,
        (0..6).map(|id| (id, format!("name-{id}"))).collect()
    );
}

#[test]
fn test_label_communities_batch_size_controls_batch_count() {
    let (graph, communities) = wide_graph(5, 1);
    let calls = Mutex::new(Vec::new());
    let mut options = LabelingOptions::new("gemini");
    options.batch_size = 2;
    options.max_concurrency = 1;
    let (labels, _) = label_communities_with(&graph, &communities, &[], &options, |request| {
        let ids = request
            .prompt
            .lines()
            .filter_map(|line| line.strip_prefix("Community "))
            .filter_map(|line| line.split_once(':'))
            .filter_map(|(id, _)| id.parse::<i64>().ok())
            .collect::<Vec<_>>();
        calls.lock().unwrap().push(ids);
        Ok(answer_prompt(&request.prompt, "n-").into())
    })
    .unwrap();
    let calls = calls.into_inner().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls.iter().map(Vec::len).sum::<usize>(), 5);
    assert_eq!(labels, (0..5).map(|id| (id, format!("n-{id}"))).collect());
}

fn tracked_call(
    now: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
) -> impl Fn(&graphoxide_graph::LabelRequest) -> anyhow::Result<LabelResponse> + Sync {
    move |request| {
        let current = now.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(current, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(30));
        now.fetch_sub(1, Ordering::SeqCst);
        Ok(answer_prompt(&request.prompt, "n-").into())
    }
}

#[test]
fn test_label_communities_runs_batches_concurrently() {
    let (graph, communities) = wide_graph(8, 1);
    let now = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut options = LabelingOptions::new("gemini");
    options.batch_size = 1;
    options.max_concurrency = 4;
    label_communities_with(
        &graph,
        &communities,
        &[],
        &options,
        tracked_call(now, Arc::clone(&peak)),
    )
    .unwrap();
    assert!(peak.load(Ordering::SeqCst) > 1);
}

#[test]
fn test_label_communities_forces_serial_for_ollama() {
    let (graph, communities) = wide_graph(8, 1);
    for backend in ["ollama", "claude-cli"] {
        let now = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut options = LabelingOptions::new(backend);
        options.batch_size = 1;
        options.max_concurrency = 8;
        label_communities_with(
            &graph,
            &communities,
            &[],
            &options,
            tracked_call(now, Arc::clone(&peak)),
        )
        .unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 1, "{backend}");
    }
}

#[test]
fn test_label_communities_salvages_truncated_reply() {
    let (graph, communities) = graph();
    let options = LabelingOptions::new("gemini");
    let (labels, _) = label_communities_with(&graph, &communities, &[], &options, |_| {
        Ok(r#"{"0":"Order Management","1":"#.into())
    })
    .unwrap();
    assert_eq!(labels[&0], "Order Management");
    assert_eq!(labels[&1], "Community 1");
}

#[test]
fn test_label_communities_accumulates_token_usage() {
    let (graph, communities) = wide_graph(6, 1);
    let mut options = LabelingOptions::new("gemini");
    options.batch_size = 2;
    options.max_concurrency = 1;
    let (labels, usage) = label_communities_with(&graph, &communities, &[], &options, |request| {
        Ok(LabelResponse {
            content: answer_prompt(&request.prompt, "Name "),
            usage: LabelUsage {
                input: 100,
                output: 10,
            },
        })
    })
    .unwrap();
    assert_eq!(labels.len(), 6);
    assert_eq!(
        usage,
        LabelUsage {
            input: 300,
            output: 30
        }
    );
}

#[test]
fn test_label_communities_counts_tokens_for_failed_batch() {
    let graph = KnowledgeGraph {
        nodes: vec![node("a", "alpha")],
        ..Default::default()
    };
    let communities = BTreeMap::from([(0, vec!["a".into()])]);
    let options = LabelingOptions::new("gemini");
    let error = label_communities_with(&graph, &communities, &[], &options, |_| {
        Ok(LabelResponse {
            content: "not json at all".into(),
            usage: LabelUsage {
                input: 50,
                output: 5,
            },
        })
    })
    .unwrap_err();
    assert_eq!(
        error.usage,
        LabelUsage {
            input: 50,
            output: 5
        }
    );
}
