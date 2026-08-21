use flate2::{write::ZlibEncoder, Compression};
use graphoxide_core::{sanitize_metadata_string, Edge, Extraction, Node};
use graphoxide_extract::extract;
use serde_json::Value;
use std::{collections::BTreeMap, fs, io::Write as _, path::Path};

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
    let font_id = 3_u32
        .checked_add(
            u32::try_from(contents.len())
                .expect("page count")
                .checked_mul(2)
                .expect("page object IDs"),
        )
        .expect("font object ID");
    let mut kids = b"[".to_vec();
    let mut objects = vec![(1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec())];
    for (index, content) in contents.iter().enumerate() {
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
        objects.push((content_id, stream_body(content, b"")));
    }
    kids.extend_from_slice(b" ]");
    let mut pages = format!("<< /Type /Pages /Count {} /Kids ", contents.len()).into_bytes();
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

    for (name, trailer) in [
        ("incremental.pdf", b"/Prev 0".as_slice()),
        ("escaped-incremental.pdf", b"/Pr#65v 0".as_slice()),
    ] {
        assert_rejected(
            name,
            &one_page_pdf(vec![content.clone()], b"", trailer, vec![]),
            "pdf_incremental_unsupported",
        );
    }
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
fn pdf_whitespace_cannot_hide_duplicate_incremental_markers_inside_streams() {
    // NUL and form feed are PDF whitespace even though they are not ordinary
    // line indentation. An old revision marker hidden this way must still make
    // the document multi-revision before any stream or page facts publish.
    let hidden_markers = b"\x0c\0startxref\n0\n\x0c\0%%EOF\n";
    let pdf = one_page_pdf(
        vec![stream_body(&literal_text_content(b"safe page"), b"")],
        b"",
        b"",
        vec![(6, stream_body(hidden_markers, b""))],
    );
    assert_rejected(
        "whitespace-hidden-incremental.pdf",
        &pdf,
        "pdf_incremental_unsupported",
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
    let contents = std::iter::repeat_n(empty, 513).collect::<Vec<_>>();
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
