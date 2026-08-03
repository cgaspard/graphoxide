//! Executable port of upstream `tests/test_skill_version_warning.py`.

use graphoxide_cli::install::{skill_version_warning, version_tuple};
use std::{fs, path::Path};
use tempfile::TempDir;

fn make_skill(root: &Path, stamped: &str) -> std::path::PathBuf {
    let skill = root.join("skills/graphoxide/SKILL.md");
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(&skill, "# graphoxide skill\n").unwrap();
    fs::write(skill.parent().unwrap().join(".graphoxide_version"), stamped).unwrap();
    skill
}

#[test]
fn test_version_tuple_orders_numerically() {
    assert!(version_tuple("0.9.2") > version_tuple("0.8.27"));
    assert!(version_tuple("0.10.0") > version_tuple("0.9.0"));
    assert_eq!(version_tuple("0.9.3"), version_tuple("0.9.3"));
    assert_eq!(version_tuple("1.0.0rc1"), version_tuple("1.0.0"));
    assert_eq!(version_tuple(""), vec![0]);
}

#[test]
fn test_skill_older_than_package_recommends_install() {
    let temp = TempDir::new().unwrap();
    let skill = make_skill(temp.path(), "0.8.27");
    let warning = skill_version_warning(&skill, "0.9.3").unwrap();
    assert!(warning.contains("Run 'graphoxide install' to update"));
    assert!(!warning.contains("downgrade"));
}

#[test]
fn test_skill_newer_than_package_recommends_upgrade_not_install() {
    let temp = TempDir::new().unwrap();
    let skill = make_skill(temp.path(), "0.9.2");
    let warning = skill_version_warning(&skill, "0.8.27").unwrap();
    assert!(!warning.contains("Run 'graphoxide install' to update"));
    assert!(warning.contains("downgrade"));
    assert!(warning.to_ascii_lowercase().contains("upgrade"));
}

#[test]
fn test_matching_version_is_silent() {
    let temp = TempDir::new().unwrap();
    let skill = make_skill(temp.path(), "0.9.3");
    assert_eq!(skill_version_warning(&skill, "0.9.3"), None);
}
