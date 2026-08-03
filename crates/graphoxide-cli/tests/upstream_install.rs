use graphoxide_cli::install::{
    self, agents_install, agents_uninstall, codebuddy_install, codebuddy_uninstall, cursor_install,
    cursor_uninstall, gemini_install, gemini_uninstall, install as install_platform,
    install_claude_hook, install_codex_hook, platform_skill_destination, remove_marker_section,
    uninstall as uninstall_platform, InstallContext, Platform, AMP_SKILL, BASE_SKILL, CLAW_SKILL,
    CODEX_SKILL, DROID_SKILL, KILO_COMMAND, KILO_SKILL, KIRO_SKILL, MANAGED_HEADING,
    OPENCODE_SKILL, TRAE_SKILL, WINDOWS_SKILL,
};
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};
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
        executable: "/opt/Graph Oxide/graphoxide".into(),
        windows: false,
        local_app_data: None,
    }
}

fn colocated_context(temp: &TempDir) -> InstallContext {
    InstallContext {
        project_root: temp.path().to_path_buf(),
        home: temp.path().to_path_buf(),
        project: false,
        executable: "/usr/bin/graphoxide".into(),
        windows: false,
        local_app_data: None,
    }
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap()
}

fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_str(&read(path)).unwrap()
}

fn run_cli(context: &InstallContext, arguments: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(arguments)
        .current_dir(&context.project_root)
        .env("HOME", &context.home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "graphoxide {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

macro_rules! platform_install_test {
    ($name:ident, $platform:ident, $relative:literal) => {
        #[test]
        fn $name() {
            let temp = TempDir::new().unwrap();
            let context = colocated_context(&temp);
            install_platform(Platform::$platform, &context).unwrap();
            assert!(temp.path().join($relative).is_file());
        }
    };
}

platform_install_test!(
    test_install_default_claude,
    Claude,
    ".claude/skills/graphoxide/SKILL.md"
);
platform_install_test!(
    test_install_codebuddy,
    CodeBuddy,
    ".codebuddy/skills/graphoxide/SKILL.md"
);
platform_install_test!(
    test_install_codex,
    Codex,
    ".codex/skills/graphoxide/SKILL.md"
);
platform_install_test!(
    test_install_opencode,
    OpenCode,
    ".config/opencode/skills/graphoxide/SKILL.md"
);

#[test]
fn test_install_positional_platform_opencode() {
    let temp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["install", "opencode"])
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(temp
        .path()
        .join(".config/opencode/skills/graphoxide/SKILL.md")
        .is_file());
    assert!(!temp
        .path()
        .join(".claude/skills/graphoxide/SKILL.md")
        .exists());
}

#[test]
fn test_install_project_claude_writes_project_scope() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    install_platform(Platform::Claude, &context).unwrap();
    assert!(context
        .project_root
        .join(".claude/skills/graphoxide/SKILL.md")
        .is_file());
    assert!(context.project_root.join(".claude/CLAUDE.md").is_file());
    assert!(!context
        .home
        .join(".claude/skills/graphoxide/SKILL.md")
        .exists());
    let guidance = read(context.project_root.join(".claude/CLAUDE.md"));
    assert!(guidance.contains("graphoxide-out/GRAPH_REPORT.md"));
}

#[test]
fn test_install_project_codex_writes_skill_and_agents() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    install_platform(Platform::Codex, &context).unwrap();
    assert!(context
        .project_root
        .join(".codex/skills/graphoxide/SKILL.md")
        .is_file());
    assert!(context.project_root.join("AGENTS.md").is_file());
    assert!(context.project_root.join(".codex/hooks.json").is_file());
    assert!(!context
        .home
        .join(".codex/skills/graphoxide/SKILL.md")
        .exists());
}

#[test]
fn test_claude_subcommand_project_install_and_uninstall_are_project_scoped() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    let user_skill = context.home.join(".claude/skills/graphoxide/SKILL.md");
    fs::create_dir_all(user_skill.parent().unwrap()).unwrap();
    fs::write(&user_skill, "user skill").unwrap();
    let install = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["claude", "install", "--project"])
        .current_dir(&context.project_root)
        .env("HOME", &context.home)
        .output()
        .unwrap();
    assert!(install.status.success());
    assert!(context.project_root.join("CLAUDE.md").is_file());
    let uninstall = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["claude", "uninstall", "--project"])
        .current_dir(&context.project_root)
        .env("HOME", &context.home)
        .output()
        .unwrap();
    assert!(uninstall.status.success());
    assert!(user_skill.is_file());
    assert!(!context
        .project_root
        .join(".claude/skills/graphoxide/SKILL.md")
        .exists());
    assert!(!context.project_root.join("CLAUDE.md").exists());
}

