#![cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]

use graphoxide_core::KnowledgeGraph;
use graphoxide_export::{derive_topic_tree, render_structured_wiki_with_catalog};
use graphoxide_extract::catalog::Catalog;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const SHA256_PLACEHOLDER: &str = "CATALOG_SOURCE_SHA256";

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(path, text).expect("write fixture");
}

fn wiki_config(root: &Path) -> PathBuf {
    let config = root.join("wiki.json");
    write(
        &config,
        r#"{"version":1,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    );
    config
}

fn wiki_page(citation: &str) -> String {
    format!("---\ntitle: Provenance\nsources:\n  - {citation}\n---\n\nCatalog-backed page.\n")
}

fn structured_page(
    title: &str,
    kind: &str,
    graph_ref: &str,
    parent: &str,
    citations: &[&str],
) -> String {
    format!(
        "---\ntitle: {title}\nkind: {kind}\ngraph_ref: {graph_ref}\nparent: {parent}\ninput_sha256: {}\nsources:\n{}---\n\n# {title}\n",
        "0".repeat(64),
        citations
            .iter()
            .map(|citation| format!("  - {citation}\n"))
            .collect::<String>()
    )
}

fn frontmatter_value(markdown: &str, field: &str) -> Option<String> {
    markdown
        .lines()
        .skip_while(|line| *line != "---")
        .skip(1)
        .take_while(|line| *line != "---")
        .find_map(|line| {
            line.strip_prefix(&format!("{field}: "))
                .map(|value| serde_json::from_str(value).unwrap_or_else(|_| value.to_owned()))
        })
}

fn rendered_page(root: &Path, kind: &str, graph_ref: &str) -> PathBuf {
    let directory = match kind {
        "topic" => "topics",
        "community" => "communities",
        "source" => "sources",
        "inventory" => "inventory",
        other => panic!("unsupported page kind {other}"),
    };
    fs::read_dir(root.join("docs").join(directory))
        .expect("read rendered page directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|extension| extension == "md")
                && fs::read_to_string(path).is_ok_and(|markdown| {
                    frontmatter_value(&markdown, "kind").as_deref() == Some(kind)
                        && frontmatter_value(&markdown, "graph_ref").as_deref() == Some(graph_ref)
                })
        })
        .unwrap_or_else(|| panic!("missing rendered {kind} page {graph_ref}"))
}

fn synthesized_page_path(root: &Path, page: &str) -> PathBuf {
    if let Some(graph_ref) = page
        .strip_prefix("topics/")
        .and_then(|page| page.strip_suffix(".md"))
    {
        return rendered_page(root, "topic", graph_ref);
    }
    if let Some(graph_ref) = page
        .strip_prefix("communities/")
        .and_then(|page| page.strip_suffix(".md"))
    {
        return rendered_page(root, "community", graph_ref);
    }
    root.join("docs").join(page)
}

fn catalog_record(source_id: &str, capture_id: &str, source_path: &str) -> String {
    format!(
        r#"{{"version":1,"entries":[{{"source_id":"{source_id}","capture_id":"{capture_id}","source_path":"{source_path}","sha256":"{SHA256_PLACEHOLDER}","captured_at":"2026-08-24T12:34:56Z","accessed_at":"2026-08-24T12:35:56Z","updated_at":"2026-08-24T12:35:56Z","representation":"markdown","source_system":"sharepoint","url":"https://sharepoint.example.test/sites/team/source","location":"Team/Knowledge/source.md"}}]}}"#,
    )
}

fn initialize_wiki(root: &Path, citation: &str) -> PathBuf {
    let config = wiki_config(root);
    write(&root.join("docs/page.md"), &wiki_page(citation));
    let output = graphoxide(root)
        .args(["wiki", "index"])
        .arg(root)
        .arg("--config")
        .arg(&config)
        .output()
        .expect("index wiki");
    assert_success(&output);
    config
}

fn initialize_structured_wiki(root: &Path, communities: &[i64], source_parent: &str) -> PathBuf {
    let config = wiki_config(root);
    write(
        &root.join("docs/index.md"),
        &structured_page("Root", "root", "root", "root", &[]),
    );
    write(
        &root.join("docs/topics/topic.md"),
        &structured_page("Topic", "topic", "topic", "index.md", &[]),
    );
    for community in communities {
        write(
            &root.join(format!("docs/communities/{community}.md")),
            &structured_page(
                &format!("Community {community}"),
                "community",
                &community.to_string(),
                "topics/topic.md",
                &[],
            ),
        );
    }
    write(
        &root.join("docs/sources/source-one.md"),
        &structured_page(
            "Source",
            "source",
            "source-one",
            source_parent,
            &["source-one#capture-active"],
        ),
    );
    let output = graphoxide(root)
        .args(["wiki", "index"])
        .arg(root)
        .arg("--config")
        .arg(&config)
        .output()
        .expect("index structured wiki");
    assert_success(&output);
    config
}

fn write_catalog(root: &Path, body: &str) {
    let source = "raw source text\n";
    write(&root.join("inputs/source.md"), source);
    let digest = hex::encode(Sha256::digest(source.as_bytes()));
    write(
        &root.join("catalog/catalog.json"),
        &body.replace(SHA256_PLACEHOLDER, &digest),
    );
}

