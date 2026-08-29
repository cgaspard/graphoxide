use serde_json::json;
use std::{fs, path::Path, process::Command};

const SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn graphoxide(current_directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command.current_dir(current_directory);
    command
}

fn write(path: &Path, value: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(path, value).expect("write fixture");
}

fn catalog() -> serde_json::Value {
    json!({
        "version": 2,
        "sources": [{
            "source_id": "guide",
            "source_system": "sharepoint",
            "url": "https://example.invalid/guide",
            "location": "Library/Guide",
            "active_capture_id": "capture-current"
        }],
        "captures": [{
            "source_id": "guide",
            "capture_id": "capture-current",
            "source_path": "private/guide.md",
            "sha256": SHA256,
            "captured_at": "2026-08-24T12:34:56Z",
            "accessed_at": "2026-08-24T12:35:56Z",
            "updated_at": "2026-08-24T12:34:56Z",
            "representation": "markdown"
        }, {
            "source_id": "guide",
            "capture_id": "capture-history",
            "source_path": "private/guide-old.md",
            "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "captured_at": "2026-08-23T12:34:56Z",
            "accessed_at": "2026-08-23T12:35:56Z",
            "updated_at": "2026-08-23T12:34:56Z",
            "representation": "markdown"
        }]
    })
}

fn active_annotation() -> serde_json::Value {
    json!({
        "source_id": "guide",
        "capture_id": "capture-current",
        "source_path": "private/guide.md",
        "sha256": SHA256,
        "captured_at": "2026-08-24T12:34:56Z",
        "accessed_at": "2026-08-24T12:35:56Z",
        "updated_at": "2026-08-24T12:34:56Z",
        "representation": "markdown",
        "source_system": "sharepoint",
        "url": "https://example.invalid/guide",
        "location": "Library/Guide"
    })
}

fn plan() -> serde_json::Value {
    json!({
        "version": 1,
        "domains": [{"id":"getting-started","title":"Getting started","slug":"getting-started"}],
        "sources": [
            {"id":"guide#capture-current","title":"Installation guide","slug":"installation-guide","domain":"getting-started","coverage":"complete"},
            {"id":"guide#capture-history","title":"Installation guide archive","slug":"installation-guide-archive","domain":"getting-started","coverage":"inventory-only"}
        ],
        "articles": [{
            "id":"installation","title":"Installation","slug":"installation","domain":"getting-started",
            "article_type":"procedure","sources":["guide#capture-current"],"aliases":[],"related":[]
        }]
    })
}

#[test]
fn canonical_render_uses_graph_and_catalog_metadata_without_raw_source_files() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    write(
        &root.join("catalog/catalog.json"),
        serde_json::to_vec(&catalog()).expect("catalog JSON"),
    );
    write(
        &root.join("wiki-plan.json"),
        serde_json::to_vec(&plan()).expect("plan JSON"),
    );
    write(
        &root.join("graph.json"),
        serde_json::to_vec(&json!({
            "nodes": [{
                "id": "heading",
                "label": "Install",
                "file_type": "markdown",
                "source_file": "private/guide.md",
                "source_location": "L1",
                "catalog": active_annotation(),
                "type": "document_heading",
                "line_start": 1
            }, {
                "id": "paragraph",
                "label": "paragraph",
                "file_type": "markdown",
                "source_file": "private/guide.md",
                "source_location": "L3",
                "catalog": active_annotation(),
                "type": "document_paragraph",
                "structured_text": "Run the installer with the approved command.",
                "structured_text_type": "string",
                "line_start": 3
            }],
            "links": [{"source":"heading","target":"paragraph","relation":"contains"}]
        }))
        .expect("graph JSON"),
    );

    let output = graphoxide(root)
        .args([
            "wiki",
            "render",
            "--graph",
            "graph.json",
            "--catalog",
            "catalog",
            "--plan",
            "wiki-plan.json",
            "--output",
            "wiki",
        ])
        .output()
        .expect("render canonical wiki");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join("private/guide.md").exists());
    assert!(root.join("wiki/index.md").exists());
    assert!(root.join("wiki/AGENTS.md").exists());
    assert!(root
        .join("wiki/inventory/installation-guide-archive.md")
        .exists());
    let reference = fs::read_to_string(
        fs::read_dir(root.join("wiki/references"))
            .expect("references")
            .next()
            .expect("reference page")
            .expect("reference entry")
            .path(),
    )
    .expect("reference text");
    assert!(reference.contains("Run the installer with the approved command."));

    write(
        &root.join("wiki.json"),
        br#"{"version":1,"roots":["wiki"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    );
    let index = graphoxide(root)
        .args(["wiki", "index", ".", "--config", "wiki.json"])
        .output()
        .expect("index canonical wiki");
    assert!(
        index.status.success(),
        "{}",
        String::from_utf8_lossy(&index.stderr)
    );
    let check = graphoxide(root)
        .args(["wiki", "check", ".", "--config", "wiki.json"])
        .args([
            "--catalog",
            "catalog",
            "--graph",
            "graph.json",
            "--plan",
            "wiki-plan.json",
            "--json",
        ])
        .output()
        .expect("check canonical wiki");
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let quality: serde_json::Value = serde_json::from_slice(&check.stdout).expect("quality JSON");
    assert_eq!(quality["status"], "ok");
    assert!(quality["page_count"].as_u64().unwrap() > 0);
    assert!(quality["canonical_page_count"].as_u64().unwrap() > 0);
    assert_eq!(quality["complete_source_count"], 1);
    assert_eq!(quality["partial_source_count"], 0);
    assert_eq!(quality["inventory_only_source_count"], 1);
    assert!(quality["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| {
            diagnostic["citation"] == "guide#capture-history"
                && diagnostic["code"] == "coverage-inventory-only"
        }));

    let source_page = root.join("wiki/sources/installation-guide.md");
    let reviewed = fs::read_to_string(&source_page).unwrap().replacen(
        "review_status: \"generated\"",
        "review_status: \"reviewed\"",
        1,
    );
    fs::write(&source_page, &reviewed).expect("mark source reviewed");
    let reindex = graphoxide(root)
        .args(["wiki", "index", ".", "--config", "wiki.json"])
        .output()
        .expect("reindex reviewed wiki");
    assert!(reindex.status.success());
    let reviewed_check = graphoxide(root)
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
        .expect("check reviewed canonical wiki");
    assert!(reviewed_check.status.success());
    fs::write(
        &source_page,
        reviewed.replacen(
            "summary: \"source page: Installation guide\"",
            "summary: \"altered source metadata\"",
            1,
        ),
    )
    .expect("mutate source page");
    let reindex = graphoxide(root)
        .args(["wiki", "index", ".", "--config", "wiki.json"])
        .output()
        .expect("reindex mutated wiki");
    assert!(reindex.status.success());
    let stale = graphoxide(root)
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
        .expect("reject mutated canonical wiki");
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr)
        .contains("no longer matches the reviewed plan render"));
}

