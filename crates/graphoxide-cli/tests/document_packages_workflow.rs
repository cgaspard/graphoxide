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
const NS_SHEET: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const NS_PRESENTATION: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const NS_DRAWING: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const NS_ODF_OFFICE: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
const NS_ODF_TEXT: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
const NS_ODF_TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
const NS_ODF_DRAW: &str = "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0";
const NS_ODF_MANIFEST: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
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

fn assert_package_coverage(project: &Path, expected: &[(&str, &str)]) {
    let report: Value = serde_json::from_slice(
        &fs::read(managed(project, "coverage.json")).expect("coverage bytes"),
    )
    .expect("coverage JSON");
    assert_eq!(report["complete"], true, "coverage report must be complete");
    let files = report["files"].as_array().expect("coverage files");
    for (source_file, format_id) in expected {
        let outcomes = files
            .iter()
            .filter(|file| file["path"] == *source_file)
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes.len(),
            1,
            "{source_file}: expected exactly one coverage outcome: {files:#?}"
        );
        assert_eq!(outcomes[0]["status"], "covered", "{source_file}: status");
        assert_eq!(
            outcomes[0]["declared_capability"], "structural_partial",
            "{source_file}: capability"
        );
        assert_eq!(
            outcomes[0]["format_id"], *format_id,
            "{source_file}: format"
        );
    }
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

fn content_types(main_part: &str, main_type: &str) -> String {
    format!(
        r#"<Types xmlns="{NS_CONTENT_TYPES}"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/{main_part}" ContentType="{main_type}"/></Types>"#
    )
}

fn package_relationships(target: &str) -> String {
    format!(
        r#"<Relationships xmlns="{NS_PACKAGE_RELS}"><Relationship Id="rIdRoot" Type="{NS_OFFICE_RELS}/officeDocument" Target="{target}"/></Relationships>"#
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
        (
            "[Content_Types].xml".into(),
            content_types(
                "word/document.xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
            )
            .into_bytes(),
        ),
        (
            "_rels/.rels".into(),
            package_relationships("word/document.xml").into_bytes(),
        ),
        ("word/document.xml".into(), document.into_bytes()),
        (
            "word/_rels/document.xml.rels".into(),
            document_relationships.into_bytes(),
        ),
        ("word/media/shared.png".into(), b"inert fixture".to_vec()),
    ])
}

fn xlsx_with_ordered_sheets() -> Vec<u8> {
    let workbook = format!(
        r#"<workbook xmlns="{NS_SHEET}" xmlns:r="{NS_OFFICE_RELS}"><sheets><sheet name="Declared second" sheetId="1" r:id="rId2"/><sheet name="Declared first" sheetId="2" r:id="rId1"/></sheets></workbook>"#
    );
    let relationships = format!(
        r#"<Relationships xmlns="{NS_PACKAGE_RELS}"><Relationship Id="rId1" Type="{NS_OFFICE_RELS}/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="{NS_OFFICE_RELS}/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#
    );
    let shared_strings = format!(
        r#"<sst xmlns="{NS_SHEET}" count="2" uniqueCount="2"><si><t>Shared one</t></si><si><t>Shared two</t></si></sst>"#
    );
    let sheet = |cell: &str, shared_index: usize| {
        format!(
            r#"<worksheet xmlns="{NS_SHEET}"><sheetData><row r="1"><c r="{cell}" t="s"><v>{shared_index}</v></c></row></sheetData></worksheet>"#
        )
    };
    zip_bytes(vec![
        (
            "[Content_Types].xml".into(),
            content_types(
                "xl/workbook.xml",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
            )
            .into_bytes(),
        ),
        (
            "_rels/.rels".into(),
            package_relationships("xl/workbook.xml").into_bytes(),
        ),
        ("xl/workbook.xml".into(), workbook.into_bytes()),
        (
            "xl/_rels/workbook.xml.rels".into(),
            relationships.into_bytes(),
        ),
        ("xl/sharedStrings.xml".into(), shared_strings.into_bytes()),
        (
            "xl/worksheets/sheet1.xml".into(),
            sheet("A1", 0).into_bytes(),
        ),
        (
            "xl/worksheets/sheet2.xml".into(),
            sheet("B2", 1).into_bytes(),
        ),
    ])
}

