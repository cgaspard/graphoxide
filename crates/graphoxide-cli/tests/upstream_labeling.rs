//! Subprocess ports of the CLI-specific pinned Graphify labeling cases.

use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::Duration;
use tempfile::tempdir;

fn node(id: &str, label: &str, community: i64) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: "sample.py".into(),
        source_location: None,
        community: Some(community),
        extra: BTreeMap::new(),
    }
}

fn two_community_graph(root: &Path) -> std::path::PathBuf {
    let output = root.join("graphoxide-out");
    fs::create_dir_all(&output).unwrap();
    let edge = |source: &str, target: &str| Edge {
        source: source.into(),
        target: target.into(),
        relation: "calls".into(),
        confidence: Confidence::Extracted,
        source_file: "sample.py".into(),
        extra: BTreeMap::new(),
    };
    let graph = KnowledgeGraph {
        nodes: vec![
            node("orders", "OrderService", 0),
            node("order_db", "OrderDB", 0),
            node("payments", "PaymentService", 1),
            node("pay_db", "PayDB", 1),
        ],
        links: vec![edge("orders", "order_db"), edge("payments", "pay_db")],
        ..Default::default()
    };
    graphoxide_core::write_graph_atomic(output.join("graph.json"), &graph, true).unwrap();
    output
}

fn run(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(arguments)
        .current_dir(root)
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT")
        .output()
        .unwrap()
}

fn run_with_endpoint(root: &Path, arguments: &[&str], endpoint: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(arguments)
        .current_dir(root)
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT")
        .env("GRAPHOXIDE_LLM_BASE_URL", endpoint)
        .env_remove("GEMINI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .output()
        .unwrap()
}

fn serve_once(label_json: &str) -> (String, Receiver<Value>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    let label_json = label_json.to_owned();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let mut body_start = None;
        let mut content_length = None;
        loop {
            let count = stream.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
            if body_start.is_none() {
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let start = index + 4;
                    let headers = String::from_utf8_lossy(&request[..index]);
                    content_length = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    });
                    body_start = Some(start);
                }
            }
            if body_start
                .zip(content_length)
                .is_some_and(|(start, length)| request.len() >= start + length)
            {
                break;
            }
        }
        let start = body_start.unwrap();
        let length = content_length.unwrap();
        let body: Value = serde_json::from_slice(&request[start..start + length]).unwrap();
        sender.send(body).unwrap();
        let response = json!({
            "choices": [{"message": {"content": label_json}}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 3}
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
    });
    (format!("http://{address}/v1"), receiver, handle)
}

fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn test_label_cli_passes_model_override() {
    let temporary = tempdir().unwrap();
    let output = two_community_graph(temporary.path());
    let (endpoint, request, server) = serve_once(r#"{"0":"Orders","1":"Payments"}"#);
    let result = run_with_endpoint(
        temporary.path(),
        &[
            "label",
            ".",
            "--backend",
            "gemini",
            "--model",
            "gemini-3.1-flash-lite",
            "--max-concurrency",
            "8",
            "--batch-size",
            "50",
        ],
        &endpoint,
    );
    assert!(result.status.success(), "{}", output_text(&result.stderr));
    let request = request.recv_timeout(Duration::from_secs(5)).unwrap();
    server.join().unwrap();
    assert_eq!(request["model"], "gemini-3.1-flash-lite");
    let prompt = request["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("Community 0:"));
    assert!(prompt.contains("Community 1:"));
    assert!(output.join(".graphoxide_labels.json").is_file());
}

#[test]
fn test_label_cli_missing_only_preserves_existing_labels() {
    let temporary = tempdir().unwrap();
    let output = two_community_graph(temporary.path());
    fs::write(
        output.join(".graphoxide_labels.json"),
        r#"{"0":"Order Management","1":"Community 1"}"#,
    )
    .unwrap();
    let (endpoint, request, server) = serve_once(r#"{"1":"Payment Flow"}"#);
    let result = run_with_endpoint(
        temporary.path(),
        &["label", ".", "--missing-only", "--backend", "gemini"],
        &endpoint,
    );
    assert!(result.status.success(), "{}", output_text(&result.stderr));
    let request = request.recv_timeout(Duration::from_secs(5)).unwrap();
    server.join().unwrap();
    let prompt = request["messages"][0]["content"].as_str().unwrap();
    assert!(!prompt.contains("Community 0:"));
    assert!(prompt.contains("Community 1:"));
    let labels: Value =
        serde_json::from_slice(&fs::read(output.join(".graphoxide_labels.json")).unwrap()).unwrap();
    assert_eq!(
        labels,
        json!({"0": "Order Management", "1": "Payment Flow"})
    );
}

#[test]
fn test_cluster_only_no_label_does_not_persist_placeholders() {
    let temporary = tempdir().unwrap();
    let output = two_community_graph(temporary.path());
    let labels = output.join(".graphoxide_labels.json");
    let result = run(
        temporary.path(),
        &["cluster-only", ".", "--no-label", "--no-viz"],
    );
    assert!(result.status.success(), "{}", output_text(&result.stderr));
    assert!(!labels.exists());
    assert!(!output.join(".graphoxide_labels.json.sig").exists());

    let result = run(temporary.path(), &["cluster-only", ".", "--no-viz"]);
    assert!(result.status.success(), "{}", output_text(&result.stderr));
    let saved: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(labels).unwrap()).unwrap();
    assert!(!saved.is_empty());
    assert!(saved
        .iter()
        .all(|(community, label)| label != &format!("Community {community}")));
}

#[test]
fn test_cluster_only_heals_persisted_placeholder_but_reuses_genuine() {
    let temporary = tempdir().unwrap();
    let output = two_community_graph(temporary.path());
    let labels_path = output.join(".graphoxide_labels.json");
    fs::write(&labels_path, r#"{"0":"Community 0","1":"Payment Flow"}"#).unwrap();
    let result = run(temporary.path(), &["cluster-only", ".", "--no-viz"]);
    assert!(result.status.success(), "{}", output_text(&result.stderr));
    let saved: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(labels_path).unwrap()).unwrap();
    assert_ne!(saved["0"], "Community 0");
    assert_eq!(saved["1"], "Payment Flow");
}
