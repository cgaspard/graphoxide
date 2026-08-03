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
    run_with_endpoint_and_env(root, arguments, endpoint, &[])
}

fn run_with_endpoint_and_env(
    root: &Path,
    arguments: &[&str],
    endpoint: &str,
    environment: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .args(arguments)
        .current_dir(root)
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT")
        .env("GRAPHOXIDE_LLM_BASE_URL", endpoint)
        .env_remove("OPENAI_API_KEY")
        .env_remove("OLLAMA_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .env_remove("GRAPHOXIDE_LLM_TIMEOUT_SECONDS")
        .env_remove("GRAPHIFY_API_TIMEOUT");
    for (key, value) in environment {
        command.env(key, value);
    }
    command.output().unwrap()
}

#[derive(Debug)]
struct CapturedRequest {
    headers: BTreeMap<String, String>,
    body: Value,
}

fn serve_once(label_json: &str) -> (String, Receiver<CapturedRequest>, JoinHandle<()>) {
    serve_once_after(label_json, Duration::ZERO)
}

fn serve_once_after(
    label_json: &str,
    delay: Duration,
) -> (String, Receiver<CapturedRequest>, JoinHandle<()>) {
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
        let mut headers = BTreeMap::new();
        loop {
            let count = stream.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
            if body_start.is_none() {
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let start = index + 4;
                    let raw_headers = String::from_utf8_lossy(&request[..index]);
                    for line in raw_headers.lines().skip(1) {
                        let Some((name, value)) = line.split_once(':') else {
                            continue;
                        };
                        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
                    }
                    content_length = headers
                        .get("content-length")
                        .and_then(|value| value.parse::<usize>().ok());
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
        sender.send(CapturedRequest { headers, body }).unwrap();
        let response = json!({
            "choices": [{"message": {"content": label_json}}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 3}
        })
        .to_string();
        std::thread::sleep(delay);
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
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
    assert_eq!(request.body["model"], "gemini-3.1-flash-lite");
    let prompt = request.body["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("Community 0:"));
    assert!(prompt.contains("Community 1:"));
    assert!(output.join(".graphoxide_labels.json").is_file());
}

#[test]
fn test_label_cli_lm_studio_sends_optional_openai_api_key() {
    let temporary = tempdir().unwrap();
    two_community_graph(temporary.path());
    let (endpoint, request, server) = serve_once(r#"{"0":"Orders","1":"Payments"}"#);
    let key = "lm-studio-test-key-must-not-be-logged";
    let result = run_with_endpoint_and_env(
        temporary.path(),
        &[
            "label",
            ".",
            "--backend",
            "lm-studio",
            "--model",
            "local-model",
        ],
        &endpoint,
        &[("OPENAI_API_KEY", key)],
    );
    let combined = format!(
        "{}{}",
        output_text(&result.stdout),
        output_text(&result.stderr)
    );
    assert!(result.status.success(), "{combined}");
    assert!(!combined.contains(key));
    let request = request.recv_timeout(Duration::from_secs(5)).unwrap();
    server.join().unwrap();
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer lm-studio-test-key-must-not-be-logged")
    );
    assert_eq!(request.body["model"], "local-model");
    assert_eq!(request.body["reasoning_effort"], "none");
}

#[test]
fn test_label_cli_lm_studio_allows_keyless_loopback() {
    let temporary = tempdir().unwrap();
    two_community_graph(temporary.path());
    let (endpoint, request, server) = serve_once(r#"{"0":"Orders","1":"Payments"}"#);
    let result = run_with_endpoint(
        temporary.path(),
        &[
            "label",
            ".",
            "--backend",
            "lm-studio",
            "--model",
            "local-model",
        ],
        &endpoint,
    );
    assert!(result.status.success(), "{}", output_text(&result.stderr));
    let request = request.recv_timeout(Duration::from_secs(5)).unwrap();
    server.join().unwrap();
    assert!(!request.headers.contains_key("authorization"));
    assert_eq!(request.body["model"], "local-model");
    assert_eq!(request.body["reasoning_effort"], "none");
}

#[test]
fn test_label_cli_generic_openai_does_not_force_reasoning_effort() {
    let temporary = tempdir().unwrap();
    two_community_graph(temporary.path());
    let (endpoint, request, server) = serve_once(r#"{"0":"Orders","1":"Payments"}"#);
    let result = run_with_endpoint(
        temporary.path(),
        &[
            "label",
            ".",
            "--backend",
            "openai",
            "--model",
            "openai-compatible-model",
        ],
        &endpoint,
    );
    assert!(result.status.success(), "{}", output_text(&result.stderr));
    let request = request.recv_timeout(Duration::from_secs(5)).unwrap();
    server.join().unwrap();
    assert!(request.body.get("reasoning_effort").is_none());
}

#[test]
fn test_label_cli_accepts_slow_local_completion_with_timeout_override() {
    let temporary = tempdir().unwrap();
    two_community_graph(temporary.path());
    let (endpoint, request, server) = serve_once_after(
        r#"{"0":"Orders","1":"Payments"}"#,
        Duration::from_millis(150),
    );
    let result = run_with_endpoint(
        temporary.path(),
        &[
            "label",
            ".",
            "--backend",
            "lm-studio",
            "--model",
            "slow-local-model",
            "--timeout-seconds",
            "1",
        ],
        &endpoint,
    );
    assert!(result.status.success(), "{}", output_text(&result.stderr));
    let request = request.recv_timeout(Duration::from_secs(5)).unwrap();
    server.join().unwrap();
    assert_eq!(request.body["reasoning_effort"], "none");
}

#[test]
fn test_label_cli_timeout_error_explains_how_to_raise_deadline() {
    let temporary = tempdir().unwrap();
    two_community_graph(temporary.path());
    let (endpoint, request, server) = serve_once_after(
        r#"{"0":"Orders","1":"Payments"}"#,
        Duration::from_millis(250),
    );
    let result = run_with_endpoint(
        temporary.path(),
        &[
            "label",
            ".",
            "--backend",
            "lm-studio",
            "--model",
            "stalled-local-model",
            "--timeout-seconds",
            "0.05",
        ],
        &endpoint,
    );
    assert!(!result.status.success());
    let stderr = output_text(&result.stderr);
    assert!(stderr.contains("timed out after 0.05s"), "{stderr}");
    assert!(stderr.contains("increase --timeout-seconds"), "{stderr}");
    let _ = request.recv_timeout(Duration::from_secs(5)).unwrap();
    server.join().unwrap();
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
    let prompt = request.body["messages"][0]["content"].as_str().unwrap();
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