fn v2_catalog(active_sha256: &str) -> serde_json::Value {
    json!({
        "version": 2,
        "sources": [{
            "source_id": "source-one",
            "source_system": "sharepoint",
            "url": "https://example.invalid/site/page",
            "location": "Site/Library/Folder/Page",
            "active_capture_id": "capture-active"
        }],
        "captures": [
            {
                "source_id": "source-one",
                "capture_id": "capture-active",
                "source_path": "raw/active.md",
                "sha256": active_sha256,
                "captured_at": "2026-08-24T12:34:56Z",
                "accessed_at": "2026-08-24T12:35:56Z",
                "updated_at": "2026-08-24T12:35:56Z",
                "representation": "markdown"
            },
            {
                "source_id": "source-one",
                "capture_id": "capture-history",
                "source_path": "raw/history.md",
                "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "captured_at": "2026-08-23T12:34:56Z",
                "accessed_at": "2026-08-23T12:35:56Z",
                "updated_at": "2026-08-23T12:35:56Z",
                "representation": "markdown"
            }
        ]
    })
}

fn v2_catalog_annotation(capture_id: &str, source_path: &str, sha256: &str) -> serde_json::Value {
    json!({
        "source_id": "source-one",
        "capture_id": capture_id,
        "source_path": source_path,
        "sha256": sha256,
        "captured_at": if capture_id == "capture-active" { "2026-08-24T12:34:56Z" } else { "2026-08-23T12:34:56Z" },
        "accessed_at": if capture_id == "capture-active" { "2026-08-24T12:35:56Z" } else { "2026-08-23T12:35:56Z" },
        "updated_at": if capture_id == "capture-active" { "2026-08-24T12:35:56Z" } else { "2026-08-23T12:35:56Z" },
        "representation": "markdown",
        "source_system": "sharepoint",
        "url": "https://example.invalid/site/page",
        "location": "Site/Library/Folder/Page"
    })
}

fn write_v2_catalog(root: &Path, active_sha256: &str) {
    write(
        &root.join("catalog/catalog.json"),
        &serde_json::to_string(&v2_catalog(active_sha256)).expect("serialize v2 catalog"),
    );
}

fn write_graph(path: &Path, catalog: Option<serde_json::Value>) {
    write_graph_communities(path, &[0], catalog);
}

fn write_graph_communities(path: &Path, communities: &[i64], catalog: Option<serde_json::Value>) {
    let nodes = communities
        .iter()
        .map(|community| {
            let mut node = json!({
                "id": format!("active-source-{community}"),
                "label": "Active source",
                "file_type": "document",
                "source_file": "raw/active.md",
                "community": community,
                "community_name": format!("Community {community}")
            });
            if let Some(catalog) = &catalog {
                node["catalog"] = catalog.clone();
            }
            node
        })
        .collect::<Vec<_>>();
    write(
        path,
        &serde_json::to_string(&json!({"nodes": nodes, "links": []})).expect("serialize graph"),
    );
}

struct SynthesizedFixture {
    _temporary: tempfile::TempDir,
    wiki_root: PathBuf,
    config: PathBuf,
    graph: PathBuf,
}

fn synthesized_fixture() -> SynthesizedFixture {
    let temporary = tempfile::tempdir().expect("fixture");
    let wiki_root = temporary.path().join("wiki");
    let graph = temporary.path().join("graph.json");
    let captures = [
        ("source-one", "capture-one", "raw/one.md", 0_i64),
        ("source-two", "capture-two", "raw/two.md", 0_i64),
        ("source-three", "capture-three", "raw/three.md", 1_i64),
    ];
    let digest = "a".repeat(64);
    let sources = captures
        .iter()
        .map(|(source_id, capture_id, _, _)| {
            json!({
                "source_id": source_id,
                "source_system": "sharepoint",
                "url": format!("https://example.invalid/{source_id}"),
                "location": format!("Site/{source_id}"),
                "active_capture_id": capture_id
            })
        })
        .chain(std::iter::once(json!({
            "source_id": "source-inventory",
            "source_system": "sharepoint",
            "url": "https://example.invalid/source-inventory",
            "location": "Site/source-inventory",
            "active_capture_id": "capture-inventory"
        })))
        .collect::<Vec<_>>();
    let capture_records = captures
        .iter()
        .map(|(source_id, capture_id, source_path, _)| {
            json!({
                "source_id": source_id,
                "capture_id": capture_id,
                "source_path": source_path,
                "sha256": digest,
                "captured_at": "2026-08-24T12:34:56Z",
                "accessed_at": "2026-08-24T12:35:56Z",
                "updated_at": "2026-08-24T12:35:56Z",
                "representation": "markdown"
            })
        })
        .chain(std::iter::once(json!({
            "source_id": "source-inventory",
            "capture_id": "capture-inventory",
            "source_path": "raw/inventory.md",
            "sha256": digest,
            "captured_at": "2026-08-24T12:34:56Z",
            "accessed_at": "2026-08-24T12:35:56Z",
            "updated_at": "2026-08-24T12:35:56Z",
            "representation": "markdown"
        })))
        .collect::<Vec<_>>();
    write(
        &wiki_root.join("catalog/catalog.json"),
        &serde_json::to_string(&json!({
            "version": 2,
            "sources": sources,
            "captures": capture_records
        }))
        .expect("serialize catalog"),
    );
    let nodes = captures
        .iter()
        .map(|(source_id, capture_id, source_path, community)| {
            json!({
                "id": format!("node-{source_id}"),
                "label": source_id,
                "file_type": "document",
                "source_file": source_path,
                "community": community,
                "catalog": {
                    "source_id": source_id,
                    "capture_id": capture_id,
                    "source_path": source_path,
                    "sha256": digest,
                    "captured_at": "2026-08-24T12:34:56Z",
                    "accessed_at": "2026-08-24T12:35:56Z",
                    "updated_at": "2026-08-24T12:35:56Z",
                    "representation": "markdown",
                    "source_system": "sharepoint",
                    "url": format!("https://example.invalid/{source_id}"),
                    "location": format!("Site/{source_id}")
                }
            })
        })
        .collect::<Vec<_>>();
    write(
        &graph,
        &serde_json::to_string(&json!({"nodes": nodes, "links": []})).expect("serialize graph"),
    );
    let catalog =
        Catalog::load_metadata(&wiki_root, Path::new("catalog")).expect("load synthesized catalog");
    let graph_value: KnowledgeGraph =
        serde_json::from_slice(&fs::read(&graph).expect("read graph")).expect("parse graph");
    let plan = render_structured_wiki_with_catalog(
        &graph_value,
        &derive_topic_tree(&graph_value).expect("derive topics"),
        &catalog.active_annotations(),
    )
    .expect("render wiki");
    for page in plan.pages {
        write(&wiki_root.join("docs").join(page.path), &page.markdown);
    }
    let config = wiki_config(&wiki_root);
    let fixture = SynthesizedFixture {
        _temporary: temporary,
        wiki_root,
        config,
        graph,
    };
    index_synthesized(&fixture);
    fixture
}

