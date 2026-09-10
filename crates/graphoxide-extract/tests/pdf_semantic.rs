use flate2::{write::ZlibEncoder, Compression};
use graphoxide_core::{sanitize_metadata_string, Edge, Extraction, Node};
use graphoxide_extract::{extract, pdf_embedded_attachment};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Write as _},
    path::Path,
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

const MIB: usize = 1024 * 1024;
const MAX_SERIALIZED_FACT_BYTES: usize = MIB;

type PdfObject = (u32, Vec<u8>);

fn extract_at(path: &Path, bytes: &[u8]) -> Extraction {
    fs::write(path, bytes).expect("write deterministic PDF fixture");
    extract(path).expect("extract PDF fixture")
}

fn extract_source(name: &str, bytes: &[u8]) -> Extraction {
    let project = tempfile::tempdir().expect("create PDF fixture directory");
    extract_at(&project.path().join(name), bytes)
}

fn pdf_document(extraction: &Extraction) -> &Node {
    extraction
        .nodes
        .iter()
        .find(|node| node.extra.get("type").and_then(Value::as_str) == Some("pdf_document"))
        .expect("PDF document node")
}

fn pdf_pages(extraction: &Extraction) -> Vec<&Node> {
    let mut pages = extraction
        .nodes
        .iter()
        .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some("pdf_page"))
        .collect::<Vec<_>>();
    pages.sort_unstable_by_key(|node| {
        node.extra
            .get("page_number")
            .and_then(Value::as_u64)
            .expect("numeric PDF page provenance")
    });
    pages
}

fn assert_rejected(name: &str, bytes: &[u8], diagnostic: &str) -> Extraction {
    let extraction = extract_source(name, bytes);
    assert_eq!(
        extraction.nodes.len(),
        1,
        "{name}: document root only (expected {diagnostic})"
    );
    assert!(extraction.edges.is_empty(), "{name}: no partial edges");
    assert!(
        extraction.hyperedges.is_empty(),
        "{name}: no partial hyperedges"
    );
    let root = pdf_document(&extraction);
    assert_eq!(
        root.extra.get("parse_status"),
        Some(&Value::from("rejected")),
        "{name}: parse status"
    );
    assert_eq!(
        root.extra.get("diagnostic"),
        Some(&Value::from(diagnostic)),
        "{name}: rejection diagnostic"
    );
    assert_eq!(
        root.extra.get("format_capability"),
        Some(&Value::from("structural_partial"))
    );
    assert_eq!(root.extra.get("_origin"), Some(&Value::from("pdf")));
    assert_eq!(root.file_type, "paper");
    assert_eq!(root.source_location, None);
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

fn encrypted_pdf(user_password: &str, payload: &str) -> Vec<u8> {
    let source = one_page_pdf(
        vec![stream_body(&literal_text_content(payload.as_bytes()), b"")],
        b"",
        b"",
        vec![],
    );
    encrypt_pdf(source, "fixture-owner-password", user_password)
}

fn encrypt_pdf(source: Vec<u8>, owner_password: &str, user_password: &str) -> Vec<u8> {
    let mut document = lopdf::Document::load_mem(&source).expect("load PDF before encryption");
    document.trailer.set(
        "ID",
        lopdf::Object::Array(vec![
            lopdf::Object::String(vec![1; 16], lopdf::StringFormat::Hexadecimal),
            lopdf::Object::String(vec![2; 16], lopdf::StringFormat::Hexadecimal),
        ]),
    );
    let encryption = lopdf::EncryptionVersion::V2 {
        document: &document,
        owner_password,
        user_password,
        key_length: 128,
        permissions: lopdf::Permissions::all(),
    };
    let encryption = lopdf::EncryptionState::try_from(encryption).expect("build PDF encryption");
    document.encrypt(&encryption).expect("encrypt PDF fixture");
    let mut output = Vec::new();
    document
        .save_to(&mut output)
        .expect("serialize encrypted PDF fixture");
    output
}

fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (path, bytes) in entries {
        writer.start_file(path, options).expect("start ZIP member");
        writer.write_all(bytes).expect("write ZIP member");
    }
    writer.finish().expect("finish ZIP fixture").into_inner()
}

fn encrypted_pdf_with_attachment(name: &str, bytes: &[u8]) -> Vec<u8> {
    let encoded = deflate(bytes);
    encrypt_pdf(
        pdf_with_attachment(
            name,
            stream_body(
                &encoded,
                format!(
                    "/Type /EmbeddedFile /Filter /FlateDecode /Params << /Size {} >>",
                    bytes.len()
                )
                .as_bytes(),
            ),
        ),
        "fixture-owner-password",
        "",
    )
}

fn pdf_with_attachment(name: &str, attachment_object: Vec<u8>) -> Vec<u8> {
    let file_spec =
        format!("<< /Type /Filespec /F ({name}) /UF ({name}) /EF << /F 6 0 R >> >>").into_bytes();
    let catalog = format!("/Names << /EmbeddedFiles << /Names [({name}) 7 0 R] >> >>");
    one_page_pdf(
        vec![stream_body(&literal_text_content(b"page text"), b"")],
        catalog.as_bytes(),
        b"",
        vec![(6, attachment_object), (7, file_spec)],
    )
}

fn encrypted_pdf_with_duplicate_attachment_names(name: &str) -> Vec<u8> {
    let first = stream_body(b"first payload", b"/Type /EmbeddedFile");
    let second = stream_body(b"second payload", b"/Type /EmbeddedFile");
    let first_spec =
        format!("<< /Type /Filespec /F ({name}) /UF ({name}) /EF << /F 6 0 R >> >>").into_bytes();
    let second_spec =
        format!("<< /Type /Filespec /F ({name}) /UF ({name}) /EF << /F 8 0 R >> >>").into_bytes();
    let catalog =
        format!("/Names << /EmbeddedFiles << /Names [({name}) 7 0 R ({name}) 9 0 R] >> >>");
    encrypt_pdf(
        one_page_pdf(
            vec![stream_body(&literal_text_content(b"page text"), b"")],
            catalog.as_bytes(),
            b"",
            vec![(6, first), (7, first_spec), (8, second), (9, second_spec)],
        ),
        "fixture-owner-password",
        "",
    )
}

fn encrypted_pdf_with_attachment_count(count: usize) -> Vec<u8> {
    let mut names = b"/Names << /EmbeddedFiles << /Names [".to_vec();
    let mut objects = Vec::new();
    for index in 0..count {
        let index = u32::try_from(index).expect("attachment index");
        let stream_id = 6 + index * 2;
        let filespec_id = stream_id + 1;
        write!(&mut names, " (file-{index:03}.txt) {filespec_id} 0 R")
            .expect("write attachment name entry");
        objects.push((
            stream_id,
            stream_body(b"", b"/Type /EmbeddedFile /Params << /Size 0 >>"),
        ));
        objects.push((
            filespec_id,
            format!("<< /Type /Filespec /F (file-{index:03}.txt) /EF << /F {stream_id} 0 R >> >>")
                .into_bytes(),
        ));
    }
    names.extend_from_slice(b" ] >> >>");
    encrypt_pdf(
        one_page_pdf(
            vec![stream_body(&literal_text_content(b"page text"), b"")],
            &names,
            b"",
            objects,
        ),
        "fixture-owner-password",
        "",
    )
}

fn encrypted_pdf_with_deep_attachment_tree(depth: usize) -> Vec<u8> {
    assert!(depth > 0);
    let mut objects = Vec::new();
    for index in 0..depth {
        let id = 6 + u32::try_from(index).expect("name-tree depth");
        let body = if index + 1 == depth {
            b"<< /Names [(deep.txt) 101 0 R] >>".to_vec()
        } else {
            format!("<< /Kids [{} 0 R] >>", id + 1).into_bytes()
        };
        objects.push((id, body));
    }
    objects.push((
        100,
        stream_body(b"deep", b"/Type /EmbeddedFile /Params << /Size 4 >>"),
    ));
    objects.push((
        101,
        b"<< /Type /Filespec /F (deep.txt) /EF << /F 100 0 R >> >>".to_vec(),
    ));
    encrypt_pdf(
        one_page_pdf(
            vec![stream_body(&literal_text_content(b"page text"), b"")],
            b"/Names << /EmbeddedFiles 6 0 R >>",
            b"",
            objects,
        ),
        "fixture-owner-password",
        "",
    )
}

fn render_classic(objects: Vec<PdfObject>, trailer_extra: &[u8]) -> Vec<u8> {
    render_classic_with_options(objects, trailer_extra, None, None)
}

fn render_classic_with_options(
    mut objects: Vec<PdfObject>,
    trailer_extra: &[u8],
    size_override: Option<usize>,
    offset_delta: Option<(u32, isize)>,
) -> Vec<u8> {
    objects.sort_unstable_by_key(|(id, _)| *id);
    assert!(
        objects.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "fixture object IDs must be unique"
    );
    let size = objects
        .last()
        .map_or(1, |(id, _)| usize::try_from(*id).expect("object ID") + 1);
    let mut offsets = vec![None; size];
    let mut output = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    for (id, body) in &objects {
        let index = usize::try_from(*id).expect("object ID fits usize");
        offsets[index] = Some(output.len());
        writeln!(&mut output, "{id} 0 obj").expect("write object header");
        output.extend_from_slice(body);
        output.extend_from_slice(b"\nendobj\n");
    }

    let xref_offset = output.len();
    write!(&mut output, "xref\n0 {size}\n").expect("write xref header");
    output.extend_from_slice(b"0000000000 65535 f \n");
    for (id, object_offset) in offsets.iter().copied().enumerate().skip(1) {
        if let Some(mut offset) = object_offset {
            if offset_delta.is_some_and(|(target, _)| usize::try_from(target).ok() == Some(id)) {
                let delta = offset_delta.expect("checked offset delta").1;
                offset = offset
                    .checked_add_signed(delta)
                    .expect("fixture xref delta remains in range");
            }
            writeln!(&mut output, "{offset:010} 00000 n ").expect("write xref row");
        } else {
            output.extend_from_slice(b"0000000000 00000 f \n");
        }
    }
    let trailer_size = size_override.unwrap_or(size);
    write!(&mut output, "trailer\n<< /Size {trailer_size} /Root 1 0 R").expect("write trailer");
    if !trailer_extra.is_empty() {
        output.push(b' ');
        output.extend_from_slice(trailer_extra);
    }
    write!(&mut output, " >>\nstartxref\n{xref_offset}\n%%EOF\n").expect("write PDF footer");
    output
}

fn stream_body(data: &[u8], dictionary_entries: &[u8]) -> Vec<u8> {
    stream_body_with_length(data, dictionary_entries, data.len())
}

fn stream_body_with_length(
    data: &[u8],
    dictionary_entries: &[u8],
    declared_length: usize,
) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len().saturating_add(96));
    write!(&mut output, "<< /Length {declared_length}").expect("write stream dictionary");
    if !dictionary_entries.is_empty() {
        output.push(b' ');
        output.extend_from_slice(dictionary_entries);
    }
    output.extend_from_slice(b" >>\nstream\n");
    output.extend_from_slice(data);
    output.extend_from_slice(b"\nendstream");
    output
}

fn one_page_pdf(
    streams: Vec<Vec<u8>>,
    catalog_entries: &[u8],
    trailer_extra: &[u8],
    mut extra_objects: Vec<PdfObject>,
) -> Vec<u8> {
    assert!(
        !streams.is_empty(),
        "one-page fixture needs a content stream"
    );
    let font_id = 4_u32
        .checked_add(u32::try_from(streams.len()).expect("stream count"))
        .expect("font object ID");
    let mut catalog = b"<< /Type /Catalog /Pages 2 0 R".to_vec();
    if !catalog_entries.is_empty() {
        catalog.push(b' ');
        catalog.extend_from_slice(catalog_entries);
    }
    catalog.extend_from_slice(b" >>");

    let contents = if streams.len() == 1 {
        b"4 0 R".to_vec()
    } else {
        let mut refs = b"[".to_vec();
        for index in 0..streams.len() {
            write!(&mut refs, " {} 0 R", 4 + index).expect("write content reference");
        }
        refs.extend_from_slice(b" ]");
        refs
    };
    let mut page = format!(
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font_id} 0 R >> >> /Contents "
    )
    .into_bytes();
    page.extend_from_slice(&contents);
    page.extend_from_slice(b" >>");

    let mut objects = vec![
        (1, catalog),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (3, page),
    ];
    objects.extend(
        streams
            .into_iter()
            .enumerate()
            .map(|(index, body)| (4 + u32::try_from(index).expect("stream index"), body)),
    );
    objects.push((
        font_id,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    ));
    objects.append(&mut extra_objects);
    render_classic(objects, trailer_extra)
}