#[test]
fn test_codex_subcommand_project_install_and_uninstall_are_project_scoped() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    let user_skill = context.home.join(".codex/skills/graphoxide/SKILL.md");
    fs::create_dir_all(user_skill.parent().unwrap()).unwrap();
    fs::write(&user_skill, "user skill").unwrap();
    let install = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["codex", "install", "--project"])
        .current_dir(&context.project_root)
        .env("HOME", &context.home)
        .output()
        .unwrap();
    assert!(install.status.success());
    let uninstall = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["codex", "uninstall", "--project"])
        .current_dir(&context.project_root)
        .env("HOME", &context.home)
        .output()
        .unwrap();
    assert!(uninstall.status.success());
    assert!(user_skill.is_file());
    assert!(!context.project_root.join("AGENTS.md").exists());
    let hooks = read(context.project_root.join(".codex/hooks.json"));
    assert!(!hooks.to_ascii_lowercase().contains("graphoxide"));
}

#[test]
fn test_antigravity_install_project_writes_project_skill() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    run_cli(&context, &["antigravity", "install", "--project"]);
    assert!(context
        .project_root
        .join(".agents/skills/graphoxide/SKILL.md")
        .is_file());
    assert!(!context
        .home
        .join(".agents/skills/graphoxide/SKILL.md")
        .exists());
}

#[test]
fn test_install_help_does_not_install_default() {
    let temp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["install", "opencode", "--help"])
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage: graphoxide install"));
    assert!(stdout.contains("PLATFORM"));
    assert!(!temp.path().join(".claude").exists());
    assert!(!temp.path().join(".config").exists());
}

platform_install_test!(
    test_install_claw,
    Claw,
    ".openclaw/skills/graphoxide/SKILL.md"
);
platform_install_test!(
    test_install_droid,
    Droid,
    ".factory/skills/graphoxide/SKILL.md"
);
platform_install_test!(test_install_trae, Trae, ".trae/skills/graphoxide/SKILL.md");
platform_install_test!(
    test_install_trae_cn,
    TraeCn,
    ".trae-cn/skills/graphoxide/SKILL.md"
);
platform_install_test!(
    test_install_windows,
    Windows,
    ".claude/skills/graphoxide/SKILL.md"
);

#[test]
fn test_install_unknown_platform_exits() {
    assert!("unknown".parse::<Platform>().is_err());
}

#[test]
fn test_codex_skill_contains_spawn_agent() {
    assert!(CODEX_SKILL.contains("spawn_agent"));
}

#[test]
fn test_codex_skill_uses_graphify_with_existing_graph() {
    assert!(CODEX_SKILL.contains("Fast path — existing graph"));
    assert!(CODEX_SKILL.contains("skip Steps 1–5 entirely"));
    for command in ["graphoxide query", "graphoxide explain", "graphoxide path"] {
        assert!(CODEX_SKILL.contains(command));
    }
}

#[test]
fn test_codex_agents_install_mentions_dirty_graph_output() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::Codex).unwrap();
    let content = read(temp.path().join("AGENTS.md"));
    assert!(content.contains("Dirty graphoxide-out/ files are expected"));
    assert!(content.contains("not a reason to skip graphoxide"));
}

#[test]
fn test_opencode_skill_contains_mention() {
    assert!(OPENCODE_SKILL.contains("@mention"));
}

#[test]
fn test_opencode_skill_uses_opencode_agent_guidance() {
    assert!(OPENCODE_SKILL.contains("@mention"));
    assert!(OPENCODE_SKILL.contains("@agent"));
    let b2 = OPENCODE_SKILL
        .split("**Step B3")
        .next()
        .unwrap()
        .split("**Step B2")
        .nth(1)
        .unwrap();
    assert!(!b2.contains("general-purpose"));
    assert!(b2.contains("OpenCode platform"));
}

#[test]
fn test_kilo_skill_mentions_task_tool() {
    assert!(KILO_SKILL.contains("Task"));
}

#[test]
fn test_kilo_skill_avoids_double_quoted_python_c_fstring_dict_keys() {
    assert!(!KILO_SKILL.contains("python -c \"print(f'"));
}

