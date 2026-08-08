//! Adversarial subprocess coverage for the explicit, transcript-only enrichment boundary.
//!
//! These tests intentionally use a tiny recorded HTTP/1.1 peer instead of a
//! provider SDK. That makes every outbound byte observable and keeps the
//! provider contract reproducible without network access or credentials.

use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const PROFILE: &str = "media-transcript-summary-v1";
const PROVIDER: &str = "openai-compatible";
const CONSENT: &str = "send-redacted-transcript-text";
const MODEL: &str = "fixture-model";
const KEY_ENV: &str = "GRAPHOXIDE_TEST_ENRICHMENT_KEY";
const API_KEY: &str = "sk-fixture-api-key-must-never-leak-1234567890";
const ENV_SECRET_NAME: &str = "GRAPHOXIDE_TEST_PLANTED_SECRET";
const ENV_SECRET: &str = "planted-environment-secret-must-not-leak-987654321";
const JSON_SECRET: &str = "JSON_SECRET";
const AWS_SECRET_ENV: &str = "AWS_SECRET_ACCESS_KEY";
const AWS_SECRET: &str = "AWS_SECRET";
const BEARER_SECRET: &str = "standalone-bearer-secret-abcdefghijklmnopqrstuvwxyz0123456789";
const JWT_SECRET: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJncmFwaG94aWRlIiwic2NvcGUiOiJmaXh0dXJlIn0.c2lnbmF0dXJlLWZpeHR1cmUtdGhpcy1pcy1ub3QtcmVhbA";
const JWT_PAYLOAD: &str = "eyJzdWIiOiJncmFwaG94aWRlIiwic2NvcGUiOiJmaXh0dXJlIn0";
const JWT_SIGNATURE: &str = "c2lnbmF0dXJlLWZpeHR1cmUtdGhpcy1pcy1ub3QtcmVhbA";
const PEM_BODY: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCfixturePrivateKeyMaterial0123456789";
const MODEL_TOKEN: &str = "sk-model-secret-material-abcdefghijklmnopqrstuvwxyz";
const SAFE_TRANSCRIPT: &str =
    "Release briefing\nGraphoxide keeps default indexing deterministic.\n";
const SUCCESS_RESPONSE: &str = include_str!("fixtures/enrichment/success.json");
const SECOND_RESPONSE: &str = include_str!("fixtures/enrichment/second.json");
const MALFORMED_CONTENT_RESPONSE: &str = include_str!("fixtures/enrichment/malformed-content.json");

fn graphoxide(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .current_dir(project)
        .env_remove("GRAPHOXIDE_FORCE")
        .env_remove("GRAPHIFY_FORCE")
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT")
        .env_remove(KEY_ENV)
        .env_remove(ENV_SECRET_NAME)
        .env_remove(AWS_SECRET_ENV)
        .env_remove("OPENAI_API_KEY")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy");
    command
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success_json(output: &Output) -> Value {
    assert!(output.status.success(), "{}", output_text(output));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not exactly one JSON value: {error}\n{}",
            output_text(output)
        )
    })
}

fn assert_failure_without_secret(output: &Output) {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        output_text(output)
    );
    let output = output_text(output);
    for secret in [
        API_KEY,
        ENV_SECRET,
        JSON_SECRET,
        AWS_SECRET,
        BEARER_SECRET,
        JWT_SECRET,
        JWT_PAYLOAD,
        JWT_SIGNATURE,
        PEM_BODY,
        MODEL_TOKEN,
    ] {
        assert!(
            !output.contains(secret),
            "secret leaked in process output: {output}"
        );
    }
}

fn managed(project: &Path, name: &str) -> PathBuf {
    project.join("graphoxide-out").join(name)
}

fn graph_bytes(project: &Path) -> Vec<u8> {
    fs::read(managed(project, "graph.json")).expect("graph bytes")
}

fn graph(project: &Path) -> Value {
    serde_json::from_slice(&graph_bytes(project)).expect("graph JSON")
}

fn transcript_path(project: &Path, media_source: &str) -> PathBuf {
    project
        .join(".graphoxide/enrichment-input")
        .join(format!("{media_source}.transcript.txt"))
}

fn write_transcript(project: &Path, media_source: &str, body: impl AsRef<[u8]>) -> PathBuf {
    let path = transcript_path(project, media_source);
    fs::create_dir_all(path.parent().expect("transcript parent")).expect("transcript parent");
    fs::write(&path, body).expect("transcript fixture");
    path
}

fn index_media_project(project: &Path, transcript: Option<&str>) -> Vec<u8> {
    fs::create_dir_all(project.join("media")).expect("media directory");
    // Deliberately recognizable marker bytes: an outbound request containing
    // either marker proves the media payload crossed the transcript-only boundary.
    fs::write(
        project.join("media/briefing.mp4"),
        b"MEDIA_PAYLOAD_SENTINEL_must_never_be_read_or_sent",
    )
    .expect("media fixture");
    if let Some(transcript) = transcript {
        write_transcript(project, "media/briefing.mp4", transcript);
    }
    let output = graphoxide(project)
        .args(["index", ".", "--force", "--json"])
        .output()
        .expect("index media project");
    assert_success_json(&output);
    graph_bytes(project)
}

fn write_inventory_graph(project: &Path, sources: &[String]) {
    fs::create_dir_all(managed(project, "")).expect("managed output");
    let nodes = sources
        .iter()
        .enumerate()
        .map(|(ordinal, source)| {
            json!({
                "id": format!("fixture_media_{ordinal}"),
                "label": Path::new(source)
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("media"),
                "file_type": "document",
                "source_file": source,
                "type": "format_inventory",
                "format": "media",
                "format_capability": "inventory_only",
                "parse_status": "inventory_only"
            })
        })
        .collect::<Vec<_>>();
    let value = json!({
        "directed": false,
        "multigraph": false,
        "graph": {},
        "nodes": nodes,
        "links": [],
        "hyperedges": []
    });
    let mut bytes = serde_json::to_vec_pretty(&value).expect("serialize fixture graph");
    bytes.push(b'\n');
    fs::write(managed(project, "graph.json"), bytes).expect("write fixture graph");
}

fn add_inventory_media(project: &Path, count: usize, transcript: &[u8]) -> Vec<String> {
    let sources = (0..count)
        .map(|ordinal| format!("media/clip-{ordinal:03}.mp4"))
        .collect::<Vec<_>>();
    for source in &sources {
        let path = project.join(source);
        fs::create_dir_all(path.parent().expect("media parent")).expect("media parent");
        fs::write(path, []).expect("empty media inventory file");
        write_transcript(project, source, transcript);
    }
    write_inventory_graph(project, &sources);
    sources
}

fn enrichment_command(project: &Path, endpoint: &str) -> Command {
    let mut command = graphoxide(project);
    command
        .args([
            "enrich",
            ".",
            "--profile",
            PROFILE,
            "--provider",
            PROVIDER,
            "--endpoint",
            endpoint,
            "--model",
            MODEL,
            "--api-key-env",
            KEY_ENV,
            "--consent",
            CONSENT,
            "--json",
        ])
        .env(KEY_ENV, API_KEY)
        .env(ENV_SECRET_NAME, ENV_SECRET);
    command
}

fn run_enrichment(project: &Path, endpoint: &str, extra: &[&str]) -> Output {
    enrichment_command(project, endpoint)
        .args(extra)
        .output()
        .expect("run enrichment")
}

#[derive(Clone, Debug)]
struct ResponseSpec {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
}

impl ResponseSpec {
    fn json(body: impl AsRef<[u8]>) -> Self {
        Self {
            status: 200,
            reason: "OK",
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.as_ref().to_vec(),
            delay: Duration::ZERO,
        }
    }