fn replace_sources(markdown: &str, citations: &[&str]) -> String {
    let start = markdown.find("\nsources:\n").expect("sources") + "\nsources:\n".len();
    let end = start + markdown[start..].find("---\n\n").expect("frontmatter end");
    let mut updated = markdown.to_owned();
    updated.replace_range(
        start..end,
        &citations
            .iter()
            .map(|citation| format!("  - {citation}\n"))
            .collect::<String>(),
    );
    updated
}

fn synthesized_page(markdown: &str, citations: &[&str]) -> String {
    let mut page = replace_sources(markdown, citations).replacen(
        "sources:\n",
        &format!(
            "draft: true\ndraft_model: \"qwen-test\"\nevidence_sha256: \"{}\"\nsources:\n",
            "b".repeat(64)
        ),
        1,
    );
    if !page.contains("\n## Sources\n") {
        page.push_str("\n## Sources\n\n");
        for citation in citations {
            page.push_str(&format!("- `{citation}`\n"));
        }
    }
    let insertion = page.find("\n## Sources\n").expect("sources section");
    page.insert_str(
        insertion,
        "\n<!-- graphoxide-draft -->\n\nSynthesized model body.\n",
    );
    page
}

fn synthesized_page_with_body(markdown: &str, citations: &[&str], body: &str) -> String {
    synthesized_page(markdown, citations).replacen("Synthesized model body.", body, 1)
}

fn index_synthesized(fixture: &SynthesizedFixture) {
    assert_success(
        &graphoxide(&fixture.wiki_root)
            .args(["wiki", "index", ".", "--config", "wiki.json"])
            .output()
            .expect("index synthesized wiki"),
    );
}

fn check_synthesized(fixture: &SynthesizedFixture) -> Output {
    check_with_graph_catalog(
        fixture._temporary.path(),
        &fixture.wiki_root,
        &fixture.config,
        &fixture.graph,
    )
}

fn reject_synthesized_edit(page: &str, edit: impl FnOnce(String) -> String, expected_error: &str) {
    let fixture = synthesized_fixture();
    let path = synthesized_page_path(&fixture.wiki_root, page);
    let updated = edit(fs::read_to_string(&path).expect("read page"));
    write(&path, &updated);
    let output = check_synthesized(&fixture);
    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains(expected_error),
        "expected {expected_error:?}: {}",
        output_text(&output)
    );
}

fn graphoxide(current_directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command.current_dir(current_directory);
    command
}

fn check_with_catalog(
    current_directory: &Path,
    wiki_root: &Path,
    config: &Path,
    raw_root: &Path,
) -> Output {
    graphoxide(current_directory)
        .args(["wiki", "check"])
        .arg(wiki_root)
        .arg("--config")
        .arg(config)
        .args(["--catalog", "catalog", "--catalog-root"])
        .arg(raw_root)
        .output()
        .expect("check catalog-backed wiki")
}

fn check_with_graph_catalog(
    current_directory: &Path,
    wiki_root: &Path,
    config: &Path,
    graph: &Path,
) -> Output {
    graphoxide(current_directory)
        .args(["wiki", "check"])
        .arg(wiki_root)
        .arg("--config")
        .arg(config)
        .args(["--catalog", "catalog", "--graph"])
        .arg(graph)
        .output()
        .expect("check graph-backed structured wiki")
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", output_text(output));
}

#[test]
fn wiki_check_accepts_a_catalog_from_a_separate_raw_project_root() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let raw_root = fixture.path().join("raw");
    let config = initialize_wiki(&wiki_root, "source-one#capture-one");
    write_catalog(
        &raw_root,
        &catalog_record("source-one", "capture-one", "inputs/source.md"),
    );

    let output = check_with_catalog(fixture.path(), &wiki_root, &config, &raw_root);

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Checked 1 wiki pages\n"
    );
}

