use graphoxide_core::{Extraction, Node};
use graphoxide_extract::extract;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read as _, Write as _},
    path::Path,
};
use zip::{
    write::{FullFileOptions, SimpleFileOptions},
    CompressionMethod, ZipArchive, ZipWriter,
};

const MAX_SERIALIZED_FACT_BYTES: usize = 1024 * 1024;
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

type ZipEntry = (String, Vec<u8>);
type ZipEntryWithMethod = (String, Vec<u8>, CompressionMethod);

fn zip_bytes(entries: Vec<ZipEntry>) -> Vec<u8> {
    zip_bytes_with_method(entries, CompressionMethod::Deflated)
}

fn zip_bytes_with_method(entries: Vec<ZipEntry>, method: CompressionMethod) -> Vec<u8> {
    zip_bytes_with_methods(
        entries
            .into_iter()
            .map(|(name, bytes)| (name, bytes, method))
            .collect(),
    )
}

fn zip_bytes_with_methods(entries: Vec<ZipEntryWithMethod>) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    for (name, value, method) in entries {
        let options = SimpleFileOptions::default().compression_method(method);
        writer.start_file(name, options).expect("start ZIP member");
        writer.write_all(&value).expect("write ZIP member");
    }
    writer.finish().expect("finish ZIP").into_inner()
}

fn document_package_zip(mimetype: &str, entries: Vec<ZipEntry>) -> Vec<u8> {
    let mut members = Vec::with_capacity(entries.len() + 1);
    members.push((
        "mimetype".to_owned(),
        mimetype.as_bytes().to_vec(),
        CompressionMethod::Stored,
    ));
    members.extend(
        entries
            .into_iter()
            .map(|(name, bytes)| (name, bytes, CompressionMethod::Deflated)),
    );
    zip_bytes_with_methods(members)
}

fn replace_zip_member(archive: &[u8], member: &str, replacement: &str) -> Vec<u8> {
    let mut reader = ZipArchive::new(std::io::Cursor::new(archive)).expect("open fixture ZIP");
    let mut entries = Vec::with_capacity(reader.len());
    let mut replaced = false;
    for index in 0..reader.len() {
        let mut file = reader.by_index(index).expect("read fixture ZIP member");
        let name = file.name().to_owned();
        let method = file.compression();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read fixture member");
        if name == member {
            bytes = replacement.as_bytes().to_vec();
            replaced = true;
        }
        entries.push((name, bytes, method));
    }
    assert!(replaced, "fixture member {member:?} exists");
    zip_bytes_with_methods(entries)
}

fn rewrite_mimetype_layout(archive: &[u8], first: bool, method: CompressionMethod) -> Vec<u8> {
    let mut reader = ZipArchive::new(std::io::Cursor::new(archive)).expect("open fixture ZIP");
    let mut entries = Vec::with_capacity(reader.len());
    let mut mimetype = None;
    for index in 0..reader.len() {
        let mut file = reader.by_index(index).expect("read fixture ZIP member");
        let name = file.name().to_owned();
        let member_method = file.compression();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read fixture member");
        if name == "mimetype" {
            mimetype = Some((name, bytes, method));
        } else {
            entries.push((name, bytes, member_method));
        }
    }
    let mimetype = mimetype.expect("mimetype fixture member");
    if first {
        entries.insert(0, mimetype);
    } else {
        entries.push(mimetype);
    }
    zip_bytes_with_methods(entries)
}

fn add_local_extra_field_to_mimetype(archive: &[u8]) -> Vec<u8> {
    let mut reader = ZipArchive::new(std::io::Cursor::new(archive)).expect("open fixture ZIP");
    let mut entries = Vec::with_capacity(reader.len());
    for index in 0..reader.len() {
        let mut file = reader.by_index(index).expect("read fixture ZIP member");
        let name = file.name().to_owned();
        let method = file.compression();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read fixture member");
        entries.push((name, bytes, method));
    }
    assert_eq!(
        entries.first().map(|entry| entry.0.as_str()),
        Some("mimetype")
    );

    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    for (index, (name, bytes, method)) in entries.into_iter().enumerate() {
        if index == 0 {
            let mut options = FullFileOptions::default().compression_method(method);
            options
                .add_extra_data(0xF00D, [0xA5], false)
                .expect("add local mimetype extra field");
            writer
                .start_file(name, options)
                .expect("start ZIP member with local extra field");
        } else {
            let options = SimpleFileOptions::default().compression_method(method);
            writer.start_file(name, options).expect("start ZIP member");
        }
        writer.write_all(&bytes).expect("write ZIP member");
    }
    writer.finish().expect("finish ZIP").into_inner()
}

fn mark_first_member_encrypted(mut archive: Vec<u8>) -> Vec<u8> {
    assert_eq!(&archive[..4], b"PK\x03\x04");
    archive[6] |= 1;
    let central = archive
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .expect("central-directory entry");
    archive[central + 8] |= 1;
    archive
}

fn extract_at(path: &Path, bytes: &[u8]) -> Extraction {
    fs::write(path, bytes).expect("write deterministic document-package fixture");
    extract(path).expect("extract document-package fixture")
}

fn extract_source(name: &str, bytes: &[u8]) -> Extraction {
    let project = tempfile::tempdir().expect("temporary document-package fixture");
    extract_at(&project.path().join(name), bytes)
}

fn package_root(extraction: &Extraction) -> &Node {
    extraction
        .nodes
        .iter()
        .find(|node| {
            node.extra.get("_origin").and_then(Value::as_str) == Some("document_package")
                && !node.extra.contains_key("unit_ordinal")
        })
        .expect("document-package root")
}

fn ordered_units<'a>(extraction: &'a Extraction, unit_type: &str) -> Vec<&'a Node> {
    let mut units = extraction
        .nodes
        .iter()
        .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some(unit_type))
        .collect::<Vec<_>>();
    units.sort_unstable_by_key(|node| {
        node.extra
            .get("unit_ordinal")
            .and_then(Value::as_u64)
            .expect("numeric semantic-unit ordinal")
    });
    units
}

fn assert_fact_sizes(extraction: &Extraction) {
    assert!(
        extraction
            .nodes
            .len()
            .saturating_add(extraction.edges.len())
            .saturating_add(extraction.hyperedges.len())
            <= 4_096,
        "document-package fact ceiling"
    );
    for node in &extraction.nodes {
        let bytes = serde_json::to_vec(node).expect("serialize package node fact");
        assert!(
            bytes.len() < MAX_SERIALIZED_FACT_BYTES,
            "node {} serialized to {} bytes",
            node.id,
            bytes.len()
        );
    }
    for edge in &extraction.edges {
        let bytes = serde_json::to_vec(edge).expect("serialize package edge fact");
        assert!(
            bytes.len() < MAX_SERIALIZED_FACT_BYTES,
            "edge {} -> {} serialized to {} bytes",
            edge.source,
            edge.target,
            bytes.len()
        );
    }
}

fn assert_contains_edges(extraction: &Extraction, root: &Node, units: &[&Node]) {
    let contains = extraction
        .edges
        .iter()
        .filter(|edge| edge.relation == "contains")
        .collect::<Vec<_>>();
    assert_eq!(contains.len(), units.len());
    for unit in units {
        let edge = contains
            .iter()
            .find(|edge| edge.true_source() == root.id && edge.true_target() == unit.id)
            .expect("root-to-unit contains edge");
        assert_eq!(
            edge.extra.get("_origin"),
            Some(&Value::from("document_package"))
        );
        assert_eq!(edge.source_file, root.source_file);
    }
}

