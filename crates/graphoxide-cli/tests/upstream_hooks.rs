use graphoxide_cli::{
    hooks::{
        self, hooks_dir, install, pinned_executable, post_checkout_script, post_commit_script,
        resolve_hooks_output, status, uninstall, CHECKOUT_MARKER, HOOK_MARKER,
        WINDOWS_DETACH_FLAGS, WORKTREE_GUARD,
    },
    watch::{COMPAT_ROOT_MARKER, ROOT_MARKER},
};
use std::{fs, path::Path, process::Command, time::Duration};
use tempfile::TempDir;

const EXE: &str = "/opt/Graph Oxide/graphoxide";

fn make_repo() -> TempDir {
    let temp = TempDir::new().unwrap();
    let output = Command::new("git")
        .args(["init", "-q", temp.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    temp
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap()
}

fn install_repo(repo: &TempDir) -> String {
    install(repo.path(), Path::new(EXE)).unwrap()
}

fn scripts() -> [String; 2] {
    [
        post_commit_script(Path::new(EXE)),
        post_checkout_script(Path::new(EXE)),
    ]
}

fn git(repo: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn test_install_creates_hook() {
    let repo = make_repo();
    let result = install_repo(&repo);
    let hook = repo.path().join(".git/hooks/post-commit");
    assert!(hook.exists());
    assert!(read(hook).contains(HOOK_MARKER));
    assert!(result.contains("installed"));
}

#[test]
fn test_install_is_executable() {
    let repo = make_repo();
    install_repo(&repo);
    let hook = repo.path().join(".git/hooks/post-commit");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(fs::metadata(hook).unwrap().permissions().mode() & 0o111, 0);
    }
    #[cfg(windows)]
    assert!(read(hook).starts_with("#!/bin/sh\n"));
}

#[test]
fn test_install_idempotent() {
    let repo = make_repo();
    install_repo(&repo);
    let result = install_repo(&repo);
    let text = read(repo.path().join(".git/hooks/post-commit"));
    assert!(result.contains("already installed"));
    assert_eq!(text.matches(HOOK_MARKER).count(), 1);
}

#[test]
fn test_install_appends_to_existing_hook() {
    let repo = make_repo();
    let hook = repo.path().join(".git/hooks/post-commit");
    fs::write(&hook, "#!/bin/bash\necho existing\n").unwrap();
    install_repo(&repo);
    let text = read(hook);
    assert!(text.contains("echo existing"));
    assert!(text.contains(HOOK_MARKER));
}

#[test]
fn test_uninstall_removes_hook() {
    let repo = make_repo();
    install_repo(&repo);
    let result = uninstall(repo.path()).unwrap();
    assert!(!repo.path().join(".git/hooks/post-commit").exists());
    assert!(result.to_ascii_lowercase().contains("removed"));
}

#[test]
fn test_uninstall_no_hook() {
    let repo = make_repo();
    let result = uninstall(repo.path()).unwrap();
    assert!(result.contains("nothing to remove"));
}

#[test]
fn test_status_installed() {
    let repo = make_repo();
    install_repo(&repo);
    assert!(status(repo.path()).contains("installed"));
}

#[test]
fn test_status_not_installed() {
    let repo = make_repo();
    assert!(status(repo.path()).contains("not installed"));
}

#[test]
fn test_no_git_repo_raises() {
    let temp = TempDir::new().unwrap();
    let error = install(temp.path(), Path::new(EXE)).unwrap_err();
    assert!(
        error.to_string().contains("No git repository"),
        "unexpected install error: {error:#}"
    );
}

#[test]
fn test_install_creates_post_checkout_hook() {
    let repo = make_repo();
    install_repo(&repo);
    let hook = repo.path().join(".git/hooks/post-checkout");
    assert!(hook.exists());
    assert!(read(hook).contains(CHECKOUT_MARKER));
}

#[test]
fn test_install_post_checkout_is_executable() {
    let repo = make_repo();
    install_repo(&repo);
    let hook = repo.path().join(".git/hooks/post-checkout");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(fs::metadata(hook).unwrap().permissions().mode() & 0o111, 0);
    }
    #[cfg(windows)]
    assert!(read(hook).starts_with("#!/bin/sh\n"));
}

#[test]
fn test_uninstall_removes_post_checkout_hook() {
    let repo = make_repo();
    install_repo(&repo);
    uninstall(repo.path()).unwrap();
    assert!(!repo.path().join(".git/hooks/post-checkout").exists());
}

#[test]
fn test_status_shows_both_hooks() {
    let repo = make_repo();
    install_repo(&repo);
    let report = status(repo.path());
    assert!(report.contains("post-commit: installed"));
    assert!(report.contains("post-checkout: installed"));
    assert!(report.matches("installed").count() >= 2);
}

#[test]
fn test_hooks_dir_resolves_relative_git_hooks_path() {
    let repo = make_repo();
    let resolved = resolve_hooks_output(repo.path(), ".git/hooks\n", true)
        .unwrap()
        .unwrap();
    assert_eq!(resolved, repo.path().join(".git/hooks"));
}

#[test]
fn test_hooks_dir_rejects_multiline_git_output() {
    let repo = make_repo();
    let resolved =
        resolve_hooks_output(repo.path(), "--path-format=absolute\n.git/hooks\n", true).unwrap();
    assert!(resolved.is_none());
    assert!(!repo.path().join("--path-format=absolute\n.git").exists());
}

#[test]
fn test_hooks_dir_accepts_absolute_git_hooks_path() {
    let repo = make_repo();
    let absolute = repo.path().join("actual-hooks");
    let resolved = resolve_hooks_output(repo.path(), &format!("{}\n", absolute.display()), true)
        .unwrap()
        .unwrap();
    assert_eq!(resolved, absolute);
}

#[test]
fn test_hook_skips_head_on_exe() {
    for script in scripts() {
        assert!(!script.contains("head -c"));
        assert!(!script.contains("_SHEBANG"));
        assert!(script.contains("_GRAPHOXIDE_BIN"));
    }
}

#[test]
fn test_install_embeds_pinned_interpreter() {
    let repo = make_repo();
    install_repo(&repo);
    for name in ["post-commit", "post-checkout"] {
        let text = read(repo.path().join(".git/hooks").join(name));
        assert!(text.contains(EXE));
        assert!(!text.contains("__PINNED_EXECUTABLE__"));
    }
}

#[test]
fn test_install_fallback_is_loud_not_silent() {
    for script in scripts() {
        assert!(script.contains("could not locate the graphoxide binary"));
        assert!(script.contains(">&2"));
    }
}

#[test]
fn test_hook_check_no_additional_context() {
    let temp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .arg("hook-check")
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn test_hooks_do_not_use_nohup() {
    for script in scripts() {
        assert!(!script.contains("nohup"));
        assert!(!script.contains("setsid"));
        assert!(!script.contains("disown"));
    }
}

#[test]
fn test_hooks_use_cross_platform_detach() {
    for script in scripts() {
        assert!(script.contains("hook-launch"));
        assert!(!script.contains(" >/dev/null 2>&1 &"));
    }
    assert_eq!(WINDOWS_DETACH_FLAGS & 0x0800_0000, 0x0800_0000);
    assert_eq!(WINDOWS_DETACH_FLAGS & 0x0000_0200, 0x0000_0200);
}

#[test]
fn test_hooks_limit_windows_workers_by_default() {
    for script in scripts() {
        assert!(script.contains(r#"[ -n "${WINDIR:-}" ] || [ -n "${MSYSTEM:-}" ]"#));
        assert!(script.contains(r#"export GRAPHOXIDE_MAX_WORKERS="${GRAPHOXIDE_MAX_WORKERS:-1}""#));
        assert!(script.contains("RAYON_NUM_THREADS"));
    }
}

#[test]
fn test_launcher_payload_is_shell_quote_safe() {
    for script in scripts() {
        assert!(script.contains(r#""$_GRAPHOXIDE_BIN" hook-launch"#));
        assert!(!script.contains("eval "));
    }
}

#[test]
fn test_launcher_and_rebuild_body_are_valid_python() {
    // Graphoxide needs no embedded Python. Validate the emitted POSIX shell itself.
    #[cfg(unix)]
    for script in scripts() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("hook");
        fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
        let output = Command::new("sh")
            .args(["-n", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_rebuild_bodies_are_shell_quote_safe() {
    for script in scripts() {
        assert!(!script.contains("python -c"));
        assert!(!script.contains("eval "));
        assert!(script.contains(r#""$_GRAPHOXIDE_BIN""#));
    }
}

#[test]
fn test_rebuild_bodies_read_graphify_root() {
    // Native rebuild context honors both the new marker and the compatibility marker.
    assert_eq!(ROOT_MARKER, ".graphoxide_root");
    assert_eq!(COMPAT_ROOT_MARKER, ".graphify_root");
    for script in scripts() {
        assert!(script.contains("hook-launch"));
        assert!(script.contains(" . \"$_GRAPHOXIDE_LOG\""));
    }
}

#[test]
fn test_rebuild_bodies_with_graphify_root_are_valid_python() {
    // There is no embedded Python body in the native port; both hook scripts pass shell syntax.
    test_launcher_and_rebuild_body_are_valid_python();
}

#[test]
fn test_rebuild_bodies_arm_a_timeout_without_sigalrm() {
    assert_eq!(hooks::rebuild_timeout_from(None), Duration::from_secs(600));
    assert_eq!(
        hooks::rebuild_timeout_from(Some("17")),
        Duration::from_secs(17)
    );
    assert_eq!(hooks::rebuild_timeout_from(Some("0")), Duration::ZERO);
    assert_ne!(WINDOWS_DETACH_FLAGS & 0x0800_0000, 0);
}

#[test]
fn test_detached_launch_targets_graphify_python() {
    for script in scripts() {
        assert!(script.contains(r#""$_GRAPHOXIDE_BIN" hook-launch"#));
        assert!(!script.contains("python"));
    }
}

#[test]
fn test_installed_hooks_contain_no_nohup() {
    let repo = make_repo();
    install_repo(&repo);
    for name in ["post-commit", "post-checkout"] {
        let text = read(repo.path().join(".git/hooks").join(name));
        assert!(!text.contains("nohup"));
        assert!(text.contains("hook-launch"));
    }
}

#[test]
fn test_windows_hookspath_rejected_no_junk_dir_on_posix() {
    let repo = make_repo();
    for candidate in [
        r"C:\Users\u\repo\.git\hooks",
        "c:/Users/u/.git/hooks",
        r"D:\hooks",
        r"some\back\slashed\path",
    ] {
        let error = resolve_hooks_output(repo.path(), candidate, true).unwrap_err();
        assert!(error.to_string().contains("Windows path"));
    }
    assert!(fs::read_dir(repo.path()).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains('\\')));
}

#[test]
fn test_posix_custom_hookspath_still_works() {
    let repo = make_repo();
    assert!(git(
        repo.path(),
        &["config", "--local", "core.hooksPath", ".husky"]
    )
    .status
    .success());
    install_repo(&repo);
    assert!(repo.path().join(".husky/post-commit").exists());
}

#[test]
fn test_default_hooks_dir_unaffected() {
    let repo = make_repo();
    install_repo(&repo);
    assert!(repo.path().join(".git/hooks/post-commit").exists());
}

#[test]
fn test_probes_use_find_spec_not_full_import() {
    // A pinned native executable eliminates all interpreter imports/probes.
    for script in scripts() {
        assert!(!script.contains("import graphify"));
        assert!(!script.contains("find_spec"));
        assert!(script.contains("command -v graphoxide"));
    }
}

#[test]
fn test_shebang_read_is_null_byte_safe() {
    for script in scripts() {
        assert!(!script.contains("head -1"));
        assert!(!script.contains("_SHEBANG"));
    }
}

#[test]
fn test_probe_prefers_sibling_python_exe_on_windows_layouts() {
    // The Rust executable is pinned directly, so no Python layout inference is needed.
    for script in scripts() {
        assert!(script.contains(EXE));
        assert!(!script.contains("python.exe"));
    }
}

#[test]
fn test_file_path_allowlist_accepts_windows_backslash_path() {
    for path in [
        r"C:\Users\u\.venv\Scripts\graphoxide.exe",
        r"C:\Graphoxide\graphoxide.exe",
    ] {
        assert_eq!(pinned_executable(Path::new(path)), Some(path));
    }
}

#[test]
fn test_shebang_allowlist_accepts_windows_backslash_path() {
    let path = r"C:\Users\u\.local\bin\graphoxide.exe";
    assert_eq!(pinned_executable(Path::new(path)), Some(path));
    assert!(post_commit_script(Path::new(path)).contains(path));
}

#[test]
fn test_python_detect_allowlists_still_reject_shell_metacharacters() {
    for path in ["foo;rm -rf /", "foo`id`", "foo$(id)", "foo$IFS"] {
        assert_eq!(pinned_executable(Path::new(path)), None, "{path}");
    }
}

#[test]
fn test_hooks_reuse_git_dir_from_env() {
    for script in scripts() {
        assert!(script.contains("GIT_DIR=${GIT_DIR:-"));
    }
}

#[test]
fn test_hooks_honor_skip_env() {
    for script in scripts() {
        assert!(script.contains("GRAPHOXIDE_SKIP_HOOK"));
        assert!(script.contains("GRAPHIFY_SKIP_HOOK"));
        assert!(script.contains("] && exit 0"));
    }
}

#[test]
fn test_hooks_skip_linked_worktrees() {
    for script in scripts() {
        assert_eq!(script.matches("_GOX_GITDIR=").count(), 1);
        assert!(script.contains("git rev-parse --git-common-dir"));
        assert!(script.contains(r#"[ "$_GOX_GITDIR" != "$_GOX_COMMONDIR" ]"#));
    }
}

#[test]
fn test_worktree_guard_runs_on_primary_skips_linked() {
    let temp = TempDir::new().unwrap();
    let primary = temp.path().join("primary");
    fs::create_dir(&primary).unwrap();
    assert!(git(&primary, &["init", "-q", "."]).status.success());
    assert!(git(&primary, &["config", "user.email", "t@t.co"])
        .status
        .success());
    assert!(git(&primary, &["config", "user.name", "t"])
        .status
        .success());
    fs::write(primary.join("a.txt"), "x").unwrap();
    assert!(git(&primary, &["add", "-A"]).status.success());
    assert!(git(&primary, &["commit", "-qm", "init"]).status.success());
    let linked = temp.path().join("linked");
    assert!(git(
        &primary,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().unwrap(),
            "-b",
            "feature"
        ]
    )
    .status
    .success());

    let snippet = format!("{WORKTREE_GUARD}echo RAN\n");
    let primary_result = Command::new("sh")
        .args(["-c", &snippet])
        .current_dir(&primary)
        .output()
        .unwrap();
    let linked_result = Command::new("sh")
        .args(["-c", &snippet])
        .current_dir(&linked)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&primary_result.stdout).contains("RAN"));
    assert!(!String::from_utf8_lossy(&linked_result.stdout).contains("RAN"));
}

fn append_duplicate_config_entries(repo: &Path) {
    let config = repo.join(".git/config");
    let mut text = read(&config);
    text.push_str(
        "[remote \"origin\"]\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[core]\n\tignorecase = true\n",
    );
    fs::write(config, text).unwrap();
}

#[test]
fn test_hooks_dir_no_warning_on_duplicate_config_keys() {
    let repo = make_repo();
    append_duplicate_config_entries(repo.path());
    assert_eq!(
        hooks_dir(repo.path()).unwrap(),
        repo.path().join(".git/hooks").canonicalize().unwrap()
    );
}

#[test]
fn test_hooks_dir_duplicate_config_keys_honor_custom_hookspath() {
    let repo = make_repo();
    assert!(git(
        repo.path(),
        &["config", "--local", "core.hooksPath", ".husky"]
    )
    .status
    .success());
    append_duplicate_config_entries(repo.path());
    assert_eq!(
        hooks_dir(repo.path()).unwrap(),
        repo.path().join(".husky").canonicalize().unwrap()
    );
}

#[test]
fn test_install_registers_merge_driver() {
    let repo = make_repo();
    let result = install_repo(&repo);
    let driver = git(repo.path(), &["config", "--get", "merge.graphoxide.driver"]);
    assert!(driver.status.success());
    assert!(String::from_utf8_lossy(&driver.stdout).contains("merge-driver %O %A %B"));
    assert!(read(repo.path().join(".gitattributes"))
        .lines()
        .any(|line| line.contains("graph.json") && line.contains("merge=graphoxide")));
    assert!(result.contains("merge driver"));
}

#[test]
fn test_install_merge_driver_idempotent() {
    let repo = make_repo();
    install_repo(&repo);
    install_repo(&repo);
    let matches = read(repo.path().join(".gitattributes"))
        .lines()
        .filter(|line| line.contains("merge=graphoxide"))
        .count();
    assert_eq!(matches, 1);
}

#[test]
fn test_install_preserves_existing_gitattributes() {
    let repo = make_repo();
    fs::write(repo.path().join(".gitattributes"), "*.png binary\n").unwrap();
    install_repo(&repo);
    let text = read(repo.path().join(".gitattributes"));
    assert!(text.contains("*.png binary"));
    assert!(text.contains("merge=graphoxide"));
}

#[test]
fn test_uninstall_removes_merge_driver_keeps_other_attrs() {
    let repo = make_repo();
    fs::write(repo.path().join(".gitattributes"), "*.png binary\n").unwrap();
    install_repo(&repo);
    uninstall(repo.path()).unwrap();
    let driver = git(repo.path(), &["config", "--get", "merge.graphoxide.driver"]);
    assert!(!driver.status.success());
    let text = read(repo.path().join(".gitattributes"));
    assert!(text.contains("*.png binary"));
    assert!(!text.contains("merge=graphoxide"));
}

#[test]
fn test_pinned_python_accepts_paths_containing_spaces() {
    for path in [
        r"C:\Users\First Last\AppData\Roaming\graphoxide\graphoxide.exe",
        r"C:\Program Files\Graphoxide\graphoxide.exe",
        "/home/first last/.local/bin/graphoxide",
    ] {
        assert_eq!(pinned_executable(Path::new(path)), Some(path));
    }
}

#[test]
fn test_pinned_python_still_rejects_shell_metacharacters() {
    for path in [
        r"C:\Users\evil\graphoxide.exe; rm -rf /",
        "/tmp/gox`id`",
        "/tmp/gox$(id)",
        "/tmp/gox$IFS",
        r"C:\Users\ev'il\graphoxide.exe",
        "/tmp/gox\"quote",
    ] {
        assert_eq!(pinned_executable(Path::new(path)), None, "{path}");
    }
}

#[test]
fn test_merge_driver_quotes_interpreter_with_spaces() {
    let repo = make_repo();
    let executable = Path::new(r"C:\Users\First Last\Graphoxide\graphoxide.exe");
    install(repo.path(), executable).unwrap();
    let driver =
        String::from_utf8(git(repo.path(), &["config", "--get", "merge.graphoxide.driver"]).stdout)
            .unwrap();
    let driver = driver.trim();
    assert!(driver.starts_with(&format!("\"{}\"", executable.display())));
    assert!(driver.ends_with("merge-driver %O %A %B"));
}

#[test]
fn test_install_pins_interpreter_path_with_spaces() {
    let repo = make_repo();
    let executable = Path::new(r"C:\Users\First Last\Graphoxide\graphoxide.exe");
    install(repo.path(), executable).unwrap();
    for name in ["post-commit", "post-checkout"] {
        let script = read(repo.path().join(".git/hooks").join(name));
        assert!(script.contains(&format!("_GRAPHOXIDE_BIN='{}'", executable.display())));
        assert!(!script.contains("_GRAPHOXIDE_BIN=''"));
    }
}