#[test]
fn test_claw_skill_uses_agent_tool_dispatch() {
    let b2 = CLAW_SKILL
        .split("**Step B3")
        .next()
        .unwrap()
        .split("**Step B2")
        .nth(1)
        .unwrap();
    assert!(b2.contains("subagent_type=\"general-purpose\""));
    assert!(!CLAW_SKILL.contains("spawn_agent"));
    assert!(!CLAW_SKILL.contains("@mention"));
}

#[test]
fn test_all_skill_files_exist_in_package() {
    let expected = [
        "skill.md",
        "skill-codex.md",
        "skill-opencode.md",
        "skill-kilo.md",
        "skill-claw.md",
        "skill-windows.md",
        "skill-droid.md",
        "skill-trae.md",
        "skill-kiro.md",
    ];
    for name in expected {
        assert!(
            install::packaged_asset_names().contains(&name),
            "missing {name}"
        );
    }
    for asset in [
        BASE_SKILL,
        CODEX_SKILL,
        OPENCODE_SKILL,
        KILO_SKILL,
        CLAW_SKILL,
        WINDOWS_SKILL,
        DROID_SKILL,
        TRAE_SKILL,
        KIRO_SKILL,
        AMP_SKILL,
    ] {
        assert!(!asset.trim().is_empty());
    }
}

#[test]
fn test_kilo_command_file_exists_in_package() {
    assert!(!KILO_COMMAND.trim().is_empty());
    assert!(install::packaged_asset_names().contains(&"command-kilo.md"));
}

#[test]
fn test_claude_install_registers_claude_md() {
    let temp = TempDir::new().unwrap();
    install_platform(Platform::Claude, &colocated_context(&temp)).unwrap();
    assert!(temp.path().join(".claude/CLAUDE.md").is_file());
}

#[test]
fn test_codex_install_does_not_write_claude_md() {
    let temp = TempDir::new().unwrap();
    install_platform(Platform::Codex, &colocated_context(&temp)).unwrap();
    assert!(!temp.path().join(".claude/CLAUDE.md").exists());
}

#[test]
fn test_codebuddy_install_writes_codebuddy_md() {
    let temp = TempDir::new().unwrap();
    codebuddy_install(temp.path(), Path::new("graphoxide")).unwrap();
    assert!(read(temp.path().join("CODEBUDDY.md")).contains("graphoxide-out/GRAPH_REPORT.md"));
}

#[test]
fn test_codebuddy_install_writes_hook() {
    let temp = TempDir::new().unwrap();
    codebuddy_install(temp.path(), Path::new("graphoxide")).unwrap();
    let settings = read_json(temp.path().join(".codebuddy/settings.json"));
    let hooks = settings["hooks"]["PreToolUse"].as_array().unwrap();
    assert!(hooks.iter().any(|hook| {
        hook["matcher"] == "Bash|Grep"
            && hook["hooks"][0]["command"] == "graphoxide hook-guard search"
    }));
    assert!(hooks.iter().any(|hook| {
        hook["matcher"] == "Read|Glob"
            && hook["hooks"][0]["command"] == "graphoxide hook-guard read"
    }));
}

#[test]
fn test_claude_hook_is_shell_agnostic() {
    let temp = TempDir::new().unwrap();
    install_claude_hook(temp.path(), Path::new("/opt/Graph Oxide/graphoxide")).unwrap();
    let settings = read_json(temp.path().join(".claude/settings.json"));
    let hooks = settings["hooks"]["PreToolUse"].as_array().unwrap();
    let matchers = hooks
        .iter()
        .filter_map(|hook| hook["matcher"].as_str())
        .collect::<Vec<_>>();
    assert!(matchers.contains(&"Bash|Grep"));
    assert!(matchers.contains(&"Read|Glob"));
    for hook in hooks {
        let command = hook["hooks"][0]["command"].as_str().unwrap();
        for token in ["$(", "case ", "[ -f", "&&", "||", ";;", "echo '"] {
            assert!(
                !command.contains(token),
                "shell syntax {token:?} in {command:?}"
            );
        }
        assert!(command.contains("graphoxide") && command.contains("hook-guard"));
        match hook["matcher"].as_str().unwrap() {
            "Bash|Grep" => assert!(command.ends_with("hook-guard search")),
            "Read|Glob" => assert!(command.ends_with("hook-guard read")),
            matcher => panic!("unexpected matcher {matcher}"),
        }
    }
}

