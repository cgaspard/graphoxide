use graphoxide_cli::install::{
    self, agents_platform_install, codebuddy_install, codebuddy_platform_install,
    codebuddy_platform_uninstall, codebuddy_uninstall, install, platform_skill_destination,
    skill_asset, InstallContext, Platform, BASE_SKILL,
};
use serde_json::Value;
use std::{
    fs,
    path::Path,
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

fn json_file(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn hook_entries(root: &Path) -> Vec<Value> {
    json_file(&root.join(".codebuddy/settings.json"))["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .clone()
}

mod codebuddy {
    use super::*;

    #[test]
    fn test_codebuddy_install_user_creates_skill_file() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        install(Platform::CodeBuddy, &context).unwrap();
        assert!(context
            .home
            .join(".codebuddy/skills/graphoxide/SKILL.md")
            .is_file());
    }

    #[test]
    fn test_codebuddy_skill_file_contains_frontmatter() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        install(Platform::CodeBuddy, &context).unwrap();
        let content =
            fs::read_to_string(context.home.join(".codebuddy/skills/graphoxide/SKILL.md")).unwrap();
        assert!(content.contains("name: graphoxide"));
        assert!(content.contains("description:"));
    }

    #[test]
    fn test_codebuddy_skill_file_references_graphify_query() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        install(Platform::CodeBuddy, &context).unwrap();
        let content =
            fs::read_to_string(context.home.join(".codebuddy/skills/graphoxide/SKILL.md")).unwrap();
        assert!(content.contains("graphoxide query"));
    }

    #[test]
    fn test_codebuddy_install_project_writes_codebuddy_md() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        codebuddy_platform_install(&context).unwrap();
        let content = fs::read_to_string(context.project_root.join("CODEBUDDY.md")).unwrap();
        assert!(content.contains("## graphoxide"));
        assert!(content.contains("graphoxide-out/"));
    }

    #[test]
    fn test_codebuddy_install_project_writes_hook() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        codebuddy_platform_install(&context).unwrap();
        assert!(hook_entries(&context.project_root)
            .iter()
            .any(|hook| hook.to_string().contains("graphoxide")));
    }

    #[test]
    fn test_codebuddy_install_hook_has_bash_matcher() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        codebuddy_platform_install(&context).unwrap();
        assert!(hook_entries(&context.project_root).iter().any(|hook| {
            hook["matcher"] == "Bash|Grep" && hook.to_string().contains("graphoxide")
        }));
    }

    #[test]
    fn test_codebuddy_install_hook_has_read_glob_matcher() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        codebuddy_platform_install(&context).unwrap();
        assert!(hook_entries(&context.project_root).iter().any(|hook| {
            hook["matcher"] == "Read|Glob" && hook.to_string().contains("graphoxide")
        }));
    }

    #[test]
    fn test_codebuddy_install_idempotent() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        codebuddy_install(&context.project_root, &context.executable).unwrap();
        codebuddy_install(&context.project_root, &context.executable).unwrap();
        let content = fs::read_to_string(context.project_root.join("CODEBUDDY.md")).unwrap();
        assert_eq!(content.matches("## graphoxide").count(), 1);
        assert_eq!(hook_entries(&context.project_root).len(), 2);
    }

    #[test]
    fn test_codebuddy_install_upgrades_stale_section() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        let markdown = context.project_root.join("CODEBUDDY.md");
        fs::write(
            &markdown,
            "old content\n\n## graphoxide\nThis is old instructions\n",
        )
        .unwrap();
        codebuddy_install(&context.project_root, &context.executable).unwrap();
        let content = fs::read_to_string(markdown).unwrap();
        assert!(content.contains("old content"));
        assert!(!content.contains("This is old instructions"));
        assert!(content.contains("graphoxide-out/"));
        assert_eq!(content.matches("## graphoxide").count(), 1);
    }

    #[test]
    fn test_codebuddy_install_merges_existing_codebuddy_md() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        fs::write(
            context.project_root.join("CODEBUDDY.md"),
            "# My project rules\n",
        )
        .unwrap();
        codebuddy_install(&context.project_root, &context.executable).unwrap();
        let content = fs::read_to_string(context.project_root.join("CODEBUDDY.md")).unwrap();
        assert!(content.contains("# My project rules"));
        assert!(content.contains("## graphoxide"));
        assert!(content.contains("graphoxide-out/"));
    }

    #[test]
    fn test_codebuddy_install_prints_no_change_on_second_run() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        assert!(codebuddy_install(&context.project_root, &context.executable).unwrap());
        assert!(!codebuddy_install(&context.project_root, &context.executable).unwrap());

        let first = run_ok(&context, &["codebuddy", "install", "--project"]);
        assert!(first.status.success());
        let second = run_ok(&context, &["codebuddy", "install", "--project"]);
        assert!(String::from_utf8_lossy(&second.stdout).contains("no change"));
    }

    #[test]
    fn test_codebuddy_install_hint_git_add() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        run_ok(&context, &["codebuddy", "install"]);
        assert!(context.project_root.join("CODEBUDDY.md").is_file());
    }

    #[test]
    fn test_codebuddy_uninstall_removes_section() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        codebuddy_install(&context.project_root, &context.executable).unwrap();
        codebuddy_uninstall(&context.project_root).unwrap();
        assert!(!context.project_root.join("CODEBUDDY.md").exists());
    }

    #[test]
    fn test_codebuddy_uninstall_removes_hook() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        codebuddy_install(&context.project_root, &context.executable).unwrap();
        codebuddy_uninstall(&context.project_root).unwrap();
        assert!(!hook_entries(&context.project_root)
            .iter()
            .any(|hook| hook.to_string().contains("graphoxide")));
    }

    #[test]
    fn test_codebuddy_uninstall_noop_if_not_installed() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        codebuddy_uninstall(&context.project_root).unwrap();
    }

    #[test]
    fn test_codebuddy_uninstall_noop_if_no_section() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        let markdown = context.project_root.join("CODEBUDDY.md");
        fs::write(&markdown, "# Some other project\n").unwrap();
        codebuddy_uninstall(&context.project_root).unwrap();
        assert!(fs::read_to_string(markdown)
            .unwrap()
            .contains("# Some other project"));
    }

    #[test]
    fn test_codebuddy_uninstall_preserves_other_content() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        let markdown = context.project_root.join("CODEBUDDY.md");
        fs::write(&markdown, "# My project rules\n").unwrap();
        codebuddy_install(&context.project_root, &context.executable).unwrap();
        codebuddy_uninstall(&context.project_root).unwrap();
        let content = fs::read_to_string(markdown).unwrap();
        assert!(!content.contains("## graphoxide"));
        assert!(content.contains("# My project rules"));
    }

    #[test]
    fn test_uninstall_all_removes_codebuddy_md() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        run_ok(&context, &["codebuddy", "install"]);
        assert!(context.project_root.join("CODEBUDDY.md").is_file());
        run_ok(&context, &["uninstall"]);
        assert!(!context.project_root.join("CODEBUDDY.md").exists());
    }

    #[test]
    fn test_uninstall_all_removes_codebuddy_hook() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        run_ok(&context, &["codebuddy", "install"]);
        run_ok(&context, &["uninstall"]);
        assert!(!hook_entries(&context.project_root)
            .iter()
            .any(|hook| hook.to_string().contains("graphoxide")));
    }

    #[test]
    fn test_codebuddy_in_platform_config() {
        assert!(Platform::CONFIG_PLATFORMS.contains(&Platform::CodeBuddy));
        assert_eq!(skill_asset(Platform::CodeBuddy), Some(BASE_SKILL));
        assert_eq!(
            "codebuddy".parse::<Platform>().unwrap(),
            Platform::CodeBuddy
        );
    }

    #[test]
    fn test_codebuddy_platform_skill_destination_user_scope() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        assert_eq!(
            platform_skill_destination(Platform::CodeBuddy, &context)
                .unwrap()
                .unwrap(),
            context.home.join(".codebuddy/skills/graphoxide/SKILL.md")
        );
    }

    #[test]
    fn test_codebuddy_platform_skill_destination_project_scope() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        assert_eq!(
            platform_skill_destination(Platform::CodeBuddy, &context)
                .unwrap()
                .unwrap(),
            context
                .project_root
                .join(".codebuddy/skills/graphoxide/SKILL.md")
        );
    }

    #[test]
    fn test_codebuddy_in_main_help_text() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        let top = run_ok(&context, &["--help"]);
        assert!(String::from_utf8_lossy(&top.stdout).contains("codebuddy"));
        let help = run_ok(&context, &["codebuddy", "--help"]);
        let help = String::from_utf8_lossy(&help.stdout);
        assert!(help.contains("install"));
        assert!(help.contains("uninstall"));
    }

    #[test]
    fn test_codebuddy_skill_file_exists_in_package() {
        assert!(!BASE_SKILL.trim().is_empty());
        assert!(install::packaged_asset_names().contains(&"skill.md"));
    }

    #[test]
    fn test_codebuddy_installation_roundtrip() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        let markdown = context.project_root.join("CODEBUDDY.md");
        fs::write(&markdown, "# My project\n").unwrap();
        codebuddy_platform_install(&context).unwrap();
        codebuddy_platform_uninstall(&context).unwrap();
        let content = fs::read_to_string(markdown).unwrap();
        assert!(!content.contains("## graphoxide"));
        assert!(content.contains("# My project"));
        assert!(!hook_entries(&context.project_root)
            .iter()
            .any(|hook| hook.to_string().contains("graphoxide")));
        assert!(!context
            .project_root
            .join(".codebuddy/skills/graphoxide/SKILL.md")
            .exists());
    }
}

