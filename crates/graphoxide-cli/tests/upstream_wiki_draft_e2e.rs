//! End-to-end coverage for the catalog-index-to-local-draft workflow.
//!
//! Secure wiki publication (draft/render) is supported on Linux x86_64 and
//! macOS, so the whole suite is gated to those platforms.
#![cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]

use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
    thread,
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

fn graphoxide(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .current_dir(project)
        .env_remove("GRAPHOXIDE_FORCE")
        .env_remove("GRAPHIFY_FORCE")
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT")
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

fn assert_success(output: Output) {
    assert!(output.status.success(), "{}", output_text(&output));
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A real classic-xref PDF fixture, so this test exercises the production
/// byte-only PDF parser rather than a test-only conversion path.
fn classic_xref_pdf(text: &str) -> Vec<u8> {
    assert!(text.is_ascii());
    let content = format!("BT\n/F1 12 Tf\n72 720 Td\n({text}) Tj\nET\n");
    let objects = [
        b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec(),
        b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n".to_vec(),
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n".to_vec(),
        [
            format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len()).into_bytes(),
            content.into_bytes(),
            b"endstream\nendobj\n".to_vec(),
        ]
        .concat(),
        b"5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>\nendobj\n".to_vec(),
    ];
    let mut pdf = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for object in objects {
        offsets.push(pdf.len());
        pdf.extend(object);
    }
    let xref_offset = pdf.len();
    pdf.extend(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
    pdf.extend(b"0000000000 65535 f \n");
    for offset in offsets {
        pdf.extend(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend(
        format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes(),
    );
    pdf
}

fn docx(text: &str) -> Vec<u8> {
    let entries = [
        (
            "[Content_Types].xml",
            b"<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>".as_slice(),
        ),
        (
            "_rels/.rels",
            b"<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>".as_slice(),
        ),
    ];
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (path, contents) in entries {
        writer.start_file(path, options).expect("start DOCX member");
        writer.write_all(contents).expect("write DOCX member");
    }
    writer
        .start_file("word/document.xml", options)
        .expect("start DOCX document");
    writer
        .write_all(
            format!(
                "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"
            )
            .as_bytes(),
        )
        .expect("write DOCX document");
    writer.finish().expect("finish DOCX").into_inner()
}

struct MockOllama {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl MockOllama {
    fn start(response: String) -> Self {
        Self::start_many(vec![response])
    }

    fn start_many(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback Ollama");
        let endpoint = format!("http://{}/v1", listener.local_addr().expect("address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let join = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept Ollama request");
                let mut wire = Vec::new();
                let mut chunk = [0_u8; 4096];
                let body = loop {
                    let read = stream.read(&mut chunk).expect("read Ollama request");
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
                        .expect("Ollama content length");
                    if wire.len() >= headers_end + length {
                        break wire[headers_end..headers_end + length].to_vec();
                    }
                };
                captured
                    .lock()
                    .expect("request lock")
                    .push(serde_json::from_slice(&body).expect("Ollama JSON request"));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .expect("write Ollama response");
            }
        });
        Self {
            endpoint,
            requests,
            join: Some(join),
        }
    }

    fn finish(mut self) -> Vec<Value> {
        self.join
            .take()
            .expect("server join")
            .join()
            .expect("server");
        Arc::try_unwrap(self.requests)
            .expect("only server owns requests")
            .into_inner()
            .expect("request lock")
    }
}

#[test]
fn catalog_bound_wiki_render_publishes_complete_model_free_pages_once() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let raw = temporary.path().join("raw");
    let wiki = temporary.path().join("wiki");
    let rendered = wiki.join("rendered");
    fs::create_dir_all(raw.join("docs")).expect("raw docs");
    fs::create_dir_all(raw.join("catalog")).expect("catalog");
    fs::create_dir_all(&wiki).expect("wiki root");

    let represented = b"catalog-backed graph source\n";
    let inventory = b"catalog-only inventory source\n";
    fs::write(raw.join("docs/source.md"), represented).expect("represented source");
    fs::write(raw.join("docs/inventory.md"), inventory).expect("inventory source");
    let represented_record = json!({
        "source_id": "source-one",
        "capture_id": "capture-one",
        "source_path": "docs/source.md",
        "sha256": sha256(represented),
        "captured_at": "2026-08-24T12:34:56Z",
        "accessed_at": "2026-08-24T12:35:56Z",
        "updated_at": "2026-08-24T12:35:56Z",
        "representation": "markdown",
        "source_system": "sharepoint",
        "url": "https://example.invalid/source-one",
        "location": "Library/docs/source.md"
    });
    let inventory_record = json!({
        "source_id": "inventory-source",
        "capture_id": "inventory-capture",
        "source_path": "docs/inventory.md",
        "sha256": sha256(inventory),
        "captured_at": "2026-08-24T12:34:56Z",
        "accessed_at": "2026-08-24T12:35:56Z",
        "updated_at": "2026-08-24T12:35:56Z",
        "representation": "markdown",
        "source_system": "sharepoint",
        "url": "https://example.invalid/inventory-source",
        "location": "Library/docs/inventory.md"
    });
    fs::write(
        raw.join("catalog/catalog.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "entries": [represented_record.clone(), inventory_record]
        }))
        .expect("catalog JSON"),
    )
    .expect("catalog");
    let graph_path = wiki.join("graph.json");
    fs::write(
        &graph_path,
        serde_json::to_vec(&json!({
            "nodes": [{
                "id": "source-node",
                "label": "source.md",
                "file_type": "document",
                "source_file": "docs/source.md",
                "community": 0,
                "community_name": "Source community",
                "catalog": represented_record
            }],
            "links": []
        }))
        .expect("graph JSON"),
    )
    .expect("graph");

    assert_success(
        graphoxide(&raw)
            .args(["wiki", "render", ".", "--graph"])
            .arg(&graph_path)
            .args(["--catalog", "catalog"])
            .arg("--output")
            .arg(&rendered)
            .output()
            .expect("render catalog-bound wiki"),
    );

    let mut pages = vec![
        PathBuf::from("index.md"),
        PathBuf::from("sources/source-one.md"),
        PathBuf::from("sources/inventory-source.md"),
        PathBuf::from("inventory/inventory-source.md"),
    ];
    for directory in ["topics", "communities"] {
        let page = fs::read_dir(rendered.join(directory))
            .expect("read rendered navigation directory")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "md"))
            .unwrap_or_else(|| panic!("find generated {directory} page"));
        pages.push(
            page.strip_prefix(&rendered)
                .expect("navigation page below output root")
                .to_path_buf(),
        );
    }
    for page in pages {
        let markdown = fs::read_to_string(rendered.join(&page))
            .unwrap_or_else(|error| panic!("read generated {}: {error}", page.display()));
        assert!(
            !markdown.contains("graphoxide-draft"),
            "{}: {markdown}",
            page.display()
        );
    }
    let root_page = fs::read_to_string(rendered.join("index.md")).expect("root page");
    let inventory_source = fs::read_to_string(rendered.join("sources/inventory-source.md"))
        .expect("inventory source page");
    assert!(
        root_page.contains("inventory/inventory-source.md"),
        "{root_page}"
    );
    assert!(
        inventory_source.contains("../inventory/inventory-source.md"),
        "{inventory_source}"
    );

    fs::write(
        wiki.join("wiki.json"),
        r#"{"version":1,"roots":["rendered"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    )
    .expect("wiki config");
    assert_success(
        graphoxide(&wiki)
            .args(["wiki", "index", ".", "--config", "wiki.json"])
            .output()
            .expect("index rendered wiki"),
    );
    assert_success(
        graphoxide(&wiki)
            .args(["wiki", "check", ".", "--config", "wiki.json"])
            .args(["--catalog", "catalog", "--catalog-root"])
            .arg(&raw)
            .arg("--graph")
            .arg(&graph_path)
            .output()
            .expect("check rendered wiki"),
    );

    fs::write(rendered.join("keep"), "existing").expect("destination sentinel");
    let refused = graphoxide(&raw)
        .args(["wiki", "render", ".", "--graph"])
        .arg(&graph_path)
        .args(["--catalog", "catalog"])
        .arg("--output")
        .arg(&rendered)
        .output()
        .expect("refuse existing render destination");
    assert!(!refused.status.success(), "{}", output_text(&refused));
    assert!(
        output_text(&refused).contains("output directory already exists"),
        "{}",
        output_text(&refused)
    );
    assert_eq!(
        fs::read_to_string(rendered.join("keep")).expect("destination sentinel"),
        "existing"
    );
}