fn assert_rejected(name: &str, bytes: &[u8], diagnostic: &str) -> Extraction {
    let extraction = extract_source(name, bytes);
    assert_eq!(extraction.nodes.len(), 1, "{name}: root only");
    assert!(extraction.edges.is_empty(), "{name}: no partial edges");
    assert!(
        extraction.hyperedges.is_empty(),
        "{name}: no partial hyperedges"
    );
    let root = package_root(&extraction);
    assert_eq!(
        root.extra.get("parse_status"),
        Some(&Value::from("rejected"))
    );
    assert_eq!(
        root.extra.get("diagnostic"),
        Some(&Value::from(diagnostic)),
        "{name}: diagnostic"
    );
    assert_eq!(
        root.extra.get("format_capability"),
        Some(&Value::from("structural_partial"))
    );
    assert_fact_sizes(&extraction);
    extraction
}

fn assert_no_payload(extraction: &Extraction, payload: &str) {
    let serialized = serde_json::to_string(extraction).expect("serialize extraction");
    assert!(
        !serialized.contains(payload),
        "unsupported payload {payload:?} leaked into graph facts"
    );
}

fn content_types(main_part: &str, main_type: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="{NS_CONTENT_TYPES}">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/{main_part}" ContentType="{main_type}"/>
</Types>"#
    )
}

fn package_relationships(target: &str) -> String {
    package_relationships_with_type(target, &format!("{NS_OFFICE_RELS}/officeDocument"))
}

fn package_relationships_with_type(target: &str, relationship_type: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="{NS_PACKAGE_RELS}">
  <Relationship Id="rIdRoot" Type="{relationship_type}" Target="{target}"/>
</Relationships>"#
    )
}

fn docx_fixture(sections: &[&str], external_target: Option<&str>) -> Vec<u8> {
    let mut body = String::new();
    for (index, text) in sections.iter().enumerate() {
        body.push_str(&format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>"));
        if index + 1 < sections.len() {
            body.push_str("<w:sectPr/>");
        }
    }
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="{NS_WORD}"><w:body>{body}</w:body></w:document>"#
    );
    let mut document_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="{NS_PACKAGE_RELS}">
  <Relationship Id="rIdImage" Type="{NS_OFFICE_RELS}/image" Target="media/image1.png"/>"#
    );
    if let Some(target) = external_target {
        document_rels.push_str(&format!(
            r#"<Relationship Id="rIdExternal" Type="{NS_OFFICE_RELS}/hyperlink" Target="{target}" TargetMode="External"/>"#
        ));
    }
    document_rels.push_str("</Relationships>");
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
            document_rels.into_bytes(),
        ),
        (
            "word/media/image1.png".into(),
            b"inert-image-placeholder".to_vec(),
        ),
    ])
}

fn docx_with_parallel_relationship_evidence() -> Vec<u8> {
    let relationships = format!(
        r#"<Relationships xmlns="{NS_PACKAGE_RELS}">
<Relationship Id="rIdTheme" Type="{NS_OFFICE_RELS}/theme" Target="media/image1.png"/>
<Relationship Id="rIdImage" Type="{NS_OFFICE_RELS}/image" Target="media/image1.png"/>
</Relationships>"#
    );
    replace_zip_member(
        &docx_fixture(&["Parallel relationship evidence"], None),
        "word/_rels/document.xml.rels",
        &relationships,
    )
}

fn xlsx_fixture() -> Vec<u8> {
    xlsx_fixture_with_sheet_type(&format!("{NS_OFFICE_RELS}/worksheet"))
}

fn xlsx_fixture_with_sheet_type(sheet_relationship_type: &str) -> Vec<u8> {
    xlsx_fixture_with_options(
        sheet_relationship_type,
        &[("Declared second", "rId2"), ("Declared first", "rId1")],
    )
}

fn xlsx_fixture_with_order(sheet_order: &[(&str, &str)]) -> Vec<u8> {
    xlsx_fixture_with_options(&format!("{NS_OFFICE_RELS}/worksheet"), sheet_order)
}

fn xlsx_fixture_with_options(
    sheet_relationship_type: &str,
    sheet_order: &[(&str, &str)],
) -> Vec<u8> {
    let sheet_declarations = sheet_order
        .iter()
        .enumerate()
        .map(|(index, (name, relationship_id))| {
            format!(
                r#"<sheet name="{name}" sheetId="{}" r:id="{relationship_id}"/>"#,
                index + 1
            )
        })
        .collect::<String>();
    let workbook = format!(
        r#"<workbook xmlns="{NS_SHEET}" xmlns:r="{NS_OFFICE_RELS}">
<sheets>{sheet_declarations}</sheets></workbook>"#
    );
    let rels = format!(
        r#"<Relationships xmlns="{NS_PACKAGE_RELS}">
<Relationship Id="rId1" Type="{sheet_relationship_type}" Target="worksheets/sheet1.xml"/>
<Relationship Id="rId2" Type="{sheet_relationship_type}" Target="worksheets/sheet2.xml"/>
</Relationships>"#
    );
    let shared = format!(
        r#"<sst xmlns="{NS_SHEET}" count="2" uniqueCount="2"><si><t>Shared one</t></si><si><t>Shared two</t></si></sst>"#
    );
    let sheet1 = format!(
        r#"<worksheet xmlns="{NS_SHEET}"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData></worksheet>"#
    );
    let sheet2 = format!(
        r#"<worksheet xmlns="{NS_SHEET}"><sheetData><row r="1"><c r="B2" t="s"><v>1</v></c></row></sheetData></worksheet>"#
    );
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
        ("xl/_rels/workbook.xml.rels".into(), rels.into_bytes()),
        ("xl/sharedStrings.xml".into(), shared.into_bytes()),
        ("xl/worksheets/sheet1.xml".into(), sheet1.into_bytes()),
        ("xl/worksheets/sheet2.xml".into(), sheet2.into_bytes()),
    ])
}

fn pptx_fixture() -> Vec<u8> {
    pptx_fixture_with_slide_type(&format!("{NS_OFFICE_RELS}/slide"))
}

fn pptx_fixture_with_slide_type(slide_relationship_type: &str) -> Vec<u8> {
    pptx_fixture_with_options(slide_relationship_type, &["rId2", "rId1"])
}

fn pptx_fixture_with_order(slide_order: &[&str]) -> Vec<u8> {
    pptx_fixture_with_options(&format!("{NS_OFFICE_RELS}/slide"), slide_order)
}

fn pptx_fixture_with_options(slide_relationship_type: &str, slide_order: &[&str]) -> Vec<u8> {
    let slide_declarations = slide_order
        .iter()
        .enumerate()
        .map(|(index, relationship_id)| {
            format!(
                r#"<p:sldId id="{}" r:id="{relationship_id}"/>"#,
                256 + index
            )
        })
        .collect::<String>();
    let presentation = format!(
        r#"<p:presentation xmlns:p="{NS_PRESENTATION}" xmlns:r="{NS_OFFICE_RELS}">
<p:sldIdLst>{slide_declarations}</p:sldIdLst></p:presentation>"#
    );
    let rels = format!(
        r#"<Relationships xmlns="{NS_PACKAGE_RELS}">
<Relationship Id="rId1" Type="{slide_relationship_type}" Target="slides/slide1.xml"/>
<Relationship Id="rId2" Type="{slide_relationship_type}" Target="slides/slide2.xml"/>
</Relationships>"#
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
        ("_rels/.rels".into(), package_relationships("ppt/presentation.xml").into_bytes()),
        ("ppt/presentation.xml".into(), presentation.into_bytes()),
        ("ppt/_rels/presentation.xml.rels".into(), rels.into_bytes()),
        ("ppt/slides/slide1.xml".into(), slide("Physical slide one").into_bytes()),
        ("ppt/slides/slide2.xml".into(), slide("Declared first slide").into_bytes()),
    ])
}

