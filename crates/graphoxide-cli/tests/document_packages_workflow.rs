use serde_json::{json, Value};
use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

const NS_CONTENT_TYPES: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const NS_PACKAGE_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const NS_OFFICE_RELS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const NS_WORD: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const NS_EPUB_CONTAINER: &str = "urn:oasis:names:tc:opendocument:xmlns:container";
const NS_OPF: &str = "http://www.idpf.org/2007/opf";
const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
const NS_XHTML: &str = "http://www.w3.org/1999/xhtml";

fn graphoxide(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .current_dir(project)
        .env_remove("GRAPHOXIDE_FORCE")
        .env_remove("GRAPHIFY_FORCE")
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT");
    command
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) -> Value {
    assert!(output.status.success(), "{}", output_text(output));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON object: {error}\n{}",
            output_text(output)
        )
    })
}

fn managed(project: &Path, name: &str) -> PathBuf {
    project.join("graphoxide-out").join(name)
}

fn artifact_bytes(project: &Path) -> [Vec<u8>; 3] {
    [
        fs::read(managed(project, "graph.json")).expect("graph"),
        fs::read(managed(project, "manifest.json")).expect("manifest"),
        fs::read(managed(project, "coverage.json")).expect("coverage"),
    ]
}

fn graph(project: &Path) -> Value {
    serde_json::from_slice(&fs::read(managed(project, "graph.json")).expect("graph bytes"))
        .expect("graph JSON")
}

fn zip_bytes(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, value) in entries {
        writer.start_file(name, options).expect("start ZIP member");
        writer.write_all(&value).expect("write ZIP member");
    }
    writer.finish().expect("finish ZIP").into_inner()
}

fn document_package_zip(mimetype: &str, entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    writer
        .start_file(
            "mimetype",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .expect("start stored mimetype member");
    writer
        .write_all(mimetype.as_bytes())
        .expect("write mimetype member");
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, value) in entries {
        writer.start_file(name, options).expect("start ZIP member");
        writer.write_all(&value).expect("write ZIP member");
    }
    writer.finish().expect("finish ZIP").into_inner()
}

fn content_types() -> String {
    format!(
        r#"<Types xmlns="{NS_CONTENT_TYPES}"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#
    )
}

fn package_relationships() -> String {
    format!(
        r#"<Relationships xmlns="{NS_PACKAGE_RELS}"><Relationship Id="rIdRoot" Type="{NS_OFFICE_RELS}/officeDocument" Target="word/document.xml"/></Relationships>"#
    )
}