fn pptx_with_ordered_slides() -> Vec<u8> {
    let presentation = format!(
        r#"<p:presentation xmlns:p="{NS_PRESENTATION}" xmlns:r="{NS_OFFICE_RELS}"><p:sldIdLst><p:sldId id="256" r:id="rId2"/><p:sldId id="257" r:id="rId1"/></p:sldIdLst></p:presentation>"#
    );
    let relationships = format!(
        r#"<Relationships xmlns="{NS_PACKAGE_RELS}"><Relationship Id="rId1" Type="{NS_OFFICE_RELS}/slide" Target="slides/slide1.xml"/><Relationship Id="rId2" Type="{NS_OFFICE_RELS}/slide" Target="slides/slide2.xml"/></Relationships>"#
    );
    let slide = |text: &str| {
        format!(
            r#"<p:sld xmlns:p="{NS_PRESENTATION}" xmlns:a="{NS_DRAWING}"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
        )
    };
    zip_bytes(vec![
        (
            "[Content_Types].xml".into(),
            content_types(
                "ppt/presentation.xml",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
            )
            .into_bytes(),
        ),
        (
            "_rels/.rels".into(),
            package_relationships("ppt/presentation.xml").into_bytes(),
        ),
        (
            "ppt/presentation.xml".into(),
            presentation.into_bytes(),
        ),
        (
            "ppt/_rels/presentation.xml.rels".into(),
            relationships.into_bytes(),
        ),
        (
            "ppt/slides/slide1.xml".into(),
            slide("Physical slide one").into_bytes(),
        ),
        (
            "ppt/slides/slide2.xml".into(),
            slide("Declared first slide").into_bytes(),
        ),
    ])
}

fn odf_with_ordered_units(extension: &str) -> Vec<u8> {
    let (media_type, body) = match extension {
        "odt" => (
            "application/vnd.oasis.opendocument.text",
            r#"<office:text><text:section text:name="Opening"><text:p>ODT opening</text:p></text:section><text:section text:name="Closing"><text:p>ODT closing</text:p></text:section></office:text>"#,
        ),
        "ods" => (
            "application/vnd.oasis.opendocument.spreadsheet",
            r#"<office:spreadsheet><table:table table:name="Budget"><table:table-row><table:table-cell office:value-type="string"><text:p>ODS first</text:p></table:table-cell></table:table-row></table:table><table:table table:name="Forecast"><table:table-row><table:table-cell office:value-type="string"><text:p>ODS second</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet>"#,
        ),
        "odp" => (
            "application/vnd.oasis.opendocument.presentation",
            r#"<office:presentation><draw:page draw:name="Intro"><draw:frame><draw:text-box><text:p>ODP first</text:p></draw:text-box></draw:frame></draw:page><draw:page draw:name="Finish"><draw:frame><draw:text-box><text:p>ODP second</text:p></draw:text-box></draw:frame></draw:page></office:presentation>"#,
        ),
        _ => panic!("unsupported ODF test suffix"),
    };
    let content = format!(
        r#"<office:document-content xmlns:office="{NS_ODF_OFFICE}" xmlns:text="{NS_ODF_TEXT}" xmlns:table="{NS_ODF_TABLE}" xmlns:draw="{NS_ODF_DRAW}" office:version="1.3"><office:body>{body}</office:body></office:document-content>"#
    );
    let manifest = format!(
        r#"<manifest:manifest xmlns:manifest="{NS_ODF_MANIFEST}" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:media-type="{media_type}"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#
    );
    document_package_zip(
        media_type,
        vec![
            ("content.xml".into(), content.into_bytes()),
            ("META-INF/manifest.xml".into(), manifest.into_bytes()),
        ],
    )
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
    package_nodes(graph, source_file, "docx_document", "document_section")
}

fn package_nodes<'a>(
    graph: &'a Value,
    source_file: &str,
    root_type: &str,
    unit_type: &str,
) -> (&'a Value, Vec<&'a Value>) {
    let nodes = graph["nodes"].as_array().expect("graph nodes");
    let roots = nodes
        .iter()
        .filter(|node| node["source_file"] == source_file && node["type"] == root_type)
        .collect::<Vec<_>>();
    assert_eq!(
        roots.len(),
        1,
        "{source_file}: expected exactly one {root_type} package root"
    );
    let root = roots[0];
    let mut units = nodes
        .iter()
        .filter(|node| node["source_file"] == source_file && node["type"] == unit_type)
        .collect::<Vec<_>>();
    units.sort_unstable_by_key(|node| node["unit_ordinal"].as_u64().expect("unit ordinal"));
    (root, units)
}

