//! Exact executable port of Graphify's install references and upgrade guards.

use graphoxide_cli::install::{
    agents_install, claude_pretooluse_hooks, cursor_install, gemini_install, install,
    install_instruction_surfaces, install_skill, install_skill_with_references,
    packaged_asset_names, packaged_skill_references, platform_skill_destination, remove_skill,
    skill_asset, skill_version_warning, uninstall, vscode_install, InstallContext, Platform,
};
use serde_json::json;
use std::{collections::BTreeSet, fs, path::Path};
use tempfile::TempDir;

const OLD_SECTION: &str = r#"## graphify

This project has a knowledge graph at graphify-out/.

Rules:
- ALWAYS read graphify-out/GRAPH_REPORT.md before reading source files.
- Prefer `graphify query "<question>"` for cross-module questions.
"#;

const OLD_VSCODE_SECTION: &str = r#"## graphify

For architecture questions, your first tool call must be to read graphify-out/GRAPH_REPORT.md.
"#;

const OLD_CURSOR_RULE: &str = r#"---
description: graphify knowledge graph context
alwaysApply: true
---

Before answering architecture questions, read graphify-out/GRAPH_REPORT.md.
"#;

const OLD_KIRO_STEERING: &str = r#"---
inclusion: always
---

If graphify-out/GRAPH_REPORT.md exists, read it before answering architecture questions.
"#;

const REFERENCE_NAMES: &[&str] = &[
    "add-watch.md",
    "exports.md",
    "extraction-spec.md",
    "github-and-merge.md",
    "hooks.md",
    "query.md",
    "transcribe.md",
    "update.md",
];

fn context(temp: &TempDir, project: bool) -> InstallContext {
    let project_root = temp.path().join("project");
    let home = temp.path().join("home");
    fs::create_dir_all(&project_root).unwrap();
    fs::create_dir_all(&home).unwrap();
    InstallContext {
        project_root,
        home,
        project,
        executable: "/opt/graphoxide".into(),
        windows: false,
        local_app_data: None,
    }
}

fn destination(platform: Platform, context: &InstallContext) -> std::path::PathBuf {
    platform_skill_destination(platform, context)
        .unwrap()
        .expect("platform skill destination")
}

fn installed_reference_names(skill: &Path) -> BTreeSet<String> {
    fs::read_dir(skill.parent().unwrap().join("references"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn version_stamps(root: &Path, found: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            version_stamps(&path, found);
        } else if matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".graphoxide_version" | ".graphify_version")
        ) {
            found.push(path);
        }
    }
}

fn assert_query_first(text: &str) {
    assert!(text.contains("graphoxide query"), "{text}");
    assert!(!text.contains("ALWAYS read graphify-out/GRAPH_REPORT.md"));
    assert!(!text.contains("first tool call must be"));
}

#[test]
fn test_install_stages_references_sidecar() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = install_skill(Platform::Claude, &context).unwrap().unwrap();
    assert!(skill.is_file());
    assert_eq!(
        installed_reference_names(&skill),
        REFERENCE_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    );
    assert!(!skill.parent().unwrap().join("references.tmp").exists());
}

#[test]
fn test_single_version_stamp_covers_skill_and_references() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = destination(Platform::Claude, &context);
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(skill.parent().unwrap().join(".graphify_version"), "0.0.1").unwrap();
    install_skill(Platform::Claude, &context).unwrap();
    let mut stamps = Vec::new();
    version_stamps(skill.parent().unwrap(), &mut stamps);
    assert_eq!(
        stamps,
        [skill.parent().unwrap().join(".graphoxide_version")]
    );
    assert_eq!(
        fs::read_to_string(&stamps[0]).unwrap(),
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn test_reinstall_replaces_references_atomically() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = install_skill(Platform::Claude, &context).unwrap().unwrap();
    let references = skill.parent().unwrap().join("references");
    fs::write(references.join("stale-old.md"), "stale\n").unwrap();
    install_skill(Platform::Claude, &context).unwrap();
    assert!(!references.join("stale-old.md").exists());
    assert!(references.join("extraction-spec.md").is_file());
    assert!(!skill.parent().unwrap().join("references.tmp").exists());
}

#[test]
fn test_uninstall_removes_references_then_walks_dirs() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = install_skill(Platform::Claude, &context).unwrap().unwrap();
    assert!(skill.parent().unwrap().join("references").is_dir());
    assert!(remove_skill(Platform::Claude, &context).unwrap());
    assert!(!skill.parent().unwrap().exists());
    assert!(!context.home.join(".claude/skills").exists());
}

