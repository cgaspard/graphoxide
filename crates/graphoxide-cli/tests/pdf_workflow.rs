use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

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

fn manifest_without_runtime_cache(bytes: &[u8]) -> Value {
    let mut manifest: Value = serde_json::from_slice(bytes).expect("manifest JSON");
    for entry in manifest
        .as_object_mut()
        .expect("manifest object")
        .values_mut()
    {
        entry
            .as_object_mut()
            .expect("manifest entry object")
            .remove("runtime_cache");
    }
    manifest
}

fn manifest_runtime_cache_count(bytes: &[u8]) -> usize {
    serde_json::from_slice::<Value>(bytes)
        .expect("manifest JSON")
        .as_object()
        .expect("manifest object")
        .values()
        .filter(|entry| entry.get("runtime_cache").is_some())
        .count()
}

fn graph(project: &Path) -> Value {
    serde_json::from_slice(&fs::read(managed(project, "graph.json")).expect("graph bytes"))
        .expect("graph JSON")
}

/// Build a deterministic one-page, classic-xref PDF without invoking an
/// external renderer. Keeping the fixture constructor here makes the E2E prove
/// the byte-only production parser rather than a test-only conversion path.
fn pdf_with_pages(texts: &[&str]) -> Vec<u8> {
    assert!(!texts.is_empty());
    assert!(texts.iter().all(|text| text.is_ascii()));
    let page_count = texts.len();
    let first_page_id = 3_usize;
    let first_content_id = first_page_id + page_count;
    let font_id = first_content_id + page_count;
    let kids = (0..page_count)
        .map(|index| format!("{} 0 R", first_page_id + index))
        .collect::<Vec<_>>()
        .join(" ");
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {page_count} >>"),
    ];
    for index in 0..page_count {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font_id} 0 R >> >> /Contents {} 0 R >>",
            first_content_id + index
        ));
    }
    for text in texts {
        let escaped = text
            .replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)");
        let content = format!("BT /F1 12 Tf 72 720 Td ({escaped}) Tj ET\n");
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ));
    }
    objects.push(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_owned(),
    );

    let mut pdf = b"%PDF-1.4\n%\x80\x80\x80\x80\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref_offset = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

fn one_page_pdf(text: &str) -> Vec<u8> {
    pdf_with_pages(&[text])
}

fn zip_bytes(name: &str, payload: &[u8]) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    writer.start_file(name, options).expect("start ZIP member");
    writer.write_all(payload).expect("write ZIP member");
    writer.finish().expect("finish ZIP").into_inner()
}

fn pdf_nodes(graph: &Value) -> (&Value, &Value) {
    let nodes = graph["nodes"].as_array().expect("graph nodes");
    let document = nodes
        .iter()
        .find(|node| node["type"] == "pdf_document")
        .expect("PDF document node");
    let page = nodes
        .iter()
        .find(|node| node["type"] == "pdf_page")
        .expect("PDF page node");
    (document, page)
}

#[test]
fn default_clustered_index_emits_bounded_pdf_page_provenance_and_warm_cache() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    fs::write(project.join("paper.pdf"), one_page_pdf("Hello bounded PDF")).expect("PDF fixture");

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
        .expect("cold PDF index");
    assert_success(&cold);

    let indexed = graph(&project);
    let (document, page) = pdf_nodes(&indexed);
    assert_eq!(document["source_file"], "paper.pdf");
    assert_eq!(document["format_capability"], "structural_partial");
    assert_eq!(page["source_file"], "paper.pdf");
    assert_eq!(page["page_number"], 1);
    assert_eq!(page["source_location"], Value::Null);
    assert!(page["text"]
        .as_str()
        .is_some_and(|text| text.contains("Hello bounded PDF")));
    assert!(indexed["links"].as_array().is_some_and(|links| {
        links
            .iter()
            .any(|edge| edge["relation"] == "contains" && edge["source_file"] == "paper.pdf")
    }));
    let accepted = artifact_bytes(&project);
    assert_eq!(
        manifest_runtime_cache_count(&accepted[1]),
        0,
        "forced extraction must reset runtime-cache authorization"
    );

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
        .expect("warm PDF index");
    assert_success(&warm);
    let warm_artifacts = artifact_bytes(&project);
    assert_eq!(
        warm_artifacts[0], accepted[0],
        "warm graph must remain byte-identical"
    );
    assert_eq!(
        warm_artifacts[2], accepted[2],
        "warm coverage must remain byte-identical"
    );
    assert_eq!(
        manifest_without_runtime_cache(&warm_artifacts[1]),
        manifest_without_runtime_cache(&accepted[1]),
        "the successful warm repair may authorize a runtime-cache artifact but must not change other manifest evidence"
    );
    assert_eq!(
        manifest_runtime_cache_count(&warm_artifacts[1]),
        1,
        "successful warm repair must authorize the persisted PDF extraction"
    );
    let report: Value = serde_json::from_slice(&fs::read(warm_report).expect("runtime report"))
        .expect("runtime report JSON");
    assert_eq!(report["cache"]["metadata_hits"], 0);
    assert_eq!(report["cache"]["payload_reads_avoided"], 0);
    assert_eq!(report["cache"]["parses_avoided"], 0);

    fs::remove_file(managed(&project, "graph.json")).expect("remove repaired graph only");
    let second_warm_report = fixture.path().join("second-warm-runtime.json");
    let second_warm = graphoxide(&project)
        .args([
            "index",
            ".",
            "--json",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
        ])
        .arg("--runtime-report")
        .arg(&second_warm_report)
        .output()
        .expect("second warm PDF index");
    assert_success(&second_warm);
    assert_eq!(
        artifact_bytes(&project),
        warm_artifacts,
        "once runtime-cache authorization is repaired, the next warm graph reconstruction must be byte-identical"
    );
    let second_report: Value =
        serde_json::from_slice(&fs::read(second_warm_report).expect("second warm runtime report"))
            .expect("second runtime report JSON");
    assert_eq!(second_report["cache"]["metadata_hits"], 1);
    assert_eq!(second_report["cache"]["payload_reads_avoided"], 1);
    assert_eq!(second_report["cache"]["parses_avoided"], 1);
}

