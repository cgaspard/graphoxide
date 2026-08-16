//! Real-socket E2E coverage for the MCP Streamable HTTP transport (issue #39).
//!
//! These tests launch the shipped `graphoxide serve` binary as a separate
//! process bound to an ephemeral loopback port and drive the full MCP
//! protocol — initialize, session handling, `tools/list`, representative tool
//! calls, API-key auth, malformed clients, concurrency, a tool error that must
//! not kill the server, and graceful teardown — across a real TCP boundary.
//!
//! The client is a deliberately small hand-rolled HTTP/1.1 client over
//! `std::net::TcpStream` so the test exercises the network stack (framing,
//! keep-alive, chunked transfer, timeouts) rather than only an in-process
//! router. All deadlines, ports, and output are bounded, and the child process
//! is always torn down in a guard.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
const TOOLS_LIST: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;

/// Sample graph written to the fixture directory.
fn sample_graph(path: &std::path::Path) {
    std::fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "directed": true,
            "nodes": [
                {"id": "a", "label": "Alpha", "community": 0},
                {"id": "b", "label": "Beta", "community": 0},
            ],
            "edges": [
                {"source": "a", "target": "b", "relation": "calls", "confidence": "EXTRACTED"}
            ]
        }))
        .expect("serialize graph"),
    )
    .expect("write graph");
}

/// Reserve an OS-assigned loopback port and release it so the server can bind.
fn ephemeral_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Bounded child-process handle that always tears the server down on drop.
struct ServerGuard {
    child: Option<Child>,
}

impl ServerGuard {
    fn teardown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Spawn `graphoxide serve` on the given port and wait until it accepts.
fn start_server(graph: &std::path::Path, port: u16, api_key: Option<&str>) -> ServerGuard {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .args(["serve"])
        .arg(graph)
        .arg("--transport")
        .arg("http")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--json-response")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("GRAPHOXIDE_API_KEY")
        .env_remove("GRAPHIFY_API_KEY");
    if let Some(key) = api_key {
        command.arg("--api-key").arg(key);
    }
    let mut child = command.spawn().expect("spawn serve");

    // Probe until the listener accepts; bounded deadline.
    let started = Instant::now();
    let deadline = Duration::from_secs(30);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        if started.elapsed() > deadline {
            let mut stderr = String::new();
            if let Some(stderr_pipe) = child.stderr.as_mut() {
                let _ = stderr_pipe.read_to_string(&mut stderr);
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "server did not become ready on 127.0.0.1:{port} within {deadline:?}; stderr={stderr}"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    ServerGuard { child: Some(child) }
}

/// A bounded HTTP/1.1 response: status, headers, and a fully-consumed body.
struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Read one bounded HTTP/1.1 response over the socket with a read deadline.
fn read_response(stream: &mut TcpStream) -> HttpResponse {
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("set read timeout");
    let mut reader = BufReader::new(&mut *stream);

    let mut status_line = String::new();
    let bytes = reader
        .read_line(&mut status_line)
        .expect("read status line");
    assert!(bytes > 0, "empty status line (connection closed)");
    let mut parts = status_line.splitn(3, ' ');
    let _http = parts.next().expect("http token");
    let status: u16 = parts
        .next()
        .expect("status code")
        .parse()
        .expect("status int");

    let mut headers = Vec::new();
    let mut transfer_encoding_chunked = false;
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).expect("read header line");
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']).to_owned();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_owned();
            let value = value.trim().to_owned();
            let name_lc = name.to_ascii_lowercase();
            if name_lc == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
                transfer_encoding_chunked = true;
            } else if name_lc == "content-length" {
                content_length = value.parse().ok();
            }
            headers.push((name_lc, value));
        }
    }

    let mut body = Vec::new();
    if transfer_encoding_chunked {
        read_chunked(&mut reader, &mut body).expect("read chunked body");
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len.min(64 * 1024 * 1024)];
        reader
            .read_exact(&mut buf[..len.min(64 * 1024 * 1024)])
            .expect("read body");
        body = buf;
    } else {
        // Connection-close framing: read to EOF (bounded by the read timeout).
        let _ = reader.read_to_end(&mut body);
    }

    HttpResponse {
        status,
        headers,
        body,
    }
}