#[test]
fn test_claude_hook_install_idempotent_and_replaces_old_bash_hook() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(".claude/settings.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec(&json!({"hooks":{"PreToolUse":[{
            "matcher":"Bash",
            "hooks":[{"type":"command","command":"[ -f graphoxide-out/graph.json ] && echo old"}]
        }]}}))
        .unwrap(),
    )
    .unwrap();
    install_claude_hook(temp.path(), Path::new("graphoxide")).unwrap();
    install_claude_hook(temp.path(), Path::new("graphoxide")).unwrap();
    let hooks = read_json(path)["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(hooks.len(), 2);
    assert!(!hooks.iter().any(|hook| hook.to_string().contains("[ -f")));
}

#[test]
fn test_codebuddy_install_idempotent() {
    let temp = TempDir::new().unwrap();
    codebuddy_install(temp.path(), Path::new("graphoxide")).unwrap();
    codebuddy_install(temp.path(), Path::new("graphoxide")).unwrap();
    assert_eq!(
        read(temp.path().join("CODEBUDDY.md"))
            .matches(MANAGED_HEADING)
            .count(),
        1
    );
}

#[test]
fn test_codebuddy_install_merges_existing_codebuddy_md() {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("CODEBUDDY.md"), "# My project rules\n").unwrap();
    codebuddy_install(temp.path(), Path::new("graphoxide")).unwrap();
    let content = read(temp.path().join("CODEBUDDY.md"));
    assert!(content.contains("# My project rules"));
    assert!(content.contains("graphoxide-out/GRAPH_REPORT.md"));
}

#[test]
fn test_codebuddy_uninstall_removes_section() {
    let temp = TempDir::new().unwrap();
    codebuddy_install(temp.path(), Path::new("graphoxide")).unwrap();
    codebuddy_uninstall(temp.path()).unwrap();
    assert!(!temp.path().join("CODEBUDDY.md").exists());
}

#[test]
fn test_codebuddy_uninstall_removes_hook() {
    let temp = TempDir::new().unwrap();
    codebuddy_install(temp.path(), Path::new("graphoxide")).unwrap();
    codebuddy_uninstall(temp.path()).unwrap();
    assert!(!read(temp.path().join(".codebuddy/settings.json"))
        .to_ascii_lowercase()
        .contains("graphoxide"));
}

#[test]
fn test_codebuddy_uninstall_noop_if_not_installed() {
    codebuddy_uninstall(TempDir::new().unwrap().path()).unwrap();
}

#[test]
fn test_uninstall_project_removes_project_skill_only() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    let global = context.home.join(".codex/skills/graphoxide/SKILL.md");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, "user").unwrap();
    install_platform(Platform::Codex, &context).unwrap();
    uninstall_platform(Platform::Codex, &context).unwrap();
    assert!(global.is_file());
    assert!(!context
        .project_root
        .join(".codex/skills/graphoxide/SKILL.md")
        .exists());
    assert!(!context.project_root.join("AGENTS.md").exists());
}

#[test]
fn test_uninstall_project_without_platform_removes_project_installs() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    let global = context.home.join(".claude/skills/graphoxide/SKILL.md");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, "user").unwrap();
    install_platform(Platform::Claude, &context).unwrap();
    uninstall_platform(Platform::Claude, &context).unwrap();
    assert!(global.is_file());
    assert!(!context.project_root.join(".claude/CLAUDE.md").exists());
}

#[test]
fn test_antigravity_uninstall_project_removes_project_skill_only() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    let global = context
        .home
        .join(".gemini/config/skills/graphoxide/SKILL.md");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, "global").unwrap();
    run_cli(&context, &["antigravity", "install", "--project"]);
    run_cli(&context, &["antigravity", "uninstall", "--project"]);
    assert!(global.is_file());
    assert!(!context
        .project_root
        .join(".agents/skills/graphoxide/SKILL.md")
        .exists());
}

#[test]
fn test_antigravity_global_install_writes_gemini_config_skills() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    run_cli(&context, &["antigravity", "install"]);
    assert!(context
        .home
        .join(".gemini/config/skills/graphoxide/SKILL.md")
        .is_file());
    assert!(!context
        .home
        .join(".agents/skills/graphoxide/SKILL.md")
        .exists());
    assert!(context
        .project_root
        .join(".agents/rules/graphoxide.md")
        .is_file());
    assert!(context
        .project_root
        .join(".agents/workflows/graphoxide.md")
        .is_file());
}

