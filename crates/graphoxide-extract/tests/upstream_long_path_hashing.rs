use graphoxide_extract::detect::{md5_file, os_path_for, stat_and_hash};
use std::{fs, path::Path};
use tempfile::TempDir;

#[test]
fn test_os_path_noop_on_posix() {
    let path = Path::new("/home/user/deep/file.py");
    assert_eq!(os_path_for(path, false), path.to_string_lossy());
}

#[test]
fn test_os_path_adds_prefix_on_win32() {
    assert!(os_path_for(Path::new("/already/abs/file.py"), true).starts_with(r"\\?\"));
}

#[test]
fn test_os_path_idempotent_on_win32() {
    let path = r"\\?\C:\a\file.py";
    assert_eq!(os_path_for(Path::new(path), true), path);
}

#[test]
fn test_hashing_still_works_and_stabilizes() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("deep/nested/module.py");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "def x():\n    return 1\n").unwrap();

    let first = md5_file(&file);
    let second = md5_file(&file);
    assert!(!first.is_empty());
    assert_eq!(first, second);

    let source = file.to_string_lossy();
    let fact = stat_and_hash(&source).unwrap();
    assert_eq!(fact.0, source);
    assert_eq!(fact.2, first);
}