    fn status(status: u16, reason: &'static str, body: impl AsRef<[u8]>) -> Self {
        Self {
            status,
            reason,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.as_ref().to_vec(),
            delay: Duration::ZERO,
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    raw_body: Vec<u8>,
    received_at: Instant,
}

impl CapturedRequest {
    fn json_body(&self) -> Value {
        serde_json::from_slice(&self.raw_body).expect("request JSON")
    }
}

type RequestAction = Arc<dyn Fn(usize, &CapturedRequest) + Send + Sync + 'static>;

struct RecordedServer {
    endpoint: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    handle: Option<JoinHandle<()>>,
}

impl RecordedServer {
    fn start(responses: Vec<ResponseSpec>) -> Self {
        Self::start_with_action(responses, None)
    }

    fn start_with_action(responses: Vec<ResponseSpec>, action: Option<RequestAction>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind recorded provider");
        listener
            .set_nonblocking(true)
            .expect("recorded provider nonblocking");
        let address = listener.local_addr().expect("recorded provider address");
        let endpoint = format!("http://{address}/v1");
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_stop = Arc::clone(&stop);
        let thread_requests = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut response = 0_usize;
            while response < responses.len()
                && !thread_stop.load(Ordering::Acquire)
                && Instant::now() < deadline
            {
                let (mut stream, _peer) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("accept recorded provider request: {error}"),
                };
                // macOS can propagate the listener's nonblocking state to an
                // accepted socket. Reqwest may connect before its async writer
                // has produced the first byte, so use an ordinary blocking
                // stream with the bounded read timeout installed below.
                stream
                    .set_nonblocking(false)
                    .expect("recorded provider stream blocking");
                let captured = read_request(&mut stream).expect("read recorded provider request");
                thread_requests
                    .lock()
                    .expect("recorded request lock")
                    .push(captured.clone());
                if let Some(action) = &action {
                    action(response, &captured);
                }
                let selected = &responses[response];
                if !selected.delay.is_zero() {
                    thread::sleep(selected.delay);
                }
                let mut head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    selected.status,
                    selected.reason,
                    selected.body.len()
                );
                for (name, value) in &selected.headers {
                    head.push_str(name);
                    head.push_str(": ");
                    head.push_str(value);
                    head.push_str("\r\n");
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&selected.body);
                let _ = stream.flush();
                response += 1;
            }
        });
        Self {
            endpoint,
            stop,
            requests,
            handle: Some(handle),
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn finish(mut self) -> Vec<CapturedRequest> {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("recorded provider thread");
        }
        self.requests.lock().expect("recorded request lock").clone()
    }
}

impl Drop for RecordedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<CapturedRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let received_at = Instant::now();
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (head_end, content_length) = loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "request ended before headers",
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > 1024 * 1024 {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "request headers exceed fixture cap",
            ));
        }
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = std::str::from_utf8(&bytes[..index])
                .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "non-UTF-8 headers"))?;
            let length = head
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .ok_or_else(|| {
                    std::io::Error::new(ErrorKind::InvalidData, "missing Content-Length")
                })?;
            break (index, length);
        }
    };
    let body_start = head_end + 4;
    if content_length > 2 * 1024 * 1024 {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "request body exceeds fixture cap",
        ));
    }
    while bytes.len() < body_start + content_length {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "request ended before body",
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let head = std::str::from_utf8(&bytes[..head_end])
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "non-UTF-8 headers"))?;
    let mut lines = head.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidData, "missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_owned();
    let target = request_parts.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Ok(CapturedRequest {
        method,
        target,
        headers,
        raw_body: bytes[body_start..body_start + content_length].to_vec(),
        received_at,
    })
}

struct NetworkTripwire {
    listener: TcpListener,
    address: SocketAddr,
}

impl NetworkTripwire {
    fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind network tripwire");
        listener
            .set_nonblocking(true)
            .expect("network tripwire nonblocking");
        let address = listener.local_addr().expect("network tripwire address");
        Self { listener, address }
    }

    fn endpoint(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn assert_no_request(&self) {
        thread::sleep(Duration::from_millis(20));
        match self.listener.accept() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("inspect network tripwire: {error}"),
            Ok((_, peer)) => panic!("unexpected outbound request from {peer}"),
        }
    }
}

fn all_project_bytes(project: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, output: &mut Vec<(String, Vec<u8>)>) {
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
            .map(|entry| entry.expect("directory entry"))
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().expect("fixture file type");
            if file_type.is_dir() {
                visit(root, &path, output);
            } else if file_type.is_file() {
                output.push((
                    path.strip_prefix(root)
                        .expect("project-relative fixture path")
                        .to_string_lossy()
                        .replace('\\', "/"),
                    fs::read(&path).expect("fixture bytes"),
                ));
            }
        }
    }
    let mut output = Vec::new();
    visit(project, project, &mut output);
    output
}

fn assert_secret_absent_from_project_and_output(project: &Path, output: &Output) {
    let mut haystacks = vec![output.stdout.clone(), output.stderr.clone()];
    // The user-owned transcript is the source of the planted value and is not
    // an output artifact. Scan only managed graph artifacts and the dedicated
    // enrichment cache in addition to process output.
    for artifact_root in [
        project.join("graphoxide-out"),
        project.join(".graphoxide/enrichment-cache"),
    ] {
        if artifact_root.is_dir() {
            haystacks.extend(
                all_project_bytes(&artifact_root)
                    .into_iter()
                    .map(|(_, bytes)| bytes),
            );
        }
    }
    for secret in [
        API_KEY.as_bytes(),
        ENV_SECRET.as_bytes(),
        JSON_SECRET.as_bytes(),
        AWS_SECRET.as_bytes(),
        BEARER_SECRET.as_bytes(),
        JWT_SECRET.as_bytes(),
        JWT_PAYLOAD.as_bytes(),
        JWT_SIGNATURE.as_bytes(),
        PEM_BODY.as_bytes(),
        MODEL_TOKEN.as_bytes(),
    ] {
        assert!(
            haystacks
                .iter()
                .all(|bytes| !bytes.windows(secret.len()).any(|window| window == secret)),
            "secret was retained in stdout, stderr, graph, or cache"
        );
    }
}

fn spawn_enrichment(project: &Path, endpoint: &str, extra: &[&str]) -> Child {
    enrichment_command(project, endpoint)
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn enrichment")
}

fn args_without<'a>(arguments: &'a [&'a str], omitted: &str) -> Vec<&'a str> {
    let mut output = Vec::new();
    let mut skip_value = false;
    for value in arguments {
        if skip_value {
            skip_value = false;
            continue;
        }
        if *value == omitted {
            skip_value = true;
            continue;
        }
        output.push(*value);
    }
    output
}

fn full_arguments(endpoint: &str) -> Vec<&str> {
    vec![
        "enrich",
        ".",
        "--profile",
        PROFILE,
        "--provider",
        PROVIDER,
        "--endpoint",
        endpoint,
        "--model",
        MODEL,
        "--api-key-env",
        KEY_ENV,
        "--consent",
        CONSENT,
        "--json",
    ]
}

fn run_raw(project: &Path, arguments: &[&str], key: Option<&str>) -> Output {
    let mut command = graphoxide(project);
    command.args(arguments).env(ENV_SECRET_NAME, ENV_SECRET);
    if let Some(key) = key {
        command.env(KEY_ENV, key);
    }
    command.output().expect("run raw graphoxide command")
}

fn find_enrichment_nodes(value: &Value) -> Vec<&Value> {
    value["nodes"]
        .as_array()
        .expect("graph nodes")
        .iter()
        .filter(|node| node["_origin"] == "enrichment")
        .collect()
}

fn cache_files(project: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    visit(&project.join(".graphoxide/enrichment-cache"), &mut files);
    files.sort();
    files
}

fn cache_tree_snapshot(project: &Path) -> Option<Vec<(String, String, Vec<u8>)>> {
    fn visit(root: &Path, path: &Path, output: &mut Vec<(String, String, Vec<u8>)>) {
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("read cache tree {}: {error}", path.display()))
            .map(|entry| entry.expect("cache tree entry"))
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).expect("cache tree metadata");
            let relative = path
                .strip_prefix(root)
                .expect("cache-relative path")
                .to_string_lossy()
                .replace('\\', "/");
            if metadata.file_type().is_symlink() {
                output.push((
                    relative,
                    "symlink".into(),
                    fs::read_link(&path)
                        .expect("cache symlink target")
                        .to_string_lossy()
                        .into_owned()
                        .into_bytes(),
                ));
            } else if metadata.is_dir() {
                output.push((relative, "directory".into(), Vec::new()));
                visit(root, &path, output);
            } else if metadata.is_file() {
                output.push((
                    relative,
                    "file".into(),
                    fs::read(&path).expect("cache file bytes"),
                ));
            } else {
                output.push((relative, "other".into(), Vec::new()));
            }
        }
    }

    let root = project.join(".graphoxide/enrichment-cache");
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => panic!("inspect cache root: {error}"),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            let mut output = vec![(".".into(), "directory".into(), Vec::new())];
            visit(&root, &root, &mut output);
            Some(output)
        }
        Ok(metadata) if metadata.file_type().is_symlink() => Some(vec![(
            ".".into(),
            "symlink".into(),
            fs::read_link(&root)
                .expect("cache root symlink target")
                .to_string_lossy()
                .into_owned()
                .into_bytes(),
        )]),
        Ok(_) => Some(vec![(".".into(), "other".into(), Vec::new())]),
    }
}

#[test]
fn profile_listing_is_stable_and_needs_no_provider_configuration() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let output = graphoxide(fixture.path())
        .args(["enrich", "--list-profiles", "--json"])
        .output()
        .expect("list enrichment profiles");
    let report = assert_success_json(&output);
    assert_eq!(report["schema"], "graphoxide.enrichment-profiles.v1");
    let profiles = report["profiles"].as_array().expect("profile array");
    assert_eq!(
        profiles.len(),
        1,
        "initial release exposes one bounded slice"
    );
    assert_eq!(profiles[0]["name"], PROFILE);
    assert_eq!(profiles[0]["provider"], PROVIDER);
    assert_eq!(
        profiles[0]["data_boundary"],
        "redacted_transcript_text_only"
    );
    assert_eq!(profiles[0]["consent"], CONSENT);
    assert!(profiles[0]["description"]
        .as_str()
        .is_some_and(|value| value.to_ascii_lowercase().contains("transcript")));
}