fn odf_fixture(extension: &str) -> Vec<u8> {
    let (media_type, body) = match extension {
        "odt" => (
            "application/vnd.oasis.opendocument.text",
            r#"<office:text><text:section text:name="Opening"><text:p>ODT opening</text:p></text:section><text:section text:name="Closing"><text:p>ODT closing</text:p></text:section></office:text>"#.to_owned(),
        ),
        "ods" => (
            "application/vnd.oasis.opendocument.spreadsheet",
            r#"<office:spreadsheet><table:table table:name="Budget"><table:table-row><table:table-cell office:value-type="string"><text:p>ODS first</text:p></table:table-cell></table:table-row></table:table><table:table table:name="Forecast"><table:table-row><table:table-cell office:value-type="string"><text:p>ODS second</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet>"#.to_owned(),
        ),
        "odp" => (
            "application/vnd.oasis.opendocument.presentation",
            r#"<office:presentation><draw:page draw:name="Intro"><draw:frame><draw:text-box><text:p>ODP first</text:p></draw:text-box></draw:frame></draw:page><draw:page draw:name="Finish"><draw:frame><draw:text-box><text:p>ODP second</text:p></draw:text-box></draw:frame></draw:page></office:presentation>"#.to_owned(),
        ),
        _ => panic!("unsupported ODF test suffix"),
    };
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="{NS_ODF_OFFICE}" xmlns:text="{NS_ODF_TEXT}" xmlns:table="{NS_ODF_TABLE}" xmlns:draw="{NS_ODF_DRAW}" office:version="1.3"><office:body>{body}</office:body></office:document-content>"#
    );
    let manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="{NS_ODF_MANIFEST}" manifest:version="1.3">
<manifest:file-entry manifest:full-path="/" manifest:media-type="{media_type}"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"#
    );
    document_package_zip(
        media_type,
        vec![
            ("content.xml".into(), content.into_bytes()),
            ("META-INF/manifest.xml".into(), manifest.into_bytes()),
        ],
    )
}

fn epub_fixture() -> Vec<u8> {
    epub_fixture_with_spine(&["two", "one"])
}

fn epub_fixture_with_spine(spine_order: &[&str]) -> Vec<u8> {
    let container = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<container xmlns="{NS_EPUB_CONTAINER}" version="1.0"><rootfiles><rootfile full-path="EPUB/package.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
    );
    let spine = spine_order
        .iter()
        .map(|id| format!(r#"<itemref idref="{id}"/>"#))
        .collect::<String>();
    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="{NS_OPF}" version="3.0" unique-identifier="book-id">
<metadata xmlns:dc="{NS_DC}"><dc:identifier id="book-id">urn:uuid:test</dc:identifier><dc:title>Bounded EPUB</dc:title></metadata>
<manifest><item id="one" href="chapter1.xhtml" media-type="application/xhtml+xml"/><item id="two" href="chapter2.xhtml" media-type="application/xhtml+xml"/></manifest>
<spine>{spine}</spine></package>"#
    );
    let chapter1 = format!(
        r#"<html xmlns="{NS_XHTML}"><head><title>Physical chapter one</title></head><body id="top"><p>EPUB physical first</p></body></html>"#
    );
    let chapter2 = format!(
        r#"<html xmlns="{NS_XHTML}"><head><title>Declared first</title></head><body><p>EPUB declared first</p><a href="chapter1.xhtml#top">Continue</a></body></html>"#
    );
    document_package_zip(
        "application/epub+zip",
        vec![
            ("META-INF/container.xml".into(), container.into_bytes()),
            ("EPUB/package.opf".into(), opf.into_bytes()),
            ("EPUB/chapter1.xhtml".into(), chapter1.into_bytes()),
            ("EPUB/chapter2.xhtml".into(), chapter2.into_bytes()),
        ],
    )
}

#[test]
fn all_seven_packages_publish_stable_ordered_units_and_provenance() {
    let fixtures = [
        (
            "book.docx",
            docx_fixture(&["DOCX first", "DOCX second"], None),
            "docx",
            "docx_document",
            "document_section",
            vec!["DOCX first", "DOCX second"],
        ),
        (
            "book.xlsx",
            xlsx_fixture(),
            "xlsx",
            "xlsx_workbook",
            "workbook_sheet",
            vec!["Shared two", "Shared one"],
        ),
        (
            "deck.pptx",
            pptx_fixture(),
            "pptx",
            "pptx_presentation",
            "presentation_slide",
            vec!["Declared first slide", "Physical slide one"],
        ),
        (
            "book.odt",
            odf_fixture("odt"),
            "odt",
            "odt_document",
            "document_section",
            vec!["ODT opening", "ODT closing"],
        ),
        (
            "book.ods",
            odf_fixture("ods"),
            "ods",
            "ods_workbook",
            "workbook_sheet",
            vec!["ODS first", "ODS second"],
        ),
        (
            "deck.odp",
            odf_fixture("odp"),
            "odp",
            "odp_presentation",
            "presentation_slide",
            vec!["ODP first", "ODP second"],
        ),
        (
            "book.epub",
            epub_fixture(),
            "epub",
            "epub_publication",
            "epub_spine_item",
            vec!["EPUB declared first", "EPUB physical first"],
        ),
    ];

    let project = tempfile::tempdir().expect("package fixture project");
    for (name, bytes, format, document_type, unit_type, expected_text) in fixtures {
        let path = project.path().join(name);
        let first = extract_at(&path, &bytes);
        let second = extract_at(&path, &bytes);
        assert_eq!(
            serde_json::to_vec(&first).expect("serialize first extraction"),
            serde_json::to_vec(&second).expect("serialize second extraction"),
            "{name}: identical path and bytes must produce identical facts"
        );
        let root = package_root(&first);
        let expected_source = path.to_string_lossy();
        assert_eq!(root.file_type, "document");
        assert_eq!(root.source_file, expected_source);
        assert_eq!(root.source_location, None);
        assert_eq!(root.extra.get("format"), Some(&Value::from(format)));
        assert_eq!(root.extra.get("type"), Some(&Value::from(document_type)));
        assert_eq!(
            root.extra.get("parse_status"),
            Some(&Value::from("complete"))
        );
        assert_eq!(
            root.extra.get("format_capability"),
            Some(&Value::from("structural_partial"))
        );
        let units = ordered_units(&first, unit_type);
        assert_eq!(units.len(), expected_text.len(), "{name}: unit count");
        for (index, (unit, text)) in units.iter().zip(expected_text).enumerate() {
            assert_eq!(unit.source_file, expected_source);
            assert_eq!(unit.file_type, "document");
            assert_eq!(
                unit.extra.get("unit_ordinal"),
                Some(&Value::from(index + 1))
            );
            assert!(
                unit.extra
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.contains(text)),
                "{name}: unit {} missing {text:?}: {:?}",
                index + 1,
                unit.extra.get("text")
            );
            let part = unit
                .extra
                .get("internal_part")
                .and_then(Value::as_str)
                .expect("internal part");
            assert_eq!(unit.source_location, None);
            assert!(!part.is_empty());
        }
        assert_contains_edges(&first, root, &units);
        assert!(first.hyperedges.is_empty());
        assert_fact_sizes(&first);
    }
}