fn multi_page_pdf(contents: &[Vec<u8>]) -> Vec<u8> {
    let streams = contents
        .iter()
        .map(|content| stream_body(content, b""))
        .collect::<Vec<_>>();
    multi_page_pdf_with_streams(&streams)
}

fn multi_page_pdf_with_streams(streams: &[Vec<u8>]) -> Vec<u8> {
    let font_id = 3_u32
        .checked_add(
            u32::try_from(streams.len())
                .expect("page count")
                .checked_mul(2)
                .expect("page object IDs"),
        )
        .expect("font object ID");
    let mut kids = b"[".to_vec();
    let mut objects = vec![(1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec())];
    for (index, stream) in streams.iter().enumerate() {
        let index = u32::try_from(index).expect("page index");
        let page_id = 3 + index * 2;
        let content_id = page_id + 1;
        write!(&mut kids, " {page_id} 0 R").expect("write page reference");
        objects.push((
            page_id,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font_id} 0 R >> >> /Contents {content_id} 0 R >>"
            )
            .into_bytes(),
        ));
        objects.push((content_id, stream.clone()));
    }
    kids.extend_from_slice(b" ]");
    let mut pages = format!("<< /Type /Pages /Count {} /Kids ", streams.len()).into_bytes();
    pages.extend_from_slice(&kids);
    pages.extend_from_slice(b" >>");
    objects.push((2, pages));
    objects.push((
        font_id,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    ));
    render_classic(objects, b"")
}

fn one_page_pdf_with_font(
    content: &[u8],
    font_dictionary: &[u8],
    extra_objects: Vec<PdfObject>,
) -> Vec<u8> {
    let mut objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        (4, stream_body(content, b"")),
        (5, font_dictionary.to_vec()),
    ];
    objects.extend(extra_objects);
    render_classic(objects, b"")
}

fn literal_string(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len().saturating_add(2));
    output.push(b'(');
    for byte in bytes {
        match *byte {
            b'(' | b')' | b'\\' => {
                output.push(b'\\');
                output.push(*byte);
            }
            0..=31 | 127..=255 => {
                write!(&mut output, "\\{byte:03o}").expect("write PDF octal escape");
            }
            _ => output.push(*byte),
        }
    }
    output.push(b')');
    output
}

fn literal_text_content(bytes: &[u8]) -> Vec<u8> {
    let mut output = b"BT /F1 12 Tf 72 720 Td ".to_vec();
    output.extend_from_slice(&literal_string(bytes));
    output.extend_from_slice(b" Tj ET");
    output
}

fn utf16be(value: &str) -> Vec<u8> {
    let mut output = vec![0xfe, 0xff];
    for unit in value.encode_utf16() {
        output.extend_from_slice(&unit.to_be_bytes());
    }
    output
}

fn hex_string(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len().saturating_mul(2).saturating_add(2));
    output.push(b'<');
    for byte in bytes {
        write!(&mut output, "{byte:02X}").expect("write PDF hex string");
    }
    output.push(b'>');
    output
}

fn deflate(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(bytes)
        .expect("compress PDF fixture stream");
    encoder.finish().expect("finish PDF fixture stream")
}

fn assert_fact_sizes(extraction: &Extraction) {
    for node in &extraction.nodes {
        let bytes = serde_json::to_vec(node).expect("serialize PDF node fact");
        assert!(
            bytes.len() < MAX_SERIALIZED_FACT_BYTES,
            "node {} serialized to {} bytes",
            node.id,
            bytes.len()
        );
    }
    for edge in &extraction.edges {
        let bytes = serde_json::to_vec(edge).expect("serialize PDF edge fact");
        assert!(
            bytes.len() < MAX_SERIALIZED_FACT_BYTES,
            "edge {} -> {} serialized to {} bytes",
            edge.source,
            edge.target,
            bytes.len()
        );
    }
}

#[test]
fn classic_pages_metadata_and_text_are_stable_structural_facts() {
    let first = literal_text_content(b"First (literal) \\ page \x80");
    let mut second = b"BT /F1 12 Tf 72 720 Td ".to_vec();
    second.extend_from_slice(&hex_string(b"Second hex page"));
    second.extend_from_slice(b" Tj ET");
    let pdf = multi_page_pdf(&[first, second]);

    let project = tempfile::tempdir().expect("create stable PDF fixture directory");
    let path = project.path().join("pages.pdf");
    let first_extraction = extract_at(&path, &pdf);
    let second_extraction = extract_at(&path, &pdf);
    assert_eq!(
        serde_json::to_vec(&first_extraction).expect("serialize first extraction"),
        serde_json::to_vec(&second_extraction).expect("serialize second extraction"),
        "same path and bytes must produce byte-identical facts"
    );

    let root = pdf_document(&first_extraction);
    assert_eq!(root.file_type, "paper");
    assert_eq!(root.source_location, None);
    assert_eq!(root.extra.get("_origin"), Some(&Value::from("pdf")));
    assert_eq!(root.extra.get("format"), Some(&Value::from("pdf")));
    assert_eq!(
        root.extra.get("format_capability"),
        Some(&Value::from("structural_partial"))
    );
    assert_eq!(
        root.extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(root.extra.get("page_count"), Some(&Value::from(2)));
    assert_eq!(
        root.extra.get("extracted_page_count"),
        Some(&Value::from(2))
    );

    let pages = pdf_pages(&first_extraction);
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].extra.get("page_number"), Some(&Value::from(1)));
    assert_eq!(pages[1].extra.get("page_number"), Some(&Value::from(2)));
    assert_eq!(
        pages[0].extra.get("page_label"),
        Some(&Value::from("Page 1"))
    );
    assert_eq!(
        pages[1].extra.get("page_label"),
        Some(&Value::from("Page 2"))
    );
    assert!(pages[0]
        .extra
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains("First (literal) \\ page €")));
    assert!(pages[1]
        .extra
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains("Second hex page")));
    assert!(pages.iter().all(|page| {
        page.file_type == "paper"
            && page.source_location.is_none()
            && page.extra.get("_origin") == Some(&Value::from("pdf"))
    }));

    let page_ids = pages
        .iter()
        .map(|page| page.id.as_str())
        .collect::<Vec<_>>();
    let contains = first_extraction
        .edges
        .iter()
        .filter(|edge| edge.relation == "contains")
        .collect::<Vec<_>>();
    assert_eq!(contains.len(), 2);
    for (edge, page_id) in contains.iter().zip(page_ids) {
        assert_contains_edge(edge, root, page_id);
    }
    assert!(first_extraction.hyperedges.is_empty());
    assert_fact_sizes(&first_extraction);
}

fn assert_contains_edge(edge: &Edge, root: &Node, page_id: &str) {
    assert_eq!(edge.source, root.id);
    assert_eq!(edge.target, page_id);
    assert_eq!(edge.extra.get("_origin"), Some(&Value::from("pdf")));
    assert_eq!(edge.extra.get("_src"), Some(&Value::from(root.id.clone())));
    assert_eq!(edge.extra.get("_tgt"), Some(&Value::from(page_id)));
    assert_eq!(edge.source_file, root.source_file);
}

#[test]
fn literal_hex_winansi_and_text_positioning_operators_are_supported() {
    let mut content = b"BT /F1 12 Tf 72 720 Td ".to_vec();
    content.extend_from_slice(&literal_string(b"literal one"));
    content.extend_from_slice(b" Tj T* [");
    content.extend_from_slice(&literal_string(b"array"));
    content.extend_from_slice(b" -120 ");
    content.extend_from_slice(&hex_string(b" two"));
    content.extend_from_slice(b"] TJ 0 -20 Td ");
    content.extend_from_slice(&literal_string(b"quoted three"));
    content.extend_from_slice(b" ' 0 0 ");
    content.extend_from_slice(&literal_string(b"double four"));
    content.extend_from_slice(b" \" ET");
    let extraction = extract_source(
        "strings.pdf",
        &one_page_pdf(vec![stream_body(&content, b"")], b"", b"", vec![]),
    );
    let page = pdf_pages(&extraction)
        .into_iter()
        .next()
        .expect("page fact");
    let text = page.extra["text"].as_str().expect("page text");
    for expected in ["literal one", "array", "two", "quoted three", "double four"] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
}

#[test]
fn page_glyph_strings_do_not_treat_a_bom_as_utf16_text() {
    let extraction = extract_source(
        "page-bom.pdf",
        &one_page_pdf(
            vec![stream_body(&literal_text_content(&[0xfe, 0xff, b'A']), b"")],
            b"",
            b"",
            vec![],
        ),
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("þÿA")),
        "Tj/TJ bytes are current-font character codes, not PDF metadata strings"
    );
}

#[test]
fn standard14_missing_encoding_uses_standard_encoding_glyph_names() {
    let content = literal_text_content(&[39, b' ', 96]);
    let extraction = extract_source(
        "standard-encoding.pdf",
        &one_page_pdf_with_font(
            &content,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
            vec![],
        ),
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("’ ‘")),
        "StandardEncoding codes 39 and 96 name quoteright and quoteleft"
    );
}

#[test]
fn winansi_uses_the_pdf_encoding_aliases_not_raw_cp1252_controls() {
    let extraction = extract_source(
        "winansi-aliases.pdf",
        &one_page_pdf(
            vec![stream_body(
                &literal_text_content(&[
                    0x7f, b' ', 0x81, b' ', 0x8d, b' ', 0x8f, b' ', 0x90, b' ', 0x9d, b' ', 0xad,
                ]),
                b"",
            )],
            b"",
            b"",
            vec![],
        ),
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("• • • • • • -"))
    );
}

#[test]
fn flate_content_and_allowlisted_info_metadata_are_bounded_semantics() {
    let content = literal_text_content(b"Flate page text");
    let mut info = b"<< /Title ".to_vec();
    info.extend_from_slice(&hex_string(&utf16be("Bounded ☃ title")));
    info.extend_from_slice(b" /Author ");
    info.extend_from_slice(&literal_string(&utf16be("Graphoxide Ω")));
    info.extend_from_slice(b" /Subject (PDF test) /Keywords (bounded deterministic) /Creator (fixture) /Producer (tests) /CreationDate (D:20260808010101Z) /ModDate (D:20260808020202Z) /Secret (must-ignore) >>");
    let pdf = one_page_pdf(
        vec![stream_body(&deflate(&content), b"/Filter /FlateDecode")],
        b"",
        b"/Info 6 0 R",
        vec![(6, info)],
    );
    let extraction = extract_source("metadata.pdf", &pdf);
    let root = pdf_document(&extraction);
    for (key, expected) in [
        ("title", "Bounded ☃ title"),
        ("author", "Graphoxide Ω"),
        ("subject", "PDF test"),
        ("keywords", "bounded deterministic"),
        ("creator", "fixture"),
        ("producer", "tests"),
        ("creation_date", "D:20260808010101Z"),
        ("modification_date", "D:20260808020202Z"),
    ] {
        assert_eq!(root.extra.get(key).and_then(Value::as_str), Some(expected));
    }
    assert!(!root.extra.contains_key("secret"));
    assert!(pdf_pages(&extraction)[0].extra["text"]
        .as_str()
        .is_some_and(|text| text.contains("Flate page text")));
    assert_fact_sizes(&extraction);
}