#[test]
fn pdf_ast_schema_migrates_once_and_then_update_is_byte_stable() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let clean = fixture.path().join("clean");
    fs::create_dir(&project).expect("project");
    fs::create_dir(&clean).expect("clean project");
    fs::write(
        project.join("paper.pdf"),
        pdf_with_pages(&["Initial PDF page", "REMOVED PDF PAGE"]),
    )
    .expect("PDF fixture");

    let initial = graphoxide(&project)
        .args(["index", ".", "--force", "--no-cluster", "--json"])
        .output()
        .expect("initial PDF index");
    assert_success(&initial);

    let manifest_path = managed(&project, "manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).expect("manifest"))
        .expect("manifest JSON");
    for entry in manifest
        .as_object_mut()
        .expect("manifest object")
        .values_mut()
    {
        entry["ast_version"] = graphoxide_extract::cache::AST_CACHE_VERSION
            .saturating_sub(1)
            .into();
    }
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("manifest bytes"),
    )
    .expect("seed prior schema");

    let migrated = graphoxide(&project)
        .args(["update", ".", "--no-cluster", "--json"])
        .output()
        .expect("migrate PDF schema");
    assert_success(&migrated);
    let migrated_manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("migrated manifest"))
            .expect("migrated manifest JSON");
    assert!(migrated_manifest
        .as_object()
        .expect("manifest object")
        .values()
        .all(|entry| {
            entry["ast_version"].as_u64()
                == Some(u64::from(graphoxide_extract::cache::AST_CACHE_VERSION))
        }));

    fs::write(project.join("paper.pdf"), one_page_pdf("Updated PDF text"))
        .expect("replace PDF fixture");
    let updated = graphoxide(&project)
        .args(["update", ".", "--no-cluster", "--json"])
        .output()
        .expect("update PDF facts");
    assert_success(&updated);
    let updated_graph = graph(&project);
    let rendered = serde_json::to_string(&updated_graph).expect("render graph");
    assert!(rendered.contains("Updated PDF text"));
    assert!(!rendered.contains("Initial PDF page"));
    assert!(!rendered.contains("REMOVED PDF PAGE"));
    assert_eq!(
        updated_graph["nodes"]
            .as_array()
            .expect("updated graph nodes")
            .iter()
            .filter(|node| node["type"] == "pdf_page")
            .count(),
        1
    );

    fs::write(clean.join("paper.pdf"), one_page_pdf("Updated PDF text"))
        .expect("clean PDF fixture");
    let rebuilt = graphoxide(&clean)
        .args(["update", ".", "--force", "--no-cluster", "--json"])
        .output()
        .expect("clean PDF rebuild");
    assert_success(&rebuilt);
    assert_eq!(
        fs::read(managed(&project, "graph.json")).expect("updated graph"),
        fs::read(managed(&clean, "graph.json")).expect("clean graph"),
        "incremental PDF facts must equal a clean isolated rebuild"
    );
    let accepted = artifact_bytes(&project);

    let unchanged = graphoxide(&project)
        .args(["update", ".", "--no-cluster", "--json"])
        .output()
        .expect("unchanged PDF update");
    assert_success(&unchanged);
    assert_eq!(artifact_bytes(&project), accepted);
    let indexed = graph(&project);
    let (_, page) = pdf_nodes(&indexed);
    assert!(page["text"]
        .as_str()
        .is_some_and(|text| text.contains("Updated PDF text")));
}

#[test]
fn bounded_pdf_dispatch_preserves_virtual_archive_provenance() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    fs::write(
        project.join("documents.zip"),
        zip_bytes(
            "reports/paper.pdf",
            &one_page_pdf("Nested archive PDF text"),
        ),
    )
    .expect("ZIP fixture");

    let indexed = graphoxide(&project)
        .args(["index", ".", "--force", "--no-cluster", "--json"])
        .output()
        .expect("nested PDF index");
    assert_success(&indexed);

    let accepted = graph(&project);
    let page = accepted["nodes"]
        .as_array()
        .expect("graph nodes")
        .iter()
        .find(|node| node["type"] == "pdf_page")
        .expect("nested PDF page");
    assert_eq!(page["source_file"], "documents.zip!/reports/paper.pdf");
    assert_eq!(page["_container_source"], "documents.zip");
    assert!(page["text"]
        .as_str()
        .is_some_and(|text| text.contains("Nested archive PDF text")));
    assert!(!project.join("reports/paper.pdf").exists());
}
