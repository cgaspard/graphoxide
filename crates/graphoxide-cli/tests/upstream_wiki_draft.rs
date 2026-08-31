//! Secure wiki publication (draft/render) is supported on Linux x86_64 and
//! macOS, so the whole suite is gated to those platforms.
#![cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]

use graphoxide_cli::{
    ollama_transport,
    wiki_draft::{draft, normalize_scopes, render, DraftArgs, DraftScope, RenderArgs, CONSENT},
    wiki_provider::{ProviderProfile, WikiModelTransport},
};
use graphoxide_core::KnowledgeGraph;
use graphoxide_export::project_wiki_evidence;
use graphoxide_extract::registry::{
    add_origin, append_capture_and_activate, initialize_tree, shard_for_source_id, RegistryCapture,
    RegistryOrigin,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    net::{IpAddr, TcpListener},
    path::PathBuf,
    process::{Command, Output},
    sync::{Arc, Mutex},
    thread,
};
use tempfile::TempDir;

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[test]
fn draft_scope_normalizes_compatibility_default_and_deduplicates() {
    assert_eq!(
        normalize_scopes(BTreeSet::new()),
        BTreeSet::from([DraftScope::Community])
    );
    assert_eq!(
        normalize_scopes(BTreeSet::from([DraftScope::Source, DraftScope::Source])),
        BTreeSet::from([DraftScope::Source])
    );
}

fn node(path: &str, community: i64, source_id: &str, capture_id: &str, hash: &str) -> Value {
    json!({
        "id": format!("{community}:{source_id}"),
        "label": source_id,
        "file_type": "document",
        "source_file": path,
        "community": community,
        "community_name": format!("Community {community}"),
        "catalog": {"source_id": source_id, "capture_id": capture_id, "sha256": hash}
    })
}

fn fixture(files: &[(&str, &[u8], i64)]) -> (TempDir, DraftArgs) {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("source");
    fs::create_dir(&root).unwrap();
    let mut nodes = Vec::new();
    for (index, (path, bytes, community)) in files.iter().enumerate() {
        let source = root.join(path);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, bytes).unwrap();
        nodes.push(node(
            path,
            *community,
            &format!("source-{index:02}"),
            &format!("capture-{index:02}"),
            &digest(bytes),
        ));
    }
    let graph = root.join("graph.json");
    fs::write(
        &graph,
        serde_json::to_vec(&json!({"nodes": nodes, "links": []})).unwrap(),
    )
    .unwrap();
    let args = DraftArgs {
        source_root: root,
        graph,
        catalog: None,
        plan: None,
        output: temporary.path().join("wiki"),
        model: "qwen-test".into(),
        scopes: BTreeSet::new(),
        consent: CONSENT.into(),
        ollama_url: String::new(),
        ollama_native: false,
        provider_profile: None,
        registry_tree: None,
    };
    (temporary, args)
}