#[test]
fn wiki_check_rejects_a_catalog_with_a_mismatched_raw_source_before_accepting_citations() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let raw_root = fixture.path().join("raw");
    let config = initialize_wiki(&wiki_root, "source-one#capture-one");
    write_catalog(
        &raw_root,
        &catalog_record("source-one", "capture-one", "inputs/source.md"),
    );
    write(
        &raw_root.join("inputs/source.md"),
        "raw source changed after capture\n",
    );

    let output = check_with_catalog(fixture.path(), &wiki_root, &config, &raw_root);

    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("catalog sha256 does not match source_path"),
        "{}",
        output_text(&output)
    );
    assert!(
        !output_text(&output).contains("unknown citation"),
        "catalog provenance must be verified before citation acceptance: {}",
        output_text(&output)
    );
}

#[test]
fn wiki_check_with_a_v1_catalog_rehashes_sources_under_the_wiki_root() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let config = initialize_wiki(&wiki_root, "source-one#capture-one");
    write_catalog(
        &wiki_root,
        &catalog_record("source-one", "capture-one", "inputs/source.md"),
    );
    write(
        &wiki_root.join("inputs/source.md"),
        "raw source changed after capture\n",
    );

    let output = graphoxide(fixture.path())
        .args(["wiki", "check"])
        .arg(&wiki_root)
        .arg("--config")
        .arg(&config)
        .args(["--catalog", "catalog"])
        .output()
        .expect("check v1 catalog under wiki root");

    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("catalog sha256 does not match source_path"),
        "{}",
        output_text(&output)
    );
}

#[test]
fn wiki_check_reports_unknown_source_and_capture_citations() {
    for citation in ["source-missing#capture-one", "source-one#capture-missing"] {
        let fixture = tempfile::tempdir().expect("fixture");
        let wiki_root = fixture.path().join("wiki");
        let raw_root = fixture.path().join("raw");
        let config = initialize_wiki(&wiki_root, citation);
        write_catalog(
            &raw_root,
            &catalog_record("source-one", "capture-one", "inputs/source.md"),
        );

        let output = check_with_catalog(fixture.path(), &wiki_root, &config, &raw_root);

        assert!(!output.status.success(), "{}", output_text(&output));
        assert!(
            output_text(&output).contains(&format!(
                "wiki page docs/page.md references unknown citation {citation}"
            )),
            "{}",
            output_text(&output)
        );
    }
}

#[test]
fn wiki_check_rejects_a_catalog_record_that_cannot_be_cited() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let raw_root = fixture.path().join("raw");
    let config = initialize_wiki(&wiki_root, "source-one#capture-one");
    write_catalog(
        &raw_root,
        &catalog_record("-not-a-source-id", "capture-one", "inputs/source.md"),
    );

    let output = check_with_catalog(fixture.path(), &wiki_root, &config, &raw_root);

    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("catalog IDs must be bounded wiki-reference identifiers"),
        "{}",
        output_text(&output)
    );
}

#[test]
fn wiki_check_rejects_an_invalid_catalog_before_validating_pages() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let raw_root = fixture.path().join("raw");
    let config = initialize_wiki(&wiki_root, "source-one#capture-one");
    write(&wiki_root.join("docs/page.md"), "---\ntitle: Broken\n");
    write_catalog(&raw_root, r#"{"version":3,"entries":[]}"#);

    let output = check_with_catalog(fixture.path(), &wiki_root, &config, &raw_root);

    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("unsupported catalog version"),
        "{}",
        output_text(&output)
    );
    assert!(
        !output_text(&output).contains("wiki page"),
        "catalog validation must precede malformed wiki validation: {}",
        output_text(&output)
    );
}

#[test]
fn wiki_check_rejects_a_catalog_entry_for_a_missing_raw_source_file() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let raw_root = fixture.path().join("raw");
    let config = initialize_wiki(&wiki_root, "source-one#capture-one");
    write_catalog(
        &raw_root,
        &catalog_record("source-one", "capture-one", "inputs/missing.md"),
    );

    let output = check_with_catalog(fixture.path(), &wiki_root, &config, &raw_root);

    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("resolve catalog source_path inputs/missing.md"),
        "{}",
        output_text(&output)
    );
}

#[test]
fn wiki_check_without_catalog_needs_only_the_small_wiki_checkout() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let config = initialize_wiki(&wiki_root, "source-one#capture-one");

    let output = graphoxide(fixture.path())
        .args(["wiki", "check"])
        .arg(&wiki_root)
        .arg("--config")
        .arg(&config)
        .output()
        .expect("check wiki without catalog");

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Checked 1 wiki pages\n"
    );
}

#[test]
fn wiki_check_accepts_v2_historical_citations_from_metadata_without_raw_files() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let config = initialize_wiki(&wiki_root, "source-one#capture-history");
    write_v2_catalog(
        &wiki_root,
        &hex::encode(Sha256::digest(b"active source text\n")),
    );

    let output = graphoxide(fixture.path())
        .args(["wiki", "check"])
        .arg(&wiki_root)
        .arg("--config")
        .arg(&config)
        .args(["--catalog", "catalog"])
        .output()
        .expect("check metadata-only v2 catalog");

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Checked 1 wiki pages\n"
    );
}