fn docx_with_sections(sections: &[&str]) -> Vec<u8> {
    let mut body = String::new();
    for (index, text) in sections.iter().enumerate() {
        body.push_str(&format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>"));
        if index + 1 < sections.len() {
            body.push_str("<w:sectPr/>");
        }
    }
    let document =
        format!(r#"<w:document xmlns:w="{NS_WORD}"><w:body>{body}</w:body></w:document>"#);
    let document_relationships = format!(
        r#"<Relationships xmlns="{NS_PACKAGE_RELS}"><Relationship Id="rIdTheme" Type="{NS_OFFICE_RELS}/theme" Target="media/shared.png"/><Relationship Id="rIdImage" Type="{NS_OFFICE_RELS}/image" Target="media/shared.png"/></Relationships>"#
    );
    zip_bytes(vec![
        ("[Content_Types].xml".into(), content_types().into_bytes()),
        ("_rels/.rels".into(), package_relationships().into_bytes()),
        ("word/document.xml".into(), document.into_bytes()),
        (
            "word/_rels/document.xml.rels".into(),
            document_relationships.into_bytes(),
        ),
        ("word/media/shared.png".into(), b"inert fixture".to_vec()),
    ])
}

fn epub_with_duplicate_labels_and_self_link() -> Vec<u8> {
    let container = format!(
        r#"<container xmlns="{NS_EPUB_CONTAINER}" version="1.0"><rootfiles><rootfile full-path="EPUB/package.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
    );
    let opf = format!(
        r#"<package xmlns="{NS_OPF}" version="3.0" unique-identifier="book-id"><metadata xmlns:dc="{NS_DC}"><dc:identifier id="book-id">urn:uuid:loop-test</dc:identifier><dc:title>Loop-safe publication</dc:title></metadata><manifest><item id="one" href="one.xhtml" media-type="application/xhtml+xml"/><item id="two" href="two.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="one"/><itemref idref="two"/></spine></package>"#
    );
    let first = format!(
        r##"<html xmlns="{NS_XHTML}"><head><title>Repeated chapter</title></head><body><p id="self">First EPUB unit</p><a href="#self">Same-unit link</a></body></html>"##
    );
    let second = format!(
        r#"<html xmlns="{NS_XHTML}"><head><title>Repeated chapter</title></head><body><p>Second EPUB unit</p></body></html>"#
    );
    document_package_zip(
        "application/epub+zip",
        vec![
            ("META-INF/container.xml".into(), container.into_bytes()),
            ("EPUB/package.opf".into(), opf.into_bytes()),
            ("EPUB/one.xhtml".into(), first.into_bytes()),
            ("EPUB/two.xhtml".into(), second.into_bytes()),
        ],
    )
}

fn document_nodes<'a>(graph: &'a Value, source_file: &str) -> (&'a Value, Vec<&'a Value>) {
    let nodes = graph["nodes"].as_array().expect("graph nodes");
    let root = nodes
        .iter()
        .find(|node| node["source_file"] == source_file && node["type"] == "docx_document")
        .expect("DOCX document root");
    let mut units = nodes
        .iter()
        .filter(|node| node["source_file"] == source_file && node["type"] == "document_section")
        .collect::<Vec<_>>();
    units.sort_unstable_by_key(|node| node["unit_ordinal"].as_u64().expect("unit ordinal"));
    (root, units)
}

fn assert_parallel_relationship_evidence(graph: &Value, source_file: &str) {
    let expected_ids = Value::from(vec!["rIdImage", "rIdTheme"]);
    let expected_kinds = Value::from(vec!["image", "theme"]);
    let expected_evidence = json!([
        {"id": "rIdImage", "kind": "image"},
        {"id": "rIdTheme", "kind": "theme"}
    ]);
    let edges = graph["links"]
        .as_array()
        .expect("graph links")
        .iter()
        .filter(|edge| {
            edge["source_file"] == source_file
                && edge["relation"] == "references"
                && edge["relationship_ids"] == expected_ids
        })
        .collect::<Vec<_>>();
    assert_eq!(
        edges.len(),
        1,
        "parallel relationship evidence must collapse losslessly: {:#?}",
        graph["links"]
    );
    assert_eq!(edges[0]["relationship_kinds"], expected_kinds);
    assert_eq!(edges[0]["relationship_evidence"], expected_evidence);
}

#[test]
fn default_clustered_index_and_warm_cache_are_worker_deterministic() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let handbook = docx_with_sections(&["Opening section", "Closing section"]);
    let publication = epub_with_duplicate_labels_and_self_link();
    fs::create_dir(&project).expect("project");
    fs::write(project.join("handbook.docx"), &handbook).expect("DOCX fixture");
    fs::write(project.join("publication.epub"), &publication).expect("EPUB fixture");
    let direct_epub = graphoxide_extract::extract(&project.join("publication.epub"))
        .expect("direct EPUB fixture extraction");
    assert!(
        direct_epub
            .nodes
            .iter()
            .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some("epub_spine_item"))
            .count()
            == 2,
        "direct EPUB facts: {}",
        serde_json::to_string_pretty(&direct_epub).expect("serialize direct EPUB facts")
    );
    let direct_first_unit = direct_epub
        .nodes
        .iter()
        .find(|node| node.extra.get("unit_ordinal").and_then(Value::as_u64) == Some(1))
        .expect("direct first EPUB unit");
    assert!(direct_epub.edges.iter().any(|edge| {
        edge.relation == "references"
            && edge.extra.get("relationship_kind").and_then(Value::as_str) == Some("hyperlink")
            && edge.true_source() == direct_first_unit.id
            && edge.true_target() == direct_first_unit.id
    }));

    let cold_report = fixture.path().join("cold-runtime.json");
    let cold = graphoxide(&project)
        .args([
            "index",
            ".",
            "--force",
            "--json",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
        ])
        .arg("--runtime-report")
        .arg(&cold_report)
        .output()
        .expect("cold package index");
    assert_success(&cold);

    let cold_workers_one = artifact_bytes(&project);
    let cold_four = graphoxide(&project)
        .args([
            "index",
            ".",
            "--force",
            "--json",
            "--io-workers",
            "1",
            "--compute-workers",
            "4",
        ])
        .output()
        .expect("four-worker forced-cold package index");
    assert_success(&cold_four);
    assert_eq!(
        artifact_bytes(&project),
        cold_workers_one,
        "forced-cold graph, manifest, and coverage artifacts must be worker deterministic"
    );

    let indexed = graph(&project);
    let (root, units) = document_nodes(&indexed, "handbook.docx");
    assert_eq!(root["format_capability"], "structural_partial");
    assert_eq!(root["parse_status"], "complete");
    assert_eq!(units.len(), 2);
    assert_eq!(units[0]["unit_ordinal"], 1);
    assert_eq!(units[0]["source_location"], Value::Null);
    assert_eq!(units[0]["internal_part"], "word/document.xml");
    assert!(units[0]["text"]
        .as_str()
        .is_some_and(|text| text.contains("Opening section")));
    assert!(units[1]["text"]
        .as_str()
        .is_some_and(|text| text.contains("Closing section")));
    let links = indexed["links"].as_array().expect("clustered graph links");
    let contains_for = |source_file: &str| {
        links
            .iter()
            .filter(|edge| edge["relation"] == "contains" && edge["source_file"] == source_file)
            .count()
    };
    assert_eq!(contains_for("handbook.docx"), 2, "{links:#?}");
    assert_eq!(contains_for("publication.epub"), 2, "{links:#?}");
    assert_parallel_relationship_evidence(&indexed, "handbook.docx");
    let mut epub_units = indexed["nodes"]
        .as_array()
        .expect("clustered graph nodes")
        .iter()
        .filter(|node| {
            node["source_file"] == "publication.epub" && node["type"] == "epub_spine_item"
        })
        .collect::<Vec<_>>();
    epub_units.sort_unstable_by_key(|node| node["unit_ordinal"].as_u64().expect("EPUB ordinal"));
    assert_eq!(epub_units.len(), 2);
    assert_eq!(epub_units[0]["label"], "Repeated chapter");
    assert_eq!(epub_units[1]["label"], "Repeated chapter");
    assert_ne!(epub_units[0]["id"], epub_units[1]["id"]);
    let first_epub_id = epub_units[0]["id"].as_str().expect("first EPUB unit ID");
    assert!(indexed["links"].as_array().is_some_and(|links| {
        links.iter().any(|edge| {
            edge["relation"] == "references"
                && edge["relationship_kind"] == "hyperlink"
                && edge["source"] == first_epub_id
                && edge["target"] == first_epub_id
        })
    }));

    let accepted = artifact_bytes(&project);

    fs::remove_file(managed(&project, "graph.json")).expect("remove graph only");
    let warm_report = fixture.path().join("warm-runtime.json");
    let warm = graphoxide(&project)
        .args([
            "index",
            ".",
            "--json",
            "--io-workers",
            "1",
            "--compute-workers",
            "4",
        ])
        .arg("--runtime-report")
        .arg(&warm_report)
        .output()
        .expect("warm package index");
    assert_success(&warm);
    assert_eq!(artifact_bytes(&project), accepted);
    let report: Value =
        serde_json::from_slice(&fs::read(warm_report).expect("warm runtime report"))
            .expect("runtime report JSON");
    assert_eq!(report["cache"]["metadata_hits"], 2);
    assert_eq!(report["cache"]["payload_reads_avoided"], 2);
    assert_eq!(report["cache"]["parses_avoided"], 2);

    let audit = graphoxide(&project)
        .args(["audit", ".", "--json", "--strict", "--force"])
        .output()
        .expect("strict document-package audit");
    assert!(audit.status.success(), "{}", output_text(&audit));
    let audit_json: Value = serde_json::from_slice(&audit.stdout).expect("strict audit JSON");
    assert_eq!(audit_json["strict_violations"], 0);
    assert_eq!(
        audit_json["findings"].as_array().map(Vec::len),
        Some(0),
        "{}",
        serde_json::to_string_pretty(&audit_json).expect("render strict audit")
    );
}

