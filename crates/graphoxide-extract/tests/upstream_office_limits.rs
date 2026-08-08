//! One-to-one executable port of pinned Graphify `tests/test_office_limits.py`.

use graphoxide_extract::detect::{
    docx_to_markdown, extract_pdf_text_with_cap, file_within_size_cap, xlsx_to_markdown,
    zip_within_caps, zip_within_caps_with, OfficeLimits,
};
use std::{fs, io::Write, path::Path};
use tempfile::TempDir;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

fn write_zip(path: &Path, members: &[(&str, &[u8])]) {
    let file = fs::File::create(path).expect("create Office ZIP fixture");
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, payload) in members {
        writer
            .start_file(*name, options)
            .expect("start Office ZIP member");
        writer.write_all(payload).expect("write Office ZIP member");
    }
    writer.finish().expect("finish Office ZIP fixture");
}

fn incompressible_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x4d59_5df4_d0f3_3173_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn classic_xref_pdf() -> Vec<u8> {
    let content = b"BT\n/F1 12 Tf\n72 720 Td\n(Hello bounded PDF) Tj\nET\n";
    let objects = [
        b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec(),
        b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n".to_vec(),
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n".to_vec(),
        [
            format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len()).into_bytes(),
            content.to_vec(),
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

#[test]
fn test_file_within_size_cap() {
    let fixture = TempDir::new().expect("temporary Office fixture");
    let file = fixture.path().join("a.bin");
    fs::write(&file, vec![b'x'; 1024]).expect("write capped file");
    assert!(file_within_size_cap(&file, 50 * 1024 * 1024));
    assert!(!file_within_size_cap(&file, 512));
    assert!(!file_within_size_cap(&fixture.path().join("missing"), 512));
}

#[test]
fn test_zip_ratio_bomb_rejected() {
    let fixture = TempDir::new().expect("temporary Office fixture");
    let bomb = fixture.path().join("bomb.xlsx");
    let payload = vec![b'0'; 5 * 1024 * 1024];
    write_zip(&bomb, &[("xl/worksheets/sheet1.xml", &payload)]);
    assert!(fs::metadata(&bomb).expect("bomb metadata").len() < 100 * 1024);
    assert!(!zip_within_caps(&bomb));
}

#[test]
fn test_legit_zip_passes() {
    let fixture = TempDir::new().expect("temporary Office fixture");
    let document = fixture.path().join("ok.docx");
    let payload = b"<xml>hello world</xml>".repeat(20);
    write_zip(&document, &[("word/document.xml", &payload)]);
    assert!(zip_within_caps(&document));
}

#[test]
fn test_non_zip_rejected() {
    let fixture = TempDir::new().expect("temporary Office fixture");
    let fake = fixture.path().join("fake.xlsx");
    fs::write(&fake, b"this is not a zip file").expect("write fake Office file");
    assert!(!zip_within_caps(&fake));
}

#[test]
fn test_converters_return_empty_for_bomb() {
    let fixture = TempDir::new().expect("temporary Office fixture");
    let payload = vec![b'0'; 5 * 1024 * 1024];
    for extension in ["docx", "xlsx"] {
        let bomb = fixture.path().join(format!("bomb.{extension}"));
        write_zip(&bomb, &[("x.xml", &payload)]);
        assert!(docx_to_markdown(&bomb).is_empty());
        assert!(xlsx_to_markdown(&bomb).is_empty());
    }
}

#[test]
fn test_legit_multi_member_passes_streaming() {
    let fixture = TempDir::new().expect("temporary Office fixture");
    let workbook = fixture.path().join("ok.xlsx");
    let workbook_xml = b"<workbook/>".repeat(100);
    let sheet_xml = b"<sheetData>rows</sheetData>".repeat(500);
    write_zip(
        &workbook,
        &[
            ("[Content_Types].xml", b"<types/>"),
            ("xl/workbook.xml", &workbook_xml),
            ("xl/worksheets/sheet1.xml", &sheet_xml),
        ],
    );
    assert!(zip_within_caps(&workbook));
}

#[test]
fn test_streaming_ceiling_rejects_oversized_actual() {
    let fixture = TempDir::new().expect("temporary Office fixture");
    let workbook = fixture.path().join("big.xlsx");
    let payload = incompressible_bytes(512 * 1024);
    write_zip(&workbook, &[("xl/x.xml", &payload)]);
    let limits = OfficeLimits {
        max_decompressed_bytes: 64 * 1024,
        ..OfficeLimits::default()
    };
    assert!(!zip_within_caps_with(&workbook, limits));
}

#[test]
fn test_pdf_over_cap_returns_empty() {
    let fixture = TempDir::new().expect("temporary Office fixture");
    let pdf = fixture.path().join("big.pdf");
    let mut payload = b"%PDF-1.4\n".to_vec();
    payload.extend(vec![b'x'; 4096]);
    fs::write(&pdf, payload).expect("write oversized PDF fixture");
    assert!(extract_pdf_text_with_cap(&pdf, 100).is_empty());
}

#[test]
fn test_classic_xref_pdf_text_is_extracted_within_cap() {
    let fixture = TempDir::new().expect("temporary PDF fixture");
    let pdf = fixture.path().join("paper.pdf");
    fs::write(&pdf, classic_xref_pdf()).expect("write classic-xref PDF fixture");

    assert_eq!(
        extract_pdf_text_with_cap(&pdf, 1024 * 1024),
        "Hello bounded PDF"
    );
}

#[test]
fn legitimate_docx_conversion_remains_available_behind_the_guards() {
    let fixture = TempDir::new().expect("temporary Office fixture");
    let document = fixture.path().join("ok.docx");
    let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
        <w:document xmlns:w="urn:word"><w:body><w:p><w:r><w:t>Hello Office</w:t></w:r></w:p></w:body></w:document>"#;
    write_zip(&document, &[("word/document.xml", xml)]);
    assert_eq!(docx_to_markdown(&document), "Hello Office");
}

#[test]
fn legitimate_xlsx_conversion_remains_available_behind_the_guards() {
    let fixture = TempDir::new().expect("temporary Office fixture");
    let workbook = fixture.path().join("ok.xlsx");
    let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
        <worksheet><sheetData><row>
          <c t="inlineStr"><is><t>Name</t></is></c>
          <c t="inlineStr"><is><t>Value</t></is></c>
        </row><row>
          <c t="inlineStr"><is><t>alpha</t></is></c>
          <c><v>42</v></c>
        </row></sheetData></worksheet>"#;
    write_zip(&workbook, &[("xl/worksheets/sheet1.xml", xml)]);
    let markdown = xlsx_to_markdown(&workbook);
    assert!(markdown.contains("## Sheet: sheet1"), "{markdown:?}");
    assert!(markdown.contains("| Name | Value |"), "{markdown:?}");
    assert!(markdown.contains("| alpha | 42 |"), "{markdown:?}");
}