#[test]
fn wiki_check_accepts_an_active_v2_catalog_annotation_on_a_graph() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let config = initialize_wiki(&wiki_root, "source-one#capture-history");
    let active = "active source text\n";
    let active_sha256 = hex::encode(Sha256::digest(active.as_bytes()));
    write(&wiki_root.join("raw/active.md"), active);
    write_v2_catalog(&wiki_root, &active_sha256);
    let graph = fixture.path().join("graph.json");
    write_graph(
        &graph,
        Some(v2_catalog_annotation(
            "capture-active",
            "raw/active.md",
            &active_sha256,
        )),
    );

    let output = graphoxide(fixture.path())
        .args(["wiki", "check"])
        .arg(&wiki_root)
        .arg("--config")
        .arg(&config)
        .args(["--catalog", "catalog", "--graph"])
        .arg(&graph)
        .output()
        .expect("check v2 catalog graph annotation");

    assert_success(&output);
}

#[test]
fn wiki_check_accepts_exact_v2_catalog_graph_pages_without_active_source_bytes() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let active_sha256 = hex::encode(Sha256::digest(b"active source text\n"));
    write_v2_catalog(&wiki_root, &active_sha256);
    let graph_path = wiki_root.join("graph.json");
    write_graph(
        &graph_path,
        Some(v2_catalog_annotation(
            "capture-active",
            "raw/active.md",
            &active_sha256,
        )),
    );

    assert!(!wiki_root.join("raw/active.md").exists());
    assert!(!wiki_root.join("raw/history.md").exists());
    let catalog = Catalog::load_metadata(&wiki_root, Path::new("catalog"))
        .expect("load metadata-only v2 catalog");
    assert_eq!(catalog.version(), 2);
    let graph: KnowledgeGraph =
        serde_json::from_slice(&fs::read(&graph_path).expect("read graph")).expect("parse graph");
    let plan = render_structured_wiki_with_catalog(
        &graph,
        &derive_topic_tree(&graph).expect("derive graph topics"),
        &catalog.active_annotations(),
    )
    .expect("render exact catalog-aware wiki pages");
    let config = wiki_config(&wiki_root);
    for page in plan.pages {
        write(&wiki_root.join("docs").join(page.path), &page.markdown);
    }
    assert_success(
        &graphoxide(fixture.path())
            .args(["wiki", "index"])
            .arg(&wiki_root)
            .arg("--config")
            .arg(&config)
            .output()
            .expect("index exact catalog-aware wiki"),
    );

    let output = check_with_graph_catalog(fixture.path(), &wiki_root, &config, &graph_path);

    assert_success(&output);
}

#[test]
fn wiki_check_requires_graph_represented_source_and_community_pages() {
    for missing in ["source", "community"] {
        let fixture = tempfile::tempdir().expect("fixture");
        let wiki_root = fixture.path().join("wiki");
        let config = wiki_config(&wiki_root);
        let active = "active source text\n";
        let active_sha256 = hex::encode(Sha256::digest(active.as_bytes()));
        write(&wiki_root.join("raw/active.md"), active);
        write_v2_catalog(&wiki_root, &active_sha256);
        write(
            &wiki_root.join("docs/index.md"),
            &structured_page("Root", "root", "root", "root", &[]),
        );
        write(
            &wiki_root.join("docs/topics/topic.md"),
            &structured_page("Topic", "topic", "topic", "index.md", &[]),
        );
        if missing != "community" {
            write(
                &wiki_root.join("docs/communities/0.md"),
                &structured_page("Community", "community", "0", "topics/topic.md", &[]),
            );
        }
        if missing != "source" {
            write(
                &wiki_root.join("docs/sources/source-one.md"),
                &structured_page(
                    "Source",
                    "source",
                    "source-one",
                    if missing == "community" {
                        "index.md"
                    } else {
                        "communities/0.md"
                    },
                    &["source-one#capture-active"],
                ),
            );
        }
        let index = graphoxide(fixture.path())
            .args(["wiki", "index"])
            .arg(&wiki_root)
            .arg("--config")
            .arg(&config)
            .output()
            .expect("index structured wiki");
        assert_success(&index);
        let graph = fixture.path().join("graph.json");
        write_graph(
            &graph,
            Some(v2_catalog_annotation(
                "capture-active",
                "raw/active.md",
                &active_sha256,
            )),
        );

        let output = graphoxide(fixture.path())
            .args(["wiki", "check"])
            .arg(&wiki_root)
            .arg("--config")
            .arg(&config)
            .args(["--catalog", "catalog", "--graph"])
            .arg(&graph)
            .output()
            .expect("check structured graph coverage");

        assert!(
            !output.status.success(),
            "{missing}: {}",
            output_text(&output)
        );
        assert!(
            output_text(&output).contains("catalog page"),
            "{missing}: {}",
            output_text(&output)
        );
    }
}