#[test]
fn test_check_skill_version_warns_on_missing_references() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = install_skill(Platform::Claude, &context).unwrap().unwrap();
    fs::remove_dir_all(skill.parent().unwrap().join("references")).unwrap();
    let warning = skill_version_warning(&skill, env!("CARGO_PKG_VERSION")).unwrap();
    assert!(
        warning.contains("references/ sidecar is missing"),
        "{warning}"
    );
}

#[test]
fn test_check_skill_version_ignores_permission_error() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = install_skill(Platform::Claude, &context).unwrap().unwrap();
    let stamp = skill.parent().unwrap().join(".graphoxide_version");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stamp, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(skill_version_warning(&skill, env!("CARGO_PKG_VERSION")).is_none());
        fs::set_permissions(&stamp, fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(not(unix))]
    {
        fs::remove_file(stamp).unwrap();
        assert!(skill_version_warning(&skill, env!("CARGO_PKG_VERSION")).is_none());
    }
}

#[test]
fn test_hard_fail_when_bundle_dir_present_but_references_missing() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let error = install_skill_with_references(Platform::Claude, &context, Some(&[]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("references bundle is empty"), "{error}");
}

#[test]
fn test_unbuilt_bundle_host_falls_back_to_monolith() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = install_skill_with_references(Platform::Claude, &context, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::read_to_string(&skill).unwrap(),
        skill_asset(Platform::Claude).unwrap()
    );
    assert!(!skill.parent().unwrap().join("references").exists());
}

#[test]
fn test_claude_install_ships_lean_core_and_references() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = install_skill(Platform::Claude, &context).unwrap().unwrap();
    let body = fs::read_to_string(&skill).unwrap();
    assert_eq!(body, skill_asset(Platform::Claude).unwrap());
    assert!(body.contains("references/extraction-spec.md"));
    assert!(!body.contains("\"file_type\":\"code|document|paper|image|rationale|concept\""));
    assert!(body.lines().count() < 800);
    assert_eq!(installed_reference_names(&skill).len(), 8);
    assert_eq!(
        fs::read_to_string(skill.parent().unwrap().join(".graphoxide_version")).unwrap(),
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn test_gemini_install_references_all_resolve() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    install(Platform::Gemini, &context).unwrap();
    let skill = destination(Platform::Gemini, &context);
    let body = fs::read_to_string(&skill).unwrap();
    let references = skill.parent().unwrap().join("references");
    assert!(body.contains("references/"));
    for name in REFERENCE_NAMES {
        assert!(
            body.contains(&format!("references/{name}")),
            "missing pointer {name}"
        );
        assert!(references.join(name).is_file(), "dead pointer {name}");
    }
}

#[test]
fn test_claude_twins_ride_the_claude_bundle() {
    let claude = packaged_skill_references(Platform::Claude).unwrap();
    for platform in [Platform::Antigravity, Platform::Kimi, Platform::Gemini] {
        let twin = packaged_skill_references(platform).unwrap();
        assert!(std::ptr::eq(claude.as_ptr(), twin.as_ptr()));
        assert_eq!(claude.len(), twin.len());
    }
}

#[test]
fn test_pyproject_declares_references_globs() {
    let references = packaged_skill_references(Platform::Claude).unwrap();
    assert_eq!(references.len(), 8);
    for (name, body) in references {
        assert!(REFERENCE_NAMES.contains(name));
        assert!(!body.is_empty(), "embedded reference {name} is empty");
    }
    assert!(!packaged_asset_names().contains(&"skills/*/SKILL.md"));
}

#[test]
fn test_built_wheel_ships_the_full_skill_payload() {
    let assets = packaged_asset_names();
    assert_eq!(
        assets
            .iter()
            .filter(|name| name.starts_with("skill") && name.ends_with(".md"))
            .count(),
        15
    );
    assert_eq!(
        packaged_skill_references(Platform::Claude).unwrap().len(),
        8
    );
    let progressive = [
        Platform::Agents,
        Platform::Claude,
        Platform::Codex,
        Platform::Copilot,
        Platform::OpenCode,
        Platform::Kilo,
        Platform::Claw,
        Platform::Droid,
        Platform::Amp,
        Platform::Trae,
        Platform::Kiro,
        Platform::Pi,
        Platform::Windows,
    ];
    assert_eq!(progressive.len(), 13);
    assert!(progressive
        .iter()
        .all(|platform| packaged_skill_references(*platform).is_some()));
    let surfaces = install_instruction_surfaces();
    for required in [
        "agents-section",
        "antigravity-rule",
        "project-section",
        "kiro-steering",
        "cursor-rule",
    ] {
        assert!(surfaces.iter().any(|(name, _)| *name == required));
    }
}