#[test]
fn odf_and_epub_require_a_conformant_mimetype_member() {
    let odf_sentinel = "ODF_MIMETYPE_LAYOUT_SENTINEL_D6B3";
    let odf_content = format!(
        r#"<office:document-content xmlns:office="{NS_ODF_OFFICE}" xmlns:text="{NS_ODF_TEXT}"><office:body><office:text><text:p>{odf_sentinel}</text:p></office:text></office:body></office:document-content>"#
    );
    let odf = replace_zip_member(&odf_fixture("odt"), "content.xml", &odf_content);
    for (name, first, method) in [
        (
            "odf-deflated-mimetype.odt",
            true,
            CompressionMethod::Deflated,
        ),
        ("odf-late-mimetype.odt", false, CompressionMethod::Stored),
    ] {
        let extraction = assert_rejected(
            name,
            &rewrite_mimetype_layout(&odf, first, method),
            "office_format_mismatch",
        );
        assert_no_payload(&extraction, odf_sentinel);
    }
    let extraction = assert_rejected(
        "odf-extra-mimetype.odt",
        &add_local_extra_field_to_mimetype(&odf),
        "office_format_mismatch",
    );
    assert_no_payload(&extraction, odf_sentinel);

    let epub_sentinel = "EPUB_MIMETYPE_LAYOUT_SENTINEL_46F1";
    let xhtml = format!(
        r#"<html xmlns="{NS_XHTML}"><head><title>Layout</title></head><body><p>{epub_sentinel}</p></body></html>"#
    );
    let epub = replace_zip_member(&epub_fixture(), "EPUB/chapter1.xhtml", &xhtml);
    for (name, first, method) in [
        (
            "epub-deflated-mimetype.epub",
            true,
            CompressionMethod::Deflated,
        ),
        ("epub-late-mimetype.epub", false, CompressionMethod::Stored),
    ] {
        let extraction = assert_rejected(
            name,
            &rewrite_mimetype_layout(&epub, first, method),
            "office_format_mismatch",
        );
        assert_no_payload(&extraction, epub_sentinel);
    }
    let extraction = assert_rejected(
        "epub-extra-mimetype.epub",
        &add_local_extra_field_to_mimetype(&epub),
        "office_format_mismatch",
    );
    assert_no_payload(&extraction, epub_sentinel);
}

#[test]
fn safe_xml_references_decode_while_custom_and_illegal_references_fail_closed() {
    let decoded = extract_source(
        "xml-references.docx",
        &docx_fixture(&["XML refs: &amp; &#169; &#x1F642;"], None),
    );
    assert_eq!(
        package_root(&decoded).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    let units = ordered_units(&decoded, "document_section");
    assert_eq!(units.len(), 1);
    assert_eq!(
        units[0].extra.get("text").and_then(Value::as_str),
        Some("XML refs: & © 🙂")
    );

    let custom = "CUSTOM_XML_REFERENCE_SENTINEL_2EE1";
    let rejected = assert_rejected(
        "custom-reference.docx",
        &docx_fixture(&[&format!("&{custom};")], None),
        "office_xml_doctype_forbidden",
    );
    assert_no_payload(&rejected, custom);

    let illegal = assert_rejected(
        "illegal-numeric-reference.docx",
        &docx_fixture(&["ILLEGAL_NUMERIC_SENTINEL_77D0 &#x110000;"], None),
        "office_xml_malformed",
    );
    assert_no_payload(&illegal, "ILLEGAL_NUMERIC_SENTINEL_77D0");
}

#[test]
fn cdata_inside_semantic_text_is_retained_as_plain_text() {
    let extraction = extract_source(
        "xml-cdata.docx",
        &docx_fixture(&["<![CDATA[A & B]]>"], None),
    );
    assert_eq!(
        package_root(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        ordered_units(&extraction, "document_section")[0]
            .extra
            .get("text")
            .and_then(Value::as_str),
        Some("A & B")
    );
}

#[test]
fn ignored_extension_attributes_must_have_bound_namespace_prefixes() {
    let sentinel = "IGNORED_ATTRIBUTE_SENTINEL_C0A8";
    let declared = format!(
        r#"<w:document xmlns:w="{NS_WORD}" xmlns:evil="urn:example:inert"><w:body><w:p evil:flag="{sentinel} safe &amp; &#169;"><w:r><w:t>safe declared extension</w:t></w:r></w:p></w:body></w:document>"#
    );
    let accepted = extract_source(
        "declared-extension.docx",
        &replace_zip_member(
            &docx_fixture(&["safe"], None),
            "word/document.xml",
            &declared,
        ),
    );
    assert_eq!(
        package_root(&accepted).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert!(ordered_units(&accepted, "document_section")[0]
        .extra
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains("safe declared extension")));
    assert_no_payload(&accepted, sentinel);

    let custom = "CUSTOM_ATTRIBUTE_ENTITY_SENTINEL_3A6C";
    let custom_reference = format!(
        r#"<w:document xmlns:w="{NS_WORD}" xmlns:evil="urn:example:inert"><w:body><w:p evil:flag="&{custom};"><w:r><w:t>must not publish</w:t></w:r></w:p></w:body></w:document>"#
    );
    let rejected = assert_rejected(
        "custom-reference-attribute.docx",
        &replace_zip_member(
            &docx_fixture(&["safe"], None),
            "word/document.xml",
            &custom_reference,
        ),
        "office_xml_doctype_forbidden",
    );
    assert_no_payload(&rejected, custom);

    let illegal = "ILLEGAL_NUMERIC_ATTRIBUTE_SENTINEL_BB8D";
    let illegal_reference = format!(
        r#"<w:document xmlns:w="{NS_WORD}" xmlns:evil="urn:example:inert"><w:body><w:p evil:flag="{illegal} &#x110000;"><w:r><w:t>must not publish</w:t></w:r></w:p></w:body></w:document>"#
    );
    let rejected = assert_rejected(
        "illegal-numeric-reference-attribute.docx",
        &replace_zip_member(
            &docx_fixture(&["safe"], None),
            "word/document.xml",
            &illegal_reference,
        ),
        "office_xml_malformed",
    );
    assert_no_payload(&rejected, illegal);

    let unbound = format!(
        r#"<w:document xmlns:w="{NS_WORD}"><w:body><w:p evil:flag="{sentinel}"><w:r><w:t>must not publish</w:t></w:r></w:p></w:body></w:document>"#
    );
    let rejected = assert_rejected(
        "unbound-extension.docx",
        &replace_zip_member(
            &docx_fixture(&["safe"], None),
            "word/document.xml",
            &unbound,
        ),
        "office_xml_namespace_unsupported",
    );
    assert_no_payload(&rejected, sentinel);
}

#[test]
fn safe_references_inside_an_ignored_extension_subtree_are_inert() {
    let sentinel = "IGNORED_EXTENSION_REFERENCE_SENTINEL_56F2";
    let document = format!(
        r#"<w:document xmlns:w="{NS_WORD}" xmlns:evil="urn:example:inert"><w:body><w:p><evil:payload>{sentinel} &amp; &#169;</evil:payload><w:r><w:t>visible text</w:t></w:r></w:p></w:body></w:document>"#
    );
    let extraction = extract_source(
        "ignored-extension-references.docx",
        &replace_zip_member(
            &docx_fixture(&["safe"], None),
            "word/document.xml",
            &document,
        ),
    );
    assert_eq!(
        package_root(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        ordered_units(&extraction, "document_section")[0]
            .extra
            .get("text")
            .and_then(Value::as_str),
        Some("visible text")
    );
    assert_no_payload(&extraction, sentinel);
}

fn unit_identity_by_part(
    extraction: &Extraction,
    unit_type: &str,
) -> BTreeMap<String, (String, u64)> {
    extraction
        .nodes
        .iter()
        .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some(unit_type))
        .map(|node| {
            (
                node.extra["internal_part"]
                    .as_str()
                    .expect("unit internal part")
                    .to_owned(),
                (
                    node.id.clone(),
                    node.extra["unit_ordinal"].as_u64().expect("unit ordinal"),
                ),
            )
        })
        .collect()
}

fn assert_reorder_preserves_part_identity(
    path: &Path,
    unit_type: &str,
    first: &[u8],
    reordered: &[u8],
) {
    let before = unit_identity_by_part(&extract_at(path, first), unit_type);
    let after = unit_identity_by_part(&extract_at(path, reordered), unit_type);
    assert_eq!(before.len(), 2);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>()
    );
    for (part, (before_id, before_ordinal)) in before {
        let (after_id, after_ordinal) = after.get(&part).expect("same unit part after reorder");
        assert_eq!(&before_id, after_id, "{part}: stable path-derived unit ID");
        assert_ne!(
            before_ordinal, *after_ordinal,
            "{part}: declaration reorder changes only the ordinal"
        );
    }
}

#[test]
fn sheet_slide_and_spine_reorders_preserve_part_based_unit_identity() {
    let project = tempfile::tempdir().expect("identity fixture project");
    assert_reorder_preserves_part_identity(
        &project.path().join("ordered.xlsx"),
        "workbook_sheet",
        &xlsx_fixture_with_order(&[("Sheet one", "rId1"), ("Sheet two", "rId2")]),
        &xlsx_fixture_with_order(&[("Sheet two", "rId2"), ("Sheet one", "rId1")]),
    );
    assert_reorder_preserves_part_identity(
        &project.path().join("ordered.pptx"),
        "presentation_slide",
        &pptx_fixture_with_order(&["rId1", "rId2"]),
        &pptx_fixture_with_order(&["rId2", "rId1"]),
    );
    assert_reorder_preserves_part_identity(
        &project.path().join("ordered.epub"),
        "epub_spine_item",
        &epub_fixture_with_spine(&["one", "two"]),
        &epub_fixture_with_spine(&["two", "one"]),
    );
}

fn docx_with_image_parts(parts: &[(&str, &str)]) -> Vec<u8> {
    let document = format!(
        r#"<w:document xmlns:w="{NS_WORD}"><w:body><w:p><w:r><w:t>Stable part identities</w:t></w:r></w:p></w:body></w:document>"#
    );
    let mut relationships = format!(r#"<Relationships xmlns="{NS_PACKAGE_RELS}">"#);
    for (id, part) in parts {
        relationships.push_str(&format!(
            r#"<Relationship Id="{id}" Type="{NS_OFFICE_RELS}/image" Target="media/{part}"/>"#
        ));
    }
    relationships.push_str("</Relationships>");
    let mut entries = vec![
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
            relationships.into_bytes(),
        ),
    ];
    entries.extend(parts.iter().map(|(_, part)| {
        (
            format!("word/media/{part}"),
            format!("inert fixture {part}").into_bytes(),
        )
    }));
    zip_bytes(entries)
}

fn part_identity(extraction: &Extraction) -> BTreeMap<String, String> {
    extraction
        .nodes
        .iter()
        .filter(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("document_package_part")
        })
        .map(|node| {
            (
                node.extra["internal_part"]
                    .as_str()
                    .expect("part provenance")
                    .to_owned(),
                node.id.clone(),
            )
        })
        .collect()
}