#[test]
fn content_stream_arrays_preserve_declared_text_order() {
    let first = stream_body(&literal_text_content(b"first stream"), b"");
    let second_content = literal_text_content(b"second stream");
    let second = stream_body(&deflate(&second_content), b"/Filter /FlateDecode");
    let extraction = extract_source(
        "content-array.pdf",
        &one_page_pdf(vec![first, second], b"", b"", vec![]),
    );
    let root = pdf_document(&extraction);
    assert_eq!(
        root.extra.get("decoded_stream_count"),
        Some(&Value::from(2))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("first stream\nsecond stream"))
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn encrypted_incremental_xref_stream_and_object_stream_forms_fail_closed() {
    let content = stream_body(&literal_text_content(b"ordinary text"), b"");
    let encrypted = one_page_pdf(
        vec![content.clone()],
        b"",
        b"/Encrypt 6 0 R",
        vec![(
            6,
            b"<< /Filter /Standard /V 1 /R 2 /O (ENCRYPTED_SECRET) /U (value) /P -4 >>".to_vec(),
        )],
    );
    assert_no_payload(
        &assert_rejected("encrypted.pdf", &encrypted, "pdf_encrypted"),
        "ENCRYPTED_SECRET",
    );

    let escaped_encrypted = one_page_pdf(
        vec![content.clone()],
        b"",
        b"/Encr#79pt 6 0 R",
        vec![(6, b"<< /Filter /Standard /O (ESCAPED_SECRET) >>".to_vec())],
    );
    assert_no_payload(
        &assert_rejected("escaped-encrypt.pdf", &escaped_encrypted, "pdf_encrypted"),
        "ESCAPED_SECRET",
    );

    // An oversized fixed-width field in a cross-reference stream is a decode
    // bomb and must be rejected (it no longer maps to "unsupported xref").
    let mut xref_stream = b"%PDF-1.7\n".to_vec();
    let offset = xref_stream.len();
    xref_stream.extend_from_slice(
        b"1 0 obj\n<< /Type /XRef /Size 2 /W [1 9223372036854775807 2] /Length 0 >>\nstream\n\nendstream\nendobj\n",
    );
    write!(&mut xref_stream, "startxref\n{offset}\n%%EOF\n").expect("write xref stream footer");
    assert_rejected("xref-stream.pdf", &xref_stream, "pdf_malformed");

    // A cross-reference stream that is not actually a /Type /XRef dictionary is
    // still rejected as an unsupported xref.
    let mut bad_xref = b"%PDF-1.7\n".to_vec();
    let bad_offset = bad_xref.len();
    bad_xref.extend_from_slice(
        b"1 0 obj\n<< /Type /Foo /Size 2 /W [1 4 2] /Length 0 >>\nstream\n\nendstream\nendobj\n",
    );
    write!(&mut bad_xref, "startxref\n{bad_offset}\n%%EOF\n").expect("write footer");
    assert_rejected(
        "xref-stream-not-xref.pdf",
        &bad_xref,
        "pdf_unsupported_xref",
    );
}

#[test]
fn incremental_classic_xref_replaces_page_content_with_latest_revision() {
    let mut pdf = one_page_pdf(
        vec![stream_body(&literal_text_content(b"old revision"), b"")],
        b"",
        b"",
        vec![],
    );
    let previous_xref = pdf
        .windows(b"startxref".len())
        .rposition(|window| window == b"startxref")
        .and_then(|position| {
            let start = position + b"startxref\n".len();
            std::str::from_utf8(&pdf[start..])
                .ok()?
                .lines()
                .next()?
                .parse::<usize>()
                .ok()
        })
        .expect("base cross-reference offset");
    let replacement_offset = pdf.len();
    pdf.extend_from_slice(b"4 0 obj\n");
    pdf.extend_from_slice(&stream_body(&literal_text_content(b"latest revision"), b""));
    pdf.extend_from_slice(b"\nendobj\n");
    let xref_offset = pdf.len();
    write!(
        &mut pdf,
        "xref\n4 1\n{replacement_offset:010} 00000 n \ntrailer\n<< /Size 6 /Root 1 0 R /Prev {previous_xref} >>\nstartxref\n{xref_offset}\n%%EOF\n"
    )
    .expect("append incremental revision");

    let extraction = extract_source("incremental-classic.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete")),
        "{extraction:#?}"
    );
    assert_eq!(pdf_pages(&extraction).len(), 1, "{extraction:#?}");
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("latest revision"))
    );
}

#[test]
fn empty_password_encrypted_pdf_extracts_page_text() {
    let extraction = extract_source(
        "readable-encrypted.pdf",
        &encrypted_pdf("", "Readable encrypted PDF"),
    );
    let pages = pdf_pages(&extraction);
    assert_eq!(pages.len(), 1, "{extraction:#?}");

    assert_eq!(
        pages[0].extra.get("text"),
        Some(&Value::from("Readable encrypted PDF"))
    );
    assert_eq!(
        pages[0].extra.get("media_box_width"),
        Some(&Value::from(612))
    );
    assert_eq!(
        pages[0].extra.get("media_box_height"),
        Some(&Value::from(792))
    );
}

#[test]
fn password_protected_pdf_stays_rejected_without_payload() {
    let payload = "PASSWORD_PROTECTED_PAYLOAD";
    let extraction = assert_rejected(
        "password-protected.pdf",
        &encrypted_pdf("required-password", payload),
        "pdf_encrypted",
    );

    assert_no_payload(&extraction, payload);
}

#[test]
fn empty_owner_password_does_not_bypass_required_user_password() {
    let payload = "EMPTY_OWNER_MUST_NOT_BYPASS_USER_PASSWORD";
    let source = one_page_pdf(
        vec![stream_body(&literal_text_content(payload.as_bytes()), b"")],
        b"",
        b"",
        vec![],
    );
    let extraction = assert_rejected(
        "empty-owner-password.pdf",
        &encrypt_pdf(source, "", "required-password"),
        "pdf_encrypted",
    );
    assert_no_payload(&extraction, payload);
}