#[test]
fn wiki_check_rejects_invented_graph_source_and_community_pages() {
    for kind in ["source", "community"] {
        let fixture = tempfile::tempdir().expect("fixture");
        let wiki_root = fixture.path().join("wiki");
        let active = "active source text\n";
        let active_sha256 = hex::encode(Sha256::digest(active.as_bytes()));
        write(&wiki_root.join("raw/active.md"), active);
        write_v2_catalog(&wiki_root, &active_sha256);
        let config = initialize_structured_wiki(&wiki_root, &[0], "communities/0.md");
        let (path, page) = if kind == "source" {
            (
                "docs/sources/stale.md",
                structured_page("Stale", "source", "source-stale", "index.md", &[]),
            )
        } else {
            (
                "docs/communities/99.md",
                structured_page("Stale", "community", "99", "topics/topic.md", &[]),
            )
        };
        write(&wiki_root.join(path), &page);
        assert_success(
            &graphoxide(&wiki_root)
                .args(["wiki", "index", ".", "--config", "wiki.json"])
                .output()
                .expect("reindex structured wiki"),
        );
        let graph = fixture.path().join("graph.json");
        write_graph(
            &graph,
            Some(v2_catalog_annotation(
                "capture-active",
                "raw/active.md",
                &active_sha256,
            )),
        );

        let output = check_with_graph_catalog(fixture.path(), &wiki_root, &config, &graph);

        assert!(!output.status.success(), "{kind}: {}", output_text(&output));
        assert!(
            output_text(&output).contains("catalog page"),
            "{kind}: {}",
            output_text(&output)
        );
    }
}

#[test]
fn wiki_check_rejects_a_source_outside_its_primary_graph_community() {
    for parent in ["index.md", "communities/1.md"] {
        let fixture = tempfile::tempdir().expect("fixture");
        let wiki_root = fixture.path().join("wiki");
        let active = "active source text\n";
        let active_sha256 = hex::encode(Sha256::digest(active.as_bytes()));
        write(&wiki_root.join("raw/active.md"), active);
        write_v2_catalog(&wiki_root, &active_sha256);
        let config = initialize_structured_wiki(&wiki_root, &[0, 1], parent);
        let graph = fixture.path().join("graph.json");
        write_graph_communities(
            &graph,
            &[0, 1],
            Some(v2_catalog_annotation(
                "capture-active",
                "raw/active.md",
                &active_sha256,
            )),
        );

        let output = check_with_graph_catalog(fixture.path(), &wiki_root, &config, &graph);

        assert!(
            !output.status.success(),
            "{parent}: {}",
            output_text(&output)
        );
        assert!(
            output_text(&output).contains("catalog page"),
            "{parent}: {}",
            output_text(&output)
        );
    }
}

fn assert_invalid_v2_graph_annotation(name: &str, annotation: Option<serde_json::Value>) {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let config = initialize_wiki(&wiki_root, "source-one#capture-history");
    let active = "active source text\n";
    let active_sha256 = hex::encode(Sha256::digest(active.as_bytes()));
    write(&wiki_root.join("raw/active.md"), active);
    write_v2_catalog(&wiki_root, &active_sha256);
    let graph = fixture.path().join("graph.json");
    write_graph(&graph, annotation);

    let output = graphoxide(fixture.path())
        .args(["wiki", "check"])
        .arg(&wiki_root)
        .arg("--config")
        .arg(&config)
        .args(["--catalog", "catalog", "--graph"])
        .arg(&graph)
        .output()
        .expect("reject invalid v2 catalog graph annotation");

    assert!(!output.status.success(), "{name}: {}", output_text(&output));
    assert!(
        output_text(&output).contains("catalog graph annotation"),
        "{name} must fail catalog annotation validation: {}",
        output_text(&output)
    );
}

#[test]
fn wiki_check_rejects_a_graph_without_a_catalog() {
    let fixture = tempfile::tempdir().expect("fixture");
    let wiki_root = fixture.path().join("wiki");
    let config = initialize_wiki(&wiki_root, "source-one#capture-history");
    let graph = fixture.path().join("graph.json");
    write_graph(&graph, None);

    let output = graphoxide(fixture.path())
        .args(["wiki", "check"])
        .arg(&wiki_root)
        .arg("--config")
        .arg(&config)
        .arg("--graph")
        .arg(&graph)
        .output()
        .expect("reject graph validation without catalog");

    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("required arguments were not provided")
            && output_text(&output).contains("--catalog"),
        "{}",
        output_text(&output)
    );
}

#[test]
fn wiki_check_rejects_a_graph_with_a_missing_v2_catalog_annotation() {
    assert_invalid_v2_graph_annotation("missing", None);
}

#[test]
fn wiki_check_rejects_a_graph_with_a_stale_v2_catalog_annotation() {
    assert_invalid_v2_graph_annotation(
        "stale",
        Some(v2_catalog_annotation(
            "capture-active",
            "raw/active.md",
            &"a".repeat(64),
        )),
    );
}

#[test]
fn wiki_check_rejects_a_graph_with_a_historical_v2_catalog_annotation() {
    assert_invalid_v2_graph_annotation(
        "historical",
        Some(v2_catalog_annotation(
            "capture-history",
            "raw/history.md",
            &"b".repeat(64),
        )),
    );
}

#[test]
fn synthesized_draft_accepts_source_community_and_topic_citations() {
    let fixture = synthesized_fixture();
    for (path, citations) in [
        (
            fixture.wiki_root.join("docs/sources/source-one.md"),
            &["source-one#capture-one"][..],
        ),
        (
            rendered_page(&fixture.wiki_root, "community", "0"),
            &["source-one#capture-one", "source-two#capture-two"][..],
        ),
        (
            rendered_page(&fixture.wiki_root, "topic", "topic-0"),
            &["source-two#capture-two"][..],
        ),
    ] {
        let markdown = fs::read_to_string(&path).expect("read page");
        write(&path, &synthesized_page(&markdown, citations));
    }
    index_synthesized(&fixture);

    assert_success(&check_synthesized(&fixture));
}