#[test]
fn prior_schema_migrates_and_two_sections_shrink_to_one_with_clean_parity() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let clean = fixture.path().join("clean");
    fs::create_dir(&project).expect("project");
    fs::create_dir(&clean).expect("clean project");
    fs::write(
        project.join("handbook.docx"),
        docx_with_sections(&["REMOVE THIS SECTION", "Survivor moves to ordinal one"]),
    )
    .expect("initial DOCX fixture");

    let initial = graphoxide(&project)
        .args(["index", ".", "--force", "--no-cluster", "--json"])
        .output()
        .expect("initial package index");
    assert_success(&initial);

    let manifest_path = managed(&project, "manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).expect("manifest"))
        .expect("manifest JSON");
    for entry in manifest
        .as_object_mut()
        .expect("manifest object")
        .values_mut()
    {
        entry["ast_version"] = Value::from(28);
    }
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("manifest bytes"),
    )
    .expect("seed schema v28");

    let migrated = graphoxide(&project)
        .args(["update", ".", "--no-cluster", "--json"])
        .output()
        .expect("migrate package schema");
    assert_success(&migrated);
    let migrated_manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("migrated manifest"))
            .expect("migrated manifest JSON");
    assert!(migrated_manifest
        .as_object()
        .expect("manifest object")
        .values()
        .all(|entry| entry["ast_version"].as_u64()
            == Some(u64::from(graphoxide_extract::cache::AST_CACHE_VERSION))));
    assert_eq!(graphoxide_extract::cache::AST_CACHE_VERSION, 29);

    let current = docx_with_sections(&["Survivor moves to ordinal one"]);
    fs::write(project.join("handbook.docx"), &current).expect("replace DOCX fixture");
    let updated = graphoxide(&project)
        .args(["update", ".", "--no-cluster", "--json"])
        .output()
        .expect("update package facts");
    assert_success(&updated);
    let updated_graph = graph(&project);
    let (_, units) = document_nodes(&updated_graph, "handbook.docx");
    assert_eq!(units.len(), 1);
    assert_eq!(units[0]["unit_ordinal"], 1);
    let rendered = serde_json::to_string(&updated_graph).expect("render updated graph");
    assert!(rendered.contains("Survivor moves to ordinal one"));
    assert!(!rendered.contains("REMOVE THIS SECTION"));
    assert_parallel_relationship_evidence(&updated_graph, "handbook.docx");

    fs::write(clean.join("handbook.docx"), current).expect("clean DOCX fixture");
    let rebuilt = graphoxide(&clean)
        .args(["update", ".", "--force", "--no-cluster", "--json"])
        .output()
        .expect("clean package rebuild");
    assert_success(&rebuilt);
    assert_eq!(
        fs::read(managed(&project, "graph.json")).expect("updated graph"),
        fs::read(managed(&clean, "graph.json")).expect("clean graph"),
        "incremental package facts must equal a clean isolated rebuild"
    );

    let accepted = artifact_bytes(&project);
    let unchanged = graphoxide(&project)
        .args(["update", ".", "--no-cluster", "--json"])
        .output()
        .expect("unchanged package update");
    assert_success(&unchanged);
    assert_eq!(artifact_bytes(&project), accepted);
}

#[test]
fn nested_outer_zip_preserves_virtual_source_and_never_writes_members() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let inner = docx_with_sections(&["Nested package text"]);
    fs::write(
        project.join("documents.zip"),
        zip_bytes(vec![("reports/handbook.docx".into(), inner)]),
    )
    .expect("outer ZIP fixture");

    let indexed = graphoxide(&project)
        .args(["index", ".", "--force", "--no-cluster", "--json"])
        .output()
        .expect("nested package index");
    assert_success(&indexed);

    let accepted = graph(&project);
    let virtual_source = "documents.zip!/reports/handbook.docx";
    let (root, units) = document_nodes(&accepted, virtual_source);
    assert_eq!(root["_container_source"], "documents.zip");
    assert_eq!(units.len(), 1);
    assert_eq!(units[0]["_container_source"], "documents.zip");
    assert!(units[0]["text"]
        .as_str()
        .is_some_and(|text| text.contains("Nested package text")));
    assert!(!project.join("reports/handbook.docx").exists());
    assert!(!project.join("word/document.xml").exists());
}
