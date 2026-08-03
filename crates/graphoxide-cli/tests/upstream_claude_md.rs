//! Executable port of pinned upstream `tests/test_claude_md.py`.

use graphoxide_cli::install::{install, uninstall, InstallContext, Platform, MANAGED_HEADING};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn context(temp: &TempDir) -> InstallContext {
    let project_root = temp.path().join("project");
    let home = temp.path().join("home");
    fs::create_dir_all(&project_root).unwrap();
    fs::create_dir_all(&home).unwrap();
    InstallContext {
        project_root,
        home,
        project: true,
        executable: Path::new(env!("CARGO_BIN_EXE_graphoxide")).to_path_buf(),
        windows: false,
        local_app_data: None,
    }
}

fn claude_md(context: &InstallContext) -> PathBuf {
    context.project_root.join("CLAUDE.md")
}

fn settings(context: &InstallContext, name: &str) -> PathBuf {
    context.project_root.join(".claude").join(name)
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

fn pretool_hooks(path: &Path) -> Vec<Value> {
    serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap()["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .clone()
}

fn has_graphoxide_hook(path: &Path, matcher: Option<&str>) -> bool {
    pretool_hooks(path).iter().any(|hook| {
        matcher.is_none_or(|expected| hook["matcher"] == expected)
            && hook.to_string().contains("graphoxide")
    })
}

mod claude_md {
    use super::*;

    #[test]
    fn test_install_creates_claude_md() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        assert!(claude_md(&context).is_file());
        assert!(fs::read_to_string(claude_md(&context))
            .unwrap()
            .contains(MANAGED_HEADING));
    }

    #[test]
    fn test_install_contains_expected_rules() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        let content = fs::read_to_string(claude_md(&context)).unwrap();
        assert!(content.contains("GRAPH_REPORT.md"));
        assert!(content.contains("wiki/index.md"));
        assert!(content.contains("graphoxide update"));
    }

    #[test]
    fn test_install_appends_to_existing_claude_md() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        fs::write(
            claude_md(&context),
            "# Existing content\n\nSome rules here.\n",
        )
        .unwrap();
        install(Platform::Claude, &context).unwrap();
        let content = fs::read_to_string(claude_md(&context)).unwrap();
        assert!(content.contains("Existing content"));
        assert!(content.contains(MANAGED_HEADING));
    }

    #[test]
    fn test_install_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        run_ok(&context, &["claude", "install", "--project"]);
        let second = run_ok(&context, &["claude", "install", "--project"]);
        assert_eq!(
            fs::read_to_string(claude_md(&context))
                .unwrap()
                .matches(MANAGED_HEADING)
                .count(),
            1
        );
        assert!(String::from_utf8_lossy(&second.stdout).contains("already configured"));
    }

    #[test]
    fn test_install_idempotent_message() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        run_ok(&context, &["claude", "install", "--project"]);
        let second = run_ok(&context, &["claude", "install", "--project"]);
        assert!(String::from_utf8_lossy(&second.stdout).contains("already configured"));
    }

    #[test]
    fn test_uninstall_removes_section() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        uninstall(Platform::Claude, &context).unwrap();
        assert!(!claude_md(&context).exists());
    }

    #[test]
    fn test_uninstall_preserves_other_content() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        fs::write(claude_md(&context), "# My Project\n\nSome rules.\n").unwrap();
        install(Platform::Claude, &context).unwrap();
        uninstall(Platform::Claude, &context).unwrap();
        let content = fs::read_to_string(claude_md(&context)).unwrap();
        assert!(content.contains("My Project"));
        assert!(content.contains("Some rules"));
        assert!(!content.contains(MANAGED_HEADING));
    }

    #[test]
    fn test_uninstall_no_op_when_not_installed() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        fs::write(claude_md(&context), "# Other stuff\n").unwrap();
        let output = run_ok(&context, &["claude", "uninstall", "--project"]);
        let output = String::from_utf8_lossy(&output.stdout);
        assert!(output.contains("not found") || output.contains("nothing to do"));
    }

    #[test]
    fn test_uninstall_no_op_when_no_file() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        let output = run_ok(&context, &["claude", "uninstall", "--project"]);
        let output = String::from_utf8_lossy(&output.stdout);
        assert!(output.contains("No CLAUDE.md") || output.contains("nothing to do"));
    }

    #[test]
    fn test_install_creates_settings_json() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        let settings = settings(&context, "settings.json");
        assert!(settings.is_file());
        assert!(has_graphoxide_hook(&settings, Some("Bash|Grep")));
    }

    #[test]
    fn test_install_settings_json_idempotent() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        install(Platform::Claude, &context).unwrap();
        let hooks = pretool_hooks(&settings(&context, "settings.json"));
        assert_eq!(
            hooks
                .iter()
                .filter(|hook| {
                    hook["matcher"] == "Bash|Grep" && hook.to_string().contains("graphoxide")
                })
                .count(),
            1
        );
    }

    #[test]
    fn test_uninstall_removes_settings_hook() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        uninstall(Platform::Claude, &context).unwrap();
        let settings = settings(&context, "settings.json");
        if settings.exists() {
            assert!(!has_graphoxide_hook(&settings, Some("Bash|Grep")));
        }
    }

    #[test]
    fn test_uninstall_removes_hook_from_settings_local_json() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        fs::rename(
            settings(&context, "settings.json"),
            settings(&context, "settings.local.json"),
        )
        .unwrap();
        uninstall(Platform::Claude, &context).unwrap();
        assert!(!has_graphoxide_hook(
            &settings(&context, "settings.local.json"),
            None
        ));
    }

    #[test]
    fn test_uninstall_removes_section_from_dot_claude_local_md() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        let local = settings(&context, "CLAUDE.local.md");
        fs::write(&local, fs::read(claude_md(&context)).unwrap()).unwrap();
        fs::remove_file(claude_md(&context)).unwrap();
        uninstall(Platform::Claude, &context).unwrap();
        assert!(!local.exists() || !fs::read_to_string(local).unwrap().contains(MANAGED_HEADING));
    }

    #[test]
    fn test_uninstall_removes_section_from_root_claude_local_md() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        let local = context.project_root.join("CLAUDE.local.md");
        fs::write(&local, fs::read(claude_md(&context)).unwrap()).unwrap();
        fs::remove_file(claude_md(&context)).unwrap();
        uninstall(Platform::Claude, &context).unwrap();
        assert!(!local.exists() || !fs::read_to_string(local).unwrap().contains(MANAGED_HEADING));
    }

    #[test]
    fn test_uninstall_cleans_both_standard_and_local() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        let local = settings(&context, "CLAUDE.local.md");
        fs::write(&local, fs::read(claude_md(&context)).unwrap()).unwrap();
        uninstall(Platform::Claude, &context).unwrap();
        for path in [claude_md(&context), local] {
            assert!(!path.exists() || !fs::read_to_string(path).unwrap().contains(MANAGED_HEADING));
        }
    }

    #[test]
    fn test_uninstall_preserves_other_content_in_local_md() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        let local = settings(&context, "CLAUDE.local.md");
        fs::write(
            &local,
            format!(
                "# Local notes\n\nkeep me\n\n{}",
                fs::read_to_string(claude_md(&context)).unwrap()
            ),
        )
        .unwrap();
        uninstall(Platform::Claude, &context).unwrap();
        let content = fs::read_to_string(local).unwrap();
        assert!(content.contains("Local notes"));
        assert!(content.contains("keep me"));
        assert!(!content.contains(MANAGED_HEADING));
    }

    #[test]
    fn test_uninstall_tolerates_unreadable_local_md() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp);
        install(Platform::Claude, &context).unwrap();
        let local = settings(&context, "CLAUDE.local.md");
        let bytes = b"\xff\xfe not valid utf-8 \x80\x81";
        fs::write(&local, bytes).unwrap();
        uninstall(Platform::Claude, &context).unwrap();
        assert_eq!(fs::read(local).unwrap(), bytes);
    }
}