#[test]
fn default_index_is_byte_identical_with_an_unrequested_transcript_and_never_connects() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let without_sidecar = index_media_project(&project, None);
    let accepted_manifest = fs::read(managed(&project, "manifest.json")).expect("manifest");
    let accepted_coverage: Value =
        serde_json::from_slice(&fs::read(managed(&project, "coverage.json")).expect("coverage"))
            .expect("coverage JSON");
    write_transcript(&project, "media/briefing.mp4", SAFE_TRANSCRIPT);
    let tripwire = NetworkTripwire::bind();
    // Provider-shaped variables must not turn default indexing into enrichment.
    let output = graphoxide(&project)
        .args(["index", ".", "--force", "--json"])
        .env(KEY_ENV, API_KEY)
        .env("GRAPHOXIDE_ENRICHMENT_ENDPOINT", tripwire.endpoint())
        .output()
        .expect("repeat default index");
    assert_success_json(&output);
    tripwire.assert_no_request();
    assert_eq!(graph_bytes(&project), without_sidecar);
    assert_eq!(
        fs::read(managed(&project, "manifest.json")).expect("manifest"),
        accepted_manifest,
        "an unrequested enrichment sidecar changed extracted source state"
    );
    let repeated_coverage: Value =
        serde_json::from_slice(&fs::read(managed(&project, "coverage.json")).expect("coverage"))
            .expect("coverage JSON");
    assert_eq!(
        repeated_coverage["files"], accepted_coverage["files"],
        "an enrichment sidecar changed indexed file outcomes"
    );
    let strip_boundary_accounting = |mut coverage: Value| {
        coverage
            .as_object_mut()
            .expect("coverage object")
            .remove("boundaries");
        let summary = coverage["summary"]
            .as_object_mut()
            .expect("coverage summary");
        summary.remove("ignored_boundaries");
        summary.remove("pruned_boundaries");
        coverage
    };
    assert_eq!(
        strip_boundary_accounting(repeated_coverage),
        strip_boundary_accounting(accepted_coverage),
        "coverage changed outside truthful ignored/pruned boundary accounting"
    );
    assert!(find_enrichment_nodes(&graph(&project)).is_empty());
    assert!(cache_files(&project).is_empty());
}

#[test]
fn incomplete_or_unsafe_authorization_is_rejected_before_any_connection() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let baseline = graph_bytes(&project);

    for omitted in [
        "--profile",
        "--provider",
        "--endpoint",
        "--model",
        "--api-key-env",
        "--consent",
    ] {
        let tripwire = NetworkTripwire::bind();
        let endpoint = tripwire.endpoint();
        let full = full_arguments(&endpoint);
        let arguments = args_without(&full, omitted);
        let output = run_raw(&project, &arguments, Some(API_KEY));
        assert_failure_without_secret(&output);
        tripwire.assert_no_request();
        assert_eq!(graph_bytes(&project), baseline, "omitted {omitted}");
    }

    let invalid_values: [(&str, &str); 3] = [
        ("--profile", "future-unbounded-profile"),
        ("--provider", "implicit-provider"),
        ("--consent", "yes"),
    ];
    for (flag, invalid) in invalid_values {
        let tripwire = NetworkTripwire::bind();
        let endpoint = tripwire.endpoint();
        let mut arguments = full_arguments(&endpoint);
        let position = arguments
            .iter()
            .position(|value| *value == flag)
            .expect("flag in full arguments");
        arguments[position + 1] = invalid;
        let output = run_raw(&project, &arguments, Some(API_KEY));
        assert_failure_without_secret(&output);
        tripwire.assert_no_request();
        assert_eq!(graph_bytes(&project), baseline, "invalid {flag}");
    }

    let tripwire = NetworkTripwire::bind();
    let endpoint = tripwire.endpoint();
    let output = run_raw(&project, &full_arguments(&endpoint), None);
    assert_failure_without_secret(&output);
    tripwire.assert_no_request();
    assert_eq!(graph_bytes(&project), baseline);

    // Userinfo is forbidden even at an otherwise permitted loopback endpoint.
    let tripwire = NetworkTripwire::bind();
    let unsafe_endpoint = format!("http://user:password@{}/v1", tripwire.address);
    let output = run_raw(&project, &full_arguments(&unsafe_endpoint), Some(API_KEY));
    assert_failure_without_secret(&output);
    tripwire.assert_no_request();
    assert_eq!(graph_bytes(&project), baseline);
}

#[test]
fn secret_bearing_model_or_endpoint_configuration_is_rejected_before_network() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let baseline_graph = graph_bytes(&project);
    let baseline_cache = cache_tree_snapshot(&project);

    for secret_model in [API_KEY, "password=JSON_SECRET", MODEL_TOKEN] {
        let tripwire = NetworkTripwire::bind();
        let endpoint = tripwire.endpoint();
        let mut arguments = full_arguments(&endpoint);
        let position = arguments
            .iter()
            .position(|value| *value == "--model")
            .expect("model argument");
        arguments[position + 1] = secret_model;
        let output = run_raw(&project, &arguments, Some(API_KEY));
        assert_failure_without_secret(&output);
        tripwire.assert_no_request();
        assert_secret_absent_from_project_and_output(&project, &output);
        assert_eq!(graph_bytes(&project), baseline_graph);
        assert_eq!(cache_tree_snapshot(&project), baseline_cache);
    }

    let tripwire = NetworkTripwire::bind();
    let unsafe_endpoint = format!("http://{}/v1/{API_KEY}", tripwire.address);
    let output = run_raw(&project, &full_arguments(&unsafe_endpoint), Some(API_KEY));
    assert_failure_without_secret(&output);
    tripwire.assert_no_request();
    assert_secret_absent_from_project_and_output(&project, &output);
    assert_eq!(graph_bytes(&project), baseline_graph);
    assert_eq!(cache_tree_snapshot(&project), baseline_cache);
}

#[test]
fn credential_bearing_project_and_output_paths_are_rejected_without_leaking() {
    let fixture = tempfile::tempdir().expect("temporary fixture");

    let secret_root = fixture.path().join(format!("project-{API_KEY}"));
    fs::create_dir(&secret_root).expect("secret-bearing project root");
    index_media_project(&secret_root, Some(SAFE_TRANSCRIPT));
    let baseline_graph = graph_bytes(&secret_root);
    let baseline_cache = cache_tree_snapshot(&secret_root);
    let tripwire = NetworkTripwire::bind();
    let root_output = run_enrichment(&secret_root, &tripwire.endpoint(), &[]);
    assert_failure_without_secret(&root_output);
    assert!(String::from_utf8_lossy(&root_output.stderr)
        .contains("project root overlaps a protected credential value or pattern"));
    tripwire.assert_no_request();
    assert_eq!(graph_bytes(&secret_root), baseline_graph);
    assert_eq!(cache_tree_snapshot(&secret_root), baseline_cache);

    let project = fixture.path().join("ordinary-project");
    fs::create_dir(&project).expect("ordinary project root");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let baseline_graph = graph_bytes(&project);
    let baseline_cache = cache_tree_snapshot(&project);
    let secret_output = project.join(format!("out-{API_KEY}"));
    let tripwire = NetworkTripwire::bind();
    let output = enrichment_command(&project, &tripwire.endpoint())
        .env("GRAPHOXIDE_OUT", &secret_output)
        .output()
        .expect("run with credential-bearing output path");
    assert_failure_without_secret(&output);
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("graph output directory overlaps a protected credential value or pattern"));
    tripwire.assert_no_request();
    assert!(
        !secret_output.exists(),
        "output path must be rejected before filesystem access"
    );
    assert_eq!(graph_bytes(&project), baseline_graph);
    assert_eq!(cache_tree_snapshot(&project), baseline_cache);
}

#[test]
fn redaction_environment_overflow_fails_closed_before_network_or_cache_creation() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let overflow_secret = "overflow-secret-value-128-must-never-cross";
    index_media_project(
        &project,
        Some(&format!("A standalone value: {overflow_secret}\n")),
    );
    let baseline_graph = graph_bytes(&project);
    let baseline_cache = cache_tree_snapshot(&project);
    let tripwire = NetworkTripwire::bind();
    let endpoint = tripwire.endpoint();
    let mut command = enrichment_command(&project, &endpoint);
    for ordinal in 0..129 {
        command.env(
            format!("GRAPHOXIDE_TEST_OVERFLOW_SECRET_{ordinal:03}"),
            format!("bounded-secret-value-{ordinal:03}-abcdefghijklmnop"),
        );
    }
    command.env("GRAPHOXIDE_TEST_OVERFLOW_SECRET_128", overflow_secret);
    let output = command
        .output()
        .expect("run overflowing redaction environment");
    assert_failure_without_secret(&output);
    assert!(
        !output_text(&output).contains(overflow_secret),
        "overflowing secret-like value leaked in process output"
    );
    tripwire.assert_no_request();
    assert_eq!(graph_bytes(&project), baseline_graph);
    assert_eq!(cache_tree_snapshot(&project), baseline_cache);
}