#[test]
fn synthesized_draft_accepts_legacy_community_digest_and_citations() {
    let fixture = synthesized_fixture();
    let path = rendered_page(&fixture.wiki_root, "community", "0");
    let mut page = replace_sources(
        &fs::read_to_string(&path).expect("read community"),
        &["source-two#capture-two"],
    );
    let start = page.find("input_sha256: \"").expect("input digest") + "input_sha256: \"".len();
    page.replace_range(start..start + 64, &"c".repeat(64));
    let insertion = page.find("\n## Sources\n").expect("sources section");
    page.insert_str(
        insertion,
        "\n<!-- graphoxide-draft -->\n\nLegacy model body.\n",
    );
    write(&path, &page);
    index_synthesized(&fixture);

    assert_success(&check_synthesized(&fixture));
}

#[test]
fn synthesized_draft_rejects_partial_metadata() {
    reject_synthesized_edit(
        "sources/source-one.md",
        |markdown| {
            synthesized_page(&markdown, &["source-one#capture-one"]).replacen(
                &format!("evidence_sha256: \"{}\"\n", "b".repeat(64)),
                "",
                1,
            )
        },
        "draft metadata",
    );
}

#[test]
fn synthesized_draft_rejects_false_draft_marker() {
    reject_synthesized_edit(
        "sources/source-one.md",
        |markdown| {
            synthesized_page(&markdown, &["source-one#capture-one"]).replacen(
                "draft: true",
                "draft: false",
                1,
            )
        },
        "draft must be true",
    );
}

#[test]
fn synthesized_draft_rejects_malformed_evidence_digest() {
    reject_synthesized_edit(
        "sources/source-one.md",
        |markdown| {
            synthesized_page(&markdown, &["source-one#capture-one"]).replacen(
                &format!("evidence_sha256: \"{}\"", "b".repeat(64)),
                "evidence_sha256: \"not-a-digest\"",
                1,
            )
        },
        "invalid evidence_sha256",
    );
}

#[test]
fn synthesized_draft_rejects_unsafe_or_oversized_model_identifiers() {
    for model in [format!(" {}", "model"), "m".repeat(257)] {
        reject_synthesized_edit(
            "sources/source-one.md",
            |markdown| {
                synthesized_page(&markdown, &["source-one#capture-one"]).replacen(
                    "draft_model: \"qwen-test\"",
                    &format!(
                        "draft_model: {}",
                        serde_json::to_string(&model).expect("quote model")
                    ),
                    1,
                )
            },
            "invalid draft_model",
        );
    }
}

#[test]
fn synthesized_draft_rejects_source_citation_outside_page_ownership() {
    reject_synthesized_edit(
        "sources/source-one.md",
        |markdown| synthesized_page(&markdown, &["source-two#capture-two"]),
        "citations",
    );
}

#[test]
fn synthesized_draft_rejects_topic_citation_outside_topic_membership() {
    reject_synthesized_edit(
        "topics/topic-0.md",
        |markdown| synthesized_page(&markdown, &["source-three#capture-three"]),
        "draft citations",
    );
}

#[test]
fn synthesized_draft_rejects_empty_or_nondeterministic_topic_citations() {
    for citations in [
        &[][..],
        &["source-two#capture-two", "source-one#capture-one"][..],
    ] {
        reject_synthesized_edit(
            "topics/topic-0.md",
            |markdown| synthesized_page(&markdown, citations),
            "draft citations",
        );
    }
}

#[test]
fn synthesized_draft_rejects_stale_structural_input_digest() {
    reject_synthesized_edit(
        "sources/source-one.md",
        |markdown| {
            let page = synthesized_page(&markdown, &["source-one#capture-one"]);
            let start =
                page.find("input_sha256: \"").expect("input digest") + "input_sha256: \"".len();
            let mut updated = page;
            updated.replace_range(start..start + 64, &"0".repeat(64));
            updated
        },
        "stale input digest",
    );
}

#[test]
fn synthesized_draft_rejects_renderer_owned_title_change() {
    let fixture = synthesized_fixture();
    let path = fixture.wiki_root.join("docs/sources/source-one.md");
    let page = synthesized_page(
        &fs::read_to_string(&path).expect("read source page"),
        &["source-one#capture-one"],
    );
    let title = page
        .lines()
        .find(|line| line.starts_with("title: "))
        .expect("rendered title");
    let page = page.replacen(title, "title: \"Forged title\"", 1);
    write(&path, &page);
    index_synthesized(&fixture);

    let output = check_synthesized(&fixture);
    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("stale title"),
        "{}",
        output_text(&output)
    );
}

#[test]
fn synthesized_draft_rejects_root_and_inventory_pages() {
    for (page, citations) in [
        ("index.md", &[][..]),
        (
            "inventory/source-inventory.md",
            &["source-inventory#capture-inventory"][..],
        ),
    ] {
        reject_synthesized_edit(
            page,
            |markdown| synthesized_page(&markdown, citations),
            "must not be draft",
        );
    }
}