#[test]
fn test_antigravity_global_uninstall_removes_gemini_config_skill() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    run_cli(&context, &["antigravity", "install"]);
    run_cli(&context, &["antigravity", "uninstall"]);
    assert!(!context
        .home
        .join(".gemini/config/skills/graphoxide/SKILL.md")
        .exists());
    assert!(!context
        .project_root
        .join(".agents/rules/graphoxide.md")
        .exists());
    assert!(!context
        .project_root
        .join(".agents/workflows/graphoxide.md")
        .exists());
}

macro_rules! agents_file_test {
    ($name:ident, $platform:ident) => {
        #[test]
        fn $name() {
            let temp = TempDir::new().unwrap();
            agents_install(temp.path(), Platform::$platform).unwrap();
            assert!(temp.path().join("AGENTS.md").is_file());
        }
    };
}

agents_file_test!(test_codex_agents_install_writes_agents_md, Codex);
agents_file_test!(test_opencode_agents_install_writes_agents_md, OpenCode);
agents_file_test!(test_claw_agents_install_writes_agents_md, Claw);

#[test]
fn test_agents_install_idempotent() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::Codex).unwrap();
    agents_install(temp.path(), Platform::Codex).unwrap();
    assert_eq!(
        read(temp.path().join("AGENTS.md"))
            .matches(MANAGED_HEADING)
            .count(),
        1
    );
}

#[test]
fn test_agents_install_appends_to_existing() {
    let temp = TempDir::new().unwrap();
    fs::write(
        temp.path().join("AGENTS.md"),
        "# Existing rules\n\nDo not break things.\n",
    )
    .unwrap();
    agents_install(temp.path(), Platform::Codex).unwrap();
    let content = read(temp.path().join("AGENTS.md"));
    assert!(content.contains("Do not break things."));
    assert!(content.contains(MANAGED_HEADING));
}

#[test]
fn test_agents_uninstall_removes_section() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::Codex).unwrap();
    assert!(agents_uninstall(temp.path(), None).unwrap());
    assert!(!temp.path().join("AGENTS.md").exists());
}

#[test]
fn test_agents_uninstall_preserves_other_content() {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("AGENTS.md"), "# Existing\n\nKeep me.\n").unwrap();
    agents_install(temp.path(), Platform::Codex).unwrap();
    agents_uninstall(temp.path(), None).unwrap();
    let content = read(temp.path().join("AGENTS.md"));
    assert!(content.contains("Keep me."));
    assert!(!content.lines().any(|line| line.trim() == MANAGED_HEADING));
}

#[test]
fn test_agents_uninstall_no_op_when_not_installed() {
    let temp = TempDir::new().unwrap();
    assert!(!agents_uninstall(temp.path(), None).unwrap());
}

#[test]
fn test_remove_marker_section_matches_exact_heading_only() {
    assert!(
        remove_marker_section("# Doc\n\n### graphoxide\n\nmy notes\n", MANAGED_HEADING).is_none()
    );
    assert!(remove_marker_section("see the ## graphoxide bullet\n", MANAGED_HEADING).is_none());
    assert!(remove_marker_section("    ## graphoxide\n", MANAGED_HEADING).is_none());
    let content = "# Doc\n\n### graphoxide\n\nmy notes\n\n## graphoxide\n\ngraphoxide stuff\n";
    let output = remove_marker_section(content, MANAGED_HEADING).unwrap();
    assert!(output.contains("### graphoxide") && output.contains("my notes"));
    assert!(!output.lines().any(|line| line.trim() == MANAGED_HEADING));
    assert!(!output.contains("graphoxide stuff"));
    let nested = "## graphoxide\n\nintro\n\n### sub\n\ninner\n\n## Keep\n\nkeep me\n";
    let output = remove_marker_section(nested, MANAGED_HEADING).unwrap();
    assert!(output.contains("## Keep") && output.contains("keep me"));
    assert!(!output.contains("inner") && !output.contains("intro"));
}

#[test]
fn test_agents_uninstall_preserves_user_h3_graphify_heading() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("AGENTS.md");
    fs::write(
        &path,
        "# My rules\n\n### graphoxide\n\nKeep this.\n\n## Other\n\nUnrelated.\n",
    )
    .unwrap();
    agents_install(temp.path(), Platform::Codex).unwrap();
    agents_uninstall(temp.path(), None).unwrap();
    let content = read(path);
    assert!(content.contains("### graphoxide") && content.contains("Keep this."));
    assert!(content.contains("## Other") && content.contains("Unrelated."));
    assert!(!content.lines().any(|line| line.trim() == MANAGED_HEADING));
}