mod agents_platform {
    use super::*;

    fn user_skill(context: &InstallContext) -> std::path::PathBuf {
        context.home.join(".agents/skills/graphoxide/SKILL.md")
    }

    #[test]
    fn test_agents_user_destination_is_user_global_dot_agents() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        assert_eq!(
            platform_skill_destination(Platform::Agents, &context)
                .unwrap()
                .unwrap(),
            user_skill(&context)
        );
        assert_ne!(
            user_skill(&context),
            context
                .home
                .join(".config/agents/skills/graphoxide/SKILL.md")
        );
    }

    #[test]
    fn test_agents_project_destination_is_dot_agents() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        assert_eq!(
            platform_skill_destination(Platform::Agents, &context)
                .unwrap()
                .unwrap(),
            context
                .project_root
                .join(".agents/skills/graphoxide/SKILL.md")
        );
    }

    #[test]
    fn test_skills_alias_resolves_to_agents() {
        assert_eq!("skills".parse::<Platform>().unwrap(), Platform::Agents);
        assert_eq!("agents".parse::<Platform>().unwrap(), Platform::Agents);
        assert_eq!("amp".parse::<Platform>().unwrap(), Platform::Amp);
    }

    fn assert_global_skill_only(alias: &str) {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        run_ok(&context, &["install", "--platform", alias]);
        let skill = user_skill(&context);
        assert!(skill.is_file());
        assert_eq!(
            fs::read_to_string(skill.parent().unwrap().join(".graphoxide_version")).unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        assert!(skill
            .parent()
            .unwrap()
            .join("references/extraction-spec.md")
            .is_file());
        assert!(!context.project_root.join("AGENTS.md").exists());
    }

    #[test]
    fn test_install_platform_agents_writes_user_global_skill_only_agents() {
        assert_global_skill_only("agents");
    }

    #[test]
    fn test_install_platform_agents_writes_user_global_skill_only_skills() {
        assert_global_skill_only("skills");
    }

    #[test]
    fn test_uninstall_platform_agents_removes_user_global_skill() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        run_ok(&context, &["install", "--platform", "agents"]);
        let skill = user_skill(&context);
        run_ok(&context, &["uninstall"]);
        assert!(!skill.exists());
        assert!(!context.home.join(".agents/skills").exists());
    }

    fn assert_global_flag_uninstall(alias: &str) {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        run_ok(&context, &["install", "--platform", alias]);
        let skill = user_skill(&context);
        assert!(skill.is_file());
        run_ok(&context, &["uninstall", "--platform", alias]);
        assert!(!skill.exists());
    }

    #[test]
    fn test_uninstall_platform_flag_global_removes_skill_agents() {
        assert_global_flag_uninstall("agents");
    }

    #[test]
    fn test_uninstall_platform_flag_global_removes_skill_skills() {
        assert_global_flag_uninstall("skills");
    }

    #[test]
    fn test_project_uninstall_all_removes_agents_skill() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        run_ok(&context, &["install", "--project", "--platform", "agents"]);
        let skill = context
            .project_root
            .join(".agents/skills/graphoxide/SKILL.md");
        assert!(skill.is_file());
        run_ok(&context, &["uninstall", "--project"]);
        assert!(!skill.exists());
    }

    #[test]
    fn test_install_platform_agents_project_writes_dot_agents() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, true);
        run_ok(&context, &["install", "--project", "--platform", "agents"]);
        let skill = context
            .project_root
            .join(".agents/skills/graphoxide/SKILL.md");
        assert!(skill.is_file());
        assert!(skill
            .parent()
            .unwrap()
            .join("references/extraction-spec.md")
            .is_file());
        assert!(!context.home.join(".agents/skills").exists());
        run_ok(
            &context,
            &["uninstall", "--project", "--platform", "agents"],
        );
        assert!(!skill.exists());
    }

    #[test]
    fn test_agents_subcommand_install_also_wires_agents_md() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        run_ok(&context, &["agents", "install"]);
        let skill = user_skill(&context);
        let markdown = context.project_root.join("AGENTS.md");
        assert!(skill.is_file());
        assert!(fs::read_to_string(&markdown)
            .unwrap()
            .contains("## graphoxide"));
        run_ok(&context, &["agents", "uninstall"]);
        assert!(!skill.exists());
        assert!(
            !markdown.exists()
                || !fs::read_to_string(markdown)
                    .unwrap()
                    .contains("## graphoxide")
        );
    }

    #[test]
    fn test_agents_subcommand_install_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        agents_platform_install(&context).unwrap();
        agents_platform_install(&context).unwrap();
        let content = fs::read_to_string(context.project_root.join("AGENTS.md")).unwrap();
        assert_eq!(content.matches("## graphoxide").count(), 1);
    }

    #[test]
    fn test_skills_subcommand_is_the_agents_subcommand() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        run_ok(&context, &["skills", "install"]);
        let skill = user_skill(&context);
        let markdown = context.project_root.join("AGENTS.md");
        assert!(skill.is_file());
        assert!(skill
            .parent()
            .unwrap()
            .join("references/extraction-spec.md")
            .is_file());
        assert!(fs::read_to_string(&markdown)
            .unwrap()
            .contains("## graphoxide"));
        run_ok(&context, &["skills", "uninstall"]);
        assert!(!skill.exists());
        assert!(
            !markdown.exists()
                || !fs::read_to_string(markdown)
                    .unwrap()
                    .contains("## graphoxide")
        );
    }

    #[test]
    fn test_bare_install_does_not_touch_dot_agents() {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        run_ok(&context, &["install"]);
        assert!(!context.home.join(".agents/skills").exists());
    }
}