#[test]
fn synthesized_draft_rejects_model_body_link_that_escapes_the_wiki() {
    reject_synthesized_edit(
        "sources/source-one.md",
        |markdown| {
            synthesized_page_with_body(
                &markdown,
                &["source-one#capture-one"],
                "[escape](../../escape.md)",
            )
        },
        "invalid model Markdown body",
    );
}

#[test]
fn draft_unsafe_link_offline_check_rejects_all_model_markdown_links_and_html() {
    for body in [
        "[inline](https://example.invalid)",
        "[outer [inner]](javascript:alert(1))",
        "[multiline\nlabel](javascript:alert(1))",
        "[reference][target]\n\n[target]: https://example.invalid",
        "<https://example.invalid>",
        "<a href=\"https://example.invalid\">raw HTML</a>",
    ] {
        reject_synthesized_edit(
            "sources/source-one.md",
            |markdown| synthesized_page_with_body(&markdown, &["source-one#capture-one"], body),
            "invalid model Markdown body",
        );
    }
}

#[test]
fn draft_unsafe_link_offline_check_rejects_legacy_model_markdown() {
    let fixture = synthesized_fixture();
    let path = rendered_page(&fixture.wiki_root, "community", "0");
    let markdown = fs::read_to_string(&path).expect("read community");
    let mut page = replace_sources(&markdown, &["source-one#capture-one"]);
    let insertion = page.find("\n## Sources\n").expect("sources section");
    page.insert_str(
        insertion,
        "\n<!-- graphoxide-draft -->\n\n[reference][target]\n\n[target]: data:text/html,bad\n",
    );
    write(&path, &page);

    let output = check_synthesized(&fixture);
    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("invalid model Markdown body"),
        "{}",
        output_text(&output)
    );
}

#[test]
fn draft_model_markdown_code_examples_remain_inert_offline() {
    let fixture = synthesized_fixture();
    let path = fixture.wiki_root.join("docs/sources/source-one.md");
    let markdown = fs::read_to_string(&path).expect("read source");
    write(
        &path,
        &synthesized_page_with_body(
            &markdown,
            &["source-one#capture-one"],
            "Literal `[outer [inner]](javascript:alert(1))` and `<tag>`.\n\n```markdown\nForged title\n============\n\nSources\n-------\n\n[multiline\nlabel](javascript:alert(1))\n[reference][target]\n[target]: data:text/html,bad\n<a href=\"file:///tmp/x\">x</a>\n```",
        ),
    );
    index_synthesized(&fixture);

    assert_success(&check_synthesized(&fixture));
}

#[test]
fn draft_setext_model_headings_fail_offline_check() {
    for body in [
        "Forged title\n============\n\nBody.",
        "Body.\n\nSources\n-------\n\nForged.",
    ] {
        reject_synthesized_edit(
            "sources/source-one.md",
            |markdown| synthesized_page_with_body(&markdown, &["source-one#capture-one"], body),
            "invalid model Markdown body",
        );
    }
}

#[test]
fn draft_unsafe_link_shared_boundary_rejects_non_http_external_destinations() {
    for destination in [
        "javascript:alert(1)",
        "javascript%3Aalert(1)",
        "data:text/html,bad",
        "file:///etc/passwd",
        "//example.invalid/path",
        "/etc/passwd",
        "C:/Windows/system.ini",
        "../../escape.md",
        "..\\escape.md",
    ] {
        let fixture = synthesized_fixture();
        let path = fixture.wiki_root.join("docs/index.md");
        let mut page = fs::read_to_string(&path).expect("read root");
        page.push_str(&format!("\n[untrusted destination]({destination})\n"));
        write(&path, &page);

        let output = graphoxide(&fixture.wiki_root)
            .args(["wiki", "index", ".", "--config", "wiki.json"])
            .output()
            .expect("index unsafe destination");
        assert!(
            !output.status.success(),
            "destination {destination:?} was accepted: {}",
            output_text(&output)
        );
        assert!(
            output_text(&output).contains("unsafe wiki link"),
            "destination {destination:?}: {}",
            output_text(&output)
        );
    }
}

#[test]
fn draft_unsafe_link_shared_boundary_rejects_nested_and_multiline_inline_links() {
    for markdown in [
        "[outer [inner]](javascript:alert(1))",
        "[multiline\nlabel](javascript:alert(1))",
    ] {
        let fixture = synthesized_fixture();
        let path = fixture.wiki_root.join("docs/index.md");
        let mut page = fs::read_to_string(&path).expect("read root");
        page.push_str(&format!("\n{markdown}\n"));
        write(&path, &page);

        let output = graphoxide(&fixture.wiki_root)
            .args(["wiki", "index", ".", "--config", "wiki.json"])
            .output()
            .expect("index structurally unsafe destination");
        assert!(
            !output.status.success(),
            "Markdown {markdown:?} was accepted: {}",
            output_text(&output)
        );
        assert!(
            output_text(&output).contains("unsafe wiki link"),
            "Markdown {markdown:?}: {}",
            output_text(&output)
        );
    }
}

#[test]
fn generated_http_and_https_destinations_remain_valid() {
    let fixture = synthesized_fixture();
    let path = fixture.wiki_root.join("docs/index.md");
    let mut page = fs::read_to_string(&path).expect("read root");
    page.push_str(
        "\n[HTTP](http://example.invalid/reference) [HTTPS](https://example.invalid/reference)\n",
    );
    write(&path, &page);

    index_synthesized(&fixture);
    assert_success(&check_synthesized(&fixture));
}