#[test]
fn test_uninstall_untouched_when_only_user_h3_present() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("AGENTS.md");
    let original = "# My rules\n\n### graphoxide\n\nHand-written. Do not touch.\n";
    fs::write(&path, original).unwrap();
    assert!(!agents_uninstall(temp.path(), None).unwrap());
    assert_eq!(fs::read(path).unwrap(), original.as_bytes());
}

#[test]
fn test_opencode_agents_install_writes_plugin() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::OpenCode).unwrap();
    assert!(
        read(temp.path().join(".opencode/plugins/graphoxide.js")).contains("tool.execute.before")
    );
}

#[test]
fn test_opencode_plugin_reminder_has_no_backticks() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::OpenCode).unwrap();
    let body = read(temp.path().join(".opencode/plugins/graphoxide.js"));
    let reminder = body
        .split("echo \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert!(!reminder.contains('`'));
    assert!(!reminder.contains("$("));
}

#[test]
fn test_opencode_plugin_uses_semicolon_not_ampersand() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::OpenCode).unwrap();
    let body = read(temp.path().join(".opencode/plugins/graphoxide.js"));
    assert!(body.contains("\" ; ' +"));
    assert!(!body.contains("\" && ' +"));
}

#[test]
fn test_opencode_agents_install_registers_plugin_in_config() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::OpenCode).unwrap();
    let config = read_json(temp.path().join(".opencode/opencode.json"));
    assert!(config["plugin"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item.as_str().unwrap().contains("graphoxide.js")));
}

#[test]
fn test_opencode_agents_install_merges_existing_config() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(".opencode/opencode.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, r#"{"model":"claude-opus","plugin":[]}"#).unwrap();
    agents_install(temp.path(), Platform::OpenCode).unwrap();
    let config = read_json(path);
    assert_eq!(config["model"], "claude-opus");
    assert_eq!(config["plugin"].as_array().unwrap().len(), 1);
}

#[test]
fn test_opencode_agents_uninstall_removes_plugin() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::OpenCode).unwrap();
    agents_uninstall(temp.path(), Some(Platform::OpenCode)).unwrap();
    assert!(!temp.path().join(".opencode/plugins/graphoxide.js").exists());
    assert!(
        read_json(temp.path().join(".opencode/opencode.json"))["plugin"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

agents_file_test!(test_kilo_agents_install_writes_agents_md, Kilo);

#[test]
fn test_kilo_agents_install_writes_plugin() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::Kilo).unwrap();
    assert!(read(temp.path().join(".kilo/plugins/graphoxide.js")).contains("tool.execute.before"));
}

#[test]
fn test_kilo_agents_install_registers_plugin_in_config() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::Kilo).unwrap();
    let uri = format!(
        "file://{}",
        temp.path().join(".kilo/plugins/graphoxide.js").display()
    );
    assert!(read_json(temp.path().join(".kilo/kilo.json"))["plugin"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == &uri));
}

#[test]
fn test_kilo_agents_install_merges_existing_config() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(".kilo/kilo.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, r#"{"model":"anthropic/claude-sonnet","plugin":[]}"#).unwrap();
    agents_install(temp.path(), Platform::Kilo).unwrap();
    let config = read_json(path);
    assert_eq!(config["model"], "anthropic/claude-sonnet");
    assert_eq!(config["plugin"].as_array().unwrap().len(), 1);
}

#[test]
fn test_kilo_agents_install_preserves_existing_jsonc_config() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(".kilo/kilo.jsonc");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = "// user comment\n{\n  // preferred model\n  \"model\": \"anthropic/claude-haiku\",\n  \"plugin\": []\n}\n";
    fs::write(&path, original).unwrap();
    agents_install(temp.path(), Platform::Kilo).unwrap();
    assert_eq!(read(&path), original);
    assert_eq!(
        read_json(temp.path().join(".kilo/kilo.json"))["model"],
        "anthropic/claude-haiku"
    );
}

