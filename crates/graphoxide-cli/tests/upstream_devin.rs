//! Executable port of pinned upstream `tests/test_devin.py`.

use graphoxide_cli::install::{
    self, devin_platform_install, devin_platform_uninstall, devin_rules_install, devin_rules_path,
    devin_rules_uninstall, install, packaged_skill_references, platform_skill_destination,
    remove_skill, skill_asset, InstallContext, Platform, DEVIN_RULES, DEVIN_SKILL,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
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
        executable: Path::new(env!("CARGO_BIN_EXE_graphoxide")).to_path_buf(),
        windows: false,
        local_app_data: None,
    }
}

fn user_skill(context: &InstallContext) -> PathBuf {
    context
        .home
        .join(".config/devin/skills/graphoxide/SKILL.md")
}

fn project_skill(context: &InstallContext) -> PathBuf {
    context
        .project_root
        .join(".devin/skills/graphoxide/SKILL.md")
}

fn run_cli(context: &InstallContext, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(arguments)
        .current_dir(&context.project_root)
        .env("HOME", &context.home)
        .env_remove("USERPROFILE")
        .output()
        .unwrap()
}

fn run_ok(context: &InstallContext, arguments: &[&str]) -> Output {
    let output = run_cli(context, arguments);
    assert!(
        output.status.success(),
        "graphoxide {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

mod devin {
    use super::*;

    #[test]
    fn test_devin_install_user_creates_skill_file() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        install(Platform::Devin, &context).unwrap();
        assert!(user_skill(&context).is_file());
    }

    #[test]
    fn test_devin_skill_file_contains_frontmatter() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        install(Platform::Devin, &context).unwrap();
        let content = fs::read_to_string(user_skill(&context)).unwrap();
        assert!(content.contains("name: graphoxide"));
        assert!(content.contains("argument-hint:"));
        assert!(content.contains("triggers:"));
    }

    #[test]
    fn test_devin_skill_file_references_graphify_query() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        install(Platform::Devin, &context).unwrap();
        assert!(fs::read_to_string(user_skill(&context))
            .unwrap()
            .contains("graphoxide query"));
    }

    #[test]
    fn test_devin_install_user_does_not_write_rules() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        install(Platform::Devin, &context).unwrap();
        assert!(!devin_rules_path(&context.project_root).exists());
    }

    #[test]
    fn test_devin_install_project_creates_skill_file() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        run_ok(&context, &["devin", "install", "--project"]);
        assert!(project_skill(&context).is_file());
        assert!(!user_skill(&context).exists());
    }

    #[test]
    fn test_devin_install_project_creates_rules_file() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        install(Platform::Devin, &context).unwrap();
        let content = fs::read_to_string(devin_rules_path(&context.project_root)).unwrap();
        assert!(content.contains("graphoxide"));
        assert!(content.contains("GRAPH_REPORT.md"));
    }

    #[test]
    fn test_devin_rules_content_recommends_graphify_query() {
        let temp = TempDir::new().unwrap();
        assert!(devin_rules_install(temp.path()).unwrap());
        assert!(fs::read_to_string(devin_rules_path(temp.path()))
            .unwrap()
            .contains("graphoxide query"));
    }

    #[test]
    fn test_devin_rules_install_idempotent() {
        let temp = TempDir::new().unwrap();
        assert!(devin_rules_install(temp.path()).unwrap());
        let first = fs::read(devin_rules_path(temp.path())).unwrap();
        assert!(!devin_rules_install(temp.path()).unwrap());
        assert_eq!(fs::read(devin_rules_path(temp.path())).unwrap(), first);

        let context = context(&temp, true);
        run_ok(&context, &["devin", "install", "--project"]);
        let second = run_ok(&context, &["devin", "install", "--project"]);
        assert!(String::from_utf8_lossy(&second.stdout).contains("no change"));
    }

    #[test]
    fn test_devin_install_project_hints_git_add() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        let output = run_ok(&context, &["devin", "install", "--project"]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("git add"));
        assert!(stdout.contains(".devin"));
        assert!(stdout.contains(".windsurf"));
    }

    #[test]
    fn test_devin_uninstall_user_removes_skill_file() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        install(Platform::Devin, &context).unwrap();
        assert!(remove_skill(Platform::Devin, &context).unwrap());
        assert!(!user_skill(&context).exists());
    }

    #[test]
    fn test_devin_uninstall_user_noop_when_not_installed() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        let output = run_ok(&context, &["devin", "uninstall"]);
        assert!(String::from_utf8_lossy(&output.stdout).contains("nothing to remove"));
    }

    #[test]
    fn test_devin_uninstall_project_removes_skill_file() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        assert!(devin_platform_install(&context).unwrap());
        assert!(devin_platform_uninstall(&context).unwrap());
        assert!(!project_skill(&context).exists());
    }

    #[test]
    fn test_devin_uninstall_project_removes_rules_file() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        install(Platform::Devin, &context).unwrap();
        install::uninstall(Platform::Devin, &context).unwrap();
        assert!(!devin_rules_path(&context.project_root).exists());
    }

    #[test]
    fn test_devin_uninstall_project_does_not_touch_user_scope() {
        let temp = TempDir::new().unwrap();
        let user_context = context(&temp, false);
        install(Platform::Devin, &user_context).unwrap();
        let project_context = InstallContext {
            project: true,
            ..user_context.clone()
        };
        devin_platform_install(&project_context).unwrap();
        devin_platform_uninstall(&project_context).unwrap();
        assert!(user_skill(&user_context).is_file());
    }

    #[test]
    fn test_devin_rules_uninstall_noop_when_not_installed() {
        let temp = TempDir::new().unwrap();
        assert!(!devin_rules_uninstall(temp.path()).unwrap());
    }

    #[test]
    fn test_devin_skill_file_exists_in_package() {
        assert!(!DEVIN_SKILL.trim().is_empty());
        assert!(install::packaged_asset_names().contains(&"skill-devin.md"));
    }

    #[test]
    fn test_devin_skill_file_uses_python_c_syntax() {
        // Graphoxide preserves the upstream portability guarantee by invoking
        // its native executable directly instead of discovering a Python runtime.
        assert!(DEVIN_SKILL.contains("invoke it directly"));
        assert!(DEVIN_SKILL.contains("graphoxide audit"));
        assert!(!DEVIN_SKILL.contains("python -c"));
        assert!(!DEVIN_SKILL.contains("#!/bin/bash"));
    }

    #[test]
    fn test_devin_skill_file_frontmatter_has_triggers() {
        let frontmatter = DEVIN_SKILL.split("---").nth(1).unwrap();
        assert!(frontmatter.contains("triggers:"));
        assert!(frontmatter.contains("model"));
    }

    #[test]
    fn test_devin_in_platform_config() {
        assert!(Platform::CONFIG_PLATFORMS.contains(&Platform::Devin));
        assert_eq!("devin".parse::<Platform>().unwrap(), Platform::Devin);
        assert_eq!(skill_asset(Platform::Devin), Some(DEVIN_SKILL));
        assert!(packaged_skill_references(Platform::Devin).is_none());
        assert_eq!(DEVIN_RULES, include_str!("../assets/devin-rules.md"));
    }

    #[test]
    fn test_devin_platform_skill_destination_user_scope() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        assert_eq!(
            platform_skill_destination(Platform::Devin, &context)
                .unwrap()
                .unwrap(),
            user_skill(&context)
        );
    }

    #[test]
    fn test_devin_in_main_help_text() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        let top = String::from_utf8(run_ok(&context, &["--help"]).stdout).unwrap();
        assert!(top.contains("devin"));
        assert!(top.contains("~/.config/devin"));

        let help = String::from_utf8(run_ok(&context, &["devin", "--help"]).stdout).unwrap();
        assert!(help.contains("install"));
        assert!(help.contains("uninstall"));
        assert!(!help.contains("--project"));
    }

    #[test]
    fn test_devin_platform_skill_destination_project_scope() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        assert_eq!(
            platform_skill_destination(Platform::Devin, &context)
                .unwrap()
                .unwrap(),
            project_skill(&context)
        );
    }
}
