use anyhow::anyhow;
use graphoxide_cli::install::{
    install, install_skill, install_skill_references_with, install_skill_with_references,
    packaged_skill_references, platform_skill_destination, remove_skill, vscode_install,
    vscode_uninstall, InstallContext, Platform,
};
use std::fs;
use tempfile::TempDir;

fn context(temp: &TempDir, project: bool) -> InstallContext {
    let project_root = temp.path().join("proj");
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

fn assert_skill_roundtrip(platform: Platform, project: bool) {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, project);
    let destination = platform_skill_destination(platform, &context)
        .unwrap()
        .expect("configured platform must have a skill destination");
    if project {
        assert!(destination.starts_with(&context.project_root));
    } else {
        assert!(destination.starts_with(&context.home));
    }

    assert_eq!(
        install_skill(platform, &context).unwrap(),
        Some(destination.clone())
    );
    assert!(destination.is_file(), "{platform} skill was not installed");
    let skill_directory = destination.parent().unwrap();
    assert_eq!(
        fs::read_to_string(skill_directory.join(".graphoxide_version")).unwrap(),
        env!("CARGO_PKG_VERSION")
    );
    let references = skill_directory.join("references");
    if packaged_skill_references(platform).is_some() {
        assert!(
            references.is_dir(),
            "{platform} references were not installed"
        );
        assert!(references.join("extraction-spec.md").is_file());
    } else {
        assert!(
            !references.exists(),
            "{platform} unexpectedly gained references"
        );
    }
    assert!(!skill_directory.join("references.tmp").exists());

    assert!(remove_skill(platform, &context).unwrap());
    assert!(!destination.exists());
    assert!(!skill_directory.join(".graphoxide_version").exists());
    assert!(!references.exists());
}

macro_rules! roundtrip_test {
    ($name:ident, $platform:ident, $project:expr) => {
        #[test]
        fn $name() {
            assert_skill_roundtrip(Platform::$platform, $project);
        }
    };
}

roundtrip_test!(test_skill_roundtrip_user_agents, Agents, false);
roundtrip_test!(test_skill_roundtrip_user_aider, Aider, false);
roundtrip_test!(test_skill_roundtrip_user_amp, Amp, false);
roundtrip_test!(test_skill_roundtrip_user_antigravity, Antigravity, false);
roundtrip_test!(
    test_skill_roundtrip_user_antigravity_windows,
    AntigravityWindows,
    false
);
roundtrip_test!(test_skill_roundtrip_user_claude, Claude, false);
roundtrip_test!(test_skill_roundtrip_user_claw, Claw, false);
roundtrip_test!(test_skill_roundtrip_user_codebuddy, CodeBuddy, false);
roundtrip_test!(test_skill_roundtrip_user_codex, Codex, false);
roundtrip_test!(test_skill_roundtrip_user_copilot, Copilot, false);
roundtrip_test!(test_skill_roundtrip_user_devin, Devin, false);
roundtrip_test!(test_skill_roundtrip_user_droid, Droid, false);
roundtrip_test!(test_skill_roundtrip_user_hermes, Hermes, false);
roundtrip_test!(test_skill_roundtrip_user_kilo, Kilo, false);
roundtrip_test!(test_skill_roundtrip_user_kimi, Kimi, false);
roundtrip_test!(test_skill_roundtrip_user_kiro, Kiro, false);
roundtrip_test!(test_skill_roundtrip_user_opencode, OpenCode, false);
roundtrip_test!(test_skill_roundtrip_user_pi, Pi, false);
roundtrip_test!(test_skill_roundtrip_user_trae, Trae, false);
roundtrip_test!(test_skill_roundtrip_user_trae_cn, TraeCn, false);
roundtrip_test!(test_skill_roundtrip_user_windows, Windows, false);

roundtrip_test!(test_skill_roundtrip_project_agents, Agents, true);
roundtrip_test!(test_skill_roundtrip_project_aider, Aider, true);
roundtrip_test!(test_skill_roundtrip_project_amp, Amp, true);
roundtrip_test!(test_skill_roundtrip_project_antigravity, Antigravity, true);
roundtrip_test!(
    test_skill_roundtrip_project_antigravity_windows,
    AntigravityWindows,
    true
);
roundtrip_test!(test_skill_roundtrip_project_claude, Claude, true);
roundtrip_test!(test_skill_roundtrip_project_claw, Claw, true);
roundtrip_test!(test_skill_roundtrip_project_codebuddy, CodeBuddy, true);
roundtrip_test!(test_skill_roundtrip_project_codex, Codex, true);
roundtrip_test!(test_skill_roundtrip_project_copilot, Copilot, true);
roundtrip_test!(test_skill_roundtrip_project_devin, Devin, true);
roundtrip_test!(test_skill_roundtrip_project_droid, Droid, true);
roundtrip_test!(test_skill_roundtrip_project_hermes, Hermes, true);
roundtrip_test!(test_skill_roundtrip_project_kilo, Kilo, true);
roundtrip_test!(test_skill_roundtrip_project_kimi, Kimi, true);
roundtrip_test!(test_skill_roundtrip_project_kiro, Kiro, true);
roundtrip_test!(test_skill_roundtrip_project_opencode, OpenCode, true);
roundtrip_test!(test_skill_roundtrip_project_pi, Pi, true);
roundtrip_test!(test_skill_roundtrip_project_trae, Trae, true);
roundtrip_test!(test_skill_roundtrip_project_trae_cn, TraeCn, true);
roundtrip_test!(test_skill_roundtrip_project_windows, Windows, true);