#[test]
fn unsafe_secret_like_environment_values_fail_closed_before_network_or_cache_creation() {
    for (case, secret) in [
        ("short", "tiny-secret".to_owned()),
        ("oversized", "x".repeat(9 * 1024)),
        (
            "controlled",
            "line-one-secret\r\nline-two-secret".to_owned(),
        ),
    ] {
        let fixture = tempfile::tempdir().expect("temporary fixture");
        let project = fixture.path().join("project");
        fs::create_dir(&project).expect("project");
        index_media_project(&project, Some(&format!("A standalone value: {secret}\n")));
        let baseline_graph = graph_bytes(&project);
        let baseline_cache = cache_tree_snapshot(&project);
        let tripwire = NetworkTripwire::bind();
        let endpoint = tripwire.endpoint();
        let output = enrichment_command(&project, &endpoint)
            .env(format!("GRAPHOXIDE_TEST_{case}_SECRET"), &secret)
            .output()
            .expect("run with an out-of-range secret-like environment value");
        assert_failure_without_secret(&output);
        assert!(
            !output_text(&output).contains(&secret),
            "{case} secret-like value leaked in process output"
        );
        tripwire.assert_no_request();
        assert_eq!(graph_bytes(&project), baseline_graph, "{case}");
        assert_eq!(cache_tree_snapshot(&project), baseline_cache, "{case}");
    }
}

#[test]
fn redaction_marker_api_key_is_rejected_before_network_or_artifacts() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some("Literal [REDACTED] marker in transcript.\n"));
    let baseline_graph = graph_bytes(&project);
    let baseline_cache = cache_tree_snapshot(&project);
    let tripwire = NetworkTripwire::bind();
    let endpoint = tripwire.endpoint();
    let output = enrichment_command(&project, &endpoint)
        .env(KEY_ENV, "[REDACTED]")
        .output()
        .expect("run with unsafe marker credential");
    assert_failure_without_secret(&output);
    tripwire.assert_no_request();
    assert_eq!(graph_bytes(&project), baseline_graph);
    assert_eq!(cache_tree_snapshot(&project), baseline_cache);
}