fn canonical_draft_fixture() -> (TempDir, DraftArgs, String) {
    let (temporary, mut args) = fixture(&[(
        "guide.md",
        b"# Install\n\nRun the approved installer command.\n",
        1,
    )]);
    let source = fs::read(args.source_root.join("guide.md")).unwrap();
    let hash = digest(&source);
    let capture = json!({
        "source_id": "guide",
        "capture_id": "capture-current",
        "source_path": "guide.md",
        "sha256": hash,
        "captured_at": "2026-08-24T12:34:56Z",
        "accessed_at": "2026-08-24T12:35:56Z",
        "updated_at": "2026-08-24T12:34:56Z",
        "representation": "markdown"
    });
    let mut annotation = capture.clone();
    annotation["source_system"] = json!("sharepoint");
    annotation["url"] = json!("https://example.invalid/guide");
    annotation["location"] = json!("Library/Guide");
    fs::write(
        &args.graph,
        serde_json::to_vec(&json!({
            "nodes": [{
                "id": "heading",
                "label": "Install",
                "file_type": "markdown",
                "source_file": "guide.md",
                "source_location": "L1",
                "catalog": annotation,
                "type": "document_heading",
                "line_start": 1
            }, {
                "id": "paragraph",
                "label": "paragraph",
                "file_type": "markdown",
                "source_file": "guide.md",
                "source_location": "L3",
                "catalog": annotation,
                "type": "document_paragraph",
                "structured_text": "Run the approved installer command.",
                "structured_text_type": "string",
                "line_start": 3
            }],
            "links": [{"source":"heading","target":"paragraph","relation":"contains"}]
        }))
        .unwrap(),
    )
    .unwrap();
    fs::create_dir(args.source_root.join("catalog")).unwrap();
    fs::write(
        args.source_root.join("catalog/catalog.json"),
        serde_json::to_vec(&json!({
            "version": 2,
            "sources": [{
                "source_id": "guide",
                "source_system": "sharepoint",
                "url": "https://example.invalid/guide",
                "location": "Library/Guide",
                "active_capture_id": "capture-current"
            }],
            "captures": [capture]
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        args.source_root.join("wiki-plan.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "domains": [{"id":"getting-started","title":"Getting started","slug":"getting-started"}],
            "sources": [{
                "id":"guide#capture-current","title":"Installation guide",
                "slug":"installation-guide","domain":"getting-started","coverage":"complete"
            }],
            "articles": [{
                "id":"installation","title":"Installation","slug":"installation",
                "domain":"getting-started","article_type":"procedure",
                "sources":["guide#capture-current"],"aliases":[],"related":[]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    args.catalog = Some("catalog".into());
    args.plan = Some("wiki-plan.json".into());
    args.output = args.source_root.join("wiki");
    let graph: KnowledgeGraph = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    let block = project_wiki_evidence(&graph, None)
        .unwrap()
        .sources
        .into_iter()
        .flat_map(|source| source.blocks)
        .find(|block| block.value.is_some())
        .unwrap()
        .id;
    (temporary, args, block)
}

fn canonical_registry(temporary: &TempDir, source_root: &std::path::Path) -> PathBuf {
    let registry = temporary.path().join("registry");
    initialize_tree(&registry, "wiki-catalog").unwrap();
    add_origin(
        &registry,
        RegistryOrigin {
            version: 1,
            origin_id: "docs".into(),
            kind: "filesystem".into(),
            logical_name: "docs".into(),
        },
    )
    .unwrap();
    append_capture_and_activate(
        &registry,
        RegistryCapture {
            version: 1,
            capture_id: "capture-current".into(),
            source_id: "guide".into(),
            relative_path: "guide.md".into(),
            sha256: digest(&fs::read(source_root.join("guide.md")).unwrap()),
            observed_at: "2026-08-24T12:34:56Z".into(),
            representation: "markdown".into(),
        },
        Some("docs"),
    )
    .unwrap();
    registry
}

fn canonical_registry_run(registry: &std::path::Path) -> Value {
    let run_directory = registry
        .join("runs")
        .join(shard_for_source_id("guide"))
        .join("guide")
        .join("capture-current")
        .join("wiki-draft");
    let run_path = fs::read_dir(run_directory)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    serde_json::from_slice(&fs::read(run_path).unwrap()).unwrap()
}

fn batched_plan_fixture() -> (TempDir, PathBuf) {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("source");
    fs::create_dir(&root).unwrap();
    let mut sources = Vec::new();
    let mut captures = Vec::new();
    let mut nodes = Vec::new();
    for index in 0..13 {
        let source_id = format!("source-{index:02}");
        let source_path = format!("docs/{source_id}.md");
        let bytes = format!("Technical content for {source_id}.\n").into_bytes();
        let hash = digest(&bytes);
        let capture = json!({
            "source_id": source_id,
            "capture_id": "capture",
            "source_path": source_path,
            "sha256": hash,
            "captured_at": "2026-08-24T12:00:00Z",
            "accessed_at": "2026-08-24T12:00:00Z",
            "updated_at": "2026-08-24T12:00:00Z",
            "representation": "markdown"
        });
        let mut annotation = capture.clone();
        annotation["source_system"] = json!("test");
        annotation["url"] = json!(format!("https://example.invalid/{index}"));
        annotation["location"] = json!(format!("Library/{index}"));
        let path = root.join(&source_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        sources.push(json!({
            "source_id": source_id,
            "source_system": "test",
            "url": format!("https://example.invalid/{index}"),
            "location": format!("Library/{index}"),
            "active_capture_id": "capture"
        }));
        captures.push(capture);
        nodes.push(json!({
            "id": format!("node-{index:02}"),
            "label": format!("Document {index}"),
            "file_type": "markdown",
            "source_file": source_path,
            "source_location": "L1",
            "catalog": annotation,
            "type": "document_paragraph",
            "structured_text": format!("Technical content for source-{index:02}."),
            "structured_text_type": "string",
            "line_start": 1
        }));
    }
    fs::write(
        root.join("graph.json"),
        serde_json::to_vec(&json!({"nodes": nodes, "links": []})).unwrap(),
    )
    .unwrap();
    fs::create_dir(root.join("catalog")).unwrap();
    fs::write(
        root.join("catalog/catalog.json"),
        serde_json::to_vec(&json!({"version": 2, "sources": sources, "captures": captures}))
            .unwrap(),
    )
    .unwrap();
    (temporary, root)
}

fn batched_plan_response(
    batch: usize,
    indexes: std::ops::Range<usize>,
    related: &[&str],
) -> &'static str {
    let sources = indexes
        .clone()
        .map(|index| {
            json!({
                "id": format!("source-{index:02}#capture"),
                "title": format!("Document {index}"),
                "slug": format!("document-{index:02}"),
                "domain": "catalog",
                "coverage": "complete"
            })
        })
        .collect::<Vec<_>>();
    let first = indexes.start;
    Box::leak(
        completion(
            &serde_json::to_string(&json!({
                "version": 1,
                "domains": [{"id":"catalog","title":"Catalog","slug":"catalog"}],
                "sources": sources,
                "articles": [{
                    "id": format!("batch-{batch}-primary"),
                    "title": format!("Batch {batch} primary"),
                    "slug": format!("batch-{batch}-primary"),
                    "domain": "catalog",
                    "article_type": "reference",
                    "sources": [format!("source-{first:02}#capture")],
                    "aliases": [],
                    "related": related
                }]
            }))
            .unwrap(),
        )
        .into_boxed_str(),
    )
}

fn maximum_metadata_fixture(evidence: &[u8]) -> (TempDir, DraftArgs, String, String) {
    let (temporary, args) = fixture(&[("maximum.txt", evidence, 1)]);
    let source_id = "s".repeat(4_096);
    let capture_id = "c".repeat(4_096);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["catalog"]["source_id"] = json!(source_id);
    graph["nodes"][0]["catalog"]["capture_id"] = json!(capture_id);
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    (temporary, args, source_id, capture_id)
}

fn maximum_plain_prompt_sizes() -> (usize, usize) {
    let evidence = vec![b'x'; 64 * 1024];
    let (_temporary, mut args, _source_id, _capture_id) =
        maximum_metadata_fixture(evidence.as_slice());
    let response = Box::leak(completion("Grounded body.").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    (
        request["messages"][0]["content"].as_str().unwrap().len(),
        request["messages"][1]["content"].as_str().unwrap().len(),
    )
}

struct MockOllama {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl MockOllama {
    fn start(responses: Vec<(u16, &'static str)>) -> Self {
        Self::start_with_after_first(responses, None)
    }

    fn start_after_first(
        responses: Vec<(u16, &'static str)>,
        after_first: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self::start_with_after_first(responses, Some(Box::new(after_first)))
    }

    fn start_with_after_first(
        responses: Vec<(u16, &'static str)>,
        mut after_first: Option<Box<dyn FnOnce() + Send>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let join = thread::spawn(move || {
            for (index, (status, body)) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                let mut wire = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let read = stream.read(&mut chunk).unwrap();
                    wire.extend_from_slice(&chunk[..read]);
                    let Some(headers_end) = wire.windows(4).position(|part| part == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers_end = headers_end + 4;
                    let headers = String::from_utf8_lossy(&wire[..headers_end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if wire.len() >= headers_end + length {
                        captured.lock().unwrap().push(
                            serde_json::from_slice(&wire[headers_end..headers_end + length])
                                .unwrap(),
                        );
                        break;
                    }
                }
                if index == 0
                    && let Some(after_first) = after_first.take()
                {
                    after_first();
                }
                let reason = if status == 200 { "OK" } else { "Error" };
                let (headers, response_body) = if (300..400).contains(&status) {
                    (format!("Location: {body}\r\n"), "{}")
                } else {
                    (String::new(), body)
                };
                write!(stream, "HTTP/1.1 {status} {reason}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}", response_body.len()).unwrap();
            }
        });
        Self {
            endpoint,
            requests,
            join: Some(join),
        }
    }

    fn finish(mut self) -> Vec<Value> {
        self.join.take().unwrap().join().unwrap();
        Arc::try_unwrap(self.requests)
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

fn wiki_stage_entries(parent: &std::path::Path) -> Vec<String> {
    fs::read_dir(parent)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".wiki-stage-"))
        .collect()
}

fn completion(body: &str) -> String {
    serde_json::to_string(&json!({
        "choices": [{"message": {"content": body}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    }))
    .unwrap()
}

fn page_input_digest(path: &std::path::Path) -> String {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("input_sha256: \""))
        .and_then(|value| value.strip_suffix('"'))
        .unwrap()
        .into()
}

fn frontmatter_sources(markdown: &str) -> Vec<&str> {
    markdown
        .lines()
        .skip_while(|line| *line != "sources:")
        .skip(1)
        .take_while(|line| line.starts_with("  - "))
        .map(|line| line.trim_start_matches("  - "))
        .collect()
}

fn frontmatter_value<'a>(markdown: &'a str, key: &str) -> Option<&'a str> {
    markdown
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}: \"")))
        .and_then(|value| value.strip_suffix('"'))
}

fn structural_page(root: &std::path::Path, kind: &str, graph_ref: &str) -> std::path::PathBuf {
    let directory = match kind {
        "topic" => "topics",
        "community" => "communities",
        _ => panic!("unsupported structured page kind {kind}"),
    };
    fs::read_dir(root.join(directory))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|extension| extension == "md")
                && fs::read_to_string(path).ok().is_some_and(|markdown| {
                    frontmatter_value(&markdown, "kind") == Some(kind)
                        && frontmatter_value(&markdown, "graph_ref") == Some(graph_ref)
                })
        })
        .unwrap_or_else(|| panic!("missing {kind} page for graph reference {graph_ref}"))
}

fn community_page(root: &std::path::Path, community: i64) -> std::path::PathBuf {
    structural_page(root, "community", &community.to_string())
}

fn topic_page(root: &std::path::Path, topic: &str) -> std::path::PathBuf {
    structural_page(root, "topic", topic)
}

fn synthesized_metadata(root: &std::path::Path) -> BTreeMap<String, (String, Vec<String>, String)> {
    let paths = vec![
        root.join("sources/source-00.md"),
        root.join("sources/source-01.md"),
        topic_page(root, "topic-0"),
    ];
    paths
        .into_iter()
        .map(|path| {
            let markdown = fs::read_to_string(&path).unwrap();
            (
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                (
                    frontmatter_value(&markdown, "input_sha256")
                        .unwrap()
                        .to_owned(),
                    frontmatter_sources(&markdown)
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    frontmatter_value(&markdown, "evidence_sha256")
                        .unwrap()
                        .to_owned(),
                ),
            )
        })
        .collect()
}

#[test]
fn source_topic_atomic_drafts_are_deterministic_under_shuffled_graph_order() {
    let (temporary, mut args) = fixture(&[
        ("alpha.md", b"Alpha evidence.", 2),
        ("beta.md", b"Beta evidence.", 1),
    ]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["links"] = json!([
        {"source": "2:source-00", "target": "1:source-01", "relation": "related"},
        {"source": "1:source-01", "target": "2:source-00", "relation": "references"}
    ]);
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    args.scopes = BTreeSet::from([DraftScope::Source, DraftScope::Topic]);

    let responses = || {
        ["MODEL-OUTPUT-ONE", "MODEL-OUTPUT-TWO", "MODEL-OUTPUT-THREE"]
            .into_iter()
            .map(|body| {
                (
                    200,
                    Box::leak(completion(body).into_boxed_str()) as &'static str,
                )
            })
            .collect()
    };
    let first = MockOllama::start(responses());
    args.ollama_url = first.endpoint.clone();
    draft(args.clone()).unwrap();
    let first_requests = first.finish();
    let first_metadata = synthesized_metadata(&args.output);
    let first_paths = relative_files(&args.output);

    graph["nodes"].as_array_mut().unwrap().reverse();
    graph["links"].as_array_mut().unwrap().reverse();
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    args.output = temporary.path().join("wiki-shuffled");
    let second = MockOllama::start(responses());
    args.ollama_url = second.endpoint.clone();
    draft(args.clone()).unwrap();
    let second_requests = second.finish();

    assert_eq!(first_paths, relative_files(&args.output));
    assert_eq!(first_metadata, synthesized_metadata(&args.output));
    let prompts = |requests: &[Value]| {
        requests
            .iter()
            .map(|request| {
                request["messages"][1]["content"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(prompts(&first_requests), prompts(&second_requests));
    assert!(prompts(&second_requests).iter().all(|prompt| {
        !prompt.contains("MODEL-OUTPUT-ONE")
            && !prompt.contains("MODEL-OUTPUT-TWO")
            && !prompt.contains("MODEL-OUTPUT-THREE")
    }));
}

#[test]
fn source_topic_atomic_transport_failure_leaves_no_output_or_stage() {
    let (_temporary, mut args) = fixture(&[
        ("alpha.md", b"Alpha evidence.", 1),
        ("beta.md", b"Beta evidence.", 1),
    ]);
    args.scopes = BTreeSet::from([DraftScope::Source, DraftScope::Topic]);
    let server = MockOllama::start(vec![
        (
            200,
            Box::leak(completion("Accepted body.").into_boxed_str()),
        ),
        (500, "{}"),
    ]);
    args.ollama_url = server.endpoint.clone();

    let error = draft(args.clone()).unwrap_err().to_string();
    server.finish();

    assert!(error.contains("HTTP 500"), "{error}");
    assert!(!args.output.exists());
    assert!(wiki_stage_entries(args.output.parent().unwrap()).is_empty());
}

#[test]
fn source_topic_atomic_rehashes_each_capture_immediately_before_its_prompt() {
    let (_temporary, mut args) = fixture(&[
        ("alpha.md", b"Alpha evidence.", 1),
        ("beta.md", b"Beta evidence.", 1),
    ]);
    args.scopes = BTreeSet::from([DraftScope::Source, DraftScope::Topic]);
    let changed = args.source_root.join("beta.md");
    let server = MockOllama::start_after_first(
        vec![(
            200,
            Box::leak(completion("Accepted body.").into_boxed_str()),
        )],
        move || fs::write(changed, b"Changed after selection.").unwrap(),
    );
    args.ollama_url = server.endpoint.clone();

    let error = draft(args.clone()).unwrap_err().to_string();
    let requests = server.finish();

    assert!(error.contains("SHA-256 recheck failed"), "{error}");
    assert_eq!(requests.len(), 1);
    assert!(!args.output.exists());
    assert!(wiki_stage_entries(args.output.parent().unwrap()).is_empty());
}

fn untrusted_evidence_bytes(prompt: &str) -> (usize, usize) {
    let evidence = prompt
        .split("<untrusted_source>\n")
        .skip(1)
        .map(|source| source.split_once("\n</untrusted_source>").unwrap().0)
        .collect::<Vec<_>>();
    (evidence.len(), evidence.iter().map(|text| text.len()).sum())
}

#[test]
fn verified_source_markup_is_projected_to_plain_prompt_evidence() {
    let raw = b"See [Platform guide](https://docs.example.invalid/guide) and https://status.example.invalid/live. <strong>Hardware behavior</strong> <angle data>.";
    let (_temporary, mut args) = fixture(&[("platform.md", raw, 1)]);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Grounded platform summary.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();
    let evidence = prompt
        .split_once("\n<untrusted_source>\n")
        .and_then(|(_, evidence)| evidence.split_once("\n</untrusted_source>"))
        .map(|(evidence, _)| evidence)
        .expect("verified evidence in the actual loopback request");

    assert!(evidence.contains("Platform guide"), "{evidence}");
    assert!(evidence.contains("Hardware behavior"), "{evidence}");
    assert!(evidence.contains("angle data"), "{evidence}");
    assert_eq!(
        evidence.matches("external reference").count(),
        2,
        "{evidence}"
    );
    assert!(!evidence.contains("http://"), "{evidence}");
    assert!(!evidence.contains("https://"), "{evidence}");
    assert!(!evidence.contains(['[', ']', '<', '>']), "{evidence}");
}

fn large_catalog_capture_fixture() -> (TempDir, DraftArgs) {
    let mut raw = br#"{"payload":""#.to_vec();
    raw.extend(std::iter::repeat_n(b'A', 17 * 1024 * 1024));
    raw.extend_from_slice(br#""}"#);
    let sha256 = digest(&raw);
    let (temporary, mut args) = fixture(&[("large.json", raw.as_slice(), 1)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    let capture = json!({
        "source_id": "source-00",
        "capture_id": "capture-00",
        "source_path": "large.json",
        "sha256": sha256,
        "captured_at": "2026-08-24T12:34:56Z",
        "accessed_at": "2026-08-24T12:35:56Z",
        "updated_at": "2026-08-24T12:35:56Z",
        "representation": "json"
    });
    let mut annotation = capture.clone();
    annotation["source_system"] = json!("sharepoint");
    annotation["url"] = json!("https://example.invalid/large.json");
    annotation["location"] = json!("Team/Knowledge/large.json");
    graph["nodes"][0]["catalog"] = annotation;
    graph["nodes"][0]["structured_value"] = json!({
        "extractor": "retained structured metadata"
    });
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    fs::create_dir(args.source_root.join("catalog")).unwrap();
    fs::write(
        args.source_root.join("catalog/catalog.json"),
        serde_json::to_vec(&json!({
            "version": 2,
            "sources": [{
                "source_id": "source-00",
                "source_system": "sharepoint",
                "url": "https://example.invalid/large.json",
                "location": "Team/Knowledge/large.json",
                "active_capture_id": "capture-00"
            }],
            "captures": [capture]
        }))
        .unwrap(),
    )
    .unwrap();
    args.catalog = Some("catalog".into());
    args.scopes = BTreeSet::from([DraftScope::Source, DraftScope::Topic]);
    (temporary, args)
}

#[test]
fn large_capture_hash_streams_complete_digest_with_bounded_prompts() {
    let (_temporary, args) = large_catalog_capture_fixture();
    let server = MockOllama::start(vec![
        (
            200,
            Box::leak(completion("Large capture source article.").into_boxed_str()),
        ),
        (
            200,
            Box::leak(completion("Large capture topic article.").into_boxed_str()),
        ),
    ]);

    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["wiki", "draft"])
        .arg(&args.source_root)
        .arg("--graph")
        .arg(&args.graph)
        .args(["--catalog", "catalog"])
        .arg("--output")
        .arg(&args.output)
        .args(["--model", "qwen-test"])
        .args(["--consent", CONSENT])
        .args(["--scope", "source", "--scope", "topic"])
        .arg("--ollama-url")
        .arg(&server.endpoint)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", command_output(&output));
    let requests = server.finish();

    assert_eq!(requests.len(), 2);
    for request in requests {
        let prompt = request["messages"][1]["content"].as_str().unwrap();
        let (captures, bytes) = untrusted_evidence_bytes(prompt);
        assert_eq!(captures, 1, "{prompt}");
        assert!(bytes <= 64 * 1024, "{bytes} evidence bytes in prompt");
        assert!(prompt.contains("{\"payload\":\"AAAA"), "{prompt}");
    }
    assert!(args.output.join("sources/source-00.md").is_file());
    assert!(topic_page(&args.output, "topic-0").is_file());
}

#[test]
fn large_capture_hash_rejects_capture_over_configured_graph_cap() {
    let (_temporary, args) = large_catalog_capture_fixture();

    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .env("GRAPHOXIDE_MAX_GRAPH_BYTES", "16MB")
        .args(["wiki", "draft"])
        .arg(&args.source_root)
        .arg("--graph")
        .arg(&args.graph)
        .args(["--catalog", "catalog"])
        .arg("--output")
        .arg(&args.output)
        .args(["--model", "qwen-test"])
        .args(["--consent", CONSENT])
        .args(["--scope", "source", "--scope", "topic"])
        .args(["--ollama-url", "http://127.0.0.1:1/v1"])
        .output()
        .unwrap();
    let error = command_output(&output);

    assert!(!output.status.success(), "{error}");
    assert!(error.contains("wiki input exceeds its byte cap"), "{error}");
    assert!(!args.output.exists());
    assert!(wiki_stage_entries(args.output.parent().unwrap()).is_empty());
}

#[test]
fn synthesis_prompt_contract_precedes_every_untrusted_source() {
    let (_temporary, mut args) = fixture(&[
        ("alpha.md", b"Alpha evidence.", 1),
        ("beta.md", b"Beta evidence.", 1),
    ]);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Explanatory body.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();
    let contract = "Return an explanatory Markdown body only. Do not include frontmatter, an H1, a Sources heading or list, citations, operational instructions, inline Markdown links, reference links, autolinks, or raw HTML.";
    let contract_index = prompt.find(contract).expect("Markdown body contract");
    let sources = prompt
        .match_indices("<untrusted_source>")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    assert_eq!(prompt.matches(contract).count(), 1, "{prompt}");
    assert_eq!(sources.len(), 2, "{prompt}");
    assert!(sources.into_iter().all(|index| contract_index < index));
}

#[test]
fn synthesis_prompt_forbids_source_markup_before_every_untrusted_source() {
    let (_temporary, mut args) = fixture(&[
        ("alpha.md", b"Alpha evidence.", 1),
        ("beta.md", b"Beta evidence.", 1),
    ]);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Explanatory body.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();
    let contract = "Describe source references in plain text; never render or reproduce source URLs, Markdown link syntax, HTML tags, or angle-bracket syntax.";
    let contract_index = prompt.find(contract).expect("source-markup contract");
    let sources = prompt
        .match_indices("<untrusted_source>")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    assert_eq!(prompt.matches(contract).count(), 1, "{prompt}");
    assert_eq!(sources.len(), 2, "{prompt}");
    assert!(sources.into_iter().all(|index| contract_index < index));
}

#[test]
fn synthesis_prompt_requires_evidence_grounding_before_every_untrusted_source() {
    let (_temporary, mut args) = fixture(&[
        ("alpha.md", b"Alpha evidence.", 1),
        ("beta.md", b"Beta evidence.", 1),
    ]);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Explanatory body.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();
    let contract = "Use only facts stated in the supplied source text; do not invent or infer facts, and state plainly when the evidence is insufficient.";
    let contract_index = prompt.find(contract).expect("evidence-grounding contract");
    let sources = prompt
        .match_indices("<untrusted_source>")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    assert_eq!(prompt.matches(contract).count(), 1, "{prompt}");
    assert_eq!(sources.len(), 2, "{prompt}");
    assert!(sources.into_iter().all(|index| contract_index < index));
}

#[test]
fn source_topic_prompts_reuse_the_twelve_capture_and_sixty_four_kib_caps() {
    let bodies = (0..13)
        .map(|index| {
            let path = Box::leak(format!("{index:02}.md").into_boxed_str());
            let bytes = Box::leak(vec![b'A' + index as u8; 5 * 1024].into_boxed_slice());
            (path as &str, bytes as &[u8], 7)
        })
        .collect::<Vec<_>>();
    let (_temporary, mut args) = fixture(&bodies);
    args.scopes = BTreeSet::from([DraftScope::Source, DraftScope::Topic]);
    let responses = (0..14)
        .map(|_| {
            (
                200,
                Box::leak(completion("Bounded body.").into_boxed_str()) as &'static str,
            )
        })
        .collect();
    let server = MockOllama::start(responses);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let requests = server.finish();

    assert_eq!(requests.len(), 14);
    for request in &requests {
        let prompt = request["messages"][1]["content"].as_str().unwrap();
        let (captures, bytes) = untrusted_evidence_bytes(prompt);
        assert!(captures <= 12, "{captures} captures in prompt");
        assert!(bytes <= 64 * 1024, "{bytes} evidence bytes in prompt");
    }
    let topic_prompt = requests.last().unwrap()["messages"][1]["content"]
        .as_str()
        .unwrap();
    assert_eq!(untrusted_evidence_bytes(topic_prompt), (12, 60 * 1024));
}

fn command_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn relative_files(root: &std::path::Path) -> Vec<String> {
    fn collect(root: &std::path::Path, directory: &std::path::Path, files: &mut Vec<String>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                collect(root, &path, files);
            } else {
                files.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let mut files = Vec::new();
    collect(root, root, &mut files);
    files.sort();
    files
}

#[test]
fn source_topic_public_workflow_drafts_indexes_and_checks_without_raw_files() {
    let (temporary, args) = fixture(&[
        ("alpha.md", b"Alpha private source evidence.", 1),
        ("beta.md", b"Beta private source evidence.", 2),
    ]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["links"] = json!([{
        "source": "1:source-00",
        "target": "2:source-01",
        "relation": "related"
    }]);
    let mut sources = Vec::new();
    let mut captures = Vec::new();
    for (index, (path, bytes)) in [
        ("alpha.md", b"Alpha private source evidence.".as_slice()),
        ("beta.md", b"Beta private source evidence.".as_slice()),
    ]
    .into_iter()
    .enumerate()
    {
        let source_id = format!("source-{index:02}");
        let capture_id = format!("capture-{index:02}");
        sources.push(json!({
            "source_id": source_id,
            "source_system": "sharepoint",
            "url": format!("https://example.invalid/{index}"),
            "location": format!("Team/Knowledge/{path}"),
            "active_capture_id": capture_id
        }));
        let capture = json!({
            "source_id": source_id,
            "capture_id": capture_id,
            "source_path": path,
            "sha256": digest(bytes),
            "captured_at": "2026-08-24T12:34:56Z",
            "accessed_at": "2026-08-24T12:35:56Z",
            "updated_at": "2026-08-24T12:35:56Z",
            "representation": "markdown"
        });
        captures.push(capture.clone());
        let mut annotation = capture;
        annotation["source_system"] = json!("sharepoint");
        annotation["url"] = json!(format!("https://example.invalid/{index}"));
        annotation["location"] = json!(format!("Team/Knowledge/{path}"));
        graph["nodes"][index]["catalog"] = annotation;
    }
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    fs::create_dir(args.source_root.join("catalog")).unwrap();
    fs::write(
        args.source_root.join("catalog/catalog.json"),
        serde_json::to_vec(&json!({
            "version": 2,
            "sources": sources,
            "captures": captures
        }))
        .unwrap(),
    )
    .unwrap();

    let wiki_root = temporary.path().join("published");
    fs::create_dir(&wiki_root).unwrap();
    let docs = wiki_root.join("docs");
    let server = MockOllama::start(
        ["Alpha article.", "Beta article.", "Combined topic article."]
            .into_iter()
            .map(|body| {
                (
                    200,
                    Box::leak(completion(body).into_boxed_str()) as &'static str,
                )
            })
            .collect(),
    );
    let draft_output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["wiki", "draft"])
        .arg(&args.source_root)
        .arg("--graph")
        .arg(&args.graph)
        .args(["--catalog", "catalog"])
        .arg("--output")
        .arg(&docs)
        .args(["--model", "qwen-test"])
        .args(["--consent", CONSENT])
        .args(["--scope", "source", "--scope", "topic"])
        .arg("--ollama-url")
        .arg(&server.endpoint)
        .output()
        .unwrap();
    assert!(
        draft_output.status.success(),
        "{}",
        command_output(&draft_output)
    );
    assert_eq!(server.finish().len(), 3);

    fs::create_dir(wiki_root.join("catalog")).unwrap();
    fs::copy(
        args.source_root.join("catalog/catalog.json"),
        wiki_root.join("catalog/catalog.json"),
    )
    .unwrap();
    fs::write(
        wiki_root.join("wiki.json"),
        r#"{"version":1,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    )
    .unwrap();
    let index_output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .current_dir(&wiki_root)
        .args(["wiki", "index", ".", "--config", "wiki.json"])
        .output()
        .unwrap();
    assert!(
        index_output.status.success(),
        "{}",
        command_output(&index_output)
    );
    let check_output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .current_dir(&wiki_root)
        .args([
            "wiki",
            "check",
            ".",
            "--config",
            "wiki.json",
            "--catalog",
            "catalog",
            "--graph",
        ])
        .arg(&args.graph)
        .output()
        .unwrap();
    assert!(
        check_output.status.success(),
        "{}",
        command_output(&check_output)
    );

    let files = relative_files(&wiki_root);
    assert!(files.contains(&"docs/index.md".to_owned()));
    assert!(files.contains(&"catalog/catalog.json".to_owned()));
    assert!(files.contains(&"llms.txt".to_owned()));
    for forbidden in [
        "alpha.md",
        "beta.md",
        "graph.json",
        "manifest.json",
        ".graphoxide-cache",
        "graphoxide-out",
    ] {
        assert!(
            files
                .iter()
                .all(|path| !path.split('/').any(|part| part == forbidden)),
            "forbidden published path {forbidden}: {files:?}"
        );
    }
    for path in &files {
        let bytes = fs::read(wiki_root.join(path)).unwrap();
        assert_ne!(bytes, b"Alpha private source evidence.");
        assert_ne!(bytes, b"Beta private source evidence.");
    }
}

#[test]
fn exact_consent_is_required_before_artifacts_or_network() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    args.consent = "yes".into();
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();

    assert!(error.contains(CONSENT), "{error}");
    assert!(!args.output.exists());
}

// Only reachable on Linux non-x86_64 under the suite gate above: unsupported
// platforms must reject before any network connection is attempted.
#[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
#[test]
fn unsupported_targets_reject_drafts_before_connecting_to_ollama() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();

    assert!(
        error.contains("only supported on Linux x86_64 and macOS"),
        "{error}"
    );
    assert!(!args.output.exists());
}

#[test]
fn local_transport_rejects_non_loopback_dns_answers() {
    let error = ollama_transport::OllamaTransport::local_with_resolver(
        "http://ollama.test:11434/v1",
        "model",
        |_, _| vec!["10.0.0.4".parse::<IpAddr>().unwrap()],
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("loopback"), "{error}");
}

#[test]
fn catalog_hash_is_required_and_rechecked_before_request() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["catalog"]["sha256"] = json!("0".repeat(64));
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();

    assert!(error.contains("SHA-256"), "{error}");
    assert!(!args.output.exists());
}

#[test]
fn catalog_identity_record_is_required_before_request() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0].as_object_mut().unwrap().remove("catalog");
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();

    assert!(error.contains("catalog record"), "{error}");
    assert!(!args.output.exists());
}

#[test]
fn draft_catalog_must_stay_beneath_the_source_root_before_network() {
    let (temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    args.catalog = Some(temporary.path().join("outside-catalog"));
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();

    assert!(
        error.contains("catalog must be beneath the source root"),
        "{error}"
    );
    assert!(!args.output.exists());
}

#[test]
fn draft_catalog_rejects_graph_annotations_that_do_not_match_active_captures() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    fs::create_dir(args.source_root.join("catalog")).unwrap();
    fs::write(
        args.source_root.join("catalog/catalog.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "entries": [{
                "source_id": "source-00",
                "capture_id": "different-capture",
                "source_path": "a.md",
                "sha256": digest(b"alpha"),
                "captured_at": "2026-08-24T12:34:56Z",
                "accessed_at": "2026-08-24T12:35:56Z",
                "updated_at": "2026-08-24T12:35:56Z",
                "representation": "markdown",
                "source_system": "sharepoint",
                "url": "https://example.invalid/source-00",
                "location": "Library/a.md"
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    args.catalog = Some("catalog".into());
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();

    assert!(
        error.contains("catalog graph annotation does not match the active capture"),
        "{error}"
    );
    assert!(!args.output.exists());
}

#[test]
fn requests_are_sequential_bounded_and_deterministically_ranked() {
    let files = (0..13)
        .map(|index| {
            let path = Box::leak(format!("{index:02}.md").into_boxed_str());
            let text = Box::leak(format!("source text {index:02}").into_boxed_str());
            (path as &str, text.as_bytes() as &[u8], 7)
        })
        .collect::<Vec<_>>();
    let (_temporary, mut args) = fixture(&files);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Body only.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).unwrap();
    let requests = server.finish();

    assert_eq!(requests.len(), 1);
    let prompt = requests[0]["messages"][1]["content"].as_str().unwrap();
    for index in 0..12 {
        assert!(
            prompt.contains(&format!("source text {index:02}")),
            "{prompt}"
        );
    }
    assert!(!prompt.contains("source text 12"), "{prompt}");
    assert!(prompt.find("source text 00") < prompt.find("source text 01"));
    let page = fs::read_to_string(community_page(&args.output, 7)).unwrap();
    assert_eq!(page.matches("  - source-").count(), 12, "{page}");
    assert_eq!(page.matches("../sources/source-").count(), 13, "{page}");
}

#[test]
fn community_source_text_is_capped_at_64_kib() {
    let first = vec![b'A'; 40 * 1024];
    let second = vec![b'B'; 30 * 1024];
    let (_temporary, mut args) = fixture(&[
        ("a.md", first.as_slice(), 7),
        ("b.md", second.as_slice(), 7),
    ]);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Bounded body.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();

    assert!(
        prompt.contains(&"A".repeat(1024)),
        "first ranked source missing"
    );
    assert!(
        !prompt.contains(&"B".repeat(1024)),
        "byte cap admitted second source"
    );
}

#[test]
fn large_text_source_is_hashed_then_truncated_to_the_community_cap() {
    let source = vec![b'Z'; 128 * 1024];
    let (_temporary, mut args) = fixture(&[("large.md", source.as_slice(), 7)]);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Bounded body.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();

    assert_eq!(prompt.matches('Z').count(), 64 * 1024, "{prompt}");
}

#[test]
fn large_text_source_is_rehashed_beyond_the_sent_prefix() {
    let source = vec![b'Z'; 128 * 1024];
    let (_temporary, mut args) = fixture(&[("large.md", source.as_slice(), 7)]);
    let mut changed = source;
    changed[96 * 1024] = b'Y';
    fs::write(args.source_root.join("large.md"), changed).unwrap();
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();

    assert!(error.contains("SHA-256"), "{error}");
    assert!(!args.output.exists());
}

#[test]
fn large_utf8_source_drops_an_incomplete_prefix_codepoint() {
    let mut source = vec![b'A'; 64 * 1024 - 1];
    source.extend_from_slice("é trailing text".as_bytes());
    let (_temporary, mut args) = fixture(&[("large.md", source.as_slice(), 7)]);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Bounded body.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();

    assert_eq!(prompt.matches('A').count(), 64 * 1024 - 1, "{prompt}");
    assert!(!prompt.contains('é'), "{prompt}");
}

#[test]
fn drafts_catalog_backed_communities_without_source_system_pages() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    let template = graph["nodes"][0].clone();
    let nodes = graph["nodes"].as_array_mut().unwrap();
    let mut second = template;
    second["id"] = json!("source-node-2");
    second["community"] = json!(2);
    nodes.push(second);
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    let first = Box::leak(completion("First body.").into_boxed_str());
    let second = Box::leak(completion("Second body.").into_boxed_str());
    let server = MockOllama::start(vec![(200, first), (200, second)]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).unwrap();
    let requests = server.finish();

    assert_eq!(requests.len(), 2);
    assert!(community_page(&args.output, 1).is_file());
    assert!(community_page(&args.output, 2).is_file());
    assert!(!fs::read_dir(&args.output)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().contains("catalog")));
}

#[test]
fn graphoxide_owns_page_chrome_and_model_supplies_only_body() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3), ("b.md", b"beta", 3)]);
    let model_body = "## Architecture\n\nModel body.";
    let response = Box::leak(completion(model_body).into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();
    let graph_before = fs::read(&args.graph).unwrap();
    let source_before = fs::read(args.source_root.join("a.md")).unwrap();

    draft(args.clone()).unwrap();
    let requests = server.finish();
    assert_eq!(requests[0]["think"], false);

    let page = fs::read_to_string(community_page(&args.output, 3)).unwrap();
    assert!(page.starts_with("---\n"), "{page}");
    assert!(page.contains("title: \"source-00\""), "{page}");
    assert!(page.contains("kind: \"community\""), "{page}");
    assert!(page.contains("graph_ref: \"3\""), "{page}");
    assert!(
        page.contains("sources:\n  - source-00#capture-00\n  - source-01#capture-01\n---\n"),
        "{page}"
    );
    assert!(page.contains("<!-- graphoxide-draft -->"), "{page}");
    assert!(page.contains("Architecture\n\nModel body."), "{page}");
    assert!(page.contains("## Sources"), "{page}");
    assert!(page.contains("source-00"), "{page}");
    assert!(page.contains("capture-01"), "{page}");
    assert_eq!(fs::read(&args.graph).unwrap(), graph_before);
    assert_eq!(
        fs::read(args.source_root.join("a.md")).unwrap(),
        source_before
    );
}

#[test]
fn model_markup_is_normalized_to_safe_visible_text_before_publish() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
    let body = "## Sources ##\n\n[Visible label](https://example.invalid/reference) and <em>visible prose</em>.";
    let server = MockOllama::start(vec![(200, Box::leak(completion(body).into_boxed_str()))]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).expect("normalized Markdown body must publish");
    assert_eq!(server.finish().len(), 1, "normalization avoids a retry");

    let page = fs::read_to_string(community_page(&args.output, 3)).unwrap();
    let (_, after_marker) = page
        .split_once("<!-- graphoxide-draft -->\n\n")
        .expect("draft body");
    let (model_body, _) = after_marker
        .split_once("\n\n## Sources\n")
        .expect("Graphoxide Sources boundary");
    assert!(model_body.contains("Visible label"), "{page}");
    assert!(model_body.contains("visible prose"), "{page}");
    assert!(!model_body.contains("https://"), "{page}");
    assert!(!model_body.contains('<'), "{page}");
    assert!(!model_body.contains("## Sources ##"), "{page}");
    assert_eq!(page.matches("## Sources").count(), 1, "{page}");
}

#[test]
fn model_output_trailing_whitespace_is_removed_before_publish() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
    let body =
        "Summary with spaces.   \nDetail with tab.\t\n\n```text \t\nvalue  \t\n```\t\nFinal body.";
    let server = MockOllama::start(vec![(200, Box::leak(completion(body).into_boxed_str()))]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).expect("trailing whitespace must be normalized");
    assert_eq!(server.finish().len(), 1, "normalization avoids a retry");

    let page = fs::read_to_string(community_page(&args.output, 3)).unwrap();
    assert!(
        page.lines().all(|line| !line.ends_with([' ', '\t'])),
        "{page:?}"
    );
    assert!(page.contains("Summary with spaces.\n"), "{page:?}");
    assert!(page.contains("value\n"), "{page:?}");
}

#[test]
fn source_evidence_projection_retains_existing_heading_and_colon_prose() {
    let source = b"# Source heading\nData: results\nFile: configuration";
    let (_temporary, mut args) = fixture(&[("a.md", source, 3)]);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Plain body.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    draft(args).expect("source evidence draft");
    let prompt = server.finish().pop().unwrap()["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_owned();

    assert!(prompt.contains("# Source heading"), "{prompt}");
    assert!(prompt.contains("Data: results"), "{prompt}");
    assert!(prompt.contains("File: configuration"), "{prompt}");
}

#[test]
fn model_output_neutralizes_extended_and_bare_urls_without_colon_prose() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
    let body = "FTP ftp://files.example.invalid/archive. Mail mailto:help@example.invalid. SMB smb://server/share. File URL file:///tmp/config. Bare example.invalid/guide and www.example.invalid/notes plus www.status.invalid. Query docs.example.invalid?view=full and anchor docs.example.invalid#install. Files graphoxide.toml firmware.bin server.rs config.yaml. Data: results. File: configuration.";
    let server = MockOllama::start(vec![(200, Box::leak(completion(body).into_boxed_str()))]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).expect("model URLs must be neutralized");
    let page = fs::read_to_string(community_page(&args.output, 3)).unwrap();
    let (_, after_marker) = page
        .split_once("<!-- graphoxide-draft -->\n\n")
        .expect("draft body");
    let (model_body, _) = after_marker
        .split_once("\n\n## Sources\n")
        .expect("Graphoxide Sources boundary");

    for url in [
        "ftp://",
        "mailto:",
        "smb://",
        "file://",
        "example.invalid/guide",
        "www.example.invalid/notes",
        "www.status.invalid",
        "docs.example.invalid?view=full",
        "docs.example.invalid#install",
    ] {
        assert!(!model_body.contains(url), "{model_body}");
    }
    assert!(model_body.contains("Data: results"), "{model_body}");
    assert!(model_body.contains("File: configuration"), "{model_body}");
    for filename in [
        "graphoxide.toml",
        "firmware.bin",
        "server.rs",
        "config.yaml",
    ] {
        assert!(model_body.contains(filename), "{model_body}");
    }
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn model_output_neutralizes_both_atx_heading_marker_sides() {
    for heading in ["## Sources ##   ", "## Sources #######"] {
        let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
        let response = format!("{heading}\n\nVisible body.");
        let server = MockOllama::start(vec![(
            200,
            Box::leak(completion(&response).into_boxed_str()),
        )]);
        args.ollama_url = server.endpoint.clone();

        draft(args.clone()).expect("ATX markers must be neutralized");
        let page = fs::read_to_string(community_page(&args.output, 3)).unwrap();
        let (_, after_marker) = page
            .split_once("<!-- graphoxide-draft -->\n\n")
            .expect("draft body");
        let (model_body, _) = after_marker
            .split_once("\n\n## Sources\n")
            .expect("Graphoxide Sources boundary");

        assert!(model_body.contains("Sources"), "{model_body}");
        assert!(!model_body.contains('#'), "{model_body}");
        assert_eq!(server.finish().len(), 1);
    }
}

#[test]
fn model_links_are_normalized_before_publish() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
    let response = Box::leak(completion("[escape](../../outside.md)").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).expect("model link must be normalized");
    assert_eq!(server.finish().len(), 1);
    assert!(community_page(&args.output, 3).is_file());
}

#[test]
fn model_h1_is_normalized_before_publish() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
    let response = Box::leak(completion("# Forged title\n\nBody.").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).expect("model H1 must be normalized");
    assert_eq!(server.finish().len(), 1);
    assert!(community_page(&args.output, 3).is_file());
}

#[test]
fn model_sources_heading_with_closing_hashes_is_normalized_before_publish() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
    let response = Box::leak(completion("Body.\n\n## Sources ##\n\nForged.").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).expect("model Sources heading must be normalized");
    assert_eq!(server.finish().len(), 1);
    assert!(community_page(&args.output, 3).is_file());
}

#[test]
fn model_setext_title_and_sources_headings_are_normalized_before_publish() {
    for body in [
        "Forged title\n============\n\nBody.",
        "Body.\n\nSources\n-------\n\nForged.",
    ] {
        let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
        let response = Box::leak(completion(body).into_boxed_str());
        let server = MockOllama::start(vec![(200, response)]);
        args.ollama_url = server.endpoint.clone();

        draft(args.clone()).expect("model setext heading must be normalized");
        assert_eq!(server.finish().len(), 1, "{body}");
        assert!(community_page(&args.output, 3).is_file(), "{body}");
    }
}

#[test]
fn invalid_first_markdown_retries_once_and_publishes_the_valid_second_body() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    let rejected = Box::leak(completion("\u{1}").into_boxed_str());
    let accepted = Box::leak(completion("Valid replacement body.").into_boxed_str());
    let server = MockOllama::start(vec![(200, rejected), (200, accepted)]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).unwrap();
    let requests = server.finish();

    assert_eq!(requests.len(), 2, "one corrective retry");
    assert_eq!(requests[0]["messages"].as_array().unwrap().len(), 2);
    let retry = requests[1]["messages"].as_array().unwrap();
    assert_eq!(retry.len(), 3);
    assert_eq!(retry[1]["content"], requests[0]["messages"][1]["content"]);
    assert!(retry[2]["content"]
        .as_str()
        .unwrap()
        .contains("immediately preceding answer was rejected"));
    assert!(retry[2]["content"]
        .as_str()
        .unwrap()
        .contains("Do not add frontmatter, a title, a draft marker, or a Sources section."));
    assert!(!retry[2]["content"]
        .as_str()
        .unwrap()
        .contains("Forged title"));
    let page = fs::read_to_string(community_page(&args.output, 1)).unwrap();
    assert!(page.contains("Valid replacement body."), "{page}");
}