#[test]
fn test_monolith_install_clears_orphan_references() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = destination(Platform::Aider, &context);
    fs::create_dir_all(skill.parent().unwrap().join("references")).unwrap();
    fs::write(
        skill.parent().unwrap().join("references/leftover.md"),
        "leftover\n",
    )
    .unwrap();
    install_skill(Platform::Aider, &context).unwrap();
    assert!(skill.is_file());
    assert!(!skill.parent().unwrap().join("references").exists());
}

#[test]
fn test_amp_user_install_carries_references() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    install(Platform::Amp, &context).unwrap();
    let skill = destination(Platform::Amp, &context);
    assert_eq!(
        skill,
        context
            .home
            .join(".config/agents/skills/graphoxide/SKILL.md")
    );
    assert!(skill
        .parent()
        .unwrap()
        .join("references/exports.md")
        .is_file());
    assert!(skill
        .parent()
        .unwrap()
        .join("references/hooks.md")
        .is_file());
    uninstall(Platform::Amp, &context).unwrap();
    assert!(!skill.parent().unwrap().exists());
}

#[test]
fn test_claude_install_upgrades_stale_section() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    let path = context.project_root.join("CLAUDE.md");
    fs::write(
        &path,
        format!("# My Project\n\nSome description.\n\n{OLD_SECTION}"),
    )
    .unwrap();
    install(Platform::Claude, &context).unwrap();
    let after = fs::read_to_string(path).unwrap();
    assert_query_first(&after);
    assert!(after.contains("# My Project") && after.contains("Some description."));
}

#[test]
fn test_claude_install_upgrades_stale_hook_payload() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    let path = context.project_root.join(".claude/settings.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let old = "case x in *) Read graphify-out/GRAPH_REPORT.md before searching raw files esac";
    fs::write(
        &path,
        serde_json::to_vec(&json!({"hooks":{"PreToolUse":[{
            "matcher":"Bash",
            "hooks":[{"type":"command","command":old}]
        }]}}))
        .unwrap(),
    )
    .unwrap();
    install(Platform::Claude, &context).unwrap();
    let after = fs::read_to_string(path).unwrap();
    assert!(!after.contains(old));
    assert!(after.contains("hook-guard"));
    assert!(!after.contains("case x in"));
    assert_eq!(claude_pretooluse_hooks(&context.executable, false).len(), 2);
}

#[test]
fn test_agents_install_upgrades_stale_section() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("AGENTS.md");
    fs::write(&path, format!("# Project agents\n\n{OLD_SECTION}")).unwrap();
    agents_install(temp.path(), Platform::Codex).unwrap();
    let after = fs::read_to_string(path).unwrap();
    assert_query_first(&after);
    assert!(after.contains("# Project agents"));
}

#[test]
fn test_gemini_install_upgrades_stale_section() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("GEMINI.md");
    fs::write(&path, OLD_SECTION).unwrap();
    gemini_install(temp.path(), Path::new("graphoxide")).unwrap();
    assert_query_first(&fs::read_to_string(path).unwrap());
}

#[test]
fn test_vscode_install_upgrades_stale_section() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let path = context.project_root.join(".github/copilot-instructions.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, OLD_VSCODE_SECTION).unwrap();
    vscode_install(&context).unwrap();
    assert_query_first(&fs::read_to_string(path).unwrap());
}

#[test]
fn test_cursor_install_upgrades_stale_rule() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(".cursor/rules/graphoxide.mdc");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, OLD_CURSOR_RULE).unwrap();
    cursor_install(temp.path()).unwrap();
    let after = fs::read_to_string(path).unwrap();
    assert!(!after.contains("Before answering architecture questions"));
    assert_query_first(&after);
    assert!(after.contains("alwaysApply: true"));
}

#[test]
fn test_kiro_install_upgrades_stale_steering() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    let path = context.project_root.join(".kiro/steering/graphoxide.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, OLD_KIRO_STEERING).unwrap();
    install(Platform::Kiro, &context).unwrap();
    let after = fs::read_to_string(path).unwrap();
    assert!(!after.contains("read it before answering architecture questions"));
    assert_query_first(&after);
    assert!(after.contains("inclusion: always"));
}

#[test]
fn test_kiro_install_ships_references_sidecar_and_version_stamp() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    install(Platform::Kiro, &context).unwrap();
    let skill = destination(Platform::Kiro, &context);
    assert!(skill.is_file());
    assert_eq!(installed_reference_names(&skill).len(), 8);
    assert_eq!(
        fs::read_to_string(skill.parent().unwrap().join(".graphoxide_version")).unwrap(),
        env!("CARGO_PKG_VERSION")
    );
    assert!(!skill.parent().unwrap().join("references.tmp").exists());
    let steering = context.project_root.join(".kiro/steering/graphoxide.md");
    assert!(steering.is_file());
    uninstall(Platform::Kiro, &context).unwrap();
    assert!(!skill.parent().unwrap().exists());
    assert!(!steering.exists());
}