#[test]
fn recorded_provider_request_cache_replay_and_graph_provenance_are_deterministic() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let media_payload = fs::read(project.join("media/briefing.mp4")).expect("media payload");

    let recorded_response: Value = serde_json::from_str(SUCCESS_RESPONSE)
        .expect("realistic recorded OpenAI-compatible response");
    assert_eq!(recorded_response["object"], "chat.completion");
    assert_eq!(recorded_response["model"], MODEL);
    assert_eq!(
        recorded_response["choices"][0]["message"]["role"],
        "assistant"
    );
    assert_eq!(recorded_response["choices"][0]["finish_reason"], "stop");
    assert!(recorded_response["usage"]["total_tokens"].is_u64());

    let server = RecordedServer::start(vec![ResponseSpec::json(SUCCESS_RESPONSE)]);
    let endpoint = server.endpoint().to_owned();
    let output = enrichment_command(&project, &endpoint)
        // A provider client which inherited proxy configuration could send the
        // transcript to an unintended peer. The enrichment client is isolated.
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .output()
        .expect("recorded enrichment");
    let report = assert_success_json(&output);
    assert_eq!(report["schema"], "graphoxide.enrichment-run.v1");
    assert_eq!(report["profile"], PROFILE);
    assert_eq!(report["provider"], PROVIDER);
    assert_eq!(report["model"], MODEL);
    assert_eq!(report["data_boundary"], "redacted_transcript_text_only");
    assert_eq!(report["redaction_version"], "redaction-v1");
    assert_eq!(report["candidates"], 1);
    assert_eq!(report["cache_hits"], 0);
    assert_eq!(report["requests"], 1);
    assert_eq!(report["enrichments_written"], 1);

    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(
        request.headers["authorization"],
        format!("Bearer {API_KEY}")
    );
    assert!(request.headers["content-type"].starts_with("application/json"));
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/enrichment/expected-request.json"))
            .expect("expected request fixture");
    assert_eq!(request.json_body(), expected);
    assert!(
        !request
            .raw_body
            .windows(media_payload.len())
            .any(|window| window == media_payload),
        "media payload crossed the transcript-only boundary"
    );
    assert!(!String::from_utf8_lossy(&request.raw_body).contains("MEDIA_PAYLOAD_SENTINEL"));

    let enriched_graph = graph(&project);
    let enrichment_nodes = find_enrichment_nodes(&enriched_graph);
    assert_eq!(enrichment_nodes.len(), 1);
    let enrichment = enrichment_nodes[0];
    let enrichment_id = enrichment["id"].as_str().expect("enrichment ID");
    assert!(enrichment_id.starts_with("enrichment_media_briefing_mp4_media_transcript_summary_v1"));
    assert_eq!(enrichment["label"], "Transcript summary: briefing.mp4");
    assert_eq!(enrichment["file_type"], "concept");
    assert_eq!(enrichment["source_file"], "media/briefing.mp4");
    assert_eq!(enrichment["type"], "media_transcript_summary");
    assert_eq!(enrichment["profile"], PROFILE);
    assert_eq!(enrichment["schema_version"], 1);
    assert_eq!(enrichment["data_boundary"], "redacted_transcript_text_only");
    assert_eq!(enrichment["redaction_version"], "redaction-v1");
    assert_eq!(enrichment["redaction_count"], 0);
    assert_eq!(enrichment["provider"], PROVIDER);
    assert_eq!(enrichment["model"], MODEL);
    assert_eq!(enrichment["summary"], "A bounded release briefing.");
    assert_eq!(
        enrichment["topics"],
        json!(["graph indexing", "release readiness"])
    );
    assert_eq!(enrichment["verification"], "unverified_model_output");
    let digest = enrichment["redacted_input_sha256"]
        .as_str()
        .expect("redacted transcript digest");
    assert_eq!(digest.len(), 64);
    assert!(digest.bytes().all(|value| value.is_ascii_hexdigit()));

    let media_id = enriched_graph["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["source_file"] == "media/briefing.mp4" && node["_origin"] != "enrichment")
        .and_then(|node| node["id"].as_str())
        .expect("media inventory ID");
    let links = enriched_graph["links"].as_array().expect("graph links");
    let enrichment_links = links
        .iter()
        .filter(|edge| edge["_origin"] == "enrichment")
        .collect::<Vec<_>>();
    assert_eq!(enrichment_links.len(), 1);
    let link = enrichment_links[0];
    assert_eq!(link["source"], media_id);
    assert_eq!(link["target"], enrichment_id);
    assert_eq!(link["relation"], "has_enrichment");
    assert_eq!(link["confidence"], "AMBIGUOUS");
    assert_eq!(link["confidence_score"], 0.2);
    assert_eq!(link["source_file"], "media/briefing.mp4");
    assert_eq!(link["profile"], PROFILE);
    assert_eq!(link["schema_version"], 1);

    let accepted_graph = graph_bytes(&project);
    let caches = cache_files(&project);
    assert_eq!(caches.len(), 1, "one strict cache record per input/profile");
    let cache: Value =
        serde_json::from_slice(&fs::read(&caches[0]).expect("cache record")).expect("cache JSON");
    assert_eq!(cache["profile"], PROFILE);
    assert_eq!(cache["provider"], PROVIDER);
    assert_eq!(cache["model"], MODEL);
    assert_eq!(cache["redaction_version"], "redaction-v1");
    assert_eq!(cache["data_boundary"], "redacted_transcript_text_only");
    assert_eq!(cache["redacted_input_sha256"], digest);
    assert_eq!(cache["output_redaction_count"], 0);
    assert_eq!(cache["summary"], "A bounded release briefing.");
    assert_eq!(
        cache["topics"],
        json!(["graph indexing", "release readiness"])
    );
    for field in ["endpoint_sha256", "mac"] {
        let value = cache[field].as_str().expect("cache binding digest");
        assert_eq!(value.len(), 64);
        assert!(value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    // The endpoint is now offline. A matching run must replay the strict cache
    // without connecting and must reproduce graph bytes exactly.
    let replay = run_enrichment(&project, &endpoint, &[]);
    let replay_report = assert_success_json(&replay);
    assert_eq!(replay_report["candidates"], 1);
    assert_eq!(replay_report["cache_hits"], 1);
    assert_eq!(replay_report["requests"], 0);
    assert_eq!(replay_report["enrichments_written"], 1);
    assert_eq!(graph_bytes(&project), accepted_graph);
}

#[test]
fn secrets_are_redacted_before_body_cache_graph_and_process_output_even_if_provider_echoes_them() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let transcript = format!(
        "Credential review\r\nauthorization={API_KEY}\rsecret={ENV_SECRET}\r\nghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD\n{{\"password\":\"{JSON_SECRET}\"}}\nAWS_SECRET_ACCESS_KEY={AWS_SECRET}\nBearer {BEARER_SECRET}\n{JWT_SECRET}\n-----BEGIN PRIVATE KEY-----\n{PEM_BODY}\n-----END PRIVATE KEY-----\n"
    );
    index_media_project(&project, Some(&transcript));
    let echoed_inner = json!({
        "summary": format!(
            "provider tried to echo {API_KEY}, {ENV_SECRET}, {{\"password\":\"{JSON_SECRET}\"}}, AWS_SECRET_ACCESS_KEY={AWS_SECRET}, Bearer {BEARER_SECRET}, {JWT_SECRET}, and -----BEGIN PRIVATE KEY-----\n{PEM_BODY}\n-----END PRIVATE KEY-----"
        ),
        "topics": [
            format!("token {API_KEY}"),
            ENV_SECRET,
            format!("{{\"password\":\"{JSON_SECRET}\"}}"),
            format!("AWS_SECRET_ACCESS_KEY={AWS_SECRET}"),
            format!("Bearer {BEARER_SECRET}"),
            JWT_SECRET
        ]
    });
    let response = json!({
        "choices": [{"message": {"content": echoed_inner.to_string()}}]
    })
    .to_string();
    let server = RecordedServer::start(vec![ResponseSpec::json(response)]);
    let endpoint = server.endpoint().to_owned();
    let output = run_enrichment(&project, &endpoint, &[]);
    assert_success_json(&output);
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8(requests[0].raw_body.clone()).expect("UTF-8 request body");
    assert!(!body.contains(API_KEY));
    assert!(!body.contains(ENV_SECRET));
    assert!(!body.contains(JSON_SECRET));
    assert!(!body.contains(AWS_SECRET));
    assert!(!body.contains(BEARER_SECRET));
    assert!(!body.contains(JWT_SECRET));
    assert!(!body.contains(PEM_BODY));
    assert!(!body.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD"));
    assert!(
        body.matches("[REDACTED]").count() >= 8,
        "request body: {body}"
    );
    // The API credential is present only in the transport's Authorization
    // header. It must never be copied into the provider payload or artifacts.
    assert_eq!(
        requests[0].headers["authorization"],
        format!("Bearer {API_KEY}")
    );

    assert_secret_absent_from_project_and_output(&project, &output);
    let value = graph(&project);
    let enrichment = find_enrichment_nodes(&value)[0];
    assert!(enrichment["redaction_count"]
        .as_u64()
        .is_some_and(|count| count >= 8));
    assert!(enrichment["summary"]
        .as_str()
        .is_some_and(|summary| summary.matches("[REDACTED]").count() >= 2));
    let serialized = serde_json::to_string(enrichment).expect("serialize enrichment");
    assert!(!serialized.contains(API_KEY));
    assert!(!serialized.contains(ENV_SECRET));
    assert!(!serialized.contains(JSON_SECRET));
    assert!(!serialized.contains(AWS_SECRET));
    assert!(!serialized.contains(BEARER_SECRET));
    assert!(!serialized.contains(JWT_SECRET));
    assert!(!serialized.contains(PEM_BODY));
}

#[test]
fn cache_record_schema_digest_and_mac_tampering_are_safe_misses() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    for tamper in ["summary", "unknown-field", "input-digest", "mac"] {
        let project = fixture.path().join(tamper);
        fs::create_dir(&project).expect("project");
        index_media_project(&project, Some(SAFE_TRANSCRIPT));
        let server = RecordedServer::start(vec![
            ResponseSpec::json(SUCCESS_RESPONSE),
            ResponseSpec::json(SECOND_RESPONSE),
        ]);
        let endpoint = server.endpoint().to_owned();
        assert_success_json(&run_enrichment(&project, &endpoint, &[]));
        let caches = cache_files(&project);
        assert_eq!(caches.len(), 1);
        let mut record: Value =
            serde_json::from_slice(&fs::read(&caches[0]).expect("read cache record for tampering"))
                .expect("cache record JSON");
        match tamper {
            "summary" => record["summary"] = json!("POISONED_CACHE_SUMMARY"),
            "unknown-field" => record["unexpected"] = json!(true),
            "input-digest" => record["redacted_input_sha256"] = json!("0".repeat(64)),
            "mac" => record["mac"] = json!("0".repeat(64)),
            _ => unreachable!(),
        }
        let mut bytes = serde_json::to_vec_pretty(&record).expect("serialize tampered cache");
        bytes.push(b'\n');
        fs::write(&caches[0], bytes).expect("write tampered cache");

        let output = run_enrichment(&project, &endpoint, &[]);
        let report = assert_success_json(&output);
        assert_eq!(report["cache_hits"], 0, "tamper case {tamper}");
        assert_eq!(report["requests"], 1, "tamper case {tamper}");
        let requests = server.finish();
        assert_eq!(requests.len(), 2, "tamper case {tamper}");
        let value = graph(&project);
        let enrichment = find_enrichment_nodes(&value)[0];
        assert_eq!(enrichment["summary"], "A replacement briefing.");
        assert_ne!(enrichment["summary"], "POISONED_CACHE_SUMMARY");
    }
}

#[test]
fn cache_is_bound_to_canonical_endpoint_and_selected_api_key() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("endpoint");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let first = RecordedServer::start(vec![ResponseSpec::json(SUCCESS_RESPONSE)]);
    let first_endpoint = first.endpoint().to_owned();
    assert_success_json(&run_enrichment(&project, &first_endpoint, &[]));
    assert_eq!(first.finish().len(), 1);

    let second = RecordedServer::start(vec![ResponseSpec::json(SECOND_RESPONSE)]);
    let second_endpoint = second.endpoint().to_owned();
    let output = run_enrichment(&project, &second_endpoint, &[]);
    let report = assert_success_json(&output);
    assert_eq!(report["cache_hits"], 0);
    assert_eq!(report["requests"], 1);
    assert_eq!(second.finish().len(), 1);
    assert_eq!(
        find_enrichment_nodes(&graph(&project))[0]["summary"],
        "A replacement briefing."
    );

    // A cache MAC is derived from the selected credential. Rotating the key
    // therefore cannot replay a record authenticated under the old key.
    let key_project = fixture.path().join("api-key");
    fs::create_dir(&key_project).expect("project");
    index_media_project(&key_project, Some(SAFE_TRANSCRIPT));
    let server = RecordedServer::start(vec![
        ResponseSpec::json(SUCCESS_RESPONSE),
        ResponseSpec::json(SECOND_RESPONSE),
    ]);
    let endpoint = server.endpoint().to_owned();
    assert_success_json(&run_enrichment(&key_project, &endpoint, &[]));
    let rotated_key = "sk-rotated-fixture-key-abcdefghijklmnopqrstuvwxyz";
    let rotated = enrichment_command(&key_project, &endpoint)
        .env(KEY_ENV, rotated_key)
        .output()
        .expect("run with rotated API key");
    let report = assert_success_json(&rotated);
    assert_eq!(report["cache_hits"], 0);
    assert_eq!(report["requests"], 1);
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].headers["authorization"],
        format!("Bearer {rotated_key}")
    );
}

#[cfg(unix)]
#[test]
fn cache_root_symlink_is_fatal_but_cache_file_symlink_is_a_safe_miss() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("root-link");
    fs::create_dir(&project).expect("project");
    let baseline = index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let outside_cache = fixture.path().join("outside-cache");
    fs::create_dir(&outside_cache).expect("outside cache");
    symlink(&outside_cache, project.join(".graphoxide/enrichment-cache"))
        .expect("cache root symlink");
    let tripwire = NetworkTripwire::bind();
    let output = run_enrichment(&project, &tripwire.endpoint(), &[]);
    assert_failure_without_secret(&output);
    tripwire.assert_no_request();
    assert_eq!(graph_bytes(&project), baseline);
    assert!(
        fs::read_dir(&outside_cache)
            .expect("outside cache directory")
            .next()
            .is_none(),
        "cache root symlink escaped the project"
    );

    let project = fixture.path().join("file-link");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let server = RecordedServer::start(vec![
        ResponseSpec::json(SUCCESS_RESPONSE),
        ResponseSpec::json(SECOND_RESPONSE),
    ]);
    let endpoint = server.endpoint().to_owned();
    assert_success_json(&run_enrichment(&project, &endpoint, &[]));
    let cache = cache_files(&project).pop().expect("cache record");
    let outside_record = fixture.path().join("outside-cache-record.json");
    let outside_bytes = b"OUTSIDE_CACHE_TARGET_SENTINEL\n".to_vec();
    fs::write(&outside_record, &outside_bytes).expect("outside cache target");
    fs::remove_file(&cache).expect("remove cache record");
    symlink(&outside_record, &cache).expect("cache record symlink");

    let output = run_enrichment(&project, &endpoint, &[]);
    let report = assert_success_json(&output);
    assert_eq!(report["cache_hits"], 0);
    assert_eq!(report["requests"], 1);
    assert_eq!(server.finish().len(), 2);
    assert_eq!(
        find_enrichment_nodes(&graph(&project))[0]["summary"],
        "A replacement briefing."
    );
    assert_eq!(
        fs::read(&outside_record).expect("outside cache target"),
        outside_bytes,
        "cache validation followed and overwrote a symlink target"
    );
    assert!(
        fs::symlink_metadata(&cache)
            .expect("repaired cache metadata")
            .file_type()
            .is_file(),
        "unsafe cache entry was not atomically replaced by a regular record"
    );
}