#[test]
fn test_kilo_agents_uninstall_preserves_existing_jsonc_config() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(".kilo/kilo.jsonc");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original =
        "// user comment\n{\n  \"model\": \"anthropic/claude-haiku\",\n  \"plugin\": []\n}\n";
    fs::write(&path, original).unwrap();
    agents_install(temp.path(), Platform::Kilo).unwrap();
    agents_uninstall(temp.path(), Some(Platform::Kilo)).unwrap();
    assert_eq!(read(&path), original);
    assert!(read_json(temp.path().join(".kilo/kilo.json"))["plugin"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn test_kilo_agents_install_idempotent() {
    let temp = TempDir::new().unwrap();
    agents_install(temp.path(), Platform::Kilo).unwrap();
    agents_install(temp.path(), Platform::Kilo).unwrap();
    assert_eq!(
        read(temp.path().join("AGENTS.md"))
            .matches(MANAGED_HEADING)
            .count(),
        1
    );
    assert_eq!(
        read_json(temp.path().join(".kilo/kilo.json"))["plugin"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn test_kilo_install_writes_global_and_project_artifacts() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    install_platform(Platform::Kilo, &context).unwrap();
    assert!(context
        .home
        .join(".config/kilo/skills/graphoxide/SKILL.md")
        .is_file());
    assert!(context
        .home
        .join(".config/kilo/command/graphoxide.md")
        .is_file());
    assert!(context.project_root.join("AGENTS.md").is_file());
    assert!(context
        .project_root
        .join(".kilo/plugins/graphoxide.js")
        .is_file());
}

#[test]
fn test_kilo_uninstall_removes_plugin_registration_and_command() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    install_platform(Platform::Kilo, &context).unwrap();
    uninstall_platform(Platform::Kilo, &context).unwrap();
    assert!(!context
        .home
        .join(".config/kilo/command/graphoxide.md")
        .exists());
    assert!(!context
        .home
        .join(".config/kilo/skills/graphoxide/SKILL.md")
        .exists());
    assert!(!context
        .project_root
        .join(".kilo/plugins/graphoxide.js")
        .exists());
    assert!(
        read_json(context.project_root.join(".kilo/kilo.json"))["plugin"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn test_cursor_install_writes_rule() {
    let temp = TempDir::new().unwrap();
    cursor_install(temp.path()).unwrap();
    let content = read(temp.path().join(".cursor/rules/graphoxide.mdc"));
    assert!(content.contains("alwaysApply: true"));
    assert!(content.contains("graphoxide-out/GRAPH_REPORT.md"));
}

#[test]
fn test_cursor_install_idempotent() {
    let temp = TempDir::new().unwrap();
    cursor_install(temp.path()).unwrap();
    let path = temp.path().join(".cursor/rules/graphoxide.mdc");
    let original = read(&path);
    cursor_install(temp.path()).unwrap();
    assert_eq!(read(path), original);
}

#[test]
fn test_cursor_uninstall_removes_rule() {
    let temp = TempDir::new().unwrap();
    cursor_install(temp.path()).unwrap();
    cursor_uninstall(temp.path()).unwrap();
    assert!(!temp.path().join(".cursor/rules/graphoxide.mdc").exists());
}

#[test]
fn test_cursor_uninstall_noop_if_not_installed() {
    cursor_uninstall(TempDir::new().unwrap().path()).unwrap();
}

#[test]
fn test_gemini_install_writes_gemini_md() {
    let temp = TempDir::new().unwrap();
    gemini_install(temp.path(), Path::new("graphoxide")).unwrap();
    assert!(read(temp.path().join("GEMINI.md")).contains("graphoxide-out/GRAPH_REPORT.md"));
}

#[test]
fn test_gemini_install_writes_hook() {
    let temp = TempDir::new().unwrap();
    gemini_install(temp.path(), Path::new("graphoxide")).unwrap();
    let settings = read_json(temp.path().join(".gemini/settings.json"));
    let hooks = settings["hooks"]["BeforeTool"].as_array().unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0]["matcher"], "read_file|list_directory");
    assert_eq!(
        hooks[0]["hooks"][0]["command"],
        "graphoxide hook-guard gemini"
    );
}

#[test]
fn test_gemini_install_idempotent() {
    let temp = TempDir::new().unwrap();
    gemini_install(temp.path(), Path::new("graphoxide")).unwrap();
    gemini_install(temp.path(), Path::new("graphoxide")).unwrap();
    assert_eq!(
        read(temp.path().join("GEMINI.md"))
            .matches(MANAGED_HEADING)
            .count(),
        1
    );
}

#[test]
fn test_gemini_install_merges_existing_gemini_md() {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("GEMINI.md"), "# My project rules\n").unwrap();
    gemini_install(temp.path(), Path::new("graphoxide")).unwrap();
    let content = read(temp.path().join("GEMINI.md"));
    assert!(content.contains("# My project rules"));
    assert!(content.contains("graphoxide-out/GRAPH_REPORT.md"));
}

#[test]
fn test_gemini_uninstall_removes_section() {
    let temp = TempDir::new().unwrap();
    gemini_install(temp.path(), Path::new("graphoxide")).unwrap();
    gemini_uninstall(temp.path()).unwrap();
    assert!(!temp.path().join("GEMINI.md").exists());
}

#[test]
fn test_gemini_uninstall_removes_hook() {
    let temp = TempDir::new().unwrap();
    gemini_install(temp.path(), Path::new("graphoxide")).unwrap();
    gemini_uninstall(temp.path()).unwrap();
    assert!(!read(temp.path().join(".gemini/settings.json"))
        .to_ascii_lowercase()
        .contains("graphoxide"));
}

#[test]
fn test_gemini_uninstall_noop_if_not_installed() {
    gemini_uninstall(TempDir::new().unwrap().path()).unwrap();
}

#[test]
fn test_amp_user_install_lands_in_config_agents() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    run_cli(&context, &["amp", "install"]);
    assert!(context
        .home
        .join(".config/agents/skills/graphoxide/SKILL.md")
        .is_file());
    assert!(!context
        .home
        .join(".amp/skills/graphoxide/SKILL.md")
        .exists());
    assert!(context.project_root.join("AGENTS.md").is_file());
}

#[test]
fn test_amp_install_cleans_legacy_amp_skills_dir() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let legacy = context.home.join(".amp/skills/graphoxide/SKILL.md");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, "legacy").unwrap();
    run_cli(&context, &["amp", "install"]);
    assert!(!legacy.exists());
    assert!(context
        .home
        .join(".config/agents/skills/graphoxide/SKILL.md")
        .is_file());
}