#[test]
fn invalid_markdown_twice_fails_without_publishing_output() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    let rejected = Box::leak(completion("---").into_boxed_str());
    let server = MockOllama::start(vec![(200, rejected), (200, rejected)]);
    args.ollama_url = server.endpoint.clone();

    let error = draft(args.clone()).expect_err("both invalid answers must fail");
    let requests = server.finish();

    assert!(
        error.to_string().contains("invalid Markdown body"),
        "{error:#}"
    );
    assert_eq!(requests.len(), 2, "one corrective retry");
    assert!(!args.output.exists(), "invalid draft was published");
}

#[test]
fn markdown_http_failure_does_not_retry() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    let server = MockOllama::start(vec![(500, "{}")]);
    args.ollama_url = server.endpoint.clone();

    let error = draft(args.clone()).expect_err("HTTP failure must not retry");
    let requests = server.finish();

    assert!(error.to_string().contains("HTTP 500"), "{error:#}");
    assert_eq!(requests.len(), 1, "HTTP failure was retried");
    assert!(!args.output.exists(), "failed draft was published");
}

#[test]
fn model_links_to_missing_generated_pages_are_normalized_before_publish() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
    let response = Box::leak(completion("[missing](missing.md)").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).expect("missing model link must be normalized");
    assert_eq!(server.finish().len(), 1);
    assert!(community_page(&args.output, 3).is_file());
}