#[test]
fn test_amp_user_install_at_corrected_agents_path() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let destination = install_skill(Platform::Amp, &context).unwrap().unwrap();
    assert_eq!(
        destination,
        context
            .home
            .join(".config/agents/skills/graphoxide/SKILL.md")
    );
    assert!(destination.is_file());
    assert!(!context.home.join(".amp/skills").exists());
    remove_skill(Platform::Amp, &context).unwrap();
    assert!(!destination.exists());
}

#[test]
fn test_amp_project_install_at_agents_path() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, true);
    let destination = install_skill(Platform::Amp, &context).unwrap().unwrap();
    assert_eq!(
        destination,
        context
            .project_root
            .join(".agents/skills/graphoxide/SKILL.md")
    );
    assert!(destination.is_file());
    remove_skill(Platform::Amp, &context).unwrap();
    assert!(!destination.exists());
}

#[test]
fn test_vscode_install_uninstall_roundtrip() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    vscode_install(&context).unwrap();
    let skill = context.home.join(".copilot/skills/graphoxide/SKILL.md");
    let instructions = context.project_root.join(".github/copilot-instructions.md");
    assert!(skill.is_file());
    assert!(instructions.is_file());
    assert!(fs::read_to_string(&instructions)
        .unwrap()
        .contains("## graphoxide"));
    assert_eq!(
        fs::read_to_string(skill.parent().unwrap().join(".graphoxide_version")).unwrap(),
        env!("CARGO_PKG_VERSION")
    );

    vscode_uninstall(&context).unwrap();
    assert!(!skill.exists());
    assert!(!context.home.join(".copilot/skills").exists());
    if instructions.exists() {
        assert!(!fs::read_to_string(instructions)
            .unwrap()
            .contains("## graphoxide"));
    }
}

#[test]
fn test_install_entrypoint_roundtrip_for_progressive_and_monolith() {
    for platform in [Platform::Claude, Platform::Aider] {
        let temp = TempDir::new().unwrap();
        let context = context(&temp, false);
        install(platform, &context).unwrap();
        let skill = platform_skill_destination(platform, &context)
            .unwrap()
            .unwrap();
        assert!(skill.is_file());
        let references = skill.parent().unwrap().join("references");
        assert_eq!(
            references.is_dir(),
            packaged_skill_references(platform).is_some()
        );
        remove_skill(platform, &context).unwrap();
        assert!(!skill.exists());
    }
}

#[test]
fn test_monolith_to_progressive_upgrade() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = platform_skill_destination(Platform::Claude, &context)
        .unwrap()
        .unwrap();
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(&skill, "old monolith body\n").unwrap();
    fs::write(skill.parent().unwrap().join(".graphoxide_version"), "0.0.1").unwrap();
    assert!(!skill.parent().unwrap().join("references").exists());

    install_skill(Platform::Claude, &context).unwrap();
    let references = skill.parent().unwrap().join("references");
    assert!(references.is_dir());
    assert!(references.join("extraction-spec.md").is_file());
    assert_eq!(
        fs::read_to_string(skill.parent().unwrap().join(".graphoxide_version")).unwrap(),
        env!("CARGO_PKG_VERSION")
    );
    assert!(!skill.parent().unwrap().join("references.tmp").exists());
}

#[test]
fn test_progressive_to_monolith_downgrade_clears_references() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = install_skill(Platform::Claude, &context).unwrap().unwrap();
    assert!(skill.parent().unwrap().join("references").is_dir());

    install_skill_with_references(Platform::Claude, &context, None).unwrap();
    assert!(skill.is_file());
    assert!(!skill.parent().unwrap().join("references").exists());
}

#[test]
fn test_interrupted_references_staging_self_heals() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = platform_skill_destination(Platform::Claude, &context)
        .unwrap()
        .unwrap();
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(&skill, "body\n").unwrap();
    let staging = skill.parent().unwrap().join("references.tmp");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("garbage.md"), "partial\n").unwrap();

    install_skill(Platform::Claude, &context).unwrap();
    let references = skill.parent().unwrap().join("references");
    assert!(references.is_dir());
    assert!(references.join("extraction-spec.md").is_file());
    assert!(!staging.exists());
    assert!(!references.join("garbage.md").exists());
}

#[test]
fn test_failed_copytree_leaves_no_partial_references() {
    let temp = TempDir::new().unwrap();
    let context = context(&temp, false);
    let skill = platform_skill_destination(Platform::Claude, &context)
        .unwrap()
        .unwrap();
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(&skill, "body\n").unwrap();
    let good = skill.parent().unwrap().join("references");
    fs::create_dir(&good).unwrap();
    fs::write(good.join("keep.md"), "keep\n").unwrap();

    let error = install_skill_references_with(&skill, |staging| {
        fs::create_dir(staging)?;
        fs::write(staging.join("partial.md"), "partial\n")?;
        Err(anyhow!("disk full"))
    })
    .unwrap_err();
    assert!(error.to_string().contains("disk full"));
    assert!(!skill.parent().unwrap().join("references.tmp").exists());
    assert!(good.is_dir());
    assert_eq!(fs::read_to_string(good.join("keep.md")).unwrap(), "keep\n");
}