#[test]
fn adding_an_earlier_unrelated_part_does_not_renumber_existing_part_nodes() {
    let project = tempfile::tempdir().expect("part identity fixture");
    let path = project.path().join("parts.docx");
    let before = part_identity(&extract_at(
        &path,
        &docx_with_image_parts(&[("rOmega", "omega.png"), ("rZeta", "zeta.png")]),
    ));
    let after = part_identity(&extract_at(
        &path,
        &docx_with_image_parts(&[
            ("rAlpha", "alpha.png"),
            ("rOmega", "omega.png"),
            ("rZeta", "zeta.png"),
        ]),
    ));
    for part in ["word/media/omega.png", "word/media/zeta.png"] {
        assert_eq!(
            before.get(part),
            after.get(part),
            "{part}: stable path-derived part ID"
        );
    }
    assert!(after.contains_key("word/media/alpha.png"));
}

#[test]
fn internal_relationships_publish_directional_edges_but_external_targets_are_inert() {
    let sentinel = "DO_NOT_RETAIN_EXTERNAL_TARGET_719B";
    let target = format!("https://example.invalid/{sentinel}");
    let extraction = extract_source(
        "links.docx",
        &docx_fixture(&["Safe document text"], Some(&target)),
    );
    let root = package_root(&extraction);
    assert_eq!(
        root.extra.get("external_relationship_count"),
        Some(&Value::from(1))
    );
    assert_no_payload(&extraction, sentinel);
    let references = extraction
        .edges
        .iter()
        .filter(|edge| edge.relation == "references")
        .collect::<Vec<_>>();
    assert!(
        !references.is_empty(),
        "safe internal package relationship edge"
    );
    for edge in references {
        assert_eq!(
            edge.extra.get("_origin"),
            Some(&Value::from("document_package"))
        );
        assert!(edge
            .extra
            .get("relationship_kind")
            .and_then(Value::as_str)
            .is_some());
        assert!(edge
            .extra
            .get("relationship_id")
            .and_then(Value::as_str)
            .is_some());
        assert_eq!(
            edge.extra.get("_src").and_then(Value::as_str),
            Some(edge.true_source())
        );
        assert_eq!(
            edge.extra.get("_tgt").and_then(Value::as_str),
            Some(edge.true_target())
        );
    }
    assert_fact_sizes(&extraction);
}