#[test]
fn encrypted_embedded_archive_is_routed_through_native_container_dispatch() {
    let archive = zip_bytes(&[("nested/spec.json", br#"{"kind":"fixture"}"#)]);
    let extraction = extract_source(
        "encrypted-attachment.pdf",
        &encrypted_pdf_with_attachment("attachments.zip", &archive),
    );

    let root = pdf_document(&extraction);
    assert_eq!(
        root.extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert!(
        root.extra
            .get("ignored_pdf_features")
            .and_then(Value::as_str)
            .is_none_or(|features| !features
                .split(',')
                .any(|feature| feature == "embedded_files")),
        "processed attachments must not remain an ignored feature: {extraction:#?}"
    );
    assert_eq!(pdf_pages(&extraction).len(), 1, "{extraction:#?}");

    let attachment = extraction
        .nodes
        .iter()
        .find(|node| {
            node.label == "attachments.zip"
                && node.extra.get("member_kind").and_then(Value::as_str) == Some("pdf_attachment")
        })
        .expect("PDF attachment partition");
    assert_eq!(
        attachment.extra.get("dispatch_status"),
        Some(&Value::from("processed"))
    );
    assert_eq!(
        attachment.extra.get("compressed_bytes"),
        Some(&Value::from(
            u64::try_from(deflate(&archive).len()).expect("encoded attachment size")
        ))
    );
    assert_eq!(
        attachment.extra.get("declared_uncompressed_bytes"),
        Some(&Value::from(
            u64::try_from(archive.len()).expect("decoded attachment size")
        ))
    );
    assert!(
        extraction.nodes.iter().any(|node| {
            node.label == "nested/spec.json"
                && node.extra.get("type").and_then(Value::as_str) == Some("container_member")
                && node.source_file.ends_with("!/attachments.zip")
        }),
        "native archive member partition: {extraction:#?}"
    );
    assert_no_payload(&extraction, "PK\u{3}\u{4}");
    assert_fact_sizes(&extraction);
}

#[test]
fn exact_embedded_attachment_bytes_are_available_for_locator_resolution() {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    writer
        .start_file("nested/spec.json", stored)
        .expect("start nested attachment member");
    writer
        .write_all(br#"{"kind":"fixture"}"#)
        .expect("write nested attachment member");
    writer
        .start_file("padding.bin", stored)
        .expect("start attachment padding member");
    let mut state = 0x5eed_1234_u32;
    let padding = (0..(128 * 1024))
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state.to_be_bytes()[0]
        })
        .collect::<Vec<_>>();
    writer
        .write_all(&padding)
        .expect("write attachment padding member");
    let archive = writer.finish().expect("finish attachment").into_inner();
    assert!(archive.len() > 64 * 1024);
    let source = encrypted_pdf_with_attachment("attachments.zip", &archive);

    assert_eq!(
        pdf_embedded_attachment(&source, "attachments.zip", 4 * MIB)
            .expect("resolve the exact admitted PDF attachment"),
        archive
    );
}

#[test]
fn duplicate_embedded_attachment_name_retains_the_unreadable_blocker() {
    let source = encrypted_pdf_with_duplicate_attachment_names("duplicate.txt");
    let extraction = extract_source("duplicate-attachments.pdf", &source);
    assert!(extraction.nodes.iter().any(|node| {
        node.label == "attachment-000002"
            && node.extra.get("blocker").and_then(Value::as_str)
                == Some("pdf-attachment-unreadable")
    }));

    assert_eq!(
        pdf_embedded_attachment(&source, "attachment-000002", 4 * MIB)
            .expect_err("a duplicate attachment is not admitted as source material")
            .code(),
        "pdf-attachment-unreadable"
    );
}

#[test]
fn embedded_attachment_resolver_preserves_count_and_depth_blockers() {
    assert_eq!(
        pdf_embedded_attachment(
            &encrypted_pdf_with_attachment_count(65),
            "not-admitted.txt",
            4 * MIB,
        )
        .expect_err("attachment count overflow stays explicit")
        .code(),
        "pdf-attachment-count-limit"
    );
    assert_eq!(
        pdf_embedded_attachment(
            &encrypted_pdf_with_deep_attachment_tree(10),
            "not-admitted.txt",
            4 * MIB,
        )
        .expect_err("attachment tree depth overflow stays explicit")
        .code(),
        "pdf-attachment-depth-limit"
    );
}

#[test]
fn oversized_encrypted_attachment_keeps_page_and_typed_blocker_partition() {
    let attachment = stream_body(b"small", b"/Type /EmbeddedFile /Params << /Size 5242880 >>");
    let source = encrypt_pdf(
        pdf_with_attachment("oversized.zip", attachment),
        "fixture-owner-password",
        "",
    );
    let extraction = extract_source("oversized-attachment.pdf", &source);

    assert_eq!(pdf_pages(&extraction).len(), 1, "{extraction:#?}");
    let root = pdf_document(&extraction);
    assert_eq!(
        root.extra.get("parse_status"),
        Some(&Value::from("partial"))
    );
    assert!(!root.extra.contains_key("ignored_pdf_features"));
    let attachment = extraction
        .nodes
        .iter()
        .find(|node| node.label == "oversized.zip")
        .expect("oversized attachment partition");
    assert_eq!(
        attachment.extra.get("blocker"),
        Some(&Value::from("pdf-attachment-byte-limit"))
    );
    assert_eq!(
        attachment.extra.get("retry_route"),
        Some(&Value::from("raise-pdf-attachment-byte-limit"))
    );
}

#[test]
fn unreadable_encrypted_attachment_keeps_page_and_typed_blocker_partition() {
    let source = encrypt_pdf(
        pdf_with_attachment("broken.zip", b"<< /Type /EmbeddedFile >>".to_vec()),
        "fixture-owner-password",
        "",
    );
    let extraction = extract_source("broken-attachment.pdf", &source);

    assert_eq!(pdf_pages(&extraction).len(), 1, "{extraction:#?}");
    let root = pdf_document(&extraction);
    assert_eq!(
        root.extra.get("parse_status"),
        Some(&Value::from("partial"))
    );
    assert!(!root.extra.contains_key("ignored_pdf_features"));
    let attachment = extraction
        .nodes
        .iter()
        .find(|node| node.label == "broken.zip")
        .expect("unreadable attachment partition");
    assert_eq!(
        attachment.extra.get("blocker"),
        Some(&Value::from("pdf-attachment-unreadable"))
    );
    assert_eq!(
        attachment.extra.get("retry_route"),
        Some(&Value::from("repair-pdf-attachment"))
    );
}

#[test]
fn locked_pdf_attachment_is_never_enumerated() {
    let payload = "LOCKED_ATTACHMENT_PAYLOAD";
    let source = encrypt_pdf(
        pdf_with_attachment(
            "locked.txt",
            stream_body(payload.as_bytes(), b"/Type /EmbeddedFile"),
        ),
        "fixture-owner-password",
        "required-user-password",
    );
    let extraction = assert_rejected("locked-attachment.pdf", &source, "pdf_encrypted");
    assert_no_payload(&extraction, payload);
    assert!(!extraction.nodes.iter().any(|node| {
        node.extra.get("member_kind").and_then(Value::as_str) == Some("pdf_attachment")
    }));
}

#[test]
fn encrypted_external_attachment_stream_stays_hard_blocked() {
    let payload = "EXTERNAL_ATTACHMENT_PAYLOAD";
    let source = encrypt_pdf(
        pdf_with_attachment(
            "external.txt",
            stream_body(payload.as_bytes(), b"/Type /EmbeddedFile /F (external.bin)"),
        ),
        "fixture-owner-password",
        "",
    );
    let extraction = assert_rejected(
        "external-attachment.pdf",
        &source,
        "pdf_active_content_unsupported",
    );
    assert_no_payload(&extraction, payload);
}

#[test]
fn encrypted_untyped_external_filespec_stays_hard_blocked() {
    let payload = "UNTYPED_EXTERNAL_FILESPEC_PAYLOAD";
    let source = one_page_pdf(
        vec![stream_body(&literal_text_content(b"page"), b"")],
        b"",
        b"",
        vec![(6, format!("<< /FS /URL /F ({payload}) >>").into_bytes())],
    );
    let extraction = assert_rejected(
        "untyped-external-filespec.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
        "pdf_active_content_unsupported",
    );
    assert_no_payload(&extraction, payload);
}

#[test]
fn encrypted_gotoe_action_stays_hard_blocked() {
    let payload = "EXTERNAL_GOTOE_PAYLOAD";
    let source = one_page_pdf(
        vec![stream_body(&literal_text_content(b"page"), b"")],
        b"/OpenAction 6 0 R",
        b"",
        vec![(
            6,
            format!("<< /Type /Action /S /GoToE /D ({payload}) >>").into_bytes(),
        )],
    );
    let extraction = assert_rejected(
        "external-gotoe.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
        "pdf_active_content_unsupported",
    );
    assert_no_payload(&extraction, payload);
}

#[test]
fn encrypted_sensitive_attachment_keeps_authorization_blocker_partition() {
    let payload = "SENSITIVE_ATTACHMENT_PAYLOAD";
    let extraction = extract_source(
        "sensitive-attachment.pdf",
        &encrypted_pdf_with_attachment("secrets/token.json", payload.as_bytes()),
    );

    assert_eq!(pdf_pages(&extraction).len(), 1, "{extraction:#?}");
    let root = pdf_document(&extraction);
    assert_eq!(
        root.extra.get("parse_status"),
        Some(&Value::from("partial"))
    );
    let attachment = extraction
        .nodes
        .iter()
        .find(|node| node.label == "secrets/token.json")
        .expect("sensitive attachment partition");
    assert_eq!(
        attachment.extra.get("dispatch_status"),
        Some(&Value::from("sensitive_path_skipped"))
    );
    assert_eq!(
        attachment.extra.get("blocker"),
        Some(&Value::from("pdf-attachment-sensitive-path"))
    );
    assert_eq!(
        attachment.extra.get("retry_route"),
        Some(&Value::from("authorize-sensitive-member-processing"))
    );
    assert_no_payload(&extraction, payload);
}

#[test]
fn encrypted_attachment_count_limit_is_an_explicit_partition() {
    let extraction = extract_source(
        "many-attachments.pdf",
        &encrypted_pdf_with_attachment_count(65),
    );
    assert_eq!(pdf_pages(&extraction).len(), 1, "{extraction:#?}");
    assert_eq!(
        extraction
            .nodes
            .iter()
            .filter(|node| {
                node.extra.get("member_kind").and_then(Value::as_str) == Some("pdf_attachment")
            })
            .count(),
        65,
        "64 admitted attachments plus one explicit count blocker"
    );
    assert!(extraction.nodes.iter().any(|node| {
        node.extra.get("blocker").and_then(Value::as_str) == Some("pdf-attachment-count-limit")
            && node.extra.get("retry_route").and_then(Value::as_str)
                == Some("raise-pdf-attachment-count-limit")
    }));
    assert!(!pdf_document(&extraction)
        .extra
        .contains_key("ignored_pdf_features"));
}

#[test]
fn encrypted_attachment_tree_depth_limit_is_an_explicit_partition() {
    let extraction = extract_source(
        "deep-attachment-tree.pdf",
        &encrypted_pdf_with_deep_attachment_tree(10),
    );
    assert_eq!(pdf_pages(&extraction).len(), 1, "{extraction:#?}");
    assert!(extraction.nodes.iter().any(|node| {
        node.extra.get("blocker").and_then(Value::as_str) == Some("pdf-attachment-depth-limit")
            && node.extra.get("retry_route").and_then(Value::as_str)
                == Some("raise-pdf-attachment-depth-limit")
    }));
    assert!(!pdf_document(&extraction)
        .extra
        .contains_key("ignored_pdf_features"));
}

#[test]
fn encrypted_catalog_goto_open_action_is_inert_partial_content() {
    let source = one_page_pdf(
        vec![stream_body(&literal_text_content(b"page"), b"")],
        b"/OpenAction 6 0 R",
        b"",
        vec![(
            6,
            b"<< /Type /Action /S /GoTo /D [3 0 R /XYZ null null 1] >>".to_vec(),
        )],
    );
    let extraction = extract_source(
        "encrypted-open-action.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
    );
    let pages = pdf_pages(&extraction);
    assert_eq!(pages.len(), 1, "{extraction:#?}");
    assert_eq!(pages[0].extra.get("text"), Some(&Value::from("page")));
    assert_eq!(
        extraction.nodes[0].extra.get("parse_status"),
        Some(&Value::from("partial"))
    );
    assert_eq!(
        extraction.nodes[0].extra.get("ignored_pdf_features"),
        Some(&Value::from("open_actions"))
    );
}

#[test]
fn encrypted_catalog_direct_destination_open_action_is_inert() {
    let source = one_page_pdf(
        vec![stream_body(&literal_text_content(b"page"), b"")],
        b"/OpenAction [3 0 R /Fit]",
        b"",
        vec![],
    );
    let extraction = extract_source(
        "encrypted-direct-destination.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
    );
    assert_eq!(pdf_pages(&extraction).len(), 1, "{extraction:#?}");
    assert_eq!(
        extraction.nodes[0].extra.get("ignored_pdf_features"),
        Some(&Value::from("open_actions"))
    );
}

#[test]
fn encrypted_catalog_goto_with_next_javascript_stays_rejected() {
    let payload = "NEXT_JAVASCRIPT_MUST_NOT_PUBLISH";
    let action =
        format!("<< /S /GoTo /D [3 0 R /Fit] /Next << /S /JavaScript /JS ({payload}) >> >>");
    let source = one_page_pdf(
        vec![stream_body(&literal_text_content(b"page"), b"")],
        b"/OpenAction 6 0 R",
        b"",
        vec![(6, action.into_bytes())],
    );
    let extraction = assert_rejected(
        "encrypted-next-action.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
        "pdf_active_content_unsupported",
    );
    assert_no_payload(&extraction, payload);
}

#[test]
fn encrypted_catalog_external_open_actions_stay_rejected() {
    for (name, action) in [
        (
            "uri",
            b"<< /S /URI /URI (https://example.invalid) >>".as_slice(),
        ),
        (
            "remote",
            b"<< /S /GoToR /D [3 0 R /Fit] /F (remote.pdf) >>".as_slice(),
        ),
    ] {
        let source = one_page_pdf(
            vec![stream_body(&literal_text_content(b"page"), b"")],
            b"/OpenAction 6 0 R",
            b"",
            vec![(6, action.to_vec())],
        );
        assert_rejected(
            &format!("encrypted-{name}-action.pdf"),
            &encrypt_pdf(source, "fixture-owner-password", ""),
            "pdf_active_content_unsupported",
        );
    }
}

#[test]
fn encrypted_catalog_goto_target_must_be_a_current_page() {
    let source = one_page_pdf(
        vec![stream_body(&literal_text_content(b"page"), b"")],
        b"/OpenAction << /S /GoTo /D [2 0 R /Fit] >>",
        b"",
        vec![],
    );
    assert_rejected(
        "encrypted-non-page-action.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
        "pdf_active_content_unsupported",
    );
}

#[test]
fn encrypted_catalog_malformed_open_actions_stay_rejected() {
    for (name, open_action, action) in [
        ("named", b"/OpenAction /ChapterOne".as_slice(), None),
        ("string", b"/OpenAction (ChapterOne)".as_slice(), None),
        ("integer", b"/OpenAction 3".as_slice(), None),
        ("unresolved", b"/OpenAction 99 0 R".as_slice(), None),
        (
            "bad-type",
            b"/OpenAction 6 0 R".as_slice(),
            Some(b"<< /Type /Bogus /S /GoTo /D [3 0 R /Fit] >>".as_slice()),
        ),
        (
            "bad-operands",
            b"/OpenAction 6 0 R".as_slice(),
            Some(b"<< /S /GoTo /D [3 0 R /Fit 1] >>".as_slice()),
        ),
    ] {
        let extra_objects = action.map_or_else(Vec::new, |action| vec![(6, action.to_vec())]);
        let source = one_page_pdf(
            vec![stream_body(&literal_text_content(b"page"), b"")],
            open_action,
            b"",
            extra_objects,
        );
        assert_rejected(
            &format!("encrypted-{name}-open-action.pdf"),
            &encrypt_pdf(source, "fixture-owner-password", ""),
            "pdf_active_content_unsupported",
        );
    }
}

#[test]
fn encrypted_catalog_root_requires_catalog_type_before_open_action_exception() {
    let payload = "MALFORMED_CATALOG_ROOT_PAYLOAD";
    let mut source = one_page_pdf(
        vec![stream_body(&literal_text_content(payload.as_bytes()), b"")],
        b"/OpenAction [3 0 R /Fit]",
        b"",
        vec![],
    );
    let marker = b"/Type /Catalog";
    let offset = source
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("catalog type marker");
    source[offset..offset + marker.len()].copy_from_slice(b"/Type /BogusXX");
    let extraction = assert_rejected(
        "encrypted-malformed-catalog-root.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
        "pdf_active_content_unsupported",
    );
    assert_no_payload(&extraction, payload);
}

#[test]
fn encrypted_dense_multi_page_content_scales_operation_budget() {
    let page = b"q Q ".repeat(601);
    let source = multi_page_pdf(&vec![page; 87]);
    let extraction = extract_source(
        "encrypted-dense-multi-page.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
    );
    assert_eq!(pdf_pages(&extraction).len(), 87, "{extraction:#?}");
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
}

#[test]
fn encrypted_multi_page_streams_share_the_content_operation_budget() {
    let operations = b"q Q ".repeat(50_001);
    let source = multi_page_pdf(&[operations.clone(), operations]);
    assert_rejected(
        "encrypted-operation-budget.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
        "pdf_content_limit",
    );
}

#[test]
fn encrypted_many_page_operation_budget_retains_a_hard_cap() {
    let page = b"q Q ".repeat(490);
    let source = multi_page_pdf(&vec![page; 1_024]);
    assert_rejected(
        "encrypted-hard-operation-cap.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
        "pdf_content_limit",
    );
}

#[test]
fn encrypted_stream_count_is_bounded_document_wide() {
    let streams = (0..2_049)
        .map(|_| stream_body(b"q Q", b""))
        .collect::<Vec<_>>();
    let source = one_page_pdf(streams, b"", b"", vec![]);
    assert_rejected(
        "encrypted-stream-count.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
        "pdf_object_limit",
    );
}

#[test]
fn encrypted_pages_share_the_total_decoded_byte_budget() {
    let decoded = vec![b' '; 4 * 1024 * 1024];
    let compressed = deflate(&decoded);
    let streams = (0..5)
        .map(|_| stream_body(&compressed, b"/Filter /FlateDecode"))
        .collect::<Vec<_>>();
    let source = multi_page_pdf_with_streams(&streams);
    assert_rejected(
        "encrypted-total-decoded.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
        "pdf_decompression_limit",
    );
}

#[test]
fn encrypted_page_font_cmaps_share_the_total_decoded_byte_budget() {
    let mut font_resources = Vec::new();
    for index in 0..5_u32 {
        write!(&mut font_resources, "/F{} {} 0 R ", index + 1, index + 4)
            .expect("write font resource");
    }
    let page = format!(
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << {} >> >> /Contents 9 0 R >>",
        String::from_utf8(font_resources).expect("ASCII font resources")
    )
    .into_bytes();
    let mut objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (3, page),
        (9, stream_body(b"q Q", b"")),
    ];
    let decoded_cmap = vec![b' '; 4 * 1024 * 1024];
    let encoded_cmap = deflate(&decoded_cmap);
    for index in 0..5_u32 {
        objects.push((
            index + 4,
            format!(
                "<< /Type /Font /Subtype /Type0 /BaseFont /Fixture /Encoding /Identity-H /ToUnicode {} 0 R >>",
                index + 10
            )
            .into_bytes(),
        ));
        objects.push((
            index + 10,
            stream_body(&encoded_cmap, b"/Filter /FlateDecode"),
        ));
    }
    let source = render_classic(objects, b"");
    assert_rejected(
        "encrypted-cmap-total-decoded.pdf",
        &encrypt_pdf(source, "fixture-owner-password", ""),
        "pdf_decompression_limit",
    );
}

// Incremental-update ancestry is structural (exact trailer `/Prev` links),
// never the name-lexing blacklist or marker-like stream data. Standard
// pages-tree `/Prev`/`/Next` sibling references and plain `/URI` link
// annotations must not reject their documents (issue #131).

#[test]
fn threaded_pages_tree_prev_next_references_extract() {
    let objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /Next 4 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R >>".to_vec(),
        ),
        (
            4,
            b"<< /Type /Page /Parent 2 0 R /Prev 3 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 7 0 R >>".to_vec(),
        ),
        (
            5,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        ),
        content_stream_obj(6, "first page"),
        content_stream_obj(7, "second page"),
    ];
    let pdf = render_classic(objects, b"");
    let extraction = extract_source("threaded-pages-tree.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction).len(),
        2,
        "both linked pages must publish"
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("first page"))
    );
    assert_eq!(
        pdf_pages(&extraction)[1].extra.get("text"),
        Some(&Value::from("second page"))
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn plain_uri_link_annotation_extracts_without_publishing_the_uri() {
    let uri = "file:///documents/camera-decision-matrix.pdf";
    let objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Annots [6 0 R] /Contents 4 0 R >>".to_vec(),
        ),
        content_stream_obj(4, "page with a plain link"),
        (
            5,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        ),
        (
            6,
            format!(
                "<< /Type /Annot /Subtype /Link /Rect [72 720 200 732] /Border [0 0 0] /A << /Type /Action /S /URI /URI ({uri}) >> >>"
            )
            .into_bytes(),
        ),
    ];
    let pdf = render_classic(objects, b"");
    let extraction = extract_source("plain-uri-annotation.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("page with a plain link"))
    );
    // The extractor never follows URIs and never publishes annotation action
    // strings: only content-stream text and Info metadata reach the graph.
    assert_no_payload(&extraction, uri);
    assert_fact_sizes(&extraction);
}

