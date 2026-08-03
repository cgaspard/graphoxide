//! Executable port of upstream `tests/test_antigravity_install.py`.

use graphoxide_cli::install::{install, uninstall, InstallContext, Platform};
use std::fs;
use tempfile::TempDir;

fn context(temp: &TempDir, project: bool) -> InstallContext {
    let project_root = temp.path().join("project");
    let home = temp.path().join("home");
    fs::create_dir_all(&project_root).unwrap();
    fs::create_dir_all(&home).unwrap();
    InstallContext {
        project_root,
        home,
        project,
        executable: "/usr/bin/graphoxide".into(),
        windows: false,
        local_app_data: None,
    }
}

#[test]
fn test_antigravity_project_install_writes_rules_and_workflows() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    install(Platform::Antigravity, &context).unwrap();
    let skill = context
        .project_root
        .join(".agents/skills/graphoxide/SKILL.md");
    assert!(skill.is_file());
    assert!(context
        .project_root
        .join(".agents/rules/graphoxide.md")
        .is_file());
    assert!(context
        .project_root
        .join(".agents/workflows/graphoxide.md")
        .is_file());
    assert!(fs::read_to_string(skill).unwrap().starts_with("---\n"));
}

fn assert_workflow_is_scope_independent(body: &str) {
    assert!(!body.contains(".gemini"));
    assert!(!body.contains("SKILL.md"));
    assert!(!body.contains('~'));
    assert!(body.to_ascii_lowercase().contains("graphoxide skill"));
}

#[test]
fn test_antigravity_workflow_names_no_skill_path() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    install(Platform::Antigravity, &context).unwrap();
    let body =
        fs::read_to_string(context.project_root.join(".agents/workflows/graphoxide.md")).unwrap();
    assert_workflow_is_scope_independent(&body);
}

#[test]
fn test_antigravity_global_install_workflow_names_no_skill_path() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    install(Platform::Antigravity, &context).unwrap();
    assert!(context
        .home
        .join(".gemini/config/skills/graphoxide/SKILL.md")
        .is_file());
    let body =
        fs::read_to_string(context.project_root.join(".agents/workflows/graphoxide.md")).unwrap();
    assert_workflow_is_scope_independent(&body);
}

#[test]
fn test_antigravity_project_uninstall_clears_rules_and_workflows() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    install(Platform::Antigravity, &context).unwrap();
    uninstall(Platform::Antigravity, &context).unwrap();
    assert!(!context
        .project_root
        .join(".agents/rules/graphoxide.md")
        .exists());
    assert!(!context
        .project_root
        .join(".agents/workflows/graphoxide.md")
        .exists());
    assert!(!context
        .project_root
        .join(".agents/skills/graphoxide/SKILL.md")
        .exists());
}
