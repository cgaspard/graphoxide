use graphoxide_extract::evidence::{
    extract_locator_candidates, ArchiveInnerLocatorCandidate, EvidenceExtractionStatus,
    EvidenceLocatorCandidate,
};
use std::{
    io::{Cursor, Write},
    path::Path,
};
use zip::{write::SimpleFileOptions, ZipWriter};

#[test]
fn markdown_evidence_reports_heading_paragraph_and_exact_byte_range_without_reading_a_path() {
    let source = "# Design\n\nFirst paragraph.\n\n## Interface\n\nSecond paragraph.\n";
    let first_start = source.find("First paragraph.").expect("first paragraph");
    let second_start = source.find("Second paragraph.").expect("second paragraph");
    let report = extract_locator_candidates(
        Path::new("/path-that-must-not-exist/reference.md"),
        source.as_bytes(),
    )
    .expect("extract ready bytes");

    assert_eq!(report.status, EvidenceExtractionStatus::Extracted);
    assert_eq!(
        report.locators,
        vec![
            EvidenceLocatorCandidate::Markdown {
                heading_path: "Design".into(),
                paragraph: 1,
                byte_start: u64::try_from(first_start).expect("byte offset"),
                byte_end: u64::try_from(first_start + "First paragraph.".len())
                    .expect("byte offset"),
            },
            EvidenceLocatorCandidate::Markdown {
                heading_path: "Design / Interface".into(),
                paragraph: 2,
                byte_start: u64::try_from(second_start).expect("byte offset"),
                byte_end: u64::try_from(second_start + "Second paragraph.".len())
                    .expect("byte offset"),
            },
        ]
    );
}

#[test]
fn long_plain_text_is_partitioned_into_bounded_contiguous_line_locators() {
    let source = (1..=640)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();

    let report = extract_locator_candidates(Path::new("reference.txt"), source.as_bytes())
        .expect("extract long plain-text locators");

    assert_eq!(report.status, EvidenceExtractionStatus::Extracted);
    assert_eq!(
        report.locators,
        vec![
            EvidenceLocatorCandidate::Text(graphoxide_extract::evidence::TextLocatorCandidate {
                line_start: 1,
                line_end: 214,
                semantic_role: None,
            }),
            EvidenceLocatorCandidate::Text(graphoxide_extract::evidence::TextLocatorCandidate {
                line_start: 215,
                line_end: 428,
                semantic_role: None,
            }),
            EvidenceLocatorCandidate::Text(graphoxide_extract::evidence::TextLocatorCandidate {
                line_start: 429,
                line_end: 640,
                semantic_role: None,
            }),
        ]
    );
}

#[test]
fn json_evidence_reports_escaped_rfc_6901_leaf_pointers() {
    let report = extract_locator_candidates(
        Path::new("schema.json"),
        br#"{"paths":{"a/b":{"~value":true}}}"#,
    )
    .expect("inspect ready bytes");

    assert_eq!(report.status, EvidenceExtractionStatus::Extracted);
    assert!(report.locators.iter().any(|locator| {
        matches!(locator, EvidenceLocatorCandidate::Json { pointer } if pointer == "/paths/a~1b/~0value")
    }));
}

#[test]
fn binary_evidence_stays_inventory_only() {
    let report = extract_locator_candidates(Path::new("missing.bin"), b"\0\xff\0")
        .expect("inspect ready bytes");

    assert_eq!(report.status, EvidenceExtractionStatus::InventoryOnly);
    assert!(report.locators.is_empty());
}

#[test]
fn pdf_evidence_reports_exact_page_locators() {
    let report = extract_locator_candidates(Path::new("captured.bin"), &single_page_pdf())
        .expect("inspect admitted PDF bytes");

    assert_eq!(report.status, EvidenceExtractionStatus::Extracted);
    assert!(report
        .locators
        .contains(&EvidenceLocatorCandidate::Pdf { page: 1 }));
}

