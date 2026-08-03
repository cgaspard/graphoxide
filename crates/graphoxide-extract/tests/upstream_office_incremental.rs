use filetime::{set_file_mtime, FileTime};
use graphoxide_extract::detect::convert_office_text;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_modified_docx_reconverts_sidecar() {
    let fixture = TempDir::new().unwrap();
    let source = fixture.path().join("doc.docx");
    let output = fixture.path().join("converted");
    fs::write(&source, b"original office payload").unwrap();
    set_file_mtime(&source, FileTime::from_unix_time(100, 0)).unwrap();

    let sidecar = convert_office_text(&source, &output, None, "original alpha content")
        .unwrap()
        .unwrap();
    assert!(fs::read_to_string(&sidecar)
        .unwrap()
        .contains("original alpha content"));

    fs::write(&source, b"revised office payload").unwrap();
    set_file_mtime(&sidecar, FileTime::from_unix_time(100, 0)).unwrap();
    set_file_mtime(&source, FileTime::from_unix_time(200, 0)).unwrap();
    let updated = convert_office_text(&source, &output, None, "revised beta content")
        .unwrap()
        .unwrap();

    assert_eq!(updated, sidecar);
    let body = fs::read_to_string(updated).unwrap();
    assert!(body.contains("revised beta content"));
    assert!(!body.contains("original alpha content"));
}

#[test]
fn test_unchanged_docx_sidecar_not_rewritten() {
    let fixture = TempDir::new().unwrap();
    let source = fixture.path().join("doc.docx");
    let output = fixture.path().join("converted");
    fs::write(&source, b"stable office payload").unwrap();
    set_file_mtime(&source, FileTime::from_unix_time(100, 0)).unwrap();

    let sidecar = convert_office_text(&source, &output, None, "stable content")
        .unwrap()
        .unwrap();
    set_file_mtime(&sidecar, FileTime::from_unix_time(200, 0)).unwrap();
    let before = fs::metadata(&sidecar).unwrap().modified().unwrap();

    let unchanged = convert_office_text(&source, &output, None, "content that must not replace it")
        .unwrap()
        .unwrap();
    assert_eq!(unchanged, sidecar);
    assert_eq!(
        fs::metadata(&unchanged).unwrap().modified().unwrap(),
        before
    );
    assert!(fs::read_to_string(unchanged)
        .unwrap()
        .contains("stable content"));
}