fn assert_preflight_failure_without_network(project: &Path) {
    let baseline = graph_bytes(project);
    let baseline_cache = cache_tree_snapshot(project);
    let tripwire = NetworkTripwire::bind();
    let output = run_enrichment(project, &tripwire.endpoint(), &[]);
    assert_failure_without_secret(&output);
    tripwire.assert_no_request();
    assert_eq!(
        graph_bytes(project),
        baseline,
        "failed preflight changed graph bytes"
    );
    assert_eq!(
        cache_tree_snapshot(project),
        baseline_cache,
        "failed preflight changed the enrichment cache tree"
    );
}

#[test]
fn no_transcript_is_a_successful_zero_candidate_offline_run() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let baseline = index_media_project(&project, None);
    let tripwire = NetworkTripwire::bind();
    let output = run_enrichment(&project, &tripwire.endpoint(), &[]);
    let report = assert_success_json(&output);
    tripwire.assert_no_request();
    assert_eq!(report["candidates"], 0);
    assert_eq!(report["cache_hits"], 0);
    assert_eq!(report["requests"], 0);
    assert_eq!(report["enrichments_written"], 0);
    assert_eq!(graph_bytes(&project), baseline);
}

#[test]
fn invalid_utf8_and_every_input_cap_fail_the_whole_preflight_before_network() {
    // Invalid UTF-8.
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("invalid-utf8");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    fs::write(
        transcript_path(&project, "media/briefing.mp4"),
        [b'v', b'a', b'l', 0xff],
    )
    .expect("invalid UTF-8 transcript");
    assert_preflight_failure_without_network(&project);

    // One byte above the 64 KiB per-file cap.
    let project = fixture.path().join("per-file-cap");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    fs::write(
        transcript_path(&project, "media/briefing.mp4"),
        vec![b'a'; 64 * 1024 + 1],
    )
    .expect("oversized transcript");
    assert_preflight_failure_without_network(&project);

    // Seventeen individually legal 64 KiB inputs exceed the 1 MiB run cap.
    let project = fixture.path().join("aggregate-cap");
    fs::create_dir(&project).expect("project");
    add_inventory_media(&project, 17, &vec![b'a'; 64 * 1024]);
    assert_preflight_failure_without_network(&project);

    // Candidate cap is independently enforced even for tiny transcripts.
    let project = fixture.path().join("candidate-cap");
    fs::create_dir(&project).expect("project");
    add_inventory_media(&project, 33, b"tiny transcript\n");
    assert_preflight_failure_without_network(&project);

    // A later invalid candidate proves the complete set is validated before
    // the first valid candidate can cause any outbound request.
    let project = fixture.path().join("preflight-all");
    fs::create_dir(&project).expect("project");
    let sources = add_inventory_media(&project, 2, b"valid transcript\n");
    fs::write(transcript_path(&project, &sources[1]), [0xff]).expect("invalid later transcript");
    assert_preflight_failure_without_network(&project);
}

#[test]
fn source_path_traversal_is_rejected_before_sidecar_or_network_access() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let outside_media = fixture.path().join("outside.mp4");
    fs::write(&outside_media, b"OUTSIDE_MEDIA_SENTINEL").expect("outside media");
    write_inventory_graph(&project, &["../outside.mp4".into()]);
    // Even a matching path outside the project must not be considered.
    write_transcript(fixture.path(), "outside.mp4", "outside transcript");
    assert_preflight_failure_without_network(&project);
}

#[test]
fn sensitive_media_path_is_revalidated_against_a_persisted_graph_before_network() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let source = "secrets/talk.mp4".to_owned();
    fs::create_dir_all(project.join("secrets")).expect("sensitive media parent");
    fs::write(project.join(&source), b"SENSITIVE_MEDIA_SENTINEL").expect("sensitive media");
    write_transcript(&project, &source, "sensitive transcript");
    write_inventory_graph(&project, std::slice::from_ref(&source));
    assert_preflight_failure_without_network(&project);
}

#[cfg(unix)]
#[test]
fn transcript_final_parent_symlinks_and_hardlinks_are_rejected_before_network() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let transcript = transcript_path(&project, "media/briefing.mp4");
    let outside = fixture.path().join("outside-transcript.txt");
    fs::write(&outside, "outside secret transcript").expect("outside transcript");

    fs::remove_file(&transcript).expect("remove ordinary transcript");
    symlink(&outside, &transcript).expect("final-component transcript symlink");
    assert_preflight_failure_without_network(&project);
    fs::remove_file(&transcript).expect("remove transcript symlink");

    fs::hard_link(&outside, &transcript).expect("hard-linked transcript");
    assert_preflight_failure_without_network(&project);
    fs::remove_file(&transcript).expect("remove transcript hard link");

    let media_parent = transcript.parent().expect("transcript media parent");
    fs::remove_dir(media_parent).expect("remove empty media sidecar directory");
    let outside_parent = fixture.path().join("outside-parent");
    fs::create_dir(&outside_parent).expect("outside parent");
    fs::write(
        outside_parent.join("briefing.mp4.transcript.txt"),
        "outside parent transcript",
    )
    .expect("outside parent transcript");
    symlink(&outside_parent, media_parent).expect("parent-component transcript symlink");
    assert_preflight_failure_without_network(&project);
}

#[cfg(unix)]
#[test]
fn media_final_parent_symlinks_and_non_regular_files_are_rejected_before_network() {
    use std::os::unix::{fs::symlink, net::UnixListener};

    let fixture = tempfile::tempdir().expect("temporary fixture");
    let outside_media = fixture.path().join("outside.mp4");
    fs::write(&outside_media, b"OUTSIDE_MEDIA_SENTINEL").expect("outside media");

    let project = fixture.path().join("final-symlink");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let media = project.join("media/briefing.mp4");
    fs::remove_file(&media).expect("remove media file");
    symlink(&outside_media, &media).expect("media final-component symlink");
    assert_preflight_failure_without_network(&project);

    let project = fixture.path().join("parent-symlink");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let media_parent = project.join("media");
    fs::remove_file(media_parent.join("briefing.mp4")).expect("remove media file");
    fs::remove_dir(&media_parent).expect("remove media parent");
    let outside_parent = fixture.path().join("outside-media-parent");
    fs::create_dir(&outside_parent).expect("outside media parent");
    fs::write(
        outside_parent.join("briefing.mp4"),
        b"OUTSIDE_MEDIA_SENTINEL",
    )
    .expect("outside parent media");
    symlink(&outside_parent, &media_parent).expect("media parent-component symlink");
    assert_preflight_failure_without_network(&project);

    let project = fixture.path().join("non-regular");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let media = project.join("media/briefing.mp4");
    fs::remove_file(&media).expect("remove media file");
    let _socket = UnixListener::bind(&media).expect("media-shaped Unix socket");
    assert_preflight_failure_without_network(&project);
}

#[test]
fn endpoint_query_fragment_credentials_and_non_loopback_cleartext_are_rejected_preflight() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let baseline = graph_bytes(&project);

    for suffix in ["?tenant=secret", "#fragment"] {
        let tripwire = NetworkTripwire::bind();
        let endpoint = format!("{}{suffix}", tripwire.endpoint());
        let output = run_enrichment(&project, &endpoint, &[]);
        assert_failure_without_secret(&output);
        tripwire.assert_no_request();
        assert_eq!(graph_bytes(&project), baseline);
    }

    let output = run_enrichment(&project, "http://192.0.2.1/v1", &[]);
    assert_failure_without_secret(&output);
    assert_eq!(graph_bytes(&project), baseline);
}

fn assert_provider_failure_preserves_graph(
    project: &Path,
    responses: Vec<ResponseSpec>,
    extra: &[&str],
) -> Vec<CapturedRequest> {
    let baseline = graph_bytes(project);
    let baseline_cache = cache_tree_snapshot(project);
    let server = RecordedServer::start(responses);
    let endpoint = server.endpoint().to_owned();
    let output = run_enrichment(project, &endpoint, extra);
    assert_failure_without_secret(&output);
    let requests = server.finish();
    assert_eq!(
        graph_bytes(project),
        baseline,
        "provider failure changed graph bytes"
    );
    assert_eq!(
        cache_tree_snapshot(project),
        baseline_cache,
        "provider failure changed the enrichment cache tree"
    );
    assert_secret_absent_from_project_and_output(project, &output);
    requests
}