#[test]
fn test_amp_user_uninstall_removes_skill_and_agents() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    run_cli(&context, &["amp", "install"]);
    run_cli(&context, &["amp", "uninstall"]);
    assert!(!context
        .home
        .join(".config/agents/skills/graphoxide/SKILL.md")
        .exists());
    assert!(!context.home.join(".config/agents/skills").exists());
    assert!(!context.project_root.join("AGENTS.md").exists());
}

#[test]
fn test_amp_project_install_lands_in_dot_agents() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    run_cli(&context, &["amp", "install", "--project"]);
    assert!(context
        .project_root
        .join(".agents/skills/graphoxide/SKILL.md")
        .is_file());
    assert!(!context
        .project_root
        .join(".amp/skills/graphoxide/SKILL.md")
        .exists());
    assert!(context.project_root.join("AGENTS.md").is_file());
    assert!(!context
        .home
        .join(".config/agents/skills/graphoxide/SKILL.md")
        .exists());
}

#[test]
fn test_uninstall_all_removes_amp_user_skill() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    run_cli(&context, &["amp", "install"]);
    run_cli(&context, &["uninstall"]);
    assert!(!context
        .home
        .join(".config/agents/skills/graphoxide/SKILL.md")
        .exists());
}

#[test]
fn test_hermes_skill_destination_windows_uses_localappdata() {
    let temp = TempDir::new().unwrap();
    let mut context = context(&temp, false);
    context.windows = true;
    context.local_app_data = Some(temp.path().join("AppDataLocal"));
    assert_eq!(
        platform_skill_destination(Platform::Hermes, &context)
            .unwrap()
            .unwrap(),
        temp.path()
            .join("AppDataLocal/hermes/skills/graphoxide/SKILL.md")
    );
}

#[test]
fn test_hermes_skill_destination_posix_uses_home() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    assert_eq!(
        platform_skill_destination(Platform::Hermes, &context)
            .unwrap()
            .unwrap(),
        context.home.join(".hermes/skills/graphoxide/SKILL.md")
    );
}

#[test]
fn test_codex_hook_command_is_a_real_cli_subcommand() {
    let temp = TempDir::new().unwrap();
    install_codex_hook(temp.path(), Path::new(env!("CARGO_BIN_EXE_graphoxide"))).unwrap();
    let hooks = read_json(temp.path().join(".codex/hooks.json"));
    let entries = hooks["hooks"]["PreToolUse"].as_array().unwrap();
    assert!(!entries.is_empty());
    for entry in entries {
        let command = entry["hooks"][0]["command"].as_str().unwrap();
        assert!(command.ends_with(" hook-check"));
    }
    assert!(Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .arg("hook-check")
        .status()
        .unwrap()
        .success());
}