fn assert_package_units(
    graph: &Value,
    source_file: &str,
    format: &str,
    root_type: &str,
    unit_type: &str,
    expected_units: &[(&str, &str)],
) {
    let (root, units) = package_nodes(graph, source_file, root_type, unit_type);
    assert_eq!(root["_origin"], "document_package", "{source_file}: origin");
    assert_eq!(root["format"], format, "{source_file}: format");
    assert_eq!(root["parse_status"], "complete", "{source_file}: status");
    assert_eq!(
        root["format_capability"], "structural_partial",
        "{source_file}: capability"
    );
    assert_eq!(root["source_location"], Value::Null);
    assert_eq!(units.len(), expected_units.len(), "{source_file}: units");
    for (index, (unit, (internal_part, expected_text))) in
        units.iter().zip(expected_units).enumerate()
    {
        assert_eq!(unit["_origin"], "document_package", "{source_file}: unit");
        assert_eq!(unit["source_file"], source_file);
        assert_eq!(unit["source_location"], Value::Null);
        assert_eq!(unit["unit_ordinal"], index + 1);
        assert_eq!(unit["internal_part"], *internal_part);
        assert!(
            unit["text"]
                .as_str()
                .is_some_and(|text| text.contains(expected_text)),
            "{source_file}: unit {} missing {expected_text:?}: {unit:#?}",
            index + 1
        );
        let contains_edges = graph["links"]
            .as_array()
            .expect("graph links")
            .iter()
            .filter(|edge| {
                edge["relation"] == "contains"
                    && edge["source_file"] == source_file
                    && edge["source"] == root["id"]
                    && edge["target"] == unit["id"]
            })
            .count();
        assert_eq!(
            contains_edges,
            1,
            "{source_file}: unit {} must have exactly one root contains edge",
            index + 1
        );
    }
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
    fs::write(project.join("workbook.xlsx"), xlsx_with_ordered_sheets()).expect("XLSX fixture");
    fs::write(project.join("slides.pptx"), pptx_with_ordered_slides()).expect("PPTX fixture");
    fs::write(project.join("notes.odt"), odf_with_ordered_units("odt")).expect("ODT fixture");
    fs::write(project.join("budget.ods"), odf_with_ordered_units("ods")).expect("ODS fixture");
    fs::write(project.join("talk.odp"), odf_with_ordered_units("odp")).expect("ODP fixture");
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
    assert_package_coverage(
        &project,
        &[
            ("handbook.docx", "office-open-xml"),
            ("workbook.xlsx", "office-open-xml"),
            ("slides.pptx", "office-container-documents"),
            ("notes.odt", "office-container-documents"),
            ("budget.ods", "office-container-documents"),
            ("talk.odp", "office-container-documents"),
            ("publication.epub", "office-container-documents"),
        ],
    );
    assert_package_units(
        &indexed,
        "handbook.docx",
        "docx",
        "docx_document",
        "document_section",
        &[
            ("word/document.xml", "Opening section"),
            ("word/document.xml", "Closing section"),
        ],
    );
    assert_package_units(
        &indexed,
        "workbook.xlsx",
        "xlsx",
        "xlsx_workbook",
        "workbook_sheet",
        &[
            ("xl/worksheets/sheet2.xml", "Shared two"),
            ("xl/worksheets/sheet1.xml", "Shared one"),
        ],
    );
    assert_package_units(
        &indexed,
        "slides.pptx",
        "pptx",
        "pptx_presentation",
        "presentation_slide",
        &[
            ("ppt/slides/slide2.xml", "Declared first slide"),
            ("ppt/slides/slide1.xml", "Physical slide one"),
        ],
    );
    assert_package_units(
        &indexed,
        "notes.odt",
        "odt",
        "odt_document",
        "document_section",
        &[
            ("content.xml", "ODT opening"),
            ("content.xml", "ODT closing"),
        ],
    );
    assert_package_units(
        &indexed,
        "budget.ods",
        "ods",
        "ods_workbook",
        "workbook_sheet",
        &[("content.xml", "ODS first"), ("content.xml", "ODS second")],
    );
    assert_package_units(
        &indexed,
        "talk.odp",
        "odp",
        "odp_presentation",
        "presentation_slide",
        &[("content.xml", "ODP first"), ("content.xml", "ODP second")],
    );
    assert_package_units(
        &indexed,
        "publication.epub",
        "epub",
        "epub_publication",
        "epub_spine_item",
        &[
            ("EPUB/one.xhtml", "First EPUB unit"),
            ("EPUB/two.xhtml", "Second EPUB unit"),
        ],
    );
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
        "the successful warm repair may authorize runtime-cache artifacts but must not change other manifest evidence"
    );
    assert_eq!(
        manifest_runtime_cache_count(&accepted[1]),
        0,
        "forced extraction must reset runtime-cache authorization"
    );
    assert_eq!(
        manifest_runtime_cache_count(&warm_artifacts[1]),
        7,
        "successful warm repair must authorize each persisted package extraction"
    );
    let report: Value =
        serde_json::from_slice(&fs::read(warm_report).expect("warm runtime report"))
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
        .expect("second warm package index");
    assert_success(&second_warm);
    assert_eq!(
        artifact_bytes(&project),
        warm_artifacts,
        "once runtime-cache authorization is repaired, the next warm graph reconstruction must be byte-identical"
    );
    let second_report: Value =
        serde_json::from_slice(&fs::read(second_warm_report).expect("second warm runtime report"))
            .expect("second runtime report JSON");
    assert_eq!(second_report["cache"]["metadata_hits"], 7);
    assert_eq!(second_report["cache"]["payload_reads_avoided"], 7);
    assert_eq!(second_report["cache"]["parses_avoided"], 7);

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
    assert_eq!(graphoxide_extract::cache::AST_CACHE_VERSION, 31);

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