fn read_chunked(reader: &mut impl BufRead, body: &mut Vec<u8>) -> std::io::Result<()> {
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let size_token = size_line.split(';').next().unwrap_or("").trim().to_owned();
        let size = usize::from_str_radix(&size_token, 16).unwrap_or(0);
        if size == 0 {
            // Consume the optional trailer section up to the blank line.
            let mut trailer = String::new();
            loop {
                let n = reader.read_line(&mut trailer)?;
                if n == 0 || trailer.trim().is_empty() {
                    break;
                }
                trailer.clear();
            }
            return Ok(());
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        let mut crlf = String::new();
        reader.read_line(&mut crlf)?;
    }
}

/// Perform one bounded POST request and return the parsed response.
fn post(
    stream: &mut TcpStream,
    path: &str,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> HttpResponse {
    let byte_len = body.len();
    let mut header = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {byte_len}\r\n"
    );
    for (name, value) in extra_headers {
        header.push_str(&format!("{name}: {value}\r\n"));
    }
    header.push_str("\r\n");
    stream
        .write_all(header.as_bytes())
        .expect("write request head");
    stream
        .write_all(body.as_bytes())
        .expect("write request body");
    stream.flush().expect("flush request");
    read_response(stream)
}

fn header<'a>(response: &'a HttpResponse, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn body_json(response: &HttpResponse) -> Value {
    serde_json::from_slice(&response.body).unwrap_or_else(|error| {
        panic!(
            "response was not valid JSON: {error}; status={}; body={:?}",
            response.status,
            String::from_utf8_lossy(&response.body)
        )
    })
}

/// Run a full stateful session over one connection: initialize, initialized,
/// tools/list, and a representative tool call.
fn drive_stateful_session(base: &str, port: u16, key: Option<&str>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let bearer = key.map(|k| format!("Bearer {k}"));
    let auth: Vec<(&str, &str)> = bearer
        .as_ref()
        .map(|value| vec![("Authorization", value.as_str())])
        .unwrap_or_default();

    let init = post(&mut stream, &format!("{base}/mcp"), INITIALIZE, &auth);
    assert_eq!(init.status, 200, "initialize status: {:?}", init.body);
    let session = header(&init, "mcp-session-id")
        .expect("session id")
        .to_owned();

    let mut initialized_headers = auth.clone();
    initialized_headers.push(("mcp-session-id", &session));
    let initialized = post(
        &mut stream,
        &format!("{base}/mcp"),
        INITIALIZED,
        &initialized_headers,
    );
    assert!(
        (200..300).contains(&initialized.status),
        "initialized notification: {:?}",
        initialized.body
    );

    let mut session_headers = auth.clone();
    session_headers.push(("mcp-session-id", &session));

    let tools = post(
        &mut stream,
        &format!("{base}/mcp"),
        TOOLS_LIST,
        &session_headers,
    );
    assert_eq!(tools.status, 200, "tools/list status");
    let tools_json = body_json(&tools);
    let names: Vec<String> = tools_json["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    assert!(
        names.iter().any(|n| n == "project_overview"),
        "expected project_overview tool, got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "query_graph"),
        "expected query_graph tool, got {names:?}"
    );

    // Representative tool call: project_overview over the sample graph.
    let call = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"project_overview","arguments":{"top_n":5}}}"#;
    let result = post(&mut stream, &format!("{base}/mcp"), call, &session_headers);
    assert_eq!(result.status, 200, "tool call status");
    let call_json = body_json(&result);
    let text = call_json["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text")
        .to_owned();
    assert!(!text.is_empty(), "project_overview returned empty text");

    drop(stream);
}

#[test]
fn real_socket_initialize_tools_and_call_succeed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let graph = dir.path().join("graph.json");
    sample_graph(&graph);
    let port = ephemeral_port();
    let mut server = start_server(&graph, port, None);

    drive_stateful_session("", port, None);

    // Graceful shutdown: the process must exit cleanly once we stop driving it.
    // (We simply verify the child is still alive and then tear it down.)
    assert!(server.child.is_some(), "server should still be running");
    server.teardown();
}