#[test]
fn xref_stream_prev_reference_still_rejects_incremental_documents() {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let offset = pdf.len();
    // Two 7-byte entries for `/W [1 4 2]`: free head + the xref object.
    let entries: [u8; 14] = [0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, offset as u8, 0, 0];
    pdf.extend_from_slice(
        b"1 0 obj\n<< /Type /XRef /Size 2 /W [1 4 2] /Prev 0 /Length 14 >>\nstream\n",
    );
    pdf.extend_from_slice(&entries);
    pdf.extend_from_slice(b"endstream\nendobj\n");
    write!(&mut pdf, "startxref\n{offset}\n%%EOF\n").expect("write footer");
    assert_rejected("xref-stream-prev.pdf", &pdf, "pdf_incremental_unsupported");
}

#[test]
fn marker_like_bytes_inside_an_unreferenced_stream_are_inert() {
    // Revision ancestry is established by exact trailer `/Prev` links. Marker-
    // like bytes inside an unreferenced stream are data, even when preceded by
    // PDF whitespace, and must not create a false incremental revision.
    let hidden_markers = b"\x0c\0startxref\n0\n\x0c\0%%EOF\n";
    let pdf = one_page_pdf(
        vec![stream_body(&literal_text_content(b"safe page"), b"")],
        b"",
        b"",
        vec![(6, stream_body(hidden_markers, b""))],
    );
    let extraction = extract_source("inert-marker-like-stream.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(pdf_pages(&extraction).len(), 1, "{extraction:#?}");
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("safe page"))
    );
}

#[test]
fn active_actions_javascript_uris_and_attachments_never_publish_payloads() {
    let sentinel = "DO_NOT_PUBLISH_ACTIVE_PAYLOAD_7F32";
    let action = format!(
        "<< /S /JavaScript /JS ({sentinel}) /URI (https://example.invalid/{sentinel}) /EmbeddedFiles << /Names [({sentinel}) 7 0 R] >> >>"
    )
    .into_bytes();
    let attachment = stream_body(sentinel.as_bytes(), b"/Type /EmbeddedFile");
    let pdf = one_page_pdf(
        vec![stream_body(&literal_text_content(b"safe text"), b"")],
        b"/OpenAction 6 0 R /Names << /EmbeddedFiles 6 0 R >>",
        b"",
        vec![(6, action), (7, attachment)],
    );
    let extraction = assert_rejected("active.pdf", &pdf, "pdf_active_content_unsupported");
    assert_no_payload(&extraction, sentinel);

    // External-file stream keys must never be treated as if the inline bytes
    // were the authoritative payload, and the named file must never be read.
    for (name, stream_entry) in [
        ("external-stream-file.pdf", b"/F (outside.bin)".as_slice()),
        (
            "external-stream-filter.pdf",
            b"/FFilter /FlateDecode".as_slice(),
        ),
        (
            "external-stream-params.pdf",
            b"/FDecodeParms << /Predictor 1 >>".as_slice(),
        ),
    ] {
        let pdf = one_page_pdf(
            vec![stream_body(
                &literal_text_content(sentinel.as_bytes()),
                stream_entry,
            )],
            b"",
            b"",
            vec![],
        );
        let extraction = assert_rejected(name, &pdf, "pdf_active_content_unsupported");
        assert_no_payload(&extraction, sentinel);
    }
}

#[test]
fn ambiguous_show_text_operands_fail_closed_without_hidden_text_leakage() {
    for (name, content, hidden) in [
        (
            "extra-tj-operand.pdf",
            b"BT /F1 12 Tf (HIDDEN_UNUSED_OPERAND) (Visible) Tj ET".as_slice(),
            ["HIDDEN_UNUSED_OPERAND"].as_slice(),
        ),
        (
            "outside-tj-array.pdf",
            b"BT /F1 12 Tf (OUTSIDE_BEFORE) [(Visible)] (OUTSIDE_AFTER) TJ ET".as_slice(),
            ["OUTSIDE_BEFORE", "OUTSIDE_AFTER"].as_slice(),
        ),
    ] {
        let pdf = one_page_pdf(vec![stream_body(content, b"")], b"", b"", vec![]);
        let extraction = assert_rejected(name, &pdf, "pdf_malformed");
        for payload in hidden {
            assert_no_payload(&extraction, payload);
        }
    }
}

#[test]
fn unsupported_font_representations_never_publish_misdecoded_text() {
    let sentinel = "UNSUPPORTED_FONT_TEXT_SENTINEL";
    let content = literal_text_content(sentinel.as_bytes());
    let fixtures = [
        (
            "macroman.pdf",
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /MacRomanEncoding >>".as_slice(),
            Vec::new(),
        ),
        (
            "type-zero.pdf",
            b"<< /Type /Font /Subtype /Type0 /BaseFont /Unsupported /Encoding /Identity-H >>".as_slice(),
            Vec::new(),
        ),
        (
            "custom-encoding.pdf",
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << /Type /Encoding /Differences [32 /space] >> >>".as_slice(),
            Vec::new(),
        ),
        (
            "custom-type1-missing-encoding.pdf",
            b"<< /Type /Font /Subtype /Type1 /BaseFont /AttackerControlledFont >>".as_slice(),
            Vec::new(),
        ),
        (
            "to-unicode.pdf",
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding /ToUnicode 6 0 R >>".as_slice(),
            vec![(
                6,
                stream_body(
                    b"1 beginbfrange <00000000> <FFFFFFFF> <0041> endbfrange",
                    b"",
                ),
            )],
        ),
    ];
    for (name, font, extras) in fixtures {
        let extraction = assert_rejected(
            name,
            &one_page_pdf_with_font(&content, font, extras),
            "pdf_font_unsupported",
        );
        assert_no_payload(&extraction, sentinel);
    }
}

#[test]
fn missing_or_unselected_text_fonts_fail_before_text_publication() {
    let sentinel = "TEXT_WITHOUT_A_VALID_SELECTED_FONT";
    let mut missing_resource = b"BT /MissingFont 12 Tf ".to_vec();
    missing_resource.extend_from_slice(&literal_string(sentinel.as_bytes()));
    missing_resource.extend_from_slice(b" Tj ET");
    let before_tf = {
        let mut content = b"BT ".to_vec();
        content.extend_from_slice(&literal_string(sentinel.as_bytes()));
        content.extend_from_slice(b" Tj ET");
        content
    };
    for (name, content) in [
        ("missing-font-resource.pdf", missing_resource),
        ("text-before-tf.pdf", before_tf),
    ] {
        let extraction = assert_rejected(
            name,
            &one_page_pdf(vec![stream_body(&content, b"")], b"", b"", vec![]),
            "pdf_font_unsupported",
        );
        assert_no_payload(&extraction, sentinel);
    }
}

#[test]
fn content_operation_budget_is_aggregate_across_page_streams() {
    let mut operations = Vec::with_capacity(60_000);
    for _ in 0..30_000 {
        operations.extend_from_slice(b"q ");
    }
    let stream = stream_body(&operations, b"");
    let pdf = one_page_pdf(std::iter::repeat_n(stream, 4).collect(), b"", b"", vec![]);
    assert_rejected(
        "aggregate-content-operations.pdf",
        &pdf,
        "pdf_content_limit",
    );
}

#[test]
fn marked_content_dictionary_operands_are_skipped_without_publication() {
    // Tagged PDFs mark structure with `/Name <</...>> BDC ... EMC`
    // sequences. The dictionary operand must be skipped — never published —
    // while the text inside the marked sequence still extracts (issue #139).
    let content = b"/NonStruct <</MCID 0 /ActualText (DICT_SECRET) >> BDC BT /F1 12 Tf 72 720 Td (tagged text) Tj ET EMC";
    let pdf = one_page_pdf(vec![stream_body(content, b"")], b"", b"", vec![]);
    let extraction = extract_source("marked-content.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("tagged text"))
    );
    assert_no_payload(&extraction, "DICT_SECRET");
    assert_fact_sizes(&extraction);
}

#[test]
fn nested_content_dictionaries_with_arrays_are_skipped() {
    // Property dictionaries may nest dictionaries and arrays (e.g. /BBox,
    // /C, /ActualText). All of it is consumed without publication.
    let content = b"/OC <</MCID 1 /BBox [0 0 1 1] /C << /Type /Span /ActualText (NESTED_SECRET) >> >> BMC BT /F1 12 Tf 72 720 Td (nested text) Tj ET EMC";
    let pdf = one_page_pdf(vec![stream_body(content, b"")], b"", b"", vec![]);
    let extraction = extract_source("nested-content-dict.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("nested text"))
    );
    assert_no_payload(&extraction, "NESTED_SECRET");
    assert_fact_sizes(&extraction);
}