#[test]
fn model_links_to_generated_pages_external_urls_and_anchors_are_normalized() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
    let response = Box::leak(
        completion("[Root](../index.md) [Web](https://example.invalid) [Section](#sources)")
            .into_boxed_str(),
    );
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).expect("model links must be normalized");
    assert_eq!(server.finish().len(), 1);
    assert!(community_page(&args.output, 3).is_file());
}

#[test]
fn model_markdown_links_references_autolinks_and_html_are_normalized_before_publish() {
    for body in [
        "[inline](https://example.invalid)",
        "[outer [inner]](javascript:alert(1))",
        "[multiline\nlabel](javascript:alert(1))",
        "[reference][target]\n\n[target]: https://example.invalid",
        "<https://example.invalid>",
        "<a href=\"https://example.invalid\">raw HTML</a>",
    ] {
        let (_temporary, args) = fixture(&[("a.md", b"alpha", 3)]);
        let server = MockOllama::start(vec![(200, Box::leak(completion(body).into_boxed_str()))]);
        let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
            .args(["wiki", "draft"])
            .arg(&args.source_root)
            .arg("--graph")
            .arg(&args.graph)
            .arg("--output")
            .arg(&args.output)
            .args(["--model", "qwen-test"])
            .args(["--consent", CONSENT])
            .arg("--ollama-url")
            .arg(&server.endpoint)
            .output()
            .unwrap();
        server.finish();

        assert!(
            output.status.success(),
            "{body}: {}",
            command_output(&output)
        );
        assert!(
            community_page(&args.output, 3).is_file(),
            "normalized draft was not published for {body}"
        );
    }
}

