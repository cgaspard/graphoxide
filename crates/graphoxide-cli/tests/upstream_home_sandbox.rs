//! Executable equivalents of upstream `tests/test_home_sandbox.py`.
//!
//! Every command runs in a child process with an explicit temporary HOME, so
//! this suite cannot read, rewrite, or delete the developer's real agent setup.

use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn run(home: &Path, project: &Path, arguments: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(arguments)
        .current_dir(project)
        .env("HOME", home)
        .env_remove("USERPROFILE")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "graphoxide {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_path_home_is_sandboxed() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("sandbox-home-path");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    run(&home, &project, &["install", "claude"]);
    assert!(home.join(".claude/skills/graphoxide/SKILL.md").is_file());
    assert!(!project.join(".claude/skills/graphoxide/SKILL.md").exists());
}

#[test]
fn test_expanduser_is_sandboxed() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("sandbox-home-expanduser");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    run(&home, &project, &["install", "codex"]);
    assert!(home.join(".codex/skills/graphoxide/SKILL.md").is_file());
}

#[test]
fn test_claude_config_dir_escape_hatch_is_cleared() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("sandbox-home-escape");
    let escape = temp.path().join("must-not-be-used");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["install", "claude"])
        .current_dir(&project)
        .env("HOME", &home)
        .env("CLAUDE_CONFIG_DIR", &escape)
        .env("XDG_CONFIG_HOME", &escape)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(home.join(".claude/skills/graphoxide/SKILL.md").is_file());
    assert!(!escape.exists());
}

#[test]
fn test_global_uninstall_is_captured_by_sandbox() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("sandbox-home-uninstall");
    let project = temp.path().join("some-project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    run(&home, &project, &["install", "claude"]);
    let skill = home.join(".claude/skills/graphoxide/SKILL.md");
    assert!(skill.is_file());

    run(&home, &project, &["uninstall", "claude", "--project"]);
    assert!(skill.is_file(), "project uninstall removed the user skill");

    run(&home, &project, &["uninstall", "claude"]);
    assert!(!skill.exists(), "global uninstall missed the sandbox skill");
    assert!(home.starts_with(temp.path()));
}