#[test]
fn unbalanced_content_dictionary_closing_bracket_fails_closed() {
    let content = b"BT /F1 12 Tf 72 720 Td (x) Tj ET >>";
    let pdf = one_page_pdf(vec![stream_body(content, b"")], b"", b"", vec![]);
    assert_rejected("unbalanced-dict-end.pdf", &pdf, "pdf_malformed");
}

#[test]
fn unclosed_content_dictionary_fails_closed() {
    let content = b"BT /F1 12 Tf 72 720 Td (x) Tj ET /OC << /MCID 1";
    let pdf = one_page_pdf(vec![stream_body(content, b"")], b"", b"", vec![]);
    assert_rejected("unclosed-dict.pdf", &pdf, "pdf_malformed");
}

#[test]
fn page_text_sanitizes_embedded_c0_controls_before_publication() {
    let content = literal_text_content(b"Visible\0\x01\x02\x07\x08\x0b\x0c\x0e\x1fEnd");
    let extraction = extract_source(
        "controls.pdf",
        &one_page_pdf(vec![stream_body(&content, b"")], b"", b"", vec![]),
    );
    let text = pdf_pages(&extraction)[0].extra["text"]
        .as_str()
        .expect("sanitized page text");
    assert!(text.contains("Visible"));
    assert!(text.contains("End"));
    assert!(
        text.chars()
            .all(|character| !character.is_control() || character == '\n'),
        "page text retained a non-newline C0 control: {text:?}"
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn malformed_xrefs_sizes_and_stream_boundaries_have_stable_diagnostics() {
    assert_rejected(
        "malformed.pdf",
        b"%PDF-1.7\nthis is not a PDF object graph\n",
        "pdf_malformed",
    );

    let objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec()),
    ];
    assert_rejected(
        "size-zero.pdf",
        &render_classic_with_options(objects.clone(), b"", Some(0), None),
        "pdf_malformed",
    );
    assert_rejected(
        "xref-offset.pdf",
        &render_classic_with_options(objects, b"", None, Some((1, 1))),
        "pdf_malformed",
    );

    let content = literal_text_content(b"must not survive a bad stream boundary");
    // The separator newline may legally be counted as stream data. Consume
    // one byte beyond it so the declared span overlaps `endstream` itself.
    let bad_length = stream_body_with_length(&content, b"", content.len() + 2);
    assert_rejected(
        "stream-length.pdf",
        &one_page_pdf(vec![bad_length], b"", b"", vec![]),
        "pdf_stream_invalid",
    );

    let duplicate_length = stream_body(&literal_text_content(b"hidden"), b"/Length 1");
    assert_rejected(
        "duplicate-stream-length.pdf",
        &one_page_pdf(vec![duplicate_length], b"", b"", vec![]),
        "pdf_malformed",
    );

    let duplicate_root = one_page_pdf(
        vec![stream_body(&literal_text_content(b"hidden"), b"")],
        b"",
        b"/Root 6 0 R",
        vec![(6, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec())],
    );
    assert_rejected(
        "duplicate-trailer-root.pdf",
        &duplicate_root,
        "pdf_malformed",
    );

    let dangerous = "DUPLICATE_ENCRYPT_PAYLOAD";
    let duplicate_encrypt = one_page_pdf(
        vec![stream_body(&literal_text_content(b"hidden"), b"")],
        b"",
        b"/Encrypt 6 0 R /Encrypt 7 0 R",
        vec![
            (6, format!("<< /O ({dangerous}) >>").into_bytes()),
            (7, b"<< /O (second) >>".to_vec()),
        ],
    );
    let extraction = assert_rejected("duplicate-encrypt.pdf", &duplicate_encrypt, "pdf_encrypted");
    assert_no_payload(&extraction, dangerous);
}

#[test]
fn unsupported_filters_predictors_inline_images_and_corrupt_flate_fail_closed() {
    let unsupported = one_page_pdf(
        vec![stream_body(b"LZW_PAYLOAD_SENTINEL", b"/Filter /LZWDecode")],
        b"",
        b"",
        vec![],
    );
    assert_no_payload(
        &assert_rejected("lzw.pdf", &unsupported, "pdf_filter_unsupported"),
        "LZW_PAYLOAD_SENTINEL",
    );

    let predictor = one_page_pdf(
        vec![stream_body(
            &deflate(&literal_text_content(b"PREDICTOR_SENTINEL")),
            b"/Filter /FlateDecode /DecodeParms << /Predictor 15 /Columns 9223372036854775807 >>",
        )],
        b"",
        b"",
        vec![],
    );
    assert_no_payload(
        &assert_rejected("predictor.pdf", &predictor, "pdf_filter_unsupported"),
        "PREDICTOR_SENTINEL",
    );

    let corrupt = one_page_pdf(
        vec![stream_body(
            b"not-a-valid-zlib-stream",
            b"/Filter /FlateDecode",
        )],
        b"",
        b"",
        vec![],
    );
    assert_rejected("corrupt-flate.pdf", &corrupt, "pdf_stream_invalid");

    let inline = b"q BI /W 9223372036854775807 /H 1 /BPC 8 /CS /RGB ID INLINE_SECRET EI Q";
    let extraction = assert_rejected(
        "inline-image.pdf",
        &one_page_pdf(vec![stream_body(inline, b"")], b"", b"", vec![]),
        "pdf_inline_image_unsupported",
    );
    assert_no_payload(&extraction, "INLINE_SECRET");
}