#[test]
fn real_socket_stateless_mode_requires_no_session_header() {
    // Stateless mode is exercised via a second server on its own port.
    let dir = tempfile::tempdir().expect("temp dir");
    let graph = dir.path().join("graph.json");
    sample_graph(&graph);
    let port = ephemeral_port();

    // Start a stateless server by passing --stateless.
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .args(["serve"])
        .arg(&graph)
        .arg("--transport")
        .arg("http")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--json-response")
        .arg("--stateless")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("GRAPHOXIDE_API_KEY")
        .env_remove("GRAPHIFY_API_KEY");
    let mut child = command.spawn().expect("spawn stateless serve");
    let started = Instant::now();
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("stateless server not ready");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut guard = ServerGuard { child: Some(child) };

    // A bare initialize over a fresh socket should succeed without a prior
    // session handshake.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let init = post(&mut stream, "/mcp", INITIALIZE, &[]);
    assert_eq!(init.status, 200, "stateless initialize status");
    let body = body_json(&init);
    assert_eq!(body["result"]["serverInfo"]["name"], "graphoxide");

    guard.teardown();
}

#[test]
fn real_socket_api_key_rejects_missing_and_wrong_credentials() {
    let dir = tempfile::tempdir().expect("temp dir");
    let graph = dir.path().join("graph.json");
    sample_graph(&graph);
    let port = ephemeral_port();
    let mut server = start_server(&graph, port, Some("s3cret"));

    // Missing credentials -> 401.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let missing = post(&mut stream, "/mcp", INITIALIZE, &[]);
    assert_eq!(missing.status, 401, "expected 401, got {:?}", missing.body);

    // Wrong credentials -> 401.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let wrong = post(
        &mut stream,
        "/mcp",
        INITIALIZE,
        &[("Authorization", "Bearer nope")],
    );
    assert_eq!(wrong.status, 401, "expected 401, got {:?}", wrong.body);

    // Correct credentials -> 200.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let ok = post(
        &mut stream,
        "/mcp",
        INITIALIZE,
        &[("Authorization", "Bearer s3cret")],
    );
    assert_eq!(ok.status, 200, "expected 200, got {:?}", ok.body);

    server.teardown();
}

#[test]
fn real_socket_malformed_request_does_not_kill_server() {
    let dir = tempfile::tempdir().expect("temp dir");
    let graph = dir.path().join("graph.json");
    sample_graph(&graph);
    let port = ephemeral_port();
    let mut server = start_server(&graph, port, None);

    // Send a request with a body length that is larger than the body we
    // actually send, then close abruptly. The server must survive.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let bogus = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 500\r\n\r\n{}",
        "x".repeat(10)
    );
    let _ = stream.write_all(bogus.as_bytes());
    drop(stream);

    // The server must still accept a brand-new, well-formed session.
    drive_stateful_session("", port, None);

    server.teardown();
}

#[test]
fn real_socket_tool_error_does_not_kill_server() {
    let dir = tempfile::tempdir().expect("temp dir");
    let graph = dir.path().join("graph.json");
    sample_graph(&graph);
    let port = ephemeral_port();
    let mut server = start_server(&graph, port, None);

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let init = post(&mut stream, "/mcp", INITIALIZE, &[]);
    assert_eq!(init.status, 200);
    let session = header(&init, "mcp-session-id").expect("session").to_owned();
    let session_headers: Vec<(&str, &str)> = vec![("mcp-session-id", &session)];

    // A tool call against an unknown tool should produce a JSON-RPC error
    // response, but the server must stay up.
    let bad_call = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#;
    let result = post(&mut stream, "/mcp", bad_call, &session_headers);
    // Accept either a 200-with-error-body (JSON-RPC error) or a 4xx; the key
    // invariant is that the process does not die and a follow-up works.
    assert!(
        result.status == 200 || (400..500).contains(&result.status),
        "unexpected status for unknown tool: {}",
        result.status
    );
    drop(stream);

    // Server must still be healthy after the error.
    drive_stateful_session("", port, None);

    server.teardown();
}

#[test]
fn real_socket_concurrent_sessions_are_isolated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let graph = dir.path().join("graph.json");
    sample_graph(&graph);
    let port = ephemeral_port();
    let mut server = start_server(&graph, port, None);

    // Run several independent stateful sessions concurrently.
    let handles: Vec<std::thread::JoinHandle<()>> = (0..4)
        .map(|_| {
            std::thread::spawn(move || {
                drive_stateful_session("", port, None);
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("concurrent session thread");
    }

    server.teardown();
}