#[test]
fn archive_evidence_does_not_relabel_an_embedded_pdf_page_as_an_outer_pdf_page() {
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    archive
        .start_file("specs/embedded.pdf", SimpleFileOptions::default())
        .expect("start embedded PDF");
    archive
        .write_all(&single_page_pdf())
        .expect("write embedded PDF");
    let archive = archive.finish().expect("finish archive").into_inner();

    let report = extract_locator_candidates(Path::new("bundle.zip"), &archive)
        .expect("extract admitted archive evidence");

    assert_eq!(report.status, EvidenceExtractionStatus::Extracted);
    assert_eq!(report.locators.len(), 2, "member and its native page");
    assert!(
        !report
            .locators
            .iter()
            .any(|locator| matches!(locator, EvidenceLocatorCandidate::Pdf { .. })),
        "an embedded page must retain its archive member provenance"
    );
}

#[test]
fn archive_evidence_reports_typed_embedded_xlsx_sheet_ranges() {
    let mut workbook = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, value) in [
        (
            "[Content_Types].xml",
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdRoot" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Ledger" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Voltage</t></is></c><c r="B1"><v>12</v></c></row></sheetData></worksheet>"#,
        ),
    ] {
        workbook
            .start_file(name, SimpleFileOptions::default())
            .expect("start workbook member");
        workbook
            .write_all(value.as_bytes())
            .expect("write workbook member");
    }
    let workbook = workbook.finish().expect("finish workbook").into_inner();
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    archive
        .start_file("records/ledger.xlsx", SimpleFileOptions::default())
        .expect("start embedded workbook");
    archive
        .write_all(&workbook)
        .expect("write embedded workbook");
    let archive = archive.finish().expect("finish outer archive").into_inner();

    let report = extract_locator_candidates(Path::new("bundle.zip"), &archive)
        .expect("extract admitted archive evidence");

    assert!(report.locators.iter().any(|locator| {
        matches!(
            locator,
            EvidenceLocatorCandidate::ArchiveChild {
                member,
                inner: ArchiveInnerLocatorCandidate::Spreadsheet { sheet, cell_range },
            } if member == "records/ledger.xlsx" && sheet == "Ledger" && cell_range == "A1:B1"
        )
    }));
}

#[test]
fn presentation_evidence_reports_exact_slide_locators() {
    let report = extract_locator_candidates(Path::new("captured.pptx"), &single_slide_pptx())
        .expect("inspect admitted presentation bytes");

    assert_eq!(report.status, EvidenceExtractionStatus::Extracted);
    assert!(report
        .locators
        .contains(&EvidenceLocatorCandidate::Presentation { slide: 1 }));
}

fn single_page_pdf() -> Vec<u8> {
    let objects: [&[u8]; 4] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>",
        b"<< /Length 0 >>\nstream\n\nendstream",
    ];
    let mut output = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(output.len());
        writeln!(&mut output, "{} 0 obj", index + 1).expect("write object header");
        output.extend_from_slice(object);
        output.extend_from_slice(b"\nendobj\n");
    }
    let xref = output.len();
    writeln!(&mut output, "xref\n0 5").expect("write xref header");
    output.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets {
        writeln!(&mut output, "{offset:010} 00000 n ").expect("write xref entry");
    }
    writeln!(
        &mut output,
        "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF"
    )
    .expect("write trailer");
    output
}

fn single_slide_pptx() -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, value) in [
        (
            "[Content_Types].xml",
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#,
        ),
        (
            "ppt/presentation.xml",
            r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#,
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#,
        ),
        (
            "ppt/slides/slide1.xml",
            r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Slide text</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
        ),
    ] {
        writer
            .start_file(name, SimpleFileOptions::default())
            .expect("start presentation ZIP member");
        writer
            .write_all(value.as_bytes())
            .expect("write presentation ZIP member");
    }
    writer
        .finish()
        .expect("finish presentation ZIP")
        .into_inner()
}