#[test]
fn canonical_check_json_reports_partial_extraction_and_stale_review_diagnostics() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let mut partial_plan = plan();
    partial_plan["sources"][0]["coverage"] = json!("partial");
    write(
        &root.join("catalog/catalog.json"),
        serde_json::to_vec(&catalog()).expect("catalog JSON"),
    );
    write(
        &root.join("wiki-plan.json"),
        serde_json::to_vec(&partial_plan).expect("plan JSON"),
    );
    write(
        &root.join("graph.json"),
        serde_json::to_vec(&json!({
            "nodes": [{
                "id": "paragraph",
                "label": "Installation details",
                "file_type": "markdown",
                "source_file": "private/guide.md",
                "source_location": "L3",
                "catalog": active_annotation(),
                "type": "document_paragraph",
                "structured_text": "The extractor retained this paragraph.",
                "structured_text_type": "string",
                "parse_status": "rejected-partial",
                "line_start": 3
            }],
            "links": []
        }))
        .expect("graph JSON"),
    );
    let render = graphoxide(root)
        .args([
            "wiki",
            "render",
            "--graph",
            "graph.json",
            "--catalog",
            "catalog",
            "--plan",
            "wiki-plan.json",
            "--output",
            "wiki",
        ])
        .output()
        .expect("render canonical wiki");
    assert!(
        render.status.success(),
        "{}",
        String::from_utf8_lossy(&render.stderr)
    );
    write(
        &root.join("wiki.json"),
        br#"{"version":1,"roots":["wiki"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    );
    let index = graphoxide(root)
        .args(["wiki", "index", ".", "--config", "wiki.json"])
        .output()
        .expect("index canonical wiki");
    assert!(index.status.success());
    let check = graphoxide(root)
        .args(["wiki", "check", ".", "--config", "wiki.json"])
        .args([
            "--catalog",
            "catalog",
            "--graph",
            "graph.json",
            "--plan",
            "wiki-plan.json",
            "--json",
        ])
        .output()
        .expect("check canonical wiki");
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let quality: serde_json::Value = serde_json::from_slice(&check.stdout).expect("quality JSON");
    let diagnostics = quality["diagnostics"].as_array().expect("diagnostic list");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["citation"] == "guide#capture-current"
            && diagnostic["code"] == "extraction-non-complete"
    }));

    let source_page = root.join("wiki/sources/installation-guide.md");
    let stale = fs::read_to_string(&source_page)
        .expect("source page")
        .replacen(
            "review_status: \"generated\"",
            "review_status: \"stale\"",
            1,
        );
    fs::write(&source_page, stale).expect("mark source stale");
    let index = graphoxide(root)
        .args(["wiki", "index", ".", "--config", "wiki.json"])
        .output()
        .expect("reindex stale wiki");
    assert!(index.status.success());
    let check = graphoxide(root)
        .args(["wiki", "check", ".", "--config", "wiki.json"])
        .args([
            "--catalog",
            "catalog",
            "--graph",
            "graph.json",
            "--plan",
            "wiki-plan.json",
            "--json",
        ])
        .output()
        .expect("check stale canonical wiki");
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let quality: serde_json::Value = serde_json::from_slice(&check.stdout).expect("quality JSON");
    assert!(quality["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| {
            diagnostic["code"] == "review-status-stale"
                && diagnostic["citation"] == "guide#capture-current"
        }));
}