#[test]
fn parallel_internal_relationship_evidence_is_sorted_and_lossless() {
    let extraction = extract_source(
        "parallel-evidence.docx",
        &docx_with_parallel_relationship_evidence(),
    );
    let target = extraction
        .nodes
        .iter()
        .find(|node| {
            node.extra.get("internal_part").and_then(Value::as_str) == Some("word/media/image1.png")
        })
        .expect("shared relationship target part");
    let references = extraction
        .edges
        .iter()
        .filter(|edge| edge.relation == "references" && edge.true_target() == target.id)
        .collect::<Vec<_>>();
    assert_eq!(references.len(), 1, "one edge per source/target/relation");
    assert_eq!(
        references[0].extra.get("relationship_ids"),
        Some(&Value::from(vec!["rIdImage", "rIdTheme"]))
    );
    assert_eq!(
        references[0].extra.get("relationship_kinds"),
        Some(&Value::from(vec!["image", "theme"]))
    );
    assert_eq!(
        references[0].extra.get("relationship_evidence"),
        Some(&json!([
            {"id": "rIdImage", "kind": "image"},
            {"id": "rIdTheme", "kind": "theme"}
        ]))
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn uri_escaped_and_nfc_equivalent_relationship_targets_resolve_to_members() {
    let document = format!(
        r#"<w:document xmlns:w="{NS_WORD}"><w:body><w:p><w:r><w:t>Canonical targets</w:t></w:r></w:p></w:body></w:document>"#
    );
    let decomposed = "cafe\u{301}.png";
    let relationships = format!(
        r#"<Relationships xmlns="{NS_PACKAGE_RELS}">
<Relationship Id="rIdSpace" Type="{NS_OFFICE_RELS}/image" Target="media/My%20Image.png"/>
<Relationship Id="rIdNfc" Type="{NS_OFFICE_RELS}/image" Target="media/{decomposed}"/>
</Relationships>"#
    );
    let extraction = extract_source(
        "canonical-targets.docx",
        &zip_bytes(vec![
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
                relationships.into_bytes(),
            ),
            ("word/media/My Image.png".into(), b"space".to_vec()),
            ("word/media/caf\u{e9}.png".into(), b"nfc".to_vec()),
        ]),
    );
    let parts = extraction
        .nodes
        .iter()
        .filter_map(|node| {
            (node.extra.get("type").and_then(Value::as_str) == Some("document_package_part"))
                .then(|| node.extra.get("internal_part").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>();
    assert!(parts.contains(&"word/media/My Image.png"));
    assert!(parts.contains(&"word/media/caf\u{e9}.png"));
    assert_eq!(
        extraction
            .edges
            .iter()
            .filter(|edge| {
                edge.relation == "references"
                    && edge.extra.get("relationship_kind") == Some(&Value::from("image"))
            })
            .count(),
        2
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn relationship_type_uris_require_exact_allowlisted_values() {
    let evil_office_document = "https://evil.invalid/officeDocument";
    let docx = zip_bytes(vec![
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
            package_relationships_with_type("word/document.xml", evil_office_document).into_bytes(),
        ),
        (
            "word/document.xml".into(),
            format!(r#"<w:document xmlns:w="{NS_WORD}"><w:body/></w:document>"#).into_bytes(),
        ),
    ]);
    assert_rejected("evil-office-uri.docx", &docx, "office_format_mismatch");

    assert_rejected(
        "evil-worksheet-uri.xlsx",
        &xlsx_fixture_with_sheet_type("https://evil.invalid/worksheet"),
        "office_relationship_invalid",
    );
    assert_rejected(
        "evil-slide-uri.pptx",
        &pptx_fixture_with_slide_type("https://evil.invalid/slide"),
        "office_relationship_invalid",
    );
}

#[test]
fn nested_odf_encryption_markup_fails_closed_without_manifest_payload() {
    let sentinel = "ODF_ENCRYPTION_SENTINEL_C841";
    let media_type = "application/vnd.oasis.opendocument.text";
    let content = format!(
        r#"<office:document-content xmlns:office="{NS_ODF_OFFICE}" xmlns:text="{NS_ODF_TEXT}"><office:body><office:text><text:p>Safe ODT text</text:p></office:text></office:body></office:document-content>"#
    );
    let manifest = format!(
        r#"<manifest:manifest xmlns:manifest="{NS_ODF_MANIFEST}">
<manifest:file-entry manifest:full-path="/" manifest:media-type="{media_type}"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"><manifest:encryption-data manifest:checksum="{sentinel}"><manifest:algorithm manifest:algorithm-name="urn:test"/></manifest:encryption-data></manifest:file-entry>
</manifest:manifest>"#
    );
    let extraction = assert_rejected(
        "encrypted.odt",
        &document_package_zip(
            media_type,
            vec![
                ("content.xml".into(), content.into_bytes()),
                ("META-INF/manifest.xml".into(), manifest.into_bytes()),
            ],
        ),
        "office_encrypted",
    );
    assert_no_payload(&extraction, sentinel);
}

fn sensitive_epub(sentinel: &str, spine_references_sensitive_item: bool) -> Vec<u8> {
    let container = format!(
        r#"<container xmlns="{NS_EPUB_CONTAINER}" version="1.0"><rootfiles><rootfile full-path="EPUB/package.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
    );
    let sensitive_manifest = if spine_references_sensitive_item {
        r#"<item id="secret" href="secrets/password.xhtml" media-type="application/xhtml+xml"/>"#
    } else {
        ""
    };
    let sensitive_spine = if spine_references_sensitive_item {
        r#"<itemref idref="secret"/>"#
    } else {
        ""
    };
    let opf = format!(
        r#"<package xmlns="{NS_OPF}" version="3.0"><metadata xmlns:dc="{NS_DC}"><dc:title>Sensitive screening</dc:title></metadata><manifest><item id="safe" href="safe.xhtml" media-type="application/xhtml+xml"/>{sensitive_manifest}</manifest><spine><itemref idref="safe"/>{sensitive_spine}</spine></package>"#
    );
    let safe = format!(
        r#"<html xmlns="{NS_XHTML}"><head><title>Safe</title></head><body><p>Visible text</p></body></html>"#
    );
    let sensitive = format!(
        r#"<html xmlns="{NS_XHTML}"><head><title>{sentinel}</title></head><body><p>{sentinel}</p></body></html>"#
    );
    document_package_zip(
        "application/epub+zip",
        vec![
            ("META-INF/container.xml".into(), container.into_bytes()),
            ("EPUB/package.opf".into(), opf.into_bytes()),
            ("EPUB/safe.xhtml".into(), safe.into_bytes()),
            ("EPUB/secrets/password.xhtml".into(), sensitive.into_bytes()),
        ],
    )
}

#[test]
fn epub_sensitive_members_are_skipped_and_never_leak_when_spine_referenced() {
    let sentinel = "EPUB_SENSITIVE_SENTINEL_59D2";
    let unreferenced = extract_source("unreferenced.epub", &sensitive_epub(sentinel, false));
    assert_eq!(
        package_root(&unreferenced).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_no_payload(&unreferenced, sentinel);

    let referenced = assert_rejected(
        "referenced.epub",
        &sensitive_epub(sentinel, true),
        "office_required_part_missing",
    );
    assert_no_payload(&referenced, sentinel);
}

#[test]
fn package_xml_requires_declared_schema_ancestors_and_section_order() {
    let sentinel = "INVALID_SHAPE_SENTINEL_23A7";
    let mut cases = Vec::new();

    let docx = format!(
        r#"<w:document xmlns:w="{NS_WORD}"><w:t>{sentinel}</w:t><w:body><w:p><w:r><w:t>safe</w:t></w:r></w:p></w:body></w:document>"#
    );
    cases.push((
        "docx-text-outside-body.docx",
        replace_zip_member(&docx_fixture(&["safe"], None), "word/document.xml", &docx),
        "office_xml_malformed",
    ));
    for element in ["tab", "br", "sectPr"] {
        let invalid = format!(
            r#"<w:document xmlns:w="{NS_WORD}"><w:{element}/><w:body><w:p><w:r><w:t>safe</w:t></w:r></w:p></w:body></w:document>"#
        );
        cases.push((
            match element {
                "tab" => "docx-empty-tab-outside-body.docx",
                "br" => "docx-empty-break-outside-body.docx",
                _ => "docx-empty-section-properties-outside-body.docx",
            },
            replace_zip_member(
                &docx_fixture(&["safe"], None),
                "word/document.xml",
                &invalid,
            ),
            "office_xml_malformed",
        ));
    }

    let workbook = format!(
        r#"<workbook xmlns="{NS_SHEET}" xmlns:r="{NS_OFFICE_RELS}"><sheet name="{sentinel}" sheetId="3" r:id="rId1"/><sheets><sheet name="safe" sheetId="1" r:id="rId1"/></sheets></workbook>"#
    );
    cases.push((
        "xlsx-sheet-wrong-ancestor.xlsx",
        replace_zip_member(&xlsx_fixture(), "xl/workbook.xml", &workbook),
        "office_xml_malformed",
    ));
    let worksheet = format!(
        r#"<worksheet xmlns="{NS_SHEET}"><sheetData><c r="A1" t="inlineStr"><t>{sentinel}</t></c></sheetData></worksheet>"#
    );
    cases.push((
        "xlsx-cell-wrong-ancestor.xlsx",
        replace_zip_member(&xlsx_fixture(), "xl/worksheets/sheet1.xml", &worksheet),
        "office_xml_malformed",
    ));
    let shared_strings =
        format!(r#"<sst xmlns="{NS_SHEET}"><ext><si><t>{sentinel}</t></si></ext></sst>"#);
    cases.push((
        "xlsx-shared-string-wrong-ancestor.xlsx",
        replace_zip_member(&xlsx_fixture(), "xl/sharedStrings.xml", &shared_strings),
        "office_xml_malformed",
    ));

    let presentation = format!(
        r#"<p:presentation xmlns:p="{NS_PRESENTATION}" xmlns:r="{NS_OFFICE_RELS}"><p:sldId id="999" r:id="rId1"/><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst><p:custData>{sentinel}</p:custData></p:presentation>"#
    );
    cases.push((
        "pptx-slide-id-wrong-ancestor.pptx",
        replace_zip_member(&pptx_fixture(), "ppt/presentation.xml", &presentation),
        "office_xml_malformed",
    ));
    let slide_text = format!(
        r#"<p:sld xmlns:p="{NS_PRESENTATION}" xmlns:a="{NS_DRAWING}"><a:t>{sentinel}</a:t><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>safe</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
    );
    cases.push((
        "pptx-text-outside-content-tree.pptx",
        replace_zip_member(&pptx_fixture(), "ppt/slides/slide1.xml", &slide_text),
        "office_xml_malformed",
    ));

    let odf_outside_body = format!(
        r#"<office:document-content xmlns:office="{NS_ODF_OFFICE}" xmlns:text="{NS_ODF_TEXT}"><office:body><office:text><text:p>safe</text:p></office:text></office:body><text:p>{sentinel}</text:p></office:document-content>"#
    );
    cases.push((
        "odf-paragraph-outside-typed-body.odt",
        replace_zip_member(&odf_fixture("odt"), "content.xml", &odf_outside_body),
        "office_xml_malformed",
    ));
    let duplicate_odf_body = format!(
        r#"<office:document-content xmlns:office="{NS_ODF_OFFICE}" xmlns:text="{NS_ODF_TEXT}"><office:body><office:text><text:p>safe</text:p></office:text></office:body><office:body><office:text><text:p>{sentinel}</text:p></office:text></office:body></office:document-content>"#
    );
    cases.push((
        "odf-duplicate-body.odt",
        replace_zip_member(&odf_fixture("odt"), "content.xml", &duplicate_odf_body),
        "office_format_mismatch",
    ));
    let backslash_manifest = format!(
        r#"<manifest:manifest xmlns:manifest="{NS_ODF_MANIFEST}"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="invalid\{sentinel}.xml" manifest:media-type="text/xml"/></manifest:manifest>"#
    );
    cases.push((
        "odf-backslash-manifest.odt",
        replace_zip_member(
            &odf_fixture("odt"),
            "META-INF/manifest.xml",
            &backslash_manifest,
        ),
        "office_relationship_invalid",
    ));

    let ods_table_outside = format!(
        r#"<office:document-content xmlns:office="{NS_ODF_OFFICE}" xmlns:text="{NS_ODF_TEXT}" xmlns:table="{NS_ODF_TABLE}"><office:body><office:spreadsheet><table:table table:name="safe"><table:table-row><table:table-cell><text:p>safe</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body><table:table table:name="{sentinel}"/></office:document-content>"#
    );
    cases.push((
        "ods-table-outside-spreadsheet.ods",
        replace_zip_member(&odf_fixture("ods"), "content.xml", &ods_table_outside),
        "office_xml_malformed",
    ));
    let ods_cell_outside = format!(
        r#"<office:document-content xmlns:office="{NS_ODF_OFFICE}" xmlns:text="{NS_ODF_TEXT}" xmlns:table="{NS_ODF_TABLE}"><office:body><office:spreadsheet><table:table table:name="safe"><table:table-row><table:table-cell><text:p>safe</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body><table:table-cell><text:p>{sentinel}</text:p></table:table-cell></office:document-content>"#
    );
    cases.push((
        "ods-cell-outside-spreadsheet.ods",
        replace_zip_member(&odf_fixture("ods"), "content.xml", &ods_cell_outside),
        "office_xml_malformed",
    ));
    let odp_page_outside = format!(
        r#"<office:document-content xmlns:office="{NS_ODF_OFFICE}" xmlns:text="{NS_ODF_TEXT}" xmlns:draw="{NS_ODF_DRAW}"><office:body><office:presentation><draw:page draw:name="safe"><draw:frame><draw:text-box><text:p>safe</text:p></draw:text-box></draw:frame></draw:page></office:presentation></office:body><draw:page draw:name="{sentinel}"/></office:document-content>"#
    );
    cases.push((
        "odp-page-outside-presentation.odp",
        replace_zip_member(&odf_fixture("odp"), "content.xml", &odp_page_outside),
        "office_xml_malformed",
    ));

    let metadata =
        format!(r#"<metadata xmlns:dc="{NS_DC}"><dc:title>{sentinel}</dc:title></metadata>"#);
    let manifest = r#"<manifest><item id="one" href="chapter1.xhtml" media-type="application/xhtml+xml"/><item id="two" href="chapter2.xhtml" media-type="application/xhtml+xml"/></manifest>"#;
    let spine = r#"<spine><itemref idref="one"/></spine>"#;
    let opf =
        |sections: &str| format!(r#"<package xmlns="{NS_OPF}" version="3.0">{sections}</package>"#);
    let epub_opf_cases = [
        (
            "epub-missing-metadata.epub",
            opf(&format!("{manifest}{spine}")),
        ),
        (
            "epub-missing-manifest.epub",
            opf(&format!("{metadata}{spine}")),
        ),
        (
            "epub-missing-spine.epub",
            opf(&format!("{metadata}{manifest}")),
        ),
        (
            "epub-duplicate-metadata.epub",
            opf(&format!("{metadata}{metadata}{manifest}{spine}")),
        ),
        (
            "epub-duplicate-manifest.epub",
            opf(&format!("{metadata}{manifest}{manifest}{spine}")),
        ),
        (
            "epub-duplicate-spine.epub",
            opf(&format!("{metadata}{manifest}{spine}{spine}")),
        ),
        (
            "epub-manifest-before-metadata.epub",
            opf(&format!("{manifest}{metadata}{spine}")),
        ),
        (
            "epub-spine-before-manifest.epub",
            opf(&format!("{metadata}{spine}{manifest}")),
        ),
        (
            "epub-duplicate-external-manifest-id.epub",
            opf(&format!(
                r#"{metadata}<manifest><item id="external" href="https://one.invalid/{sentinel}" media-type="application/xhtml+xml"/><item id="external" href="https://two.invalid/{sentinel}" media-type="application/xhtml+xml"/><item id="one" href="chapter1.xhtml" media-type="application/xhtml+xml"/></manifest>{spine}"#
            )),
        ),
    ];
    for (name, invalid_opf) in epub_opf_cases {
        cases.push((
            name,
            replace_zip_member(&epub_fixture(), "EPUB/package.opf", &invalid_opf),
            "office_xml_malformed",
        ));
    }

    let xhtml_cases = [
        (
            "epub-missing-body.epub",
            format!(r#"<html xmlns="{NS_XHTML}"><head><title>{sentinel}</title></head></html>"#),
        ),
        (
            "epub-nested-body.epub",
            format!(
                r#"<html xmlns="{NS_XHTML}"><head/><body><div><body><p>{sentinel}</p></body></div></body></html>"#
            ),
        ),
        (
            "epub-multiple-body.epub",
            format!(
                r#"<html xmlns="{NS_XHTML}"><head/><body><p>safe</p></body><body><p>{sentinel}</p></body></html>"#
            ),
        ),
        (
            "epub-link-outside-body.epub",
            format!(
                r#"<html xmlns="{NS_XHTML}"><head><a href="https://example.invalid/{sentinel}">outside</a></head><body><p>safe</p></body></html>"#
            ),
        ),
    ];
    for (name, invalid_xhtml) in xhtml_cases {
        cases.push((
            name,
            replace_zip_member(&epub_fixture(), "EPUB/chapter1.xhtml", &invalid_xhtml),
            "office_xml_malformed",
        ));
    }

    for (name, archive, diagnostic) in cases {
        let extraction = assert_rejected(name, &archive, diagnostic);
        assert_no_payload(&extraction, sentinel);
    }
}

#[test]
fn worst_case_json_escaped_text_hits_the_fact_cap_before_publication() {
    let escaped_text = "\\\"".repeat(128 * 1024 - 1);
    assert_eq!(escaped_text.len(), 256 * 1024 - 2);
    let document = format!(
        r#"<w:document xmlns:w="{NS_WORD}"><w:body><w:p><w:r><w:t>{escaped_text}</w:t></w:r></w:p></w:body></w:document>"#
    );
    let extraction = assert_rejected(
        "escaped-fact.docx",
        &zip_bytes_with_method(
            vec![
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
            ],
            CompressionMethod::Stored,
        ),
        "office_fact_limit",
    );
    assert_no_payload(&extraction, "\\\"\\\"\\\"\\\"");
}

#[test]
fn hostile_xml_namespaces_active_parts_and_targets_fail_closed() {
    let sentinel = "FORBIDDEN_XML_PAYLOAD_6F2A";
    // A separate minimal package avoids mutating ZIP internals in-place.
    let dangerous_document = format!(
        r#"<?xml version="1.0"?><!DOCTYPE w:document [<!ENTITY x "{sentinel}">]><w:document xmlns:w="{NS_WORD}"><w:body><w:p><w:r><w:t>&x;</w:t></w:r></w:p></w:body></w:document>"#
    );
    let doctype = zip_bytes(vec![
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
        ("word/document.xml".into(), dangerous_document.into_bytes()),
    ]);
    assert_no_payload(
        &assert_rejected("doctype.docx", &doctype, "office_xml_doctype_forbidden"),
        sentinel,
    );

    let wrong_namespace = zip_bytes(vec![
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
        (
            "word/document.xml".into(),
            format!(r#"<w:document><w:body><w:p>{sentinel}</w:p></w:body></w:document>"#)
                .into_bytes(),
        ),
    ]);
    assert_no_payload(
        &assert_rejected(
            "namespace.docx",
            &wrong_namespace,
            "office_xml_namespace_unsupported",
        ),
        sentinel,
    );

    let mut active_entries = vec![
        ("[Content_Types].xml".into(), content_types("word/document.xml", "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml").into_bytes()),
        ("_rels/.rels".into(), package_relationships("word/document.xml").into_bytes()),
        ("word/document.xml".into(), format!(r#"<w:document xmlns:w="{NS_WORD}"><w:body><w:p><w:r><w:t>safe</w:t></w:r></w:p></w:body></w:document>"#).into_bytes()),
    ];
    active_entries.push(("word/vbaProject.bin".into(), sentinel.as_bytes().to_vec()));
    assert_no_payload(
        &assert_rejected(
            "active.docx",
            &zip_bytes(active_entries),
            "office_active_content_unsupported",
        ),
        sentinel,
    );

    let encoded_traversal = format!(
        r#"<Relationships xmlns="{NS_PACKAGE_RELS}"><Relationship Id="rIdRoot" Type="{NS_OFFICE_RELS}/officeDocument" Target="%2e%2e/{sentinel}.xml"/></Relationships>"#
    );
    let traversal_package = zip_bytes(vec![
        (
            "[Content_Types].xml".into(),
            content_types(
                "word/document.xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
            )
            .into_bytes(),
        ),
        ("_rels/.rels".into(), encoded_traversal.into_bytes()),
        (
            "word/document.xml".into(),
            format!(r#"<w:document xmlns:w="{NS_WORD}"><w:body/></w:document>"#).into_bytes(),
        ),
    ]);
    assert_no_payload(
        &assert_rejected(
            "target-traversal.docx",
            &traversal_package,
            "office_relationship_invalid",
        ),
        sentinel,
    );
}

#[test]
fn archive_relationship_and_nesting_ceilings_are_stable_rejections() {
    assert_rejected(
        "encrypted.docx",
        &mark_first_member_encrypted(docx_fixture(&["not published"], None)),
        "office_encrypted",
    );

    assert_rejected(
        "traversal.docx",
        &zip_bytes(vec![("../outside.xml".into(), b"not published".to_vec())]),
        "office_archive_invalid",
    );

    let mut too_many_members = Vec::with_capacity(1_025);
    for index in 0..1_025 {
        too_many_members.push((format!("padding/{index:04}.bin"), Vec::new()));
    }
    assert_rejected(
        "members.docx",
        &zip_bytes(too_many_members),
        "office_archive_limit",
    );

    let ratio_payload = vec![b'A'; 512 * 1024];
    let ratio = zip_bytes(vec![("word/document.xml".into(), ratio_payload)]);
    assert_rejected("ratio.docx", &ratio, "office_archive_limit");

    let mut relationship_xml = format!("<Relationships xmlns=\"{NS_PACKAGE_RELS}\">");
    for index in 0..2_049 {
        relationship_xml.push_str(&format!(
            "<Relationship Id=\"r{index}\" Type=\"{NS_OFFICE_RELS}/customXml\" Target=\"word/item{index}.xml\"/>"
        ));
    }
    relationship_xml.push_str("</Relationships>");
    let relationship_limit = zip_bytes(vec![
        (
            "[Content_Types].xml".into(),
            content_types(
                "word/document.xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
            )
            .into_bytes(),
        ),
        ("_rels/.rels".into(), relationship_xml.into_bytes()),
        (
            "word/document.xml".into(),
            format!(r#"<w:document xmlns:w="{NS_WORD}"><w:body/></w:document>"#).into_bytes(),
        ),
    ]);
    assert_rejected(
        "relationships.docx",
        &relationship_limit,
        "office_relationship_limit",
    );

    let mut nested = format!(r#"<w:document xmlns:w="{NS_WORD}"><w:body>"#);
    nested.push_str(&"<w:custom>".repeat(129));
    nested.push_str("safe");
    nested.push_str(&"</w:custom>".repeat(129));
    nested.push_str("</w:body></w:document>");
    let nesting_limit = zip_bytes(vec![
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
        ("word/document.xml".into(), nested.into_bytes()),
    ]);
    assert_rejected("nesting.docx", &nesting_limit, "office_xml_nesting_limit");
}