#[test]
fn markdown_links_and_html_in_code_remain_literal_model_text() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
    let body = "Literal `[outer [inner]](javascript:alert(1))` and `<tag>`.\n\n```markdown\nForged title\n============\n\nSources\n-------\n\n[multiline\nlabel](javascript:alert(1))\n[reference][target]\n[target]: data:text/html,bad\n<a href=\"file:///tmp/x\">x</a>\n```";
    let server = MockOllama::start(vec![(200, Box::leak(completion(body).into_boxed_str()))]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).expect("code examples are inert model text");
    server.finish();

    assert!(community_page(&args.output, 3).is_file());
}

#[test]
fn citation_admission_skips_an_overflowing_maximum_catalog_id_and_keeps_a_later_source() {
    let (_temporary, mut args) = fixture(&[
        ("large-0.md", b"x", 1),
        ("large-1.md", b"x", 1),
        ("large-2.md", b"x", 1),
        ("large-3.md", b"x", 1),
        ("large-4.md", b"x", 1),
        ("large-5.md", b"x", 1),
        ("large-6.md", b"x", 1),
        ("large-7.md", b"x", 1),
        ("later.md", b"x", 1),
    ]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    for (index, node) in graph["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        let catalog = node["catalog"].as_object_mut().unwrap();
        if index < 8 {
            catalog.insert(
                "source_id".into(),
                json!(format!("a{}{}", index, "s".repeat(4_094))),
            );
            catalog.insert(
                "capture_id".into(),
                json!(format!("b{}{}", index, "c".repeat(4_094))),
            );
        } else {
            catalog.insert("source_id".into(), json!("z-later"));
            catalog.insert("capture_id".into(), json!("later"));
        }
    }
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Grounded body.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).expect("citation admission must leave a renderable page");
    server.finish();

    let page = fs::read_to_string(community_page(&args.output, 1)).unwrap();
    let citations = frontmatter_sources(&page);
    assert_eq!(citations.len(), 8, "{page}");
    assert!(citations.contains(&"z-later#later"));
    let overflow = format!("a7{}#b7{}", "s".repeat(4_094), "c".repeat(4_094));
    assert!(
        citations
            .iter()
            .all(|citation| *citation != overflow.as_str()),
        "overflowing citation was admitted"
    );
}

#[test]
fn community_digest_tracks_model_identity_without_changing_structural_digests() {
    let (temporary, mut args) = fixture(&[("a.md", b"alpha", 3)]);
    let first = MockOllama::start(vec![(
        200,
        Box::leak(completion("Same body.").into_boxed_str()),
    )]);
    args.model = "model-a".into();
    args.ollama_url = first.endpoint.clone();
    draft(args.clone()).unwrap();
    first.finish();

    let first_output = args.output.clone();
    args.output = temporary.path().join("wiki-model-b");
    let second = MockOllama::start(vec![(
        200,
        Box::leak(completion("Same body.").into_boxed_str()),
    )]);
    args.model = "model-b".into();
    args.ollama_url = second.endpoint.clone();
    draft(args.clone()).unwrap();
    second.finish();

    assert_ne!(
        page_input_digest(&community_page(&first_output, 3)),
        page_input_digest(&community_page(&args.output, 3))
    );
    for path in [
        first_output.join("index.md"),
        topic_page(&first_output, "topic-0"),
        first_output.join("sources/source-00.md"),
    ] {
        let relative = path.strip_prefix(&first_output).unwrap();
        assert_eq!(
            page_input_digest(&path),
            page_input_digest(&args.output.join(relative)),
            "structural digest changed for {}",
            relative.display()
        );
    }
}