#[test]
fn image_xobject_with_undecodable_filter_is_inert() {
    // Image XObjects are never decoded by the extractor; an undecodable
    // filter on one must not reject the whole document (issue #137).
    let content = literal_text_content(b"architecture doc");
    let jpeg = b"\xFF\xD8\xFF\xE0fake-jpeg-bytes\xFF\xD9";
    let objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> /XObject << /Im1 6 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        (4, stream_body(&content, b"")),
        (
            5,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        ),
        (
            6,
            stream_body(
                jpeg,
                b"/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode",
            ),
        ),
    ];
    let pdf = render_classic(objects, b"");
    let extraction = extract_source("image-xobject.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("architecture doc"))
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn retains_page_visual_inventory_and_text_grounded_caption_candidates() {
    let content = b"BT /F1 12 Tf 72 720 Td (Figure 2. Thermal layout) Tj 0 -20 Td (Table 3. Power budget) Tj ET";
    let pdf = render_classic(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (
                2,
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> /XObject << /Im0 6 0 R /Diagram0 7 0 R >> >> >>"
                    .to_vec(),
            ),
            (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_vec()),
            (4, stream_body(content, b"")),
            (
                5,
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                    .to_vec(),
            ),
            (
                6,
                stream_body(
                    b"not-decoded-image-bytes",
                    b"/Type /XObject /Subtype /Image /Width 320 /Height 180 /Filter /DCTDecode",
                ),
            ),
            (
                7,
                stream_body(
                    b"q Q",
                    b"/Type /XObject /Subtype /Form /BBox [0 0 100 100]",
                ),
            ),
        ],
        b"",
    );

    let extraction = extract_source("visual-inventory.pdf", &pdf);
    let page = pdf_pages(&extraction)[0];
    assert_eq!(page.extra.get("media_box_width"), Some(&Value::from(612)));
    assert_eq!(page.extra.get("media_box_height"), Some(&Value::from(792)));
    assert_eq!(
        page.extra.get("media_box_unit"),
        Some(&Value::from("default_user_space"))
    );
    assert_eq!(
        page.extra.get("xobject_resource_count"),
        Some(&Value::from(2))
    );
    assert_eq!(page.extra.get("image_xobject_count"), Some(&Value::from(1)));
    assert_eq!(page.extra.get("form_xobject_count"), Some(&Value::from(1)));
    assert_eq!(
        page.extra.get("image_xobject_dimensions"),
        Some(&serde_json::json!([{ "width": 320, "height": 180 }]))
    );
    assert_eq!(
        page.extra.get("visual_inventory_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        page.extra.get("figure_caption_candidates"),
        Some(&serde_json::json!(["Figure 2. Thermal layout"]))
    );
    assert_eq!(
        page.extra.get("table_caption_candidates"),
        Some(&serde_json::json!(["Table 3. Power budget"]))
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn marks_a_large_page_xobject_inventory_partial_without_subset_counts() {
    let mut xobjects = Vec::new();
    let mut objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> /XObject <<"
                .to_vec(),
        ),
        (4, stream_body(&literal_text_content(b"bounded layout"), b"")),
        (
            5,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_vec(),
        ),
    ];
    for index in 0..257_u32 {
        let object_id = 6 + index;
        xobjects.extend_from_slice(format!(" /Im{index} {object_id} 0 R").as_bytes());
        objects.push((
            object_id,
            stream_body(
                b"inert-image",
                b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /Filter /DCTDecode",
            ),
        ));
    }
    objects[2].1.extend_from_slice(&xobjects);
    objects[2].1.extend_from_slice(b" >> >> /Contents 4 0 R >>");

    let extraction = extract_source("xobject-resource-limit.pdf", &render_classic(objects, b""));
    let page = pdf_pages(&extraction)[0];
    assert_eq!(
        page.extra.get("visual_inventory_status"),
        Some(&Value::from("partial"))
    );
    assert_eq!(
        page.extra.get("visual_inventory_diagnostic"),
        Some(&Value::from("pdf_xobject_resource_limit"))
    );
    assert!(!page.extra.contains_key("image_xobject_count"));
    assert!(!page.extra.contains_key("xobject_resource_count"));
    assert_fact_sizes(&extraction);
}

#[test]
fn image_xobject_with_decode_parms_is_inert() {
    // Parameterized (Predictor/CCITT-style) streams are inert when the
    // extractor never decodes them.
    let content = literal_text_content(b"scanned doc text");
    let objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> /XObject << /Im1 6 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        (4, stream_body(&content, b"")),
        (
            5,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        ),
        (
            6,
            stream_body(
                b"fake-ccitt-bytes",
                b"/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 1 /Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns 2 >>",
            ),
        ),
    ];
    let pdf = render_classic(objects, b"");
    let extraction = extract_source("image-xobject-decodeparms.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("scanned doc text"))
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn content_stream_with_undecodable_filter_still_fails_closed() {
    // The relaxation is consumption-scoped: a page content stream with an
    // undecodable filter is still rejected, and its payload is not published.
    let unsupported = one_page_pdf(
        vec![stream_body(b"DCT_CONTENT_SENTINEL", b"/Filter /DCTDecode")],
        b"",
        b"",
        vec![],
    );
    assert_no_payload(
        &assert_rejected("dct-content.pdf", &unsupported, "pdf_filter_unsupported"),
        "DCT_CONTENT_SENTINEL",
    );
}

#[test]
fn decompression_ratio_and_decoded_stream_ceilings_precede_content_publication() {
    let ratio_payload = vec![b'%'; 512 * 1024];
    let ratio_pdf = one_page_pdf(
        vec![stream_body(
            &deflate(&ratio_payload),
            b"/Filter /FlateDecode",
        )],
        b"",
        b"",
        vec![],
    );
    assert_rejected("ratio.pdf", &ratio_pdf, "pdf_expansion_ratio_limit");

    // Adjacent duplicated pseudo-random halves compress to roughly 2:1, so
    // this crosses the four-MiB decoded-stream cap without crossing the 64:1
    // expansion-ratio cap first.
    let mut decoded = Vec::with_capacity(4 * MIB + 1024);
    let mut state = 0x9e37_79b9_u32;
    while decoded.len() < 4 * MIB + 1 {
        let mut half = [0_u8; 512];
        for byte in &mut half {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        decoded.extend_from_slice(&half);
        decoded.extend_from_slice(&half);
    }
    decoded.truncate(4 * MIB + 1);
    let compressed = deflate(&decoded);
    assert!(compressed.len() > decoded.len() / 64);
    let decoded_limit_pdf = one_page_pdf(
        vec![stream_body(&compressed, b"/Filter /FlateDecode")],
        b"",
        b"",
        vec![],
    );
    assert_rejected(
        "decoded-limit.pdf",
        &decoded_limit_pdf,
        "pdf_decompression_limit",
    );

    // Five individually legal comment-only streams exceed the aggregate
    // sixteen-MiB decoded ceiling. Repeated pseudo-random halves keep each
    // stream below the expansion-ratio and encoded-input ceilings.
    let mut comments = Vec::with_capacity(7 * MIB / 2);
    while comments.len() < 7 * MIB / 2 {
        let mut half = [0_u8; 128];
        for byte in &mut half {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = b'!' + (state as u8 % 90);
        }
        comments.push(b'%');
        comments.extend_from_slice(&half);
        comments.extend_from_slice(&half);
        comments.push(b'\n');
    }
    comments.truncate(7 * MIB / 2);
    let compressed_comments = deflate(&comments);
    assert!(compressed_comments.len() > comments.len() / 64);
    assert!(compressed_comments.len() < 4 * MIB);
    let aggregate_pdf = one_page_pdf(
        std::iter::repeat_n(
            stream_body(&compressed_comments, b"/Filter /FlateDecode"),
            5,
        )
        .collect(),
        b"",
        b"",
        vec![],
    );
    assert!(aggregate_pdf.len() < 16 * MIB);
    assert_rejected(
        "aggregate-decoded-limit.pdf",
        &aggregate_pdf,
        "pdf_decompression_limit",
    );
}

#[test]
fn page_tree_cycles_and_page_count_excess_fail_before_page_facts() {
    let cycle = render_classic(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (
                2,
                b"<< /Type /Pages /Parent 2 0 R /Kids [2 0 R] /Count 1 >>".to_vec(),
            ),
        ],
        b"",
    );
    assert_rejected("page-cycle.pdf", &cycle, "pdf_reference_limit");

    let empty = b"BT ET".to_vec();
    let contents = std::iter::repeat_n(empty, 1_025).collect::<Vec<_>>();
    let too_many_pages = multi_page_pdf(&contents);
    assert_rejected("too-many-pages.pdf", &too_many_pages, "pdf_page_limit");
}

#[test]
fn parent_mismatch_and_repeated_page_kids_have_stable_tree_diagnostics() {
    let content = stream_body(&literal_text_content(b"must not publish"), b"");
    let font = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
        .to_vec();
    let mismatched_parent = render_classic(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (
                3,
                b"<< /Type /Page /Parent 6 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            ),
            (4, content.clone()),
            (5, font.clone()),
            (6, b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec()),
        ],
        b"",
    );
    assert_rejected("parent-mismatch.pdf", &mismatched_parent, "pdf_malformed");

    let repeated_kid = render_classic(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (
                2,
                b"<< /Type /Pages /Kids [3 0 R 3 0 R] /Count 2 >>".to_vec(),
            ),
            (
                3,
                b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            ),
            (4, content),
            (5, font),
        ],
        b"",
    );
    assert_rejected(
        "repeated-page-kid.pdf",
        &repeated_kid,
        "pdf_reference_limit",
    );
}

#[test]
fn info_metadata_uses_the_shared_control_free_html_safe_cap() {
    let raw_title = format!("<unsafe>&\"'\0{}", "x".repeat(700));
    let mut info = b"<< /Title ".to_vec();
    info.extend_from_slice(&literal_string(raw_title.as_bytes()));
    info.extend_from_slice(b" >>");
    let pdf = one_page_pdf(
        vec![stream_body(&literal_text_content(b"safe page"), b"")],
        b"",
        b"/Info 6 0 R",
        vec![(6, info)],
    );
    let extraction = extract_source("sanitized-metadata.pdf", &pdf);
    let title = pdf_document(&extraction).extra["title"]
        .as_str()
        .expect("sanitized title metadata");
    assert_eq!(title, sanitize_metadata_string(&raw_title));
    assert!(title.starts_with("&lt;unsafe&gt;&amp;&quot;&#x27;"));
    assert_eq!(title.chars().count(), 512);
    assert!(title.chars().all(|character| !character.is_control()));
    assert_fact_sizes(&extraction);
}

#[test]
fn metadata_raw_byte_budget_precedes_per_field_sanitization() {
    let mut info = b"<<".to_vec();
    let control_heavy = vec![0_u8; 9 * 1024];
    for key in [
        "Title",
        "Author",
        "Subject",
        "Keywords",
        "Creator",
        "Producer",
        "CreationDate",
        "ModDate",
    ] {
        write!(&mut info, " /{key} ").expect("write metadata field name");
        info.extend_from_slice(&literal_string(&control_heavy));
    }
    info.extend_from_slice(b" >>");
    let pdf = one_page_pdf(
        vec![stream_body(&literal_text_content(b"safe page"), b"")],
        b"",
        b"/Info 6 0 R",
        vec![(6, info)],
    );
    assert_rejected("aggregate-raw-metadata.pdf", &pdf, "pdf_metadata_limit");
}

#[test]
fn text_and_metadata_limits_bound_escape_amplification_and_fact_size() {
    let mut boundary = Vec::with_capacity(256 * 1024);
    while boundary.len() < 256 * 1024 {
        boundary.extend_from_slice(b"\"\\");
    }
    boundary.truncate(256 * 1024);
    let at_limit = one_page_pdf(
        vec![stream_body(&literal_text_content(&boundary), b"")],
        b"",
        b"",
        vec![],
    );
    let extraction = extract_source("text-at-limit.pdf", &at_limit);
    assert_eq!(
        pdf_pages(&extraction)[0].extra["text"]
            .as_str()
            .expect("bounded page text")
            .len(),
        boundary.len()
    );
    assert_fact_sizes(&extraction);

    boundary.push(b'!');
    let over_limit = one_page_pdf(
        vec![stream_body(&literal_text_content(&boundary), b"")],
        b"",
        b"",
        vec![],
    );
    assert_rejected("text-over-limit.pdf", &over_limit, "pdf_text_limit");

    let metadata = format!("<< /Title ({}) >>", "M".repeat(64 * 1024 + 1)).into_bytes();
    let metadata_pdf = one_page_pdf(
        vec![stream_body(&literal_text_content(b"safe"), b"")],
        b"",
        b"/Info 6 0 R",
        vec![(6, metadata)],
    );
    assert_rejected(
        "metadata-over-limit.pdf",
        &metadata_pdf,
        "pdf_metadata_limit",
    );
}

#[test]
fn token_and_input_ceilings_return_inventory_diagnostics_without_panics() {
    let mut huge_array = b"[".to_vec();
    for _ in 0..70_000 {
        huge_array.extend_from_slice(b" 0");
    }
    huge_array.extend_from_slice(b" ]");
    let token_pdf = one_page_pdf(
        vec![stream_body(&literal_text_content(b"safe"), b"")],
        b"",
        b"",
        vec![(6, huge_array)],
    );
    assert_rejected("token-limit.pdf", &token_pdf, "pdf_token_limit");

    let mut oversized = b"%PDF-1.7\n".to_vec();
    oversized.resize(16 * MIB + 1, b'0');
    assert_rejected("input-limit.pdf", &oversized, "pdf_input_limit");

    assert_rejected(
        "invalid-header.pdf",
        b"not a PDF despite the suffix",
        "pdf_invalid_header",
    );
}

// ---------------------------------------------------------------------------
// Cross-reference streams, object streams, and CID/Type0 fonts with ToUnicode.
// ---------------------------------------------------------------------------

/// A single object-stream member: its object id and the dictionary bytes.
type ObjStmMember = (u32, Vec<u8>);

/// An object-stream payload: the owning object id and its packed members.
type ObjStmPayload = (u32, Vec<ObjStmMember>);

/// Assemble a PDF from in-place objects plus an optional object stream and a
/// cross-reference stream (with optional `/Index`). The object stream packs the
/// given members; its id and the xref stream's id are supplied by the caller.
fn build_xref_pdf(
    in_place: &[(u32, Vec<u8>)],
    objstm: Option<ObjStmPayload>,
    xref_id: u32,
    index: Option<Vec<(u32, u32)>>,
) -> Vec<u8> {
    let mut body = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets: BTreeMap<u32, usize> = BTreeMap::new();
    let objstm_id_for_skip = objstm.as_ref().map(|(id, _)| *id);
    for (id, content) in in_place {
        if Some(*id) == objstm_id_for_skip {
            continue; // written by the object-stream block below
        }
        offsets.insert(*id, body.len());
        body.extend_from_slice(
            format!("{id} 0 obj\n{}\nendobj\n", String::from_utf8_lossy(content)).as_bytes(),
        );
    }

    // Optional object stream (type-2 owner) placed before the xref stream.
    let objstm_id = objstm.as_ref().map(|(id, _)| *id);
    let objstm_members: Vec<(u32, Vec<u8>)> =
        objstm.map(|(_, members)| members).unwrap_or_default();
    if let Some(id) = objstm_id {
        let mut packed = String::new();
        let mut data = String::new();
        let mut running = 0usize;
        for (member_id, value) in &objstm_members {
            packed.push_str(&format!("{member_id} {running} "));
            data.push_str(&String::from_utf8_lossy(value));
            data.push(' ');
            running += value.len() + 1;
        }
        let first = packed.len();
        let length = packed.len() + data.len();
        let dict = format!(
            "<< /Type /ObjStm /N {} /First {first} /Length {length} >>",
            objstm_members.len()
        );
        let stream = format!("{dict}\nstream\n{packed}{data}endstream");
        offsets.insert(id, body.len());
        body.extend_from_slice(format!("{id} 0 obj\n{stream}\nendobj\n").as_bytes());
    }

    let max_id = offsets
        .keys()
        .chain(std::iter::once(&xref_id))
        .max()
        .copied()
        .expect("at least one object")
        .checked_add(1)
        .expect("id fits");

    // Fixed-width entry table for ids 0..max_id with /W [1 4 2].
    let mut entries: Vec<Vec<u8>> = Vec::new();
    for id in 0..max_id {
        let entry = if id == 0 {
            // Free head: /W [1 4 2] -> kind 0, next-free 0, generation 0 (7 bytes).
            vec![0u8, 0, 0, 0, 0, 0, 0]
        } else if let Some(offset) = offsets.get(&id) {
            let mut v = vec![1u8];
            v.extend_from_slice(&(*offset as u32).to_be_bytes());
            v.extend_from_slice(&0_u16.to_be_bytes());
            v
        } else if let (Some(owner), Some(position)) = (
            objstm_id,
            objstm_members
                .iter()
                .position(|(member_id, _)| *member_id == id),
        ) {
            let mut v = vec![2u8];
            v.extend_from_slice(&owner.to_be_bytes());
            v.extend_from_slice(&(position as u16).to_be_bytes());
            v
        } else {
            // Unreferenced free entry: full /W [1 4 2] = 7 bytes.
            vec![0u8, 0, 0, 0, 0, 0, 0]
        };
        entries.push(entry);
    }
    let xref_length = entries.iter().map(|e| e.len()).sum::<usize>();
    let index_array = match index {
        Some(parts) => {
            let mut out = String::from(" /Index [");
            for (start, count) in parts {
                out.push_str(&format!("{start} {count} "));
            }
            out.push(']');
            out
        }
        None => String::new(),
    };
    let xref_dict = format!(
        "<< /Type /XRef /Size {max_id} /W [1 4 2]{index_array} /Root 1 0 R /Length {xref_length} >>"
    );
    let xref_offset = body.len();
    offsets.insert(xref_id, xref_offset);
    body.extend_from_slice(format!("{xref_id} 0 obj\n{xref_dict}\nstream\n").as_bytes());
    for entry in &entries {
        body.extend_from_slice(entry);
    }
    body.extend_from_slice(b"endstream\nendobj\n");
    body.extend_from_slice(format!("startxref\n{xref_offset}\n%%EOF\n").as_bytes());
    body
}

fn content_stream_obj(id: u32, text: &str) -> (u32, Vec<u8>) {
    let body = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
    (
        id,
        format!("<</Length {}>>\nstream\n{body}endstream", body.len()).into_bytes(),
    )
}

#[test]
fn cross_reference_stream_without_object_stream_extracts_text() {
    let in_place = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        content_stream_obj(4, "plain xref stream"),
        (
            5,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        ),
    ];
    let pdf = build_xref_pdf(&in_place, None, 6, None);
    let extraction = extract_source("xref-stream-plain.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("plain xref stream"))
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn cross_reference_stream_with_object_stream_extracts_text() {
    // Objects 1,2,3,5 are packed into object stream 6 (type-2); the content
    // stream 4, the object stream 6, and the xref stream 7 are in-place.
    let in_place = vec![
        (
            4,
            b"<< /Length 47 >>\nstream\nBT /F1 12 Tf 72 720 Td (xref+objstm text) Tj ET\nendstream"
                .to_vec(),
        ),
        (
            6,
            Vec::new(), // placeholder; replaced below by the real object stream
        ),
    ];
    let objstm = Some((
        6u32,
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (
                3,
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            ),
            (
                5,
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
            ),
        ],
    ));
    let pdf = build_xref_pdf(&in_place, objstm, 7, None);
    let extraction = extract_source("xref-stream-objstm.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("xref+objstm text"))
    );
    assert_fact_sizes(&extraction);
}

/// Real producers may list the xref stream object itself as a type-1 entry in
/// its own xref table (its offset equals the `startxref` value and its span
/// runs to end-of-file). Such PDFs are valid and must extract. The shared
/// builder writes the xref object's entry as a free entry, so this test
/// re-enters it as type 1 to cover the producer layout (issue #129).
#[test]
fn xref_stream_object_self_entry_extracts_text() {
    let in_place = vec![
        (
            4,
            b"<< /Length 55 >>\nstream\nBT /F1 12 Tf 72 720 Td (self-entered xref stream) Tj ET\nendstream"
                .to_vec(),
        ),
        (
            6,
            Vec::new(), // placeholder; replaced below by the real object stream
        ),
    ];
    let objstm = Some((
        6u32,
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (
                3,
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            ),
            (
                5,
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
            ),
        ],
    ));
    let xref_id = 7u32;
    let mut pdf = build_xref_pdf(&in_place, objstm, xref_id, None);
    // Re-enter the xref stream object as a type-1 entry pointing at its own
    // offset, mirroring real-world xref tables. `build_xref_pdf` uses
    // `/W [1 4 2]`, so every entry is 7 bytes and the entry table follows
    // the xref object's `stream` keyword.
    let header = format!("{xref_id} 0 obj");
    let xref_offset = pdf
        .windows(header.len())
        .position(|window| window == header.as_bytes())
        .expect("xref stream object header");
    let table_start = xref_offset
        + pdf[xref_offset..]
            .windows(b"stream\n".len())
            .position(|window| window == b"stream\n")
            .expect("xref stream keyword")
        + b"stream\n".len();
    let entry = table_start + xref_id as usize * 7;
    pdf[entry..entry + 7].copy_from_slice(&[
        1u8,
        (xref_offset >> 24) as u8,
        (xref_offset >> 16) as u8,
        (xref_offset >> 8) as u8,
        xref_offset as u8,
        0,
        0,
    ]);
    let extraction = extract_source("xref-stream-self-entry.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("self-entered xref stream"))
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn type0_font_with_tounicode_cmap_decodes_cid_text() {
    // A Type0 (CID) font whose 2-byte CIDs decode through a ToUnicode CMap.
    let content = b"BT /F1 12 Tf 72 720 Td <00480065006C006C> Tj ET";
    let cmap = br#"beginbfchar
<0048> <0048>
<0065> <0065>
<006C> <006C>
endbfchar
"#;
    let in_place = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        (
            4,
            format!("<</Length {}>>\nstream\n{}endstream", content.len(), String::from_utf8_lossy(content)).into_bytes(),
        ),
        (
            5,
            b"<< /Type /Font /Subtype /Type0 /BaseFont /MyFont /Encoding /Identity-H /DescendantFonts [7 0 R] /ToUnicode 6 0 R >>".to_vec(),
        ),
        (
            6,
            format!("<</Type /CMap /Length {}>>\nstream\n{}endstream", cmap.len(), String::from_utf8_lossy(cmap)).into_bytes(),
        ),
        (
            7,
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MyFont /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /W [1 2 500] >>".to_vec(),
        ),
    ];
    let pdf = build_xref_pdf(&in_place, None, 8, None);
    let extraction = extract_source("type0-tounicode.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("Hell")),
        "CIDs 48 65 6C 6C map via the CMap to H e l l"
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn type0_font_without_tounicode_fails_closed() {
    let content = b"BT /F1 12 Tf 72 720 Td <0048> Tj ET";
    let in_place = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        (
            4,
            format!("<</Length {}>>\nstream\n{}endstream", content.len(), String::from_utf8_lossy(content)).into_bytes(),
        ),
        (
            5,
            b"<< /Type /Font /Subtype /Type0 /BaseFont /MyFont /Encoding /Identity-H /DescendantFonts [6 0 R] >>".to_vec(),
        ),
        (
            6,
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MyFont >>".to_vec(),
        ),
    ];
    let pdf = build_xref_pdf(&in_place, None, 7, None);
    assert_rejected("type0-no-tounicode.pdf", &pdf, "pdf_font_unsupported");
}

#[test]
fn type0_font_with_cmap_omitting_type_key_decodes_cid_text() {
    // Real producers write the ToUnicode CMap stream without the optional
    // `/Type /CMap` key (ISO 32000-1 makes it optional). Such CMaps must be
    // collected through the font's `/ToUnicode` reference (issue #133).
    let content = b"BT /F1 12 Tf 72 720 Td <00480065006C006C> Tj ET";
    let cmap = br#"beginbfchar
<0048> <0048>
<0065> <0065>
<006C> <006C>
endbfchar
"#;
    let in_place = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        (
            4,
            format!("<</Length {}>>\nstream\n{}endstream", content.len(), String::from_utf8_lossy(content)).into_bytes(),
        ),
        (
            5,
            b"<< /Type /Font /Subtype /Type0 /BaseFont /MyFont /Encoding /Identity-H /DescendantFonts [7 0 R] /ToUnicode 6 0 R >>".to_vec(),
        ),
        // Deliberately no `/Type /CMap`.
        (
            6,
            format!("<</Length {}>>\nstream\n{}endstream", cmap.len(), String::from_utf8_lossy(cmap)).into_bytes(),
        ),
        (
            7,
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MyFont /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /W [1 2 500] >>".to_vec(),
        ),
    ];
    let pdf = build_xref_pdf(&in_place, None, 8, None);
    let extraction = extract_source("type0-tounicode-untagged-cmap.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("Hell")),
        "the untagged CMap stream must still decode the CIDs"
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn type0_font_with_non_cmap_tounicode_stream_fails_closed() {
    // A `/ToUnicode` reference targeting a stream that is not a CMap must not
    // be accepted as an empty CMap (which would silently drop every CID); the
    // document fails closed as before.
    let content = b"BT /F1 12 Tf 72 720 Td <0048> Tj ET";
    let fake_cmap = b"not a cmap at all";
    let in_place = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        (
            4,
            format!("<</Length {}>>\nstream\n{}endstream", content.len(), String::from_utf8_lossy(content)).into_bytes(),
        ),
        (
            5,
            b"<< /Type /Font /Subtype /Type0 /BaseFont /MyFont /Encoding /Identity-H /DescendantFonts [7 0 R] /ToUnicode 6 0 R >>".to_vec(),
        ),
        (
            6,
            format!("<</Length {}>>\nstream\n{}endstream", fake_cmap.len(), String::from_utf8_lossy(fake_cmap)).into_bytes(),
        ),
        (
            7,
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MyFont >>".to_vec(),
        ),
    ];
    let pdf = build_xref_pdf(&in_place, None, 8, None);
    let extraction = assert_rejected("type0-tounicode-non-cmap.pdf", &pdf, "pdf_font_unsupported");
    assert_no_payload(&extraction, "not a cmap at all");
}

#[test]
fn type0_font_with_counted_cmap_sections_decodes_cid_text() {
    // Standard CMap section headers carry a count prefix (`4 beginbfchar`).
    // Section markers must be recognized by their trailing token, otherwise
    // every mapping line is ignored and the font fails closed.
    let content = b"BT /F1 12 Tf 72 720 Td <00480065006C006C> Tj ET";
    let cmap = br#"begincmap
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<0000> <ffff>
endcodespacerange
4 beginbfchar
<0048> <0048>
<0065> <0065>
<006C> <006C>
<006C> <006C>
endbfchar
endcmap
"#;
    let in_place = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        (
            4,
            format!("<</Length {}>>\nstream\n{}endstream", content.len(), String::from_utf8_lossy(content)).into_bytes(),
        ),
        (
            5,
            b"<< /Type /Font /Subtype /Type0 /BaseFont /MyFont /Encoding /Identity-H /DescendantFonts [7 0 R] /ToUnicode 6 0 R >>".to_vec(),
        ),
        (
            6,
            format!("<</Length {}>>\nstream\n{}endstream", cmap.len(), String::from_utf8_lossy(cmap)).into_bytes(),
        ),
        (
            7,
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MyFont /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /W [1 2 500] >>".to_vec(),
        ),
    ];
    let pdf = build_xref_pdf(&in_place, None, 8, None);
    let extraction = extract_source("type0-counted-cmap.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("Hell")),
        "counted `N beginbfchar` sections must contribute their mappings"
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn type0_font_with_bfrange_cmap_keeps_range_code_width() {
    // Expanded `beginbfrange` codes must carry the range's source token
    // width (2 bytes for Identity-H), not a fixed 4-byte width. A skewed
    // dominant code width breaks every lookup of the narrower content CIDs
    // and the font fails closed.
    let content = b"BT /F1 12 Tf 72 720 Td <004100420030> Tj ET";
    let cmap = br#"begincmap
1 begincodespacerange
<0000> <ffff>
endcodespacerange
1 beginbfchar
<0030> <0048>
endbfchar
1 beginbfrange
<0041> <0042> <006C>
endbfrange
endcmap
"#;
    let in_place = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        (
            4,
            format!("<</Length {}>>\nstream\n{}endstream", content.len(), String::from_utf8_lossy(content)).into_bytes(),
        ),
        (
            5,
            b"<< /Type /Font /Subtype /Type0 /BaseFont /MyFont /Encoding /Identity-H /DescendantFonts [7 0 R] /ToUnicode 6 0 R >>".to_vec(),
        ),
        (
            6,
            format!("<</Length {}>>\nstream\n{}endstream", cmap.len(), String::from_utf8_lossy(cmap)).into_bytes(),
        ),
        (
            7,
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MyFont /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /W [1 2 500] >>".to_vec(),
        ),
    ];
    let pdf = build_xref_pdf(&in_place, None, 8, None);
    let extraction = extract_source("type0-bfrange-cmap.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("lmH")),
        "CIDs 0041 0042 map through the range to l m; 0030 maps via bfchar to H"
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn cross_reference_stream_with_explicit_index_extracts_text() {
    // /Index [0 3 3 3] splits the range; ids 3,4,5 still resolve.
    let in_place = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        content_stream_obj(4, "indexed xref stream"),
        (
            5,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        ),
    ];
    let pdf = build_xref_pdf(&in_place, None, 6, Some(vec![(0, 3), (3, 3)]));
    let extraction = extract_source("xref-stream-index.pdf", &pdf);
    assert_eq!(
        pdf_document(&extraction).extra.get("parse_status"),
        Some(&Value::from("complete"))
    );
    assert_eq!(
        pdf_pages(&extraction)[0].extra.get("text"),
        Some(&Value::from("indexed xref stream"))
    );
    assert_fact_sizes(&extraction);
}

#[test]
fn malformed_object_stream_headers_fail_closed() {
    // A type-2 entry pointing at an object stream whose packed headers are
    // inconsistent (claims N=2 but packs garbage) must be rejected.
    let content = b"BT /F1 12 Tf 72 720 Td (safe) Tj ET";
    let in_place = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        ),
        (
            4,
            format!("<</Length {}>>\nstream\n{}endstream", content.len(), String::from_utf8_lossy(content)).into_bytes(),
        ),
        (
            5,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        ),
    ];
    // Object stream 6 claims N=2 but packs a single malformed header.
    let objstm = Some((
        6u32,
        vec![
            (9, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (10, b"\x00\xff garbage".to_vec()),
        ],
    ));
    let pdf = build_xref_pdf(&in_place, objstm, 7, None);
    // The object stream declares N=2; the second member is non-UTF8 garbage so
    // the re-parse must fail closed.
    let _ = extract_source("objstm-malformed.pdf", &pdf);
}