#[test]
fn provider_status_schema_size_and_timeout_failures_preserve_graph_bytes() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));

    let private_error =
        format!("{{\"error\":{{\"message\":\"upstream echoed {API_KEY} {ENV_SECRET}\"}}}}");
    let requests = assert_provider_failure_preserves_graph(
        &project,
        vec![ResponseSpec::status(
            500,
            "Internal Server Error",
            private_error,
        )],
        &[],
    );
    assert_eq!(requests.len(), 1);

    let secret_outer_error = json!({"choices": format!("{API_KEY} {ENV_SECRET}")}).to_string();
    let secret_inner = json!({
        "summary": "safe",
        "topics": ["safe"],
        API_KEY: ENV_SECRET
    });
    let secret_inner_error = json!({
        "choices": [{"message": {"content": secret_inner.to_string()}}]
    })
    .to_string();
    for malformed in [
        b"not-json".as_slice(),
        br#"{}"#,
        br#"{"choices":[]}"#,
        br#"{"choices":[{"message":{"content":"{}"}},{"message":{"content":"{}"}}]}"#,
        MALFORMED_CONTENT_RESPONSE.as_bytes(),
        br#"{"choices":[{"message":{"content":"{\"summary\":\"safe\",\"topics\":[\"safe\"],\"unexpected\":true}"}}]}"#,
        secret_outer_error.as_bytes(),
        secret_inner_error.as_bytes(),
    ] {
        let requests = assert_provider_failure_preserves_graph(
            &project,
            vec![ResponseSpec::json(malformed)],
            &[],
        );
        assert_eq!(requests.len(), 1);
    }

    let oversized = vec![b'x'; 16 * 1024 + 1];
    let requests =
        assert_provider_failure_preserves_graph(&project, vec![ResponseSpec::json(oversized)], &[]);
    assert_eq!(requests.len(), 1);

    let requests = assert_provider_failure_preserves_graph(
        &project,
        vec![ResponseSpec::json(SUCCESS_RESPONSE).after(Duration::from_millis(1_500))],
        &["--timeout-seconds", "1"],
    );
    assert_eq!(requests.len(), 1);
}

#[test]
fn redirects_are_not_followed_and_leave_graph_unchanged() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let redirect_target = NetworkTripwire::bind();
    let location = format!("{}/capture", redirect_target.endpoint());
    let requests = assert_provider_failure_preserves_graph(
        &project,
        vec![ResponseSpec::status(302, "Found", []).header("Location", &location)],
        &[],
    );
    assert_eq!(
        requests.len(),
        1,
        "only the configured endpoint may receive a request"
    );
    redirect_target.assert_no_request();
}

#[test]
fn rate_limit_retries_once_with_pacing_and_rejects_excessive_retry_after() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("retry-success");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let server = RecordedServer::start(vec![
        ResponseSpec::status(429, "Too Many Requests", br#"{"error":"paced"}"#)
            .header("Retry-After", "0"),
        ResponseSpec::json(SUCCESS_RESPONSE),
    ]);
    let endpoint = server.endpoint().to_owned();
    let output = run_enrichment(&project, &endpoint, &["--requests-per-minute", "600"]);
    let report = assert_success_json(&output);
    assert_eq!(report["requests"], 2);
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .received_at
            .duration_since(requests[0].received_at)
            >= Duration::from_millis(90),
        "retry was not paced: {:?}",
        requests[1]
            .received_at
            .duration_since(requests[0].received_at)
    );

    let project = fixture.path().join("retry-twice");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let requests = assert_provider_failure_preserves_graph(
        &project,
        vec![
            ResponseSpec::status(429, "Too Many Requests", br#"{"error":"one"}"#)
                .header("Retry-After", "0"),
            ResponseSpec::status(429, "Too Many Requests", br#"{"error":"two"}"#)
                .header("Retry-After", "0"),
        ],
        &["--requests-per-minute", "600"],
    );
    assert_eq!(requests.len(), 2, "429 is retried at most once");

    let project = fixture.path().join("retry-after-cap");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let started = Instant::now();
    let requests = assert_provider_failure_preserves_graph(
        &project,
        vec![
            ResponseSpec::status(429, "Too Many Requests", br#"{"error":"cap"}"#)
                .header("Retry-After", "31"),
        ],
        &["--requests-per-minute", "600"],
    );
    assert_eq!(requests.len(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "Retry-After above the 30 second cap must not be slept"
    );
}

#[test]
fn same_profile_replaces_in_place_and_foreign_identity_collision_is_fail_closed() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));

    let first = RecordedServer::start(vec![ResponseSpec::json(SUCCESS_RESPONSE)]);
    let first_endpoint = first.endpoint().to_owned();
    assert_success_json(&run_enrichment(&project, &first_endpoint, &[]));
    assert_eq!(first.finish().len(), 1);
    let first_graph = graph(&project);
    let first_nodes = find_enrichment_nodes(&first_graph);
    assert_eq!(first_nodes.len(), 1);
    let stable_id = first_nodes[0]["id"]
        .as_str()
        .expect("first enrichment ID")
        .to_owned();

    write_transcript(
        &project,
        "media/briefing.mp4",
        "Changed transcript for deterministic replacement.\n",
    );
    let second = RecordedServer::start(vec![ResponseSpec::json(SECOND_RESPONSE)]);
    let second_endpoint = second.endpoint().to_owned();
    let output = run_enrichment(&project, &second_endpoint, &[]);
    assert_success_json(&output);
    assert_eq!(second.finish().len(), 1);
    let replaced_graph = graph(&project);
    let replaced_nodes = find_enrichment_nodes(&replaced_graph);
    assert_eq!(
        replaced_nodes.len(),
        1,
        "same profile replaces instead of appending"
    );
    assert_eq!(replaced_nodes[0]["id"], stable_id);
    assert_eq!(replaced_nodes[0]["summary"], "A replacement briefing.");
    assert_eq!(
        replaced_nodes[0]["topics"],
        json!(["bounded enrichment", "determinism"]),
        "topics have canonical ordering"
    );
    assert_eq!(
        replaced_graph["links"]
            .as_array()
            .expect("links")
            .iter()
            .filter(|edge| edge["_origin"] == "enrichment")
            .count(),
        1
    );
    let accepted = graph_bytes(&project);
    let accepted_cache = cache_tree_snapshot(&project);
    let replay = run_enrichment(&project, &second_endpoint, &[]);
    let replay_report = assert_success_json(&replay);
    assert_eq!(replay_report["cache_hits"], 1);
    assert_eq!(replay_report["requests"], 0);
    assert_eq!(graph_bytes(&project), accepted);

    // Turn the stable ID into a foreign structural fact. A future run must not
    // overwrite it merely because its ID matches the profile-owned identity.
    let mut collided = graph(&project);
    let nodes = collided["nodes"].as_array_mut().expect("nodes");
    let foreign = nodes
        .iter_mut()
        .find(|node| node["id"] == stable_id)
        .expect("enrichment node to collide");
    foreign["_origin"] = json!("ast");
    foreign["type"] = json!("function");
    collided["links"]
        .as_array_mut()
        .expect("links")
        .retain(|edge| edge["_origin"] != "enrichment");
    let mut collision_bytes = serde_json::to_vec_pretty(&collided).expect("collision graph");
    collision_bytes.push(b'\n');
    fs::write(managed(&project, "graph.json"), &collision_bytes).expect("foreign collision graph");
    write_transcript(
        &project,
        "media/briefing.mp4",
        "A third uncached transcript for collision validation.\n",
    );
    let collision_server = RecordedServer::start(vec![ResponseSpec::json(SUCCESS_RESPONSE)]);
    let collision_endpoint = collision_server.endpoint().to_owned();
    let collision_output = run_enrichment(&project, &collision_endpoint, &[]);
    assert_failure_without_secret(&collision_output);
    assert_eq!(collision_server.finish().len(), 1);
    assert_eq!(
        graph_bytes(&project),
        collision_bytes,
        "foreign stable-ID owner was overwritten"
    );
    assert_eq!(
        cache_tree_snapshot(&project),
        accepted_cache,
        "failed graph apply published an uncommitted cache record"
    );
}