#[test]
fn catalog_aware_draft_publishes_pdf_docx_and_inventory_sources() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let raw = temporary.path().join("raw");
    let wiki = temporary.path().join("wiki");
    fs::create_dir_all(raw.join("docs")).expect("raw docs");
    fs::create_dir_all(raw.join("catalog")).expect("catalog");
    fs::create_dir_all(&wiki).expect("wiki root");

    let pdf = classic_xref_pdf("Catalog PDF evidence");
    let docx = docx("Catalog DOCX evidence");
    let inventory = b"intentionally excluded active source\n";
    fs::write(raw.join("docs/paper.pdf"), &pdf).expect("PDF fixture");
    fs::write(raw.join("docs/spec.docx"), &docx).expect("DOCX fixture");
    fs::write(raw.join("docs/no-node.md"), inventory).expect("inventory fixture");
    let entries = [
        (
            "pdf-source",
            "pdf-capture",
            "docs/paper.pdf",
            pdf.as_slice(),
            "pdf",
        ),
        (
            "docx-source",
            "docx-capture",
            "docs/spec.docx",
            docx.as_slice(),
            "docx",
        ),
        (
            "no-node-source",
            "no-node-capture",
            "docs/no-node.md",
            inventory.as_slice(),
            "markdown",
        ),
    ]
    .into_iter()
    .map(
        |(source_id, capture_id, source_path, bytes, representation)| {
            json!({
                "source_id": source_id,
                "capture_id": capture_id,
                "source_path": source_path,
                "sha256": sha256(bytes),
                "captured_at": "2026-08-24T12:34:56Z",
                "accessed_at": "2026-08-24T12:35:56Z",
                "updated_at": "2026-08-24T12:35:56Z",
                "representation": representation,
                "source_system": "sharepoint",
                "url": format!("https://example.invalid/{source_id}"),
                "location": format!("Library/{source_path}")
            })
        },
    )
    .collect::<Vec<_>>();
    fs::write(
        raw.join("catalog/catalog.json"),
        serde_json::to_vec(&json!({"version": 1, "entries": entries})).expect("catalog JSON"),
    )
    .expect("catalog");

    assert_success(
        graphoxide(&raw)
            .args([
                "index",
                ".",
                "--catalog",
                "catalog",
                "--exclude",
                "catalog",
                "--exclude",
                "docs/no-node.md",
                "--force",
            ])
            .output()
            .expect("index catalog-backed PDF and DOCX"),
    );
    let graph_path = raw.join("graphoxide-out/graph.json");
    let graph_bytes = fs::read(&graph_path).expect("graph");
    let graph: Value = serde_json::from_slice(&graph_bytes).expect("JSON");
    for (source_path, source_id) in [
        ("docs/paper.pdf", "pdf-source"),
        ("docs/spec.docx", "docx-source"),
    ] {
        assert!(
            graph["nodes"]
                .as_array()
                .expect("nodes")
                .iter()
                .any(|node| {
                    node["source_file"] == source_path && node["catalog"]["source_id"] == source_id
                }),
            "missing extracted catalog annotation for {source_path}: {graph}"
        );
    }
    assert!(
        !graph["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .any(|node| { node["source_file"] == "docs/no-node.md" }),
        "the excluded active source must enter the inventory path"
    );

    let community_count = graph["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|node| node["community"].as_i64())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    assert!(
        community_count > 0,
        "extracted graph needs communities: {graph}"
    );
    let topic_tree = graphoxide_export::derive_topic_tree(
        &serde_json::from_slice(&graph_bytes).expect("knowledge graph"),
    )
    .expect("topic tree");
    let response = |content| {
        serde_json::to_string(&json!({
            "choices": [{"message": {"content": content}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }))
        .expect("Ollama JSON")
    };
    let mut responses = vec![
        response("Catalog DOCX evidence"),
        response("Catalog PDF evidence"),
    ];
    responses.extend(
        topic_tree
            .topics
            .iter()
            .map(|_| response("Catalog PDF evidence and Catalog DOCX evidence")),
    );
    let expected_requests = responses.len();
    let server = MockOllama::start_many(responses);
    let drafts = wiki.join("drafts");
    assert_success(
        graphoxide(&raw)
            .args(["wiki", "draft", "."])
            .arg("--graph")
            .arg(&graph_path)
            .args(["--catalog", "catalog"])
            .arg("--output")
            .arg(&drafts)
            .args([
                "--model",
                "fixture-model",
                "--consent",
                "send-source-text-to-local-ollama",
                "--scope",
                "source",
                "--scope",
                "topic",
                "--ollama-url",
            ])
            .arg(&server.endpoint)
            .output()
            .expect("draft catalog-backed PDF and DOCX wiki"),
    );
    let requests = server.finish();
    assert_eq!(requests.len(), expected_requests);
    let prompts = requests
        .iter()
        .map(|request| {
            request["messages"][1]["content"]
                .as_str()
                .expect("draft prompt")
        })
        .collect::<Vec<_>>();
    assert!(
        prompts
            .iter()
            .any(|prompt| prompt.contains("Catalog PDF evidence")),
        "PDF extraction did not reach the local draft prompt: {prompts:?}"
    );
    assert!(
        prompts
            .iter()
            .any(|prompt| prompt.contains("Catalog DOCX evidence")),
        "DOCX extraction did not reach the local draft prompt: {prompts:?}"
    );
    assert!(
        prompts
            .iter()
            .all(|prompt| !prompt.contains("intentionally excluded active source")),
        "an inventory-only source must never be read or sent to Ollama: {prompts:?}"
    );
    for page in [
        "sources/pdf-source.md",
        "sources/docx-source.md",
        "sources/no-node-source.md",
        "inventory/no-node-source.md",
    ] {
        assert!(drafts.join(page).is_file(), "missing generated page {page}");
    }
    for (path, evidence, citation) in [
        (
            "sources/pdf-source.md",
            "Catalog PDF evidence",
            "pdf-source#pdf-capture",
        ),
        (
            "sources/docx-source.md",
            "Catalog DOCX evidence",
            "docx-source#docx-capture",
        ),
    ] {
        let page = fs::read_to_string(drafts.join(path)).expect("synthesized source page");
        assert!(page.contains("<!-- graphoxide-draft -->"), "{path}: {page}");
        assert!(page.contains(evidence), "{path}: {page}");
        assert!(page.contains(citation), "{path}: {page}");
    }
    let topic_pages = fs::read_dir(drafts.join("topics"))
        .expect("read topic directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect::<Vec<_>>();
    assert_eq!(topic_pages.len(), topic_tree.topics.len());
    for path in topic_pages {
        let page = fs::read_to_string(&path).expect("synthesized topic page");
        assert!(
            page.contains("<!-- graphoxide-draft -->"),
            "{}: {page}",
            path.display()
        );
        assert!(
            page.contains("Catalog PDF evidence") || page.contains("Catalog DOCX evidence"),
            "{}: {page}",
            path.display()
        );
        assert!(
            page.contains("pdf-source#pdf-capture") || page.contains("docx-source#docx-capture"),
            "{}: {page}",
            path.display()
        );
    }

    fs::write(
        wiki.join("wiki.json"),
        r#"{"version":1,"roots":["drafts"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    )
    .expect("wiki config");
    assert_success(
        graphoxide(&wiki)
            .args(["wiki", "index", ".", "--config", "wiki.json"])
            .output()
            .expect("index catalog-aware generated wiki"),
    );
    assert_success(
        graphoxide(&wiki)
            .args(["wiki", "check", ".", "--config", "wiki.json"])
            .args(["--catalog", "catalog", "--catalog-root"])
            .arg(&raw)
            .args(["--graph"])
            .arg(&graph_path)
            .output()
            .expect("check complete catalog-aware generated wiki"),
    );

    let inventory_path = drafts.join("inventory/no-node-source.md");
    let source_path = drafts.join("sources/no-node-source.md");
    let root_path = drafts.join("index.md");
    let inventory = fs::read(&inventory_path).expect("inventory page");
    let source = fs::read_to_string(&source_path).expect("source page");
    let root = fs::read_to_string(&root_path).expect("root page");
    fs::remove_file(&inventory_path).expect("remove inventory page");
    fs::write(
        &source_path,
        source
            .lines()
            .filter(|line| !line.contains("inventory/no-node-source.md"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("remove inventory link");
    fs::write(
        &root_path,
        root.lines()
            .filter(|line| !line.contains("inventory/no-node-source.md"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("remove root inventory link");
    assert_success(
        graphoxide(&wiki)
            .args(["wiki", "index", ".", "--config", "wiki.json"])
            .output()
            .expect("reindex missing-inventory wiki"),
    );
    let missing = graphoxide(&wiki)
        .args(["wiki", "check", ".", "--config", "wiki.json"])
        .args(["--catalog", "catalog", "--catalog-root"])
        .arg(&raw)
        .args(["--graph"])
        .arg(&graph_path)
        .output()
        .expect("reject missing inventory page");
    assert!(!missing.status.success(), "{}", output_text(&missing));
    assert!(
        output_text(&missing).contains("missing catalog page inventory/no-node-source.md"),
        "{}",
        output_text(&missing)
    );

    fs::write(&inventory_path, &inventory).expect("restore inventory");
    fs::write(&source_path, source).expect("restore source");
    fs::write(&root_path, root).expect("restore root");
    let extra = drafts.join("sources/invented.md");
    fs::write(
        &extra,
        fs::read_to_string(&source_path)
            .expect("restored source")
            .replace("graph_ref: \"no-node-source\"", "graph_ref: \"invented\""),
    )
    .expect("invented source");
    assert_success(
        graphoxide(&wiki)
            .args(["wiki", "index", ".", "--config", "wiki.json"])
            .output()
            .expect("reindex invented-source wiki"),
    );
    let invented = graphoxide(&wiki)
        .args(["wiki", "check", ".", "--config", "wiki.json"])
        .args(["--catalog", "catalog", "--catalog-root"])
        .arg(&raw)
        .args(["--graph"])
        .arg(&graph_path)
        .output()
        .expect("reject invented source page");
    assert!(!invented.status.success(), "{}", output_text(&invented));
    assert!(
        output_text(&invented).contains("unexpected catalog page sources/invented.md"),
        "{}",
        output_text(&invented)
    );

    fs::remove_file(&extra).expect("remove invented source");
    let inventory_text = String::from_utf8(inventory.clone()).expect("inventory UTF-8");
    fs::write(
        &inventory_path,
        inventory_text.replace(
            "parent: \"sources/no-node-source.md\"",
            "parent: \"index.md\"",
        ),
    )
    .expect("bad inventory parent");
    let bad_parent = graphoxide(&wiki)
        .args(["wiki", "index", ".", "--config", "wiki.json"])
        .output()
        .expect("reject bad inventory parent");
    assert!(!bad_parent.status.success(), "{}", output_text(&bad_parent));
    assert!(
        output_text(&bad_parent).contains("invalid parent kind root"),
        "{}",
        output_text(&bad_parent)
    );

    fs::write(&inventory_path, &inventory).expect("restore inventory after parent check");
    fs::write(drafts.join("inventory/duplicate.md"), &inventory).expect("duplicate inventory");
    let duplicate = graphoxide(&wiki)
        .args(["wiki", "index", ".", "--config", "wiki.json"])
        .output()
        .expect("reject duplicate inventory");
    assert!(!duplicate.status.success(), "{}", output_text(&duplicate));
    assert!(
        output_text(&duplicate).contains("duplicate inventory placement no-node-source"),
        "{}",
        output_text(&duplicate)
    );
}

#[test]
fn catalog_index_to_local_draft_emits_cited_wiki_pages() {
    let temporary = tempfile::tempdir().expect("tempdir");
    // Catalog admission requires a symlink-free project root; macOS resolves
    // /var to /private/var, so canonicalize the fixture root before use.
    let root = fs::canonicalize(temporary.path()).expect("canonical tempdir");
    let raw = root.join("raw");
    let wiki = root.join("wiki");
    let source_path = "docs/source [draft].md";
    let source = raw.join(source_path);
    let source_text = b"Catalog-backed source text for the local wiki draft.\n";
    fs::create_dir_all(source.parent().expect("source parent")).expect("source parent");
    fs::create_dir_all(raw.join("catalog")).expect("catalog directory");
    fs::create_dir_all(&wiki).expect("wiki directory");
    fs::write(&source, source_text).expect("source fixture");
    fs::write(
        raw.join("catalog/catalog.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "entries": [{
                "source_id": "source-one",
                "capture_id": "capture-one",
                "source_path": source_path,
                "sha256": sha256(source_text),
                "captured_at": "2025-01-02T03:04:05Z",
                "accessed_at": "2025-01-02T03:04:05Z",
                "updated_at": "2025-01-02T03:04:05Z",
                "representation": "markdown",
                "source_system": "sharepoint",
                "url": "https://sharepoint.example.test/sites/wiki/docs/source.md",
                "location": "/sites/wiki/docs/source.md"
            }]
        }))
        .expect("catalog JSON"),
    )
    .expect("catalog fixture");

    assert_success(
        graphoxide(&raw)
            .args([
                "index",
                ".",
                "--catalog",
                "catalog",
                "--exclude",
                "catalog",
                "--force",
            ])
            .output()
            .expect("index catalog-backed source"),
    );
    let graph_path = raw.join("graphoxide-out/graph.json");
    let graph: Value =
        serde_json::from_slice(&fs::read(&graph_path).expect("indexed graph")).expect("graph JSON");
    assert!(
        graph["nodes"]
            .as_array()
            .expect("graph nodes")
            .iter()
            .any(|node| {
                node["source_file"] == source_path
                    && node["catalog"]["source_id"] == "source-one"
                    && node["catalog"]["capture_id"] == "capture-one"
            }),
        "catalog annotation missing from indexed graph: {graph}"
    );

    let model_body = "Model body\n\nOnly the local model supplied this Markdown body.";
    let response = serde_json::to_string(&json!({
        "choices": [{"message": {"content": model_body}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    }))
    .expect("Ollama response JSON");
    let server = MockOllama::start(response);
    let drafts = wiki.join("drafts");
    assert_success(
        graphoxide(&raw)
            .args(["wiki", "draft", "."])
            .arg("--graph")
            .arg(&graph_path)
            .arg("--output")
            .arg(&drafts)
            .args([
                "--model",
                "fixture-model",
                "--consent",
                "send-source-text-to-local-ollama",
                "--ollama-url",
            ])
            .arg(&server.endpoint)
            .output()
            .expect("draft catalog-backed wiki"),
    );
    let requests = server.finish();
    assert_eq!(requests.len(), 1, "expected one sequential local request");
    assert!(
        requests[0]["messages"][1]["content"]
            .as_str()
            .expect("draft prompt")
            .contains("Catalog-backed source text"),
        "catalog-backed source text was not sent to local Ollama"
    );

    let root_page = fs::read_to_string(drafts.join("index.md")).expect("root page");
    let topic_path = fs::read_dir(drafts.join("topics"))
        .expect("topic directory")
        .map(|entry| entry.expect("topic entry").path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| !name.to_string_lossy().starts_with("topic-0-"))
        })
        .expect("readable topic page");
    let topic_page = fs::read_to_string(&topic_path).expect("topic page");
    let source_page =
        fs::read_to_string(drafts.join("sources/source-one.md")).expect("source page");
    for structural_page in [&root_page, &topic_page, &source_page] {
        assert!(!structural_page.contains("graphoxide-draft"));
        assert!(!structural_page.contains(model_body));
    }

    let draft_path = fs::read_dir(drafts.join("communities"))
        .expect("community directory")
        .map(|entry| entry.expect("community entry").path())
        .find(|path| {
            path.file_name().is_some_and(|name| {
                !name
                    .to_string_lossy()
                    .trim_end_matches(".md")
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
            })
        })
        .expect("readable community draft page");
    let draft = fs::read_to_string(draft_path).expect("staged draft output");
    assert!(
        draft.starts_with("---\ntitle: ")
            && draft.contains("\nkind: \"community\"\n")
            && !draft.contains("title: \"Community 0\""),
        "{draft}"
    );
    assert!(
        draft.contains("sources:\n  - source-one#capture-one\n---"),
        "{draft}"
    );
    let (_, generated_body) = draft
        .split_once("<!-- graphoxide-draft -->\n\n")
        .expect("Graphoxide draft marker");
    let (body, _) = generated_body
        .rsplit_once("\n\n## Sources\n")
        .expect("Graphoxide Sources section");
    assert_eq!(
        body, model_body,
        "only the model response may supply the body"
    );
    assert!(draft.contains("](../sources/source-one.md)"), "{draft}");
    assert!(
        !fs::read_dir(&wiki)
            .expect("wiki parent entries")
            .any(|entry| {
                entry
                    .expect("wiki parent entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".wiki-stage-")
            }),
        "successful staged output must be atomically published"
    );

    fs::write(
        wiki.join("wiki.json"),
        r#"{"version":1,"roots":["drafts"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    )
    .expect("wiki config");
    assert_success(
        graphoxide(&wiki)
            .args(["wiki", "index", ".", "--config", "wiki.json"])
            .output()
            .expect("index generated draft tree"),
    );
    assert_success(
        graphoxide(&wiki)
            .args(["wiki", "check", ".", "--config", "wiki.json", "--catalog"])
            .arg(raw.join("catalog"))
            .args(["--catalog-root"])
            .arg(&raw)
            .output()
            .expect("check generated draft citations against raw catalog"),
    );
    let index = fs::read_to_string(wiki.join("llms.txt")).expect("wiki index");
    assert!(
        index.contains("drafts/communities/")
            && !index.contains("drafts/communities/0.md")
            && !index.contains("drafts/topics/topic-0.md"),
        "{index}"
    );
    assert!(!index.contains("[\"source [draft].md\"]"), "{index}");
    assert!(
        index.contains("drafts/topics/source-draft-")
            && index.contains("drafts/communities/source-draft-")
            && index.contains("drafts/sources/source-one.md"),
        "{index}"
    );
}

#[test]
fn wiki_draft_refuses_a_non_loopback_ollama_url_before_output() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let raw = temporary.path().join("raw");
    let wiki = temporary.path().join("wiki");
    let source = raw.join("docs/source.md");
    let source_text = b"Catalog-backed source text.\n";
    fs::create_dir_all(source.parent().expect("source parent")).expect("source parent");
    fs::create_dir_all(&wiki).expect("wiki directory");
    fs::write(&source, source_text).expect("source fixture");
    let graph = raw.join("graph.json");
    fs::write(
        &graph,
        serde_json::to_vec(&json!({
            "nodes": [{
                "id": "source",
                "label": "source.md",
                "file_type": "document",
                "source_file": "docs/source.md",
                "community": 0,
                "community_name": "source.md",
                "catalog": {
                    "source_id": "source-one",
                    "capture_id": "capture-one",
                    "source_path": "docs/source.md",
                    "sha256": sha256(source_text)
                }
            }],
            "links": []
        }))
        .expect("graph JSON"),
    )
    .expect("graph fixture");
    let output = wiki.join("drafts");

    let result = graphoxide(&raw)
        .args(["wiki", "draft", "."])
        .arg("--graph")
        .arg(&graph)
        .arg("--output")
        .arg(&output)
        .args([
            "--model",
            "fixture-model",
            "--consent",
            "send-source-text-to-local-ollama",
            "--ollama-url",
            "http://192.0.2.1:11434/v1",
        ])
        .output()
        .expect("run non-loopback draft");

    assert!(!result.status.success(), "{}", output_text(&result));
    assert!(
        output_text(&result).contains("loopback"),
        "{}",
        output_text(&result)
    );
    assert!(
        !output.exists(),
        "the rejected endpoint must not produce a draft directory"
    );
}