#[test]
fn catalog_backed_container_uses_owner_hash_and_extracted_text() {
    let raw = b"PK\0raw binary must not leave the process";
    let (_temporary, mut args) = fixture(&[("bundle.zip", raw, 4)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["source_file"] = json!("bundle.zip!/docs/page.pdf");
    graph["nodes"][0][graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE] = json!("bundle.zip");
    graph["nodes"][0]["text"] = json!("Bounded extracted container text.");
    let mut second = graph["nodes"][0].clone();
    second["id"] = json!("container-member-two");
    second["source_file"] = json!("bundle.zip!/docs/page-two.pdf");
    second["text"] = json!("Second extracted fragment.");
    graph["nodes"].as_array_mut().unwrap().push(second);
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    let response = Box::leak(completion("Container body.").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).unwrap();
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();
    let page = fs::read_to_string(community_page(&args.output, 4)).unwrap();

    assert!(
        prompt.contains("Bounded extracted container text."),
        "{prompt}"
    );
    assert!(prompt.contains("Second extracted fragment."), "{prompt}");
    assert!(!prompt.contains("raw binary"), "{prompt}");
    assert_eq!(page.matches("source-00#capture-00").count(), 1, "{page}");
    assert!(page.contains("[bundle](../sources/source-00.md)"), "{page}");
}

#[test]
fn catalog_backed_container_uses_compact_structured_value_evidence() {
    let raw = b"PK\0raw binary must not leave the process";
    let (_temporary, mut args) = fixture(&[("bundle.zip", raw, 4)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["source_file"] = json!("bundle.zip!/docs/manifest.json");
    graph["nodes"][0][graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE] = json!("bundle.zip");
    graph["nodes"][0]["structured_value"] = json!({"widget": true});
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    let response = Box::leak(completion("Container body.").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let prompt = server.finish().pop().unwrap()["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_owned();

    assert!(prompt.contains("{\"widget\":true}"), "{prompt}");
    assert!(!prompt.contains("raw binary"), "{prompt}");
}

#[test]
fn assembled_container_text_respects_the_64_kib_community_cap() {
    let raw = b"PK\0binary owner";
    let (_temporary, mut args) = fixture(&[("bundle.zip", raw, 4)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["source_file"] = json!("bundle.zip!/a.txt");
    graph["nodes"][0][graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE] = json!("bundle.zip");
    graph["nodes"][0]["text"] = json!("A".repeat(40 * 1024));
    let mut second = graph["nodes"][0].clone();
    second["id"] = json!("container-b");
    second["source_file"] = json!("bundle.zip!/b.txt");
    second["text"] = json!("B".repeat(30 * 1024));
    graph["nodes"].as_array_mut().unwrap().push(second);
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    let response = Box::leak(completion("Bounded container body.").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();

    assert!(prompt.contains(&"A".repeat(1024)));
    assert!(!prompt.contains(&"B".repeat(1024)));
}

#[test]
fn model_free_render_does_not_require_draft_prompt_evidence() {
    let raw = b"PK\0binary owner";
    let (temporary, args) = fixture(&[("bundle.zip", raw, 4)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["source_file"] = json!("bundle.zip!/a.txt");
    graph["nodes"][0][graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE] = json!("bundle.zip");
    graph["nodes"][0]["text"] = json!("A".repeat(64 * 1024 + 1));
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    let output = temporary.path().join("rendered");

    render(RenderArgs {
        source_root: args.source_root,
        graph: args.graph,
        catalog: None,
        output: output.clone(),
    })
    .unwrap();

    assert!(community_page(&output, 4).is_file());
}

#[test]
fn citable_binary_without_extracted_text_fails_closed() {
    let (_temporary, mut args) = fixture(&[("paper.pdf", b"%PDF-binary", 1)]);
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();

    assert!(error.contains("extracted text"), "{error}");
    assert!(!args.output.exists());
}

#[test]
fn source_scope_keeps_no_evidence_binary_pages_structural() {
    let (_temporary, mut args) = fixture(&[("paper.pdf", b"%PDF-binary", 1)]);
    args.scopes = BTreeSet::from([DraftScope::Source]);
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    draft(args.clone()).unwrap();

    assert!(args.output.join("sources/source-00.md").is_file());
    assert!(community_page(&args.output, 1).is_file());
    assert!(
        !fs::read_to_string(args.output.join("sources/source-00.md"))
            .unwrap()
            .contains("<!-- graphoxide-draft -->")
    );
}

#[test]
fn source_and_topic_scopes_keep_empty_text_pages_structural_without_requests() {
    let (_temporary, mut args) = fixture(&[("empty.md", b"", 1), ("blank.md", b" \n\t", 2)]);
    args.scopes = BTreeSet::from([DraftScope::Source, DraftScope::Topic]);
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    draft(args.clone()).unwrap();

    for path in [
        args.output.join("sources/source-00.md"),
        args.output.join("sources/source-01.md"),
        topic_page(&args.output, "topic-0"),
        topic_page(&args.output, "topic-1"),
    ] {
        let page = fs::read_to_string(path).unwrap();
        assert!(page.contains("No admissible textual evidence"), "{page}");
        assert!(!page.contains("<!-- graphoxide-draft -->"), "{page}");
    }
}

#[test]
fn default_community_scope_keeps_empty_text_fail_closed() {
    let (_temporary, mut args) = fixture(&[("empty.md", b"", 1)]);
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();

    assert!(error.contains("community has no source"), "{error}");
    assert!(!args.output.exists());
}

#[test]
fn source_and_topic_drafts_use_extracted_evidence_and_preserve_structure() {
    let raw_container = b"PK\0raw binary must not reach Ollama";
    let raw_unsupported = b"%PDF unsupported raw bytes";
    let (temporary, mut args) = fixture(&[
        ("guide.md", b"Markdown source evidence.", 1),
        ("bundle.zip", raw_container, 2),
        ("paper.pdf", raw_unsupported, 3),
    ]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][1]["source_file"] = json!("bundle.zip!/docs/firmware.pdf");
    graph["nodes"][1][graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE] = json!("bundle.zip");
    graph["nodes"][1]["structured_text"] = json!("Extracted firmware register description.");
    graph["links"] = json!([{
        "source": "1:source-00",
        "target": "2:source-01",
        "relation": "related"
    }]);
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();

    let structural = temporary.path().join("structural");
    render(RenderArgs {
        source_root: args.source_root.clone(),
        graph: args.graph.clone(),
        catalog: None,
        output: structural.clone(),
    })
    .unwrap();
    let structural_source_digest = page_input_digest(&structural.join("sources/source-00.md"));
    let structural_topic_digest = page_input_digest(&topic_page(&structural, "topic-0"));

    let server = MockOllama::start(vec![
        (
            200,
            Box::leak(completion("Grounded Markdown explanation.").into_boxed_str()),
        ),
        (
            200,
            Box::leak(completion("Grounded firmware explanation.").into_boxed_str()),
        ),
        (
            200,
            Box::leak(completion("Grounded topic explanation.").into_boxed_str()),
        ),
    ]);
    args.scopes = BTreeSet::from([DraftScope::Source, DraftScope::Topic]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).unwrap();
    let requests = server.finish();

    assert_eq!(requests.len(), 3);
    let prompts = requests
        .iter()
        .map(|request| request["messages"][1]["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(prompts[0].contains("Markdown source evidence."));
    assert!(prompts[1].contains("Extracted firmware register description."));
    assert!(prompts[2].contains("Markdown source evidence."));
    assert!(prompts[2].contains("Extracted firmware register description."));
    assert!(prompts.iter().all(|prompt| !prompt.contains("raw binary")));
    assert!(prompts
        .iter()
        .all(|prompt| !prompt.contains("unsupported raw bytes")));

    let source = fs::read_to_string(args.output.join("sources/source-00.md")).unwrap();
    let binary = fs::read_to_string(args.output.join("sources/source-01.md")).unwrap();
    let unsupported = fs::read_to_string(args.output.join("sources/source-02.md")).unwrap();
    let topic = fs::read_to_string(topic_page(&args.output, "topic-0")).unwrap();
    for page in [&source, &binary, &topic] {
        assert!(page.contains("draft: true"), "{page}");
        assert!(page.contains("draft_model: \"qwen-test\""), "{page}");
        let evidence = frontmatter_value(page, "evidence_sha256").unwrap();
        assert_eq!(evidence.len(), 64, "{page}");
        assert!(evidence
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert!(page.find("<!-- graphoxide-draft -->") < page.find("## Sources"));
    }
    assert_eq!(frontmatter_sources(&source), vec!["source-00#capture-00"]);
    assert_eq!(frontmatter_sources(&binary), vec!["source-01#capture-01"]);
    assert_eq!(
        frontmatter_sources(&topic),
        vec!["source-00#capture-00", "source-01#capture-01"]
    );
    assert_eq!(
        page_input_digest(&args.output.join("sources/source-00.md")),
        structural_source_digest
    );
    assert_eq!(
        page_input_digest(&topic_page(&args.output, "topic-0")),
        structural_topic_digest
    );
    assert!(source.contains("Grounded Markdown explanation."));
    assert!(binary.contains("Grounded firmware explanation."));
    assert!(topic.contains("Grounded topic explanation."));
    assert!(unsupported.contains("No admissible textual evidence"));
    assert!(!unsupported.contains("<!-- graphoxide-draft -->"));
    let unsupported_topic = fs::read_to_string(topic_page(&args.output, "topic-1")).unwrap();
    assert!(unsupported_topic.contains("No admissible textual evidence"));
    assert!(!unsupported_topic.contains("<!-- graphoxide-draft -->"));
    for community in [1, 2, 3] {
        assert!(!fs::read_to_string(community_page(&args.output, community))
            .unwrap()
            .contains("<!-- graphoxide-draft -->"));
    }
}

#[test]
fn synthesized_draft_body_follows_renderer_h1_after_frontmatter_blank_line() {
    let (_temporary, mut args) = fixture(&[("guide.md", b"Markdown source evidence.", 1)]);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Grounded Markdown explanation.").into_boxed_str()),
    )]);
    args.scopes = BTreeSet::from([DraftScope::Source]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).unwrap();
    server.finish();

    let page = fs::read_to_string(args.output.join("sources/source-00.md")).unwrap();
    let frontmatter_end = page.find("---\n\n").unwrap() + "---\n\n".len();
    let heading = page[frontmatter_end..].find("# ").unwrap() + frontmatter_end;
    let draft = page.find("<!-- graphoxide-draft -->").unwrap();
    assert!(heading < draft, "{page}");
    assert!(page[heading..draft].ends_with("\n\n"), "{page}");
}

#[test]
fn unsafe_raw_prefix_uses_retained_extractor_evidence_and_keeps_rehashing() {
    let raw = b"{\"unsafe\":\"raw\x0bprefix\"}";
    let (temporary, mut args) = fixture(&[("record.json", raw, 1)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["structured_value"] = json!({
        "platform": "Retained extractor platform metadata",
        "api": {"method": "POST", "path": "/v1/widgets"}
    });
    let capture = json!({
        "source_id": "source-00",
        "capture_id": "capture-00",
        "source_path": "record.json",
        "sha256": digest(raw),
        "captured_at": "2026-08-24T12:34:56Z",
        "accessed_at": "2026-08-24T12:35:56Z",
        "updated_at": "2026-08-24T12:35:56Z",
        "representation": "json"
    });
    let mut annotation = capture.clone();
    annotation["source_system"] = json!("sharepoint");
    annotation["url"] = json!("https://example.invalid/record.json");
    annotation["location"] = json!("Team/Knowledge/record.json");
    graph["nodes"][0]["catalog"] = annotation;
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    fs::create_dir(args.source_root.join("catalog")).unwrap();
    fs::write(
        args.source_root.join("catalog/catalog.json"),
        serde_json::to_vec(&json!({
            "version": 2,
            "sources": [{
                "source_id": "source-00",
                "source_system": "sharepoint",
                "url": "https://example.invalid/record.json",
                "location": "Team/Knowledge/record.json",
                "active_capture_id": "capture-00"
            }],
            "captures": [capture]
        }))
        .unwrap(),
    )
    .unwrap();
    args.catalog = Some("catalog".into());
    args.scopes = BTreeSet::from([DraftScope::Source]);
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Grounded source article.").into_boxed_str()),
    )]);
    args.ollama_url = server.endpoint.clone();

    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["wiki", "draft"])
        .arg(&args.source_root)
        .arg("--graph")
        .arg(&args.graph)
        .args(["--catalog", "catalog"])
        .arg("--output")
        .arg(&args.output)
        .args(["--model", "qwen-test"])
        .args(["--consent", CONSENT])
        .args(["--scope", "source"])
        .arg("--ollama-url")
        .arg(&server.endpoint)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", command_output(&output));
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    let prompt = requests[0]["messages"][1]["content"].as_str().unwrap();
    assert!(
        prompt.contains("Retained extractor platform metadata"),
        "{prompt}"
    );
    assert!(prompt.contains("/v1/widgets"), "{prompt}");
    assert!(!prompt.contains("raw\u{b}prefix"), "{prompt}");
    assert!(!prompt.contains('\u{b}'), "{prompt}");

    let files = relative_files(&args.output);
    for forbidden in [
        "record.json",
        "graph.json",
        "manifest.json",
        ".graphoxide-cache",
        "graphoxide-out",
    ] {
        assert!(
            files
                .iter()
                .all(|path| !path.split('/').any(|part| part == forbidden)),
            "forbidden published path {forbidden}: {files:?}"
        );
    }
    for path in &files {
        let bytes = fs::read(args.output.join(path)).unwrap();
        assert!(!bytes.windows(raw.len()).any(|window| window == raw));
        assert!(!bytes.contains(&b'\x0b'));
    }

    let rehash_output = temporary.path().join("rehash-wiki");
    let changed_source = args.source_root.join("record.json");
    let server = MockOllama::start_after_first(
        vec![(
            200,
            Box::leak(completion("First article.").into_boxed_str()),
        )],
        move || fs::write(changed_source, b"changed after first prompt").unwrap(),
    );
    args.output = rehash_output.clone();
    args.scopes = BTreeSet::from([DraftScope::Source, DraftScope::Topic]);
    args.ollama_url = server.endpoint.clone();

    let error = draft(args).expect_err("source mutation must fail the immediate rehash");
    server.finish();
    assert!(
        error.to_string().contains("SHA-256 recheck failed"),
        "{error:#}"
    );
    assert!(!rehash_output.exists());
    assert!(wiki_stage_entries(temporary.path()).is_empty());
}

#[test]
fn non_binary_extracted_json_evidence_survives_a_saturated_raw_prefix() {
    let mut raw = br#"{"raw":""#.to_vec();
    raw.extend(std::iter::repeat_n(
        b'R',
        64 * 1024 - raw.len() - br#""}"#.len(),
    ));
    raw.extend_from_slice(br#""}"#);
    assert_eq!(raw.len(), 64 * 1024);
    let (_temporary, mut args) = fixture(&[("record.json", raw.as_slice(), 1)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["structured_value"] = json!({
        "platform": "Retained extractor-only platform metadata",
        "api": {"method": "POST", "path": "/v1/widgets"}
    });
    let capture = json!({
        "source_id": "source-00",
        "capture_id": "capture-00",
        "source_path": "record.json",
        "sha256": digest(&raw),
        "captured_at": "2026-08-24T12:34:56Z",
        "accessed_at": "2026-08-24T12:35:56Z",
        "updated_at": "2026-08-24T12:35:56Z",
        "representation": "json"
    });
    let mut annotation = capture.clone();
    annotation["source_system"] = json!("sharepoint");
    annotation["url"] = json!("https://example.invalid/record.json");
    annotation["location"] = json!("Team/Knowledge/record.json");
    graph["nodes"][0]["catalog"] = annotation;
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    fs::create_dir(args.source_root.join("catalog")).unwrap();
    fs::write(
        args.source_root.join("catalog/catalog.json"),
        serde_json::to_vec(&json!({
            "version": 2,
            "sources": [{
                "source_id": "source-00",
                "source_system": "sharepoint",
                "url": "https://example.invalid/record.json",
                "location": "Team/Knowledge/record.json",
                "active_capture_id": "capture-00"
            }],
            "captures": [capture]
        }))
        .unwrap(),
    )
    .unwrap();
    args.catalog = Some("catalog".into());
    args.scopes = BTreeSet::from([DraftScope::Source, DraftScope::Topic]);
    let server = MockOllama::start(vec![
        (
            200,
            Box::leak(completion("Source article.").into_boxed_str()),
        ),
        (
            200,
            Box::leak(completion("Topic article.").into_boxed_str()),
        ),
    ]);
    args.ollama_url = server.endpoint.clone();
    let output = args.output.clone();

    draft(args).unwrap();
    let requests = server.finish();

    assert_eq!(requests.len(), 2);
    for request in &requests {
        let prompt = request["messages"][1]["content"].as_str().unwrap();
        assert!(prompt.contains("{\"raw\":\"RRRR"), "{prompt}");
        assert!(
            prompt.contains("Retained extractor-only platform metadata"),
            "{prompt}"
        );
        assert!(prompt.contains("/v1/widgets"), "{prompt}");
    }

    let prompt = requests[0]["messages"][1]["content"].as_str().unwrap();
    let evidence = prompt
        .split_once("\n<untrusted_source>\n")
        .and_then(|(_, evidence)| evidence.split_once("\n</untrusted_source>"))
        .map(|(evidence, _)| evidence)
        .expect("bounded source evidence in public Ollama prompt");
    assert_eq!(evidence.len(), 64 * 1024);

    let source_page = fs::read_to_string(output.join("sources/source-00.md")).unwrap();
    let graph_ref = frontmatter_value(&source_page, "graph_ref").unwrap();
    let input_sha256 = frontmatter_value(&source_page, "input_sha256").unwrap();
    let source_sha256 = digest(&raw);
    let sent_evidence_sha256 = digest(evidence.as_bytes());
    let mut expected = Sha256::new();
    for value in [
        "graphoxide-wiki-synthesis-v1",
        "source",
        graph_ref,
        input_sha256,
        "qwen-test",
        "source-00#capture-00",
        &source_sha256,
        &sent_evidence_sha256,
    ] {
        expected.update((value.len() as u64).to_be_bytes());
        expected.update(value.as_bytes());
    }
    let expected = hex::encode(expected.finalize());
    assert_eq!(
        frontmatter_value(&source_page, "evidence_sha256"),
        Some(expected.as_str()),
        "published digest must bind the exact text re-hashed and sent to Ollama"
    );
}

#[test]
fn citation_identifiers_must_match_wiki_core_grammar() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["catalog"]["source_id"] = json!("bad:id");
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args).unwrap_err().to_string();

    assert!(error.contains("source_id"), "{error}");
}

#[test]
fn source_scope_rejects_citations_at_the_redaction_boundary_before_network() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["catalog"]["source_id"] = json!("sk-abcdefghijklmnop1234");
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    args.scopes = BTreeSet::from([DraftScope::Source]);
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();

    assert!(error.contains("redaction boundary"), "{error}");
    assert!(!args.output.exists());
}

#[test]
fn failure_leaves_no_partial_output_and_existing_output_is_never_overwritten() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1), ("b.md", b"beta", 2)]);
    let ok = Box::leak(completion("first").into_boxed_str());
    let server = MockOllama::start(vec![(200, ok), (500, "{}")]);
    args.ollama_url = server.endpoint.clone();
    assert!(draft(args.clone()).is_err());
    server.finish();
    assert!(!args.output.exists());

    fs::create_dir(&args.output).unwrap();
    fs::write(args.output.join("keep"), "untouched").unwrap();
    let error = draft(args.clone()).unwrap_err().to_string();
    assert!(error.contains("already exists"), "{error}");
    assert_eq!(
        fs::read_to_string(args.output.join("keep")).unwrap(),
        "untouched"
    );
}

#[test]
fn source_text_reaches_local_request_and_evidence_digest_unredacted() {
    let secret = "sk-abcdefghijklmnop1234";
    let (_temporary, mut args) = fixture(&[("a.md", secret.as_bytes(), 1)]);
    let response = Box::leak(completion("safe body").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();
    args.scopes = BTreeSet::from([DraftScope::Source]);
    let output = args.output.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    assert!(request.to_string().contains(secret), "{request}");
    let prompt = request["messages"][1]["content"].as_str().unwrap();
    assert!(prompt.contains(secret), "{prompt}");
    let evidence = prompt
        .split_once("\n<untrusted_source>\n")
        .and_then(|(_, evidence)| evidence.split_once("\n</untrusted_source>"))
        .map(|(evidence, _)| evidence)
        .expect("source evidence in sent prompt");
    let source_page = fs::read_to_string(output.join("sources/source-00.md")).unwrap();
    let graph_ref = frontmatter_value(&source_page, "graph_ref").unwrap();
    let input_sha256 = frontmatter_value(&source_page, "input_sha256").unwrap();
    let source_sha256 = digest(secret.as_bytes());
    let sent_evidence_sha256 = digest(evidence.as_bytes());
    let mut expected = Sha256::new();
    for value in [
        "graphoxide-wiki-synthesis-v1",
        "source",
        graph_ref,
        input_sha256,
        "qwen-test",
        "source-00#capture-00",
        &source_sha256,
        &sent_evidence_sha256,
    ] {
        expected.update((value.len() as u64).to_be_bytes());
        expected.update(value.as_bytes());
    }
    assert_eq!(
        frontmatter_value(&source_page, "evidence_sha256"),
        Some(hex::encode(expected.finalize()).as_str()),
        "published digest must bind the unredacted evidence sent to Ollama"
    );
}

#[test]
fn maximum_catalog_metadata_stays_out_of_the_bounded_loopback_prompt() {
    let evidence = vec![b'x'; 64 * 1024];
    let (_temporary, mut args, source_id, capture_id) =
        maximum_metadata_fixture(evidence.as_slice());
    let response = Box::leak(completion("Grounded body.").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();

    assert!(prompt.len() <= 73_216, "{} prompt bytes", prompt.len());
    assert_eq!(prompt.matches("<untrusted_source>").count(), 1, "{prompt}");
    assert!(!prompt.contains(&source_id), "{prompt}");
    assert!(!prompt.contains(&capture_id), "{prompt}");
    assert!(!prompt.contains("source_id="), "{prompt}");
    assert!(!prompt.contains("capture_id="), "{prompt}");
    assert!(!prompt.contains("sha256="), "{prompt}");
    assert!(!prompt.contains("path="), "{prompt}");
}

#[test]
fn complete_chat_envelope_bounds_the_maximum_raw_source_evidence() {
    const CONTEXT_BYTES: usize = 73_728;
    const COMPLETION_BYTES: usize = 512;
    const CHAT_TEMPLATE_BYTES: usize = 1_024;
    const RETRY_INSTRUCTION_BYTES: usize = "Your immediately preceding answer was rejected because it violated the required Markdown body-only contract. Write only a dense explanatory Markdown body of at most roughly 400 words for the requested wiki page. Do not add frontmatter, a title, a draft marker, or a Sources section.".len();
    let (system_bytes, prompt_bytes) = maximum_plain_prompt_sizes();
    assert!(
        system_bytes
            + prompt_bytes
            + RETRY_INSTRUCTION_BYTES
            + CHAT_TEMPLATE_BYTES
            + COMPLETION_BYTES
            <= CONTEXT_BYTES
    );
}

#[test]
fn direct_markdown_completion_rejects_one_user_byte_over_before_loopback() {
    let response = Box::leak(completion("Sentinel body.").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    let transport = ollama_transport::OllamaTransport::local(&server.endpoint, "model").unwrap();
    let oversized = "x".repeat(72_020);

    match transport.complete_markdown(&oversized) {
        Ok(_) => {
            let request = server.finish().pop().unwrap();
            let prompt_bytes = request["messages"][1]["content"].as_str().unwrap().len();
            assert_eq!(prompt_bytes, 72_020);
            panic!("direct over-limit prompt reached the local model");
        }
        Err(error) => {
            assert!(error.to_string().contains("prompt byte cap"), "{error}");
            transport.complete_markdown("sentinel").unwrap();
            let requests = server.finish();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["messages"][1]["content"], "sentinel");
        }
    }
}

#[test]
fn markdown_retry_instruction_is_reserved_before_the_first_request() {
    let response = Box::leak(completion("Sentinel body.").into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    let transport = ollama_transport::OllamaTransport::local(&server.endpoint, "model").unwrap();
    let prompt = "x".repeat(72_019);

    match transport.complete_markdown(&prompt) {
        Ok(_) => {
            let request = server.finish().pop().unwrap();
            let prompt_bytes = request["messages"][1]["content"].as_str().unwrap().len();
            assert_eq!(prompt_bytes, 72_019);
            panic!("retry-unbudgeted prompt reached the local model");
        }
        Err(error) => {
            assert!(error.to_string().contains("prompt byte cap"), "{error}");
            transport.complete_markdown("sentinel").unwrap();
            let requests = server.finish();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["messages"][1]["content"], "sentinel");
        }
    }
}

#[test]
fn credential_bearing_source_reaches_model_and_wiki_unredacted() {
    let secret = "fake-default-password-for-wiki-test";
    let source = format!("default_username=admin\ndefault_password={secret}\n");
    let (_temporary, mut args) = fixture(&[("a.md", source.as_bytes(), 1)]);
    let response = Box::leak(completion(&format!("body {secret}")).into_boxed_str());
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).unwrap();
    let requests = server.finish();
    let page = fs::read_to_string(community_page(&args.output, 1)).unwrap();

    assert!(requests[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains(secret));
    assert!(page.contains(secret), "{page}");
}

#[cfg(unix)]
#[test]
fn source_symlinks_are_refused() {
    use std::os::unix::fs::symlink;
    let (_temporary, mut args) = fixture(&[("real.md", b"alpha", 1)]);
    let root = &args.source_root;
    symlink(root.join("real.md"), root.join("alias.md")).unwrap();
    let mut graph: Value = serde_json::from_slice(&fs::read(&args.graph).unwrap()).unwrap();
    graph["nodes"][0]["source_file"] = json!("alias.md");
    fs::write(&args.graph, serde_json::to_vec(&graph).unwrap()).unwrap();
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    assert!(draft(args.clone()).is_err());
    assert!(!args.output.exists());
}

#[test]
fn markdown_completion_requests_a_dense_bounded_body() {
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion("Dense body.").into_boxed_str()),
    )]);
    let transport = ollama_transport::OllamaTransport::local(&server.endpoint, "model").unwrap();

    assert_eq!(
        transport.complete_markdown("prompt").unwrap(),
        "Dense body."
    );
    let request = server.finish().pop().unwrap();
    let instruction = request["messages"][0]["content"].as_str().unwrap();

    assert_eq!(request["max_tokens"], 512);
    assert_eq!(request["options"]["num_ctx"], 73_728);
    assert!(instruction.contains("dense explanatory Markdown body"));
    assert!(instruction.contains("at most roughly 400 words"));
}

#[test]
fn json_completion_reuses_the_loopback_transport_contract() {
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion(r#"{"sections":[]}"#).into_boxed_str()),
    )]);
    let transport = ollama_transport::OllamaTransport::local(&server.endpoint, "model").unwrap();

    assert_eq!(
        transport
            .complete_json_object("Return JSON.", "prompt")
            .unwrap(),
        json!({"sections": []})
    );
    let request = server.finish().pop().unwrap();
    assert_eq!(request["max_tokens"], 1_024);
    assert_eq!(request["messages"][0]["content"], "Return JSON.");
    assert_eq!(request["messages"][1]["content"], "prompt");
}

#[test]
fn native_ollama_chat_uses_native_request_and_response_contract() {
    let server = MockOllama::start(vec![(
        200,
        Box::leak(
            serde_json::to_string(&json!({"message": {"content": r#"{"sections":[]}"#}}))
                .unwrap()
                .into_boxed_str(),
        ),
    )]);
    let base = server.endpoint.strip_suffix("/v1").unwrap();
    let transport = ollama_transport::OllamaTransport::local_native(base, "model").unwrap();

    assert_eq!(
        transport
            .complete_json_object("Return JSON.", "prompt")
            .unwrap(),
        json!({"sections": []})
    );
    let request = server.finish().pop().unwrap();
    assert_eq!(request["stream"], false);
    assert_eq!(request["format"], "json");
    assert_eq!(request["options"]["num_predict"], 1_024);
    assert!(request.get("max_tokens").is_none());
}

#[test]
fn openai_compatible_profile_uses_pinned_loopback_transport() {
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();
    let server = MockOllama::start(vec![(
        200,
        Box::leak(completion(r#"{"sections":[]}"#).into_boxed_str()),
    )]);
    let profile = ProviderProfile::from_json(
        format!(
            r#"{{"version":1,"id":"test-openai","protocol":"openai-compatible","endpoint":"{}","model":"test-model","api_key_env":"GRAPHOXIDE_WIKI_TEST_KEY","source_egress_consent":"send-source-text-to-test-openai"}}"#,
            server.endpoint
        )
        .as_bytes(),
    )
    .unwrap();
    unsafe { std::env::set_var("GRAPHOXIDE_WIKI_TEST_KEY", "test-control-plane-key") };
    let transport = WikiModelTransport::from_profile(&profile).unwrap();
    let response = transport
        .complete_json_object("Return JSON.", "credential-bearing source text")
        .unwrap();
    unsafe { std::env::remove_var("GRAPHOXIDE_WIKI_TEST_KEY") };

    assert_eq!(response, json!({"sections": []}));
    let request = server.finish().pop().unwrap();
    assert_eq!(request["model"], "test-model");
    assert_eq!(request["response_format"]["type"], "json_object");
    assert_eq!(
        request["messages"][1]["content"],
        "credential-bearing source text"
    );
}

#[test]
fn canonical_draft_synthesizes_validated_sections_without_changing_static_evidence() {
    let (_temporary, mut args, block) = canonical_draft_fixture();
    let response = Box::leak(
        completion(
            &serde_json::to_string(&json!({
                "sections": [{
                    "heading": "Procedure",
                    "evidence_block_ids": [block.clone()],
                    "body": "Run the approved installer command."
                }]
            }))
            .unwrap(),
        )
        .into_boxed_str(),
    );
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args.clone()).unwrap();
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains(&block));
    let article = args
        .output
        .join("getting-started/installation--installation.md");
    let markdown = fs::read_to_string(&article).unwrap();
    assert!(markdown.contains("<!-- graphoxide-draft sha256="));
    assert!(markdown.contains("## Procedure"));
    assert!(markdown.contains(&format!("Evidence blocks: `{block}`")));
    assert!(!markdown.contains("## Technical reference"));

    fs::write(
        args.source_root.join("wiki.json"),
        br#"{"version":1,"roots":["wiki"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    )
    .unwrap();
    let index = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .current_dir(&args.source_root)
        .args(["wiki", "index", ".", "--config", "wiki.json"])
        .output()
        .unwrap();
    assert!(index.status.success(), "{}", command_output(&index));
    let check = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .current_dir(&args.source_root)
        .args(["wiki", "check", ".", "--config", "wiki.json"])
        .args([
            "--catalog",
            "catalog",
            "--graph",
            "graph.json",
            "--plan",
            "wiki-plan.json",
        ])
        .output()
        .unwrap();
    assert!(check.status.success(), "{}", command_output(&check));

    fs::write(&article, markdown.replacen("approved", "altered", 1)).unwrap();
    let reindex = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .current_dir(&args.source_root)
        .args(["wiki", "index", ".", "--config", "wiki.json"])
        .output()
        .unwrap();
    assert!(reindex.status.success(), "{}", command_output(&reindex));
    let stale = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .current_dir(&args.source_root)
        .args(["wiki", "check", ".", "--config", "wiki.json"])
        .args([
            "--catalog",
            "catalog",
            "--graph",
            "graph.json",
            "--plan",
            "wiki-plan.json",
        ])
        .output()
        .unwrap();
    assert!(!stale.status.success());
    assert!(command_output(&stale).contains("no longer matches the reviewed plan render"));
}

#[test]
fn canonical_draft_records_secret_free_registry_provenance() {
    let (temporary, mut args, block) = canonical_draft_fixture();
    let registry = canonical_registry(&temporary, &args.source_root);
    args.registry_tree = Some(registry.clone());
    let response = Box::leak(
        completion(
            &serde_json::to_string(&json!({
                "sections": [{
                    "heading": "Procedure",
                    "evidence_block_ids": [block],
                    "body": "Run the approved installer command."
                }]
            }))
            .unwrap(),
        )
        .into_boxed_str(),
    );
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    draft(args).unwrap();
    server.finish();

    let run = canonical_registry_run(&registry);
    assert_eq!(run["status"], "succeeded");
    assert_eq!(run["processor"], "graphoxide-wiki");
    assert_eq!(run["actor"], "graphoxide-cli");
    assert_eq!(run["model_requested"], "qwen-test");
    assert_eq!(run["model_reported"], Value::Null);
    assert!(run["prompt_schema_digest"]
        .as_str()
        .is_some_and(|value| value.len() == 64));
    assert!(run["evidence_manifest_digest"]
        .as_str()
        .is_some_and(|value| value.len() == 64));
    assert!(run["output_digest"]
        .as_str()
        .is_some_and(|value| value.len() == 64));
    let stored = serde_json::to_string(&run).unwrap();
    assert!(!stored.contains("approved installer command"));
    assert!(!run.as_object().unwrap().contains_key("prompt"));
}

#[test]
fn canonical_draft_records_failed_registry_provenance() {
    let (temporary, mut args, _) = canonical_draft_fixture();
    let registry = canonical_registry(&temporary, &args.source_root);
    args.registry_tree = Some(registry.clone());
    let server = MockOllama::start(vec![(500, "{}")]);
    args.ollama_url = server.endpoint.clone();

    let error = draft(args.clone()).unwrap_err().to_string();
    server.finish();

    assert!(error.contains("HTTP 500"), "{error}");
    assert!(!args.output.exists());
    let run = canonical_registry_run(&registry);
    assert_eq!(run["status"], "failed");
    assert_eq!(run["output_digest"], Value::Null);
    assert_eq!(run["error_class"], "model-completion-failed");
    assert!(!serde_json::to_string(&run)
        .unwrap()
        .contains("approved installer command"));
}

#[test]
fn canonical_draft_rejects_unknown_evidence_without_publishing() {
    let (_temporary, mut args, _) = canonical_draft_fixture();
    let response = Box::leak(
        completion(
            r#"{"sections":[{"heading":"Procedure","evidence_block_ids":["unknown"],"body":"Unsupported claim."}]}"#,
        )
        .into_boxed_str(),
    );
    let server = MockOllama::start(vec![(200, response)]);
    args.ollama_url = server.endpoint.clone();

    let error = draft(args.clone()).unwrap_err().to_string();
    server.finish();
    assert!(error.contains("unsupported evidence block"), "{error}");
    assert!(!args.output.exists());
    assert!(wiki_stage_entries(args.output.parent().unwrap()).is_empty());
}

#[test]
fn canonical_draft_rehashes_after_its_final_model_response() {
    let (_temporary, mut args, block) = canonical_draft_fixture();
    let source = args.source_root.join("guide.md");
    let response = Box::leak(
        completion(
            &serde_json::to_string(&json!({
                "sections": [{
                    "heading": "Procedure",
                    "evidence_block_ids": [block],
                    "body": "Run the approved installer command."
                }]
            }))
            .unwrap(),
        )
        .into_boxed_str(),
    );
    let server = MockOllama::start_after_first(vec![(200, response)], move || {
        fs::write(source, "changed after final model request\n").unwrap();
    });
    args.ollama_url = server.endpoint.clone();

    let error = draft(args.clone()).unwrap_err().to_string();
    server.finish();
    assert!(error.contains("sha256 does not match"), "{error}");
    assert!(!args.output.exists());
}

#[test]
fn canonical_draft_rejects_scopes_before_contacting_the_model() {
    let (_temporary, mut args, _) = canonical_draft_fixture();
    args.scopes.insert(DraftScope::Source);
    args.ollama_url = "http://127.0.0.1:1/v1".into();

    let error = draft(args.clone()).unwrap_err().to_string();
    assert!(error.contains("--scope is not supported"), "{error}");
    assert!(!args.output.exists());
}

#[test]
fn wiki_plan_proposal_is_metadata_only_review_input_and_never_overwrites() {
    let (_temporary, args, _) = canonical_draft_fixture();
    let output = args.source_root.join("wiki-plan.proposed.json");
    let response = Box::leak(
        completion(
        r#"{"version":1,"domains":[{"id":"getting-started","title":"Getting started","slug":"getting-started"}],"sources":[{"id":"guide#capture-current","title":"Installation guide","slug":"installation-guide","domain":"getting-started","coverage":"complete"}],"articles":[{"id":"batch-1-installation","title":"Installation","slug":"installation","domain":"getting-started","article_type":"procedure","sources":["guide#capture-current"],"aliases":[],"related":[]}]}"#,
        )
        .into_boxed_str(),
    );
    let server = MockOllama::start(vec![(200, response)]);
    let result = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .current_dir(&args.source_root)
        .args([
            "wiki",
            "plan",
            "--graph",
            "graph.json",
            "--catalog",
            "catalog",
            "--output",
            "wiki-plan.proposed.json",
            "--model",
            "qwen-test",
            "--consent",
            CONSENT,
            "--ollama-url",
            &server.endpoint,
        ])
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", command_output(&result));
    let request = server.finish().pop().unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();
    assert!(prompt.contains("guide#capture-current"));
    assert!(!prompt.contains("Run the approved installer command."));
    let proposal = fs::read_to_string(&output).unwrap();
    assert!(proposal.contains("\"version\": 1"));

    let repeated = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .current_dir(&args.source_root)
        .args([
            "wiki",
            "plan",
            "--graph",
            "graph.json",
            "--catalog",
            "catalog",
            "--output",
            "wiki-plan.proposed.json",
            "--model",
            "qwen-test",
            "--consent",
            CONSENT,
            "--ollama-url",
            "http://127.0.0.1:1/v1",
        ])
        .output()
        .unwrap();
    assert!(!repeated.status.success());
    assert!(command_output(&repeated).contains("already exists"));
    assert_eq!(fs::read_to_string(output).unwrap(), proposal);
}

#[test]
fn wiki_plan_proposal_keeps_cross_batch_article_relationships() {
    let (_temporary, root) = batched_plan_fixture();
    let output = root.join("wiki-plan.proposed.json");
    let server = MockOllama::start(vec![
        (200, batched_plan_response(1, 0..12, &[])),
        (200, batched_plan_response(2, 12..13, &["batch-1-primary"])),
    ]);
    let result = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .current_dir(&root)
        .args([
            "wiki",
            "plan",
            "--graph",
            "graph.json",
            "--catalog",
            "catalog",
            "--output",
            "wiki-plan.proposed.json",
            "--model",
            "qwen-test",
            "--consent",
            CONSENT,
            "--ollama-url",
            &server.endpoint,
        ])
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", command_output(&result));
    assert_eq!(server.finish().len(), 2);
    let proposal: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert_eq!(
        proposal["articles"][1]["related"],
        json!(["batch-1-primary"])
    );
}

#[test]
fn wiki_plan_proposal_second_batch_failure_leaves_no_output() {
    let (_temporary, root) = batched_plan_fixture();
    let output = root.join("failed.proposed.json");
    let server = MockOllama::start(vec![
        (200, batched_plan_response(1, 0..12, &[])),
        (500, "{}"),
    ]);
    let result = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .current_dir(&root)
        .args([
            "wiki",
            "plan",
            "--graph",
            "graph.json",
            "--catalog",
            "catalog",
            "--output",
            "failed.proposed.json",
            "--model",
            "qwen-test",
            "--consent",
            CONSENT,
            "--ollama-url",
            &server.endpoint,
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(server.finish().len(), 2);
    assert!(!output.exists());
}

#[test]
fn loopback_transport_refuses_redirects_and_ignores_proxy_environment() {
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();
    let redirect_target = TcpListener::bind("127.0.0.1:0").unwrap();
    redirect_target.set_nonblocking(true).unwrap();
    let location = Box::leak(
        format!("http://{}/capture", redirect_target.local_addr().unwrap()).into_boxed_str(),
    );
    let redirect = MockOllama::start(vec![(302, location)]);
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    unsafe {
        std::env::set_var(
            "HTTP_PROXY",
            format!("http://{}", proxy.local_addr().unwrap()),
        )
    };
    let transport = ollama_transport::OllamaTransport::local(&redirect.endpoint, "model").unwrap();
    let error = transport
        .complete_markdown("prompt")
        .unwrap_err()
        .to_string();
    unsafe { std::env::remove_var("HTTP_PROXY") };
    redirect.finish();

    assert!(error.contains("HTTP 302"), "{error}");
    assert!(proxy.accept().is_err(), "request used proxy");
    assert!(redirect_target.accept().is_err(), "redirect was followed");
}

#[test]
fn shared_transport_preserves_ollama_label_request_and_response_shape() {
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();
    let response = Box::leak(
        serde_json::to_string(&json!({
            "choices": [{"message": {"content": "[{\"community\":1,\"label\":\"Core\"}]"}}],
            "usage": {"prompt_tokens": 9, "completion_tokens": 4}
        }))
        .unwrap()
        .into_boxed_str(),
    );
    let server = MockOllama::start(vec![(200, response)]);
    unsafe {
        std::env::set_var("OLLAMA_BASE_URL", &server.endpoint);
        std::env::set_var("OLLAMA_MODEL", "label-model");
    }
    let transport = ollama_transport::OllamaTransport::for_labeling(None, Some(3.0)).unwrap();
    assert!(transport.warning().is_none());
    let result = transport
        .call_label(&graphoxide_graph::LabelRequest {
            prompt: "label prompt".into(),
            backend: "ollama".into(),
            model: None,
            max_tokens: 321,
        })
        .unwrap();
    unsafe {
        std::env::remove_var("OLLAMA_BASE_URL");
        std::env::remove_var("OLLAMA_MODEL");
    }
    let requests = server.finish();

    assert_eq!(result.usage.input, 9);
    assert_eq!(result.usage.output, 4);
    assert_eq!(requests[0]["model"], "label-model");
    assert_eq!(requests[0]["max_tokens"], 321);
    assert_eq!(requests[0]["temperature"], 0);
    assert_eq!(requests[0]["messages"][0]["content"], "label prompt");
}

#[test]
fn output_parent_must_be_a_real_directory() {
    let (_temporary, mut args) = fixture(&[("a.md", b"alpha", 1)]);
    args.output = args.source_root.join("missing-parent/wiki");
    args.ollama_url = "http://127.0.0.1:1/v1".into();
    assert!(draft(args).is_err());
}