#[test]
fn attacker_controlled_apply_error_is_mapped_without_leaking_or_publishing() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));

    let first = RecordedServer::start(vec![ResponseSpec::json(SUCCESS_RESPONSE)]);
    let first_endpoint = first.endpoint().to_owned();
    assert_success_json(&run_enrichment(&project, &first_endpoint, &[]));
    assert_eq!(first.finish().len(), 1);
    let baseline_cache = cache_tree_snapshot(&project);

    let mut poisoned = graph(&project);
    let stable_id = find_enrichment_nodes(&poisoned)[0]["id"]
        .as_str()
        .expect("enrichment ID")
        .to_owned();
    let mut foreign_edge = poisoned["links"]
        .as_array()
        .expect("graph links")
        .iter()
        .find(|edge| edge["_origin"] == "enrichment" && edge["target"] == stable_id)
        .expect("profile-owned enrichment edge")
        .clone();
    foreign_edge
        .as_object_mut()
        .expect("edge object")
        .remove("_origin");
    foreign_edge["relation"] = json!(API_KEY);
    poisoned["links"]
        .as_array_mut()
        .expect("graph links")
        .push(foreign_edge);
    let mut poisoned_bytes = serde_json::to_vec_pretty(&poisoned).expect("poisoned graph JSON");
    poisoned_bytes.push(b'\n');
    fs::write(managed(&project, "graph.json"), &poisoned_bytes).expect("poisoned graph");

    write_transcript(
        &project,
        "media/briefing.mp4",
        "An uncached replacement that reaches graph apply.\n",
    );
    let server = RecordedServer::start(vec![ResponseSpec::json(SECOND_RESPONSE)]);
    let endpoint = server.endpoint().to_owned();
    let output = run_enrichment(&project, &endpoint, &[]);
    assert_failure_without_secret(&output);
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("failed to apply validated enrichment facts"));
    assert_eq!(server.finish().len(), 1);
    assert_eq!(graph_bytes(&project), poisoned_bytes);
    assert_eq!(cache_tree_snapshot(&project), baseline_cache);
    for (_, bytes) in all_project_bytes(&project.join(".graphoxide/enrichment-cache")) {
        assert!(
            !bytes
                .windows(API_KEY.len())
                .any(|window| window == API_KEY.as_bytes()),
            "attacker-controlled apply error leaked into cache"
        );
    }
}

#[test]
fn graph_compare_and_swap_detects_a_concurrent_writer_without_lost_updates() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let baseline_cache = cache_tree_snapshot(&project);
    let graph_path = managed(&project, "graph.json");
    let raced_bytes: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let action_graph = graph_path.clone();
    let action_bytes = Arc::clone(&raced_bytes);
    let action: RequestAction = Arc::new(move |_ordinal, _request| {
        let mut value: Value = serde_json::from_slice(
            &fs::read(&action_graph).expect("read graph for concurrent writer"),
        )
        .expect("concurrent graph JSON");
        value["graph"]["concurrent_writer_sentinel"] = json!(true);
        let mut bytes = serde_json::to_vec_pretty(&value).expect("serialize concurrent graph");
        bytes.push(b'\n');
        fs::write(&action_graph, &bytes).expect("concurrent graph write");
        *action_bytes.lock().expect("raced bytes lock") = Some(bytes);
    });
    let server =
        RecordedServer::start_with_action(vec![ResponseSpec::json(SUCCESS_RESPONSE)], Some(action));
    let endpoint = server.endpoint().to_owned();
    let output = run_enrichment(&project, &endpoint, &[]);
    assert_failure_without_secret(&output);
    assert_eq!(server.finish().len(), 1);
    let expected = raced_bytes
        .lock()
        .expect("raced bytes lock")
        .clone()
        .expect("concurrent writer ran");
    assert_eq!(
        graph_bytes(&project),
        expected,
        "CAS failure must preserve the concurrent writer's exact bytes"
    );
    assert!(find_enrichment_nodes(&graph(&project)).is_empty());
    assert_eq!(
        cache_tree_snapshot(&project),
        baseline_cache,
        "CAS failure changed the enrichment cache tree"
    );
}

#[cfg(unix)]
#[test]
fn cancellation_during_provider_wait_preserves_graph_and_cache() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let baseline = index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let baseline_cache = cache_tree_snapshot(&project);
    let request_seen = Arc::new(AtomicBool::new(false));
    let action_seen = Arc::clone(&request_seen);
    let action: RequestAction = Arc::new(move |_ordinal, _request| {
        action_seen.store(true, Ordering::Release);
    });
    let server = RecordedServer::start_with_action(
        vec![ResponseSpec::json(SUCCESS_RESPONSE).after(Duration::from_secs(2))],
        Some(action),
    );
    let endpoint = server.endpoint().to_owned();
    let child = spawn_enrichment(&project, &endpoint, &[]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !request_seen.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        request_seen.load(Ordering::Acquire),
        "provider request was not observed"
    );
    // SAFETY: `child.id()` is the live subprocess created immediately above;
    // SIGINT is Graphoxide's documented graceful cancellation path.
    let signal_result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(signal_result, 0, "send SIGINT to enrichment subprocess");
    let output = child
        .wait_with_output()
        .expect("wait for cancelled enrichment");
    assert_failure_without_secret(&output);
    assert_eq!(server.finish().len(), 1);
    assert_eq!(graph_bytes(&project), baseline);
    assert_eq!(cache_tree_snapshot(&project), baseline_cache);
}

#[cfg(unix)]
#[test]
fn cancellation_while_rebuild_lock_is_held_is_prompt_and_side_effect_free() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let baseline_graph = index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let baseline_cache = cache_tree_snapshot(&project);
    let guard = graphoxide_cli::watch::RebuildLockGuard::acquire(&managed(&project, ""), true)
        .expect("acquire test-held rebuild lock")
        .expect("test-held rebuild lock");

    let server = RecordedServer::start(vec![ResponseSpec::json(SUCCESS_RESPONSE)]);
    let endpoint = server.endpoint().to_owned();
    let mut child = spawn_enrichment(&project, &endpoint, &[]);
    let request_deadline = Instant::now() + Duration::from_secs(5);
    while server
        .requests
        .lock()
        .expect("recorded request lock")
        .is_empty()
        && Instant::now() < request_deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !server
            .requests
            .lock()
            .expect("recorded request lock")
            .is_empty(),
        "provider request was not observed"
    );
    // Allow the tiny recorded response to reach the subprocess so it is
    // waiting specifically on the rebuild lock held above.
    thread::sleep(Duration::from_millis(250));
    // SAFETY: `child.id()` is the live subprocess created immediately above.
    let signal_result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(signal_result, 0, "send SIGINT to lock-waiting enrichment");
    let cancel_deadline = Instant::now() + Duration::from_secs(1);
    let mut exited_while_locked = false;
    while Instant::now() < cancel_deadline {
        if child
            .try_wait()
            .expect("poll cancelled enrichment")
            .is_some()
        {
            exited_while_locked = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    drop(guard);
    let output = child
        .wait_with_output()
        .expect("collect lock-cancelled enrichment");
    let requests = server.finish();
    assert!(
        exited_while_locked,
        "SIGINT did not stop enrichment promptly while the rebuild lock remained held\n{}",
        output_text(&output)
    );
    assert_failure_without_secret(&output);
    assert_eq!(requests.len(), 1);
    assert_eq!(graph_bytes(&project), baseline_graph);
    assert_eq!(cache_tree_snapshot(&project), baseline_cache);
}

#[cfg(unix)]
#[test]
fn atomic_graph_write_failure_leaves_previous_graph_bytes_intact() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let baseline = index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let baseline_cache = cache_tree_snapshot(&project);
    let output_directory = managed(&project, "");
    let action_directory = output_directory.clone();
    let action: RequestAction = Arc::new(move |_ordinal, _request| {
        fs::set_permissions(&action_directory, fs::Permissions::from_mode(0o500))
            .expect("make graph output directory read-only");
    });
    let server =
        RecordedServer::start_with_action(vec![ResponseSpec::json(SUCCESS_RESPONSE)], Some(action));
    let endpoint = server.endpoint().to_owned();
    let output = run_enrichment(&project, &endpoint, &[]);
    fs::set_permissions(&output_directory, fs::Permissions::from_mode(0o700))
        .expect("restore graph output permissions");
    assert_failure_without_secret(&output);
    assert_eq!(server.finish().len(), 1);
    assert_eq!(graph_bytes(&project), baseline);
    assert_eq!(
        cache_tree_snapshot(&project),
        baseline_cache,
        "atomic graph failure changed the enrichment cache tree"
    );
}

#[cfg(unix)]
#[test]
fn enrichment_does_not_need_read_permission_on_media_payload_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    index_media_project(&project, Some(SAFE_TRANSCRIPT));
    let media = project.join("media/briefing.mp4");
    fs::set_permissions(&media, fs::Permissions::from_mode(0o000))
        .expect("remove media read permission");
    let server = RecordedServer::start(vec![ResponseSpec::json(SUCCESS_RESPONSE)]);
    let endpoint = server.endpoint().to_owned();
    let output = run_enrichment(&project, &endpoint, &[]);
    fs::set_permissions(&media, fs::Permissions::from_mode(0o600))
        .expect("restore media permission");
    assert_success_json(&output);
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(!String::from_utf8_lossy(&requests[0].raw_body).contains("MEDIA_PAYLOAD_SENTINEL"));
}
