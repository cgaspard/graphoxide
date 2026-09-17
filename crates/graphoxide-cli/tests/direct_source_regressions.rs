//! Public-command regressions for direct-source lifecycle and evidence handling.
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use graphoxide_cli::wiki_source::{self, SourceEntry, SourceStatus};
use lopdf::dictionary;
use serde_json::{json, Value};

struct Provider {
    address: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    fail: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    allow_disconnect: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Provider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(false));
        let allow_disconnect = Arc::new(AtomicBool::new(false));
        let disconnected = allow_disconnect.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (recorded, failing, stopped) = (requests.clone(), fail.clone(), stop.clone());
        let paused = pause.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let request = read_request(&mut stream);
                        let mut requests = recorded.lock().unwrap();
                        let review = request["messages"][0]["content"]
                            .as_str()
                            .unwrap()
                            .contains("Independently verify");
                        requests.push(request);
                        let answer = if review {
                            json!({"decision": "approve"})
                        } else {
                            json!({
                                "title": "Protocol overview",
                                "markdown": format!("Derived protocol explanation, revision {}.", requests.len()),
                                "primary_subject": "interfaces-and-protocols/grpc",
                                "facets": {}, "applicability": [],
                            })
                        };
                        let body =
                            json!({"choices": [{"message": {"content": answer.to_string()}}]})
                                .to_string();
                        drop(requests);
                        let deadline = Instant::now() + Duration::from_secs(10);
                        while paused.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire) {
                            assert!(
                                Instant::now() < deadline,
                                "fixture provider pause timed out"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        let status = if failing.load(Ordering::Acquire) {
                            "400 Bad Request"
                        } else {
                            "200 OK"
                        };
                        if let Err(error) = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()) {
                            assert!(
                                disconnected.load(Ordering::Acquire)
                                    && matches!(error.kind(), std::io::ErrorKind::BrokenPipe
                                        | std::io::ErrorKind::ConnectionReset
                                        | std::io::ErrorKind::ConnectionAborted),
                                "write fixture response: {error}",
                            );
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept fixture request: {error}"),
                }
            }
        });
        Self {
            address,
            requests,
            fail,
            pause,
            allow_disconnect,
            stop,
            worker: Some(worker),
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && let Err(error) = worker.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0, "fixture received incomplete request");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(
            bytes.len() < 2 * 1024 * 1024,
            "fixture request exceeds limit"
        );
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .expect("request content length");
            if bytes.len() >= end + 4 + length {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
            }
        }
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    root: PathBuf,
    provider: Provider,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("knowledgebase");
        fs::create_dir_all(root.join("config")).unwrap();
        super::init_git(&root);
        let provider = Provider::start();
        fs::write(root.join("config/provider.json"), json!({
            "version": 1, "id": "fixture", "protocol": "openai-compatible",
            "endpoint": format!("http://{}/v1", provider.address),
            "credential_env": "GRAPHOXIDE_TEST_DIRECT_KEY", "source_egress_consent": "fixture-consent",
            "models": [{"id": "model", "api_model": "fixture", "label": "Fixture",
                "capabilities": ["structured-output", "text-generation"]}],
        }).to_string()).unwrap();
        fs::write(
            root.join("config/input.json"),
            json!({
                "provider_profile": "config/provider.json", "author_model": "model",
                "reviewer_model": "model", "source_egress_consent": "fixture-consent",
            })
            .to_string(),
        )
        .unwrap();
        let fixture = Self {
            directory,
            root,
            provider,
        };
        fixture.success(&["wiki", "init", "--authoring-profile", "config/input.json"]);
        fixture
    }

    fn command(&self) -> Command {
        let mut command = super::graphoxide();
        command
            .current_dir(&self.root)
            .env("GRAPHOXIDE_TEST_DIRECT_KEY", "fixture-key");
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command().args(arguments).output().unwrap()
    }

    fn success(&self, arguments: &[&str]) -> Output {
        let output = self.run(arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn external(&self, name: &str, body: &[u8]) -> PathBuf {
        let path = self.directory.path().canonicalize().unwrap().join(name);
        fs::write(&path, body).unwrap();
        path
    }

    fn add(&self, path: &Path) -> SourceEntry {
        let output = self
            .command()
            .args(["wiki", "source", "add", "--allow-model-egress"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "add: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        serde_json::from_value(value["sources"][0].clone()).unwrap()
    }

    fn source(&self, id: &str) -> SourceEntry {
        wiki_source::source_status(&self.root)
            .unwrap()
            .into_iter()
            .find(|entry| entry.source_id == id)
            .unwrap()
    }

    fn confirm(&self, id: &str) {
        self.success(&["wiki", "source", "review", id, "--allow-model-egress"]);
        self.success(&["wiki", "source", "confirm", id]);
        assert_eq!(self.source(id).status, SourceStatus::HumanConfirmed);
    }

    fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let relative = path.strip_prefix(root).unwrap();
                if relative == Path::new(".git") || relative.starts_with(".graphoxide/hugo") {
                    continue;
                }
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.insert(relative.to_owned(), fs::read(&path).unwrap());
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(&self.root, &self.root, &mut files);
        files
    }

    #[cfg(unix)]
    fn preview(&self) {
        use std::os::unix::fs::PermissionsExt;
        let hugo = self.external(
            "hugo-fixture",
            b"#!/bin/sh\nif [ \"$1\" = version ]; then echo 'hugo v0.165.0'; fi\nexit 0\n",
        );
        fs::set_permissions(&hugo, fs::Permissions::from_mode(0o700)).unwrap();
        let output = self
            .command()
            .env("GRAPHOXIDE_HUGO_BINARY", &hugo)
            .args(["wiki", "live", ".", "--port", "1313"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "preview: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

const ORIGINAL: &[u8] = b"ORBITAL_SENTINEL_417 protocol requires nine acknowledgement rounds.\n";
const CHANGED: &[u8] =
    b"ORBITAL_SENTINEL_418 protocol now requires eleven acknowledgement rounds.\n";

#[test]
fn repeated_init_preserves_populated_knowledgebase_byte_for_byte() {
    let fixture = Fixture::new();
    let path = fixture.external("protocol.md", ORIGINAL);
    let source = fixture.add(&path);
    fixture.confirm(&source.source_id);
    let before = fixture.snapshot();
    let requests = fixture.provider.count();
    let output = fixture.run(&["wiki", "init", "--authoring-profile", "config/input.json"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already initialized"));
    assert_eq!(fixture.snapshot(), before);
    assert_eq!(fixture.provider.count(), requests);
    assert_eq!(fs::read(path).unwrap(), ORIGINAL);
}

#[test]
fn changed_refresh_regenerates_confirmed_source_and_recovers_from_provider_failure() {
    let fixture = Fixture::new();
    let path = fixture.external("protocol.md", ORIGINAL);
    let source = fixture.add(&path);
    fixture.confirm(&source.source_id);
    let before = fixture.snapshot();
    let requests = fixture.provider.count();
    fixture.success(&[
        "wiki",
        "source",
        "refresh",
        &source.source_id,
        "--allow-model-egress",
    ]);
    assert_eq!(fixture.snapshot(), before);
    assert_eq!(fixture.provider.count(), requests);

    fs::write(&path, CHANGED).unwrap();
    fixture.provider.fail.store(true, Ordering::Release);
    let output = fixture.run(&[
        "wiki",
        "source",
        "refresh",
        &source.source_id,
        "--allow-model-egress",
    ]);
    assert!(!output.status.success());
    assert_eq!(
        fixture.snapshot(),
        before,
        "failed authoring must preserve last coherent revision"
    );

    fixture.provider.fail.store(false, Ordering::Release);
    fixture.success(&[
        "wiki",
        "source",
        "refresh",
        &source.source_id,
        "--allow-model-egress",
    ]);
    let refreshed = fixture.source(&source.source_id);
    assert_eq!(refreshed.status, SourceStatus::Provisional);
    assert_ne!(refreshed.content_sha256, source.content_sha256);
    let id = source.source_id.strip_prefix("src:").unwrap();
    for kind in ["ai", "human"] {
        assert!(!fixture
            .root
            .join(format!("taxonomy/reviews/{id}-{kind}.json"))
            .exists());
    }
    let assignments: Value =
        serde_json::from_slice(&fs::read(fixture.root.join("taxonomy/assignments.json")).unwrap())
            .unwrap();
    assert_eq!(
        assignments["assignments"][0]["content_sha256"],
        refreshed.content_sha256
    );
    #[cfg(unix)]
    fixture.preview();
    fixture.confirm(&source.source_id);
    let second = fixture.external(
        "independent.md",
        b"An independent protocol defines message envelope validation.\n",
    );
    fixture.add(&second);
    #[cfg(unix)]
    fixture.preview();
}

#[test]
fn retire_removes_owned_artifacts_and_preserves_other_sources_and_future_adds() {
    let fixture = Fixture::new();
    let first_path = fixture.external("first.md", ORIGINAL);
    let second_path = fixture.external(
        "second.md",
        b"Independent protocol evidence for request sequencing.\n",
    );
    let first = fixture.add(&first_path);
    let second = fixture.add(&second_path);
    fixture.confirm(&first.source_id);
    fixture.confirm(&second.source_id);
    #[cfg(unix)]
    fixture.preview();
    let before = fixture.snapshot();
    fixture.success(&["wiki", "source", "retire", &first.source_id]);
    assert_eq!(wiki_source::source_status(&fixture.root).unwrap().len(), 1);
    assert_eq!(
        fixture.source(&second.source_id).status,
        SourceStatus::HumanConfirmed
    );
    let first_id = first.source_id.strip_prefix("src:").unwrap();
    let second_id = second.source_id.strip_prefix("src:").unwrap();
    let after = fixture.snapshot();
    for (path, bytes) in &before {
        if path.to_string_lossy().contains(second_id) {
            assert_eq!(
                after.get(path),
                Some(bytes),
                "unrelated source artifact changed"
            );
        }
    }
    assert!(after
        .keys()
        .all(|path| !path.to_string_lossy().contains(first_id)));
    let assignments: Value =
        serde_json::from_slice(&after[Path::new("taxonomy/assignments.json")]).unwrap();
    assert_eq!(assignments["assignments"].as_array().unwrap().len(), 1);
    assert_eq!(assignments["assignments"][0]["source_id"], second.source_id);
    assert_eq!(fs::read(&first_path).unwrap(), ORIGINAL);
    #[cfg(unix)]
    fixture.preview();
    let third = fixture.external(
        "third.md",
        b"A third protocol specifies header field lengths.\n",
    );
    fixture.add(&third);
    #[cfg(unix)]
    fixture.preview();
}

#[test]
fn binary_evidence_is_rejected_without_model_requests_or_published_artifacts() {
    let fixture = Fixture::new();
    let mut document = lopdf::Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let font_id = document.add_object(lopdf::dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let stream_id = document.add_object(lopdf::Stream::new(
        lopdf::dictionary! {},
        b"BT /F1 12 Tf 72 720 Td (Compressed binary evidence) Tj ET".to_vec(),
    ));
    let page_id = document.add_object(lopdf::dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => stream_id,
        "Resources" => lopdf::dictionary! { "Font" => lopdf::dictionary! { "F1" => font_id } },
    });
    document.objects.insert(
        pages_id,
        lopdf::dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }
        .into(),
    );
    let catalog =
        document.add_object(lopdf::dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    document.trailer.set("Root", catalog);
    document.compress();
    let mut binary = Vec::new();
    document.save_to(&mut binary).unwrap();
    assert!(std::str::from_utf8(&binary).is_err());
    assert!(lopdf::Document::load_mem(&binary)
        .unwrap()
        .extract_text(&[1])
        .unwrap()
        .contains("Compressed binary evidence"));
    let path = fixture.external("evidence.pdf", &binary);
    let before = fixture.snapshot();
    let output = fixture
        .command()
        .args(["wiki", "source", "add", "--allow-model-egress"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fixture.provider.count(), 0);
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("UTF-8") || diagnostic.contains("text"),
        "{diagnostic}"
    );
    assert!(wiki_source::source_status(&fixture.root)
        .unwrap()
        .is_empty());
    for (path, bytes) in before {
        if path == Path::new(".gitignore") {
            continue; // Admission may add the ignored, body-free local-binding rule.
        }
        assert_eq!(fs::read(fixture.root.join(path)).unwrap(), bytes);
    }
    assert!(!fixture.root.join("taxonomy/assignments.json").exists());
    assert!(
        !fixture.root.join("content/provisional").exists()
            || fs::read_dir(fixture.root.join("content/provisional"))
                .unwrap()
                .next()
                .is_none()
    );
    let text = fixture.external("control.md", ORIGINAL);
    let source = fixture.add(&text);
    fixture.confirm(&source.source_id);
    let requests = fixture.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert!(request.to_string().contains("ORBITAL_SENTINEL_417"));
        assert!(!request.to_string().contains("[non-text source]"));
    }
}

#[test]
fn remote_git_refresh_fails_before_model_or_index_mutation_even_with_network_consent() {
    use sha2::{Digest, Sha256};
    let fixture = Fixture::new();
    let location = wiki_source::SourceLocation::Git {
        remote: "https://example.invalid/pinned.git".into(),
        commit: "a".repeat(40),
        path: "guide.md".into(),
    };
    let source = SourceEntry {
        source_id: location.source_id(),
        location,
        content_sha256: hex::encode(Sha256::digest(ORIGINAL)),
        bytes: ORIGINAL.len() as u64,
        status: SourceStatus::Provisional,
    };
    wiki_source::write_source_index(
        &fixture.root,
        &wiki_source::SourceIndex {
            schema: "graphoxide.source-index".into(),
            sources: vec![source.clone()],
        },
    )
    .unwrap();
    let before = fixture.snapshot();
    for allow_network in [false, true] {
        let mut command = fixture.command();
        command.args([
            "wiki",
            "source",
            "refresh",
            &source.source_id,
            "--allow-model-egress",
        ]);
        if allow_network {
            command.arg("--allow-network");
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains("Git") && diagnostic.contains("disabled"),
            "{diagnostic}"
        );
        assert_eq!(fixture.snapshot(), before);
        assert_eq!(fixture.provider.count(), 0);
    }
}

#[test]
fn overlapping_refresh_cannot_replace_an_add_that_is_waiting_for_its_author() {
    let fixture = Fixture::new();
    let path = fixture.external("protocol.md", ORIGINAL);
    fixture.provider.pause.store(true, Ordering::Release);
    let add = fixture
        .command()
        .args(["wiki", "source", "add", "--allow-model-egress"])
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.provider.count() == 0 {
        assert!(Instant::now() < deadline, "add never reached the model");
        thread::sleep(Duration::from_millis(5));
    }
    let source = wiki_source::source_status(&fixture.root).unwrap().remove(0);
    let output = fixture.run(&[
        "wiki",
        "source",
        "refresh",
        &source.source_id,
        "--allow-model-egress",
    ]);
    fixture.provider.pause.store(false, Ordering::Release);
    let added = add.wait_with_output().unwrap();
    assert!(!output.status.success(), "overlapping refresh must not run");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("busy"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert_eq!(fixture.provider.count(), 1);
    assert_eq!(
        fixture.source(&source.source_id).status,
        SourceStatus::Provisional
    );
    fixture.success(&[
        "wiki",
        "source",
        "refresh",
        &source.source_id,
        "--allow-model-egress",
    ]);
    fixture.confirm(&source.source_id);
    #[cfg(unix)]
    fixture.preview();
}

#[test]
fn wiki_activity_tracks_authored_pages_review_and_failure() {
    use graphoxide_cli::activity_progress::ACTIVITY_PROGRESS_PREFIX;
    let fixture = Fixture::new();
    let source = fixture.external(
        "private-reference.md",
        b"Source text must not enter progress.\n",
    );
    let nonce = "0123456789abcdef0123456789abcdef";
    let events = |output: &Output| {
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter_map(|line| line.strip_prefix(ACTIVITY_PROGRESS_PREFIX))
            .map(|payload| {
                assert!(payload.len() < 512);
                assert!(!payload.contains("private-reference"));
                assert!(!payload.contains("Source text"));
                assert!(!payload.contains("fixture-key"));
                let value: Value = serde_json::from_str(payload).unwrap();
                assert_eq!(value["run_nonce"], nonce);
                assert_eq!(value["operation"], "wiki");
                value
            })
            .collect::<Vec<_>>()
    };
    let added = fixture
        .command()
        .args([
            "wiki",
            "source",
            "add",
            "--allow-model-egress",
            "--progress=json",
        ])
        .arg(&source)
        .env("GRAPHOXIDE_PROGRESS_NONCE", nonce)
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let progress = events(&added);
    assert_eq!(progress.first().unwrap()["type"], "started");
    assert_eq!(progress.last().unwrap()["type"], "completed");
    assert!(progress.iter().any(|event| event["phase"] == "admitting"));
    assert!(progress.iter().any(|event| event["phase"] == "authoring"
        && event["processed"] == 0
        && event["total"] == 1));
    assert!(progress.iter().any(|event| event["phase"] == "authoring"
        && event["processed"] == 1
        && event["total"] == 1));
    assert_eq!(progress[progress.len() - 2]["phase"], "publishing");
    let value: Value = serde_json::from_slice(&added.stdout).unwrap();
    let source_id = value["sources"][0]["source_id"].as_str().unwrap();
    let reviewed = fixture
        .command()
        .args([
            "wiki",
            "source",
            "review",
            source_id,
            "--allow-model-egress",
            "--progress=json",
        ])
        .env("GRAPHOXIDE_PROGRESS_NONCE", nonce)
        .output()
        .unwrap();
    assert!(
        reviewed.status.success(),
        "{}",
        String::from_utf8_lossy(&reviewed.stderr)
    );
    let progress = events(&reviewed);
    assert!(progress
        .iter()
        .any(|event| event["phase"] == "reviewing" && event["processed"] == 0));
    assert!(progress
        .iter()
        .any(|event| event["phase"] == "reviewing" && event["processed"] == 1));
    assert_eq!(progress.last().unwrap()["type"], "completed");
    let failed = fixture
        .command()
        .args([
            "wiki",
            "source",
            "add",
            "missing-reference.md",
            "--allow-model-egress",
            "--progress=json",
        ])
        .env("GRAPHOXIDE_PROGRESS_NONCE", nonce)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let progress = events(&failed);
    assert_eq!(progress.first().unwrap()["type"], "started");
    assert_eq!(progress.last().unwrap()["type"], "failed");
    assert!(!progress.iter().any(|event| event["type"] == "completed"));
    let silent = fixture.success(&["wiki", "source", "status", "--json"]);
    assert!(!String::from_utf8_lossy(&silent.stderr).contains(ACTIVITY_PROGRESS_PREFIX));
}

#[test]
fn cooperative_wiki_cancel_rolls_back_add_and_preserves_refresh_and_review() {
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    for (operation, close_stdin) in [
        ("add", false),
        ("refresh", false),
        ("review", false),
        ("add", true),
    ] {
        let fixture = Fixture::new();
        let prior_path = fixture.external("prior.md", ORIGINAL);
        let prior = fixture.add(&prior_path);
        let new_path = fixture.external("cancelled.md", b"New source must be rolled back.\n");
        let target = if operation == "add" {
            new_path.to_string_lossy().into_owned()
        } else {
            if operation == "refresh" {
                fs::write(&prior_path, b"Changed source must not be published.\n").unwrap();
            }
            prior.source_id.clone()
        };
        let before = fixture.snapshot();
        let requests = fixture.provider.count();
        fixture.provider.pause.store(true, Ordering::Release);
        let mut child = fixture
            .command()
            .args([
                "wiki",
                "source",
                operation,
                &target,
                "--allow-model-egress",
                "--progress=json",
            ])
            .env("GRAPHOXIDE_CANCEL_STDIN", "1")
            .env("GRAPHOXIDE_PROGRESS_NONCE", NONCE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while fixture.provider.count() == requests {
            if Instant::now() >= deadline {
                fixture.provider.pause.store(false, Ordering::Release);
                let _ = child.kill();
                panic!("{operation} did not reach the model");
            }
            thread::sleep(Duration::from_millis(5));
        }
        if close_stdin {
            drop(child.stdin.take());
        } else {
            writeln!(child.stdin.take().unwrap(), "{NONCE}").unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                fixture.provider.pause.store(false, Ordering::Release);
                let _ = child.kill();
                panic!("{operation} cancellation waited for the blocked model");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        // The client has exited while its response was held. A failed write
        // to that deliberately closed socket is expected only in this test.
        fixture
            .provider
            .allow_disconnect
            .store(true, Ordering::Release);
        fixture.provider.pause.store(false, Ordering::Release);
        assert!(!output.status.success(), "cancelled {operation} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Wiki operation cancelled"), "{stderr}");
        let events = stderr
            .lines()
            .filter_map(|line| {
                line.strip_prefix(graphoxide_cli::activity_progress::ACTIVITY_PROGRESS_PREFIX)
            })
            .map(|payload| serde_json::from_str::<Value>(payload).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.last().unwrap()["type"], "failed");
        assert!(!events.iter().any(|event| event["type"] == "completed"));
        assert_eq!(
            fixture.snapshot(),
            before,
            "cancelled {operation} changed prior knowledgebase artifacts"
        );
        assert_eq!(
            wiki_source::source_status(&fixture.root).unwrap(),
            vec![prior]
        );
    }
}
