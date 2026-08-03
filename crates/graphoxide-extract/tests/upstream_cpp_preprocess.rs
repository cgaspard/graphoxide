use graphoxide_extract::extract;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_cpp_preprocess_passes_absolute_path() {
    // Graphoxide does not spawn `cpp` for Fortran: it performs the compatible
    // extraction in-process. An attacker-controlled option-like filename is
    // therefore never interpreted as a command-line argument at all.
    let fixture = TempDir::new().unwrap();
    let source = fixture.path().join("-include.F90");
    fs::write(&source, "program x\nend program x\n").unwrap();
    let extraction = extract(&source).unwrap();
    assert!(extraction
        .nodes
        .iter()
        .any(|node| node.label.eq_ignore_ascii_case("x")));
}
