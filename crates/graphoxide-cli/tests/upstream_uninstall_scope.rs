//! Executable port of pinned upstream `tests/test_uninstall_scope.py`.

use graphoxide_cli::install::{platform_skill_destination, uninstall, InstallContext, Platform};
use std::{fs, path::Path};
use tempfile::TempDir;

fn contexts(temp: &TempDir) -> (InstallContext, InstallContext) {
    let project_root = temp.path().join("project");
    let home = temp.path().join("home");
    fs::create_dir_all(&project_root).unwrap();
    fs::create_dir_all(&home).unwrap();
    let user = InstallContext {
        project_root,
        home,
        project: false,
        executable: "graphoxide".into(),
        windows: false,
        local_app_data: None,
    };
    let project = InstallContext {
        project: true,
        ..user.clone()
    };
    (user, project)
}

fn skill_path(platform: Platform, context: &InstallContext) -> std::path::PathBuf {
    platform_skill_destination(platform, context)
        .unwrap()
        .expect("scoped platform must expose a skill destination")
}

fn plant_skill_tree(skill: &Path) -> std::path::PathBuf {
    let directory = skill.parent().unwrap();
    fs::create_dir_all(directory.join("references")).unwrap();
    fs::write(skill, "# graphoxide skill\n").unwrap();
    fs::write(directory.join("references/x.md"), "ref\n").unwrap();
    fs::write(directory.join(".graphoxide_version"), "0.0.0-test").unwrap();
    directory.to_path_buf()
}

fn assert_tree_present(directory: &Path) {
    assert!(directory.join("SKILL.md").is_file());
    assert!(directory.join("references/x.md").is_file());
    assert!(directory.join(".graphoxide_version").is_file());
}

fn project_dir_call_never_touches_global(platform: Platform) {
    let temp = TempDir::new().unwrap();
    let (user, project) = contexts(&temp);
    let global_tree = plant_skill_tree(&skill_path(platform, &user));
    let project_tree = plant_skill_tree(&skill_path(platform, &project));

    uninstall(platform, &project).unwrap();

    assert_tree_present(&global_tree);
    assert!(!project_tree.exists());
}

fn bare_call_still_removes_global(platform: Platform) {
    let temp = TempDir::new().unwrap();
    let (user, _) = contexts(&temp);
    let global_tree = plant_skill_tree(&skill_path(platform, &user));

    uninstall(platform, &user).unwrap();

    assert!(!global_tree.exists());
}

fn remove_user_skill_opt_in_with_project_dir(platform: Platform) {
    let temp = TempDir::new().unwrap();
    let (user, project) = contexts(&temp);
    let global_tree = plant_skill_tree(&skill_path(platform, &user));
    let project_tree = plant_skill_tree(&skill_path(platform, &project));

    // Rust makes the upstream opt-in explicit: the same project root is kept,
    // while `project: false` selects the user-global skill for removal.
    uninstall(platform, &user).unwrap();

    assert!(!global_tree.exists());
    assert_tree_present(&project_tree);
}

fn project_true_removes_only_project_tree(platform: Platform) {
    let temp = TempDir::new().unwrap();
    let (user, project) = contexts(&temp);
    let global_tree = plant_skill_tree(&skill_path(platform, &user));
    let project_tree = plant_skill_tree(&skill_path(platform, &project));

    uninstall(platform, &project).unwrap();

    assert_tree_present(&global_tree);
    assert!(!project_tree.exists());
}

mod uninstall_scope {
    use super::*;

    macro_rules! platform_scope_cases {
        (
            $platform:ident,
            $project_dir:ident,
            $bare:ident,
            $remove_user:ident,
            $project_true:ident
        ) => {
            #[test]
            fn $project_dir() {
                project_dir_call_never_touches_global(Platform::$platform);
            }

            #[test]
            fn $bare() {
                bare_call_still_removes_global(Platform::$platform);
            }

            #[test]
            fn $remove_user() {
                remove_user_skill_opt_in_with_project_dir(Platform::$platform);
            }

            #[test]
            fn $project_true() {
                project_true_removes_only_project_tree(Platform::$platform);
            }
        };
    }

    platform_scope_cases!(
        Claude,
        test_project_dir_call_never_touches_global_claude,
        test_bare_call_still_removes_global_claude,
        test_remove_user_skill_opt_in_with_project_dir_claude,
        test_project_true_removes_only_project_tree_claude
    );
    platform_scope_cases!(
        Gemini,
        test_project_dir_call_never_touches_global_gemini,
        test_bare_call_still_removes_global_gemini,
        test_remove_user_skill_opt_in_with_project_dir_gemini,
        test_project_true_removes_only_project_tree_gemini
    );
    platform_scope_cases!(
        CodeBuddy,
        test_project_dir_call_never_touches_global_codebuddy,
        test_bare_call_still_removes_global_codebuddy,
        test_remove_user_skill_opt_in_with_project_dir_codebuddy,
        test_project_true_removes_only_project_tree_codebuddy
    );

    #[test]
    fn test_project_uninstall_codebuddy_spares_global() {
        let temp = TempDir::new().unwrap();
        let (user, project) = contexts(&temp);
        let global_tree = plant_skill_tree(&skill_path(Platform::CodeBuddy, &user));
        let project_tree = plant_skill_tree(&skill_path(Platform::CodeBuddy, &project));

        uninstall(Platform::CodeBuddy, &project).unwrap();

        assert_tree_present(&global_tree);
        assert!(!project_tree.exists());
    }
}
