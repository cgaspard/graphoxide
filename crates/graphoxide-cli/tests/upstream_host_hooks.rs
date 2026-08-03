use filetime::{set_file_mtime, FileTime};
use graphoxide_cli::{
    hook_guard::strict_enabled_with_override,
    install::{claude_pretooluse_hooks, gemini_hook as install_gemini_hook},
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

fn run_raw(cwd: &Path, args: &[&str], input: &[u8], environment: &[(&str, String)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT")
        .env_remove("GRAPHOXIDE_HOOK_STRICT")
        .env_remove("GRAPHIFY_HOOK_STRICT")
        .env_remove("GRAPHOXIDE_HOOK_STRICT_TTL")
        .env_remove("GRAPHIFY_HOOK_STRICT_TTL")
        .env_remove("CLAUDE_PROJECT_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut child = command.spawn().unwrap();
    use std::io::Write as _;
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn run_hook(
    cwd: &Path,
    mode: &str,
    payload: &Value,
    strict: bool,
    environment: &[(&str, String)],
) -> Output {
    let mut args = vec!["hook-guard", mode];
    if strict {
        args.push("--strict");
    }
    run_raw(
        cwd,
        &args,
        &serde_json::to_vec(payload).unwrap(),
        environment,
    )
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn make_graph(root: &Path) {
    let output = root.join("graphoxide-out");
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("graph.json"), "{}").unwrap();
}

fn strict_fixture(indexed: bool, fresh: bool) -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("src/mod.py");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "def x():\n    return 1\n").unwrap();
    let output = temp.path().join("graphoxide-out");
    fs::create_dir_all(&output).unwrap();
    let manifest = if indexed {
        json!({"src/mod.py":{"mtime":1}})
    } else {
        json!({"other/z.py":{"mtime":1}})
    };
    fs::write(
        output.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(output.join("graph.json"), r#"{"nodes":[],"links":[]}"#).unwrap();
    set_file_mtime(&source, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();
    set_file_mtime(
        output.join("graph.json"),
        FileTime::from_unix_time(1_700_000_100, 0),
    )
    .unwrap();
    if !fresh {
        set_file_mtime(&source, FileTime::from_unix_time(1_700_000_200, 0)).unwrap();
    }
    (temp, source)
}

fn read_payload(file: &Path, session: Option<&str>) -> Value {
    let mut payload = json!({
        "tool_name":"Read",
        "tool_input":{"file_path":file},
    });
    if let Some(session) = session {
        payload["session_id"] = Value::String(session.to_owned());
    }
    payload
}

fn is_deny(output: &str) -> bool {
    serde_json::from_str::<Value>(output)
        .ok()
        .and_then(|payload| {
            payload
                .pointer("/hookSpecificOutput/permissionDecision")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("deny")
}

mod hook_strict {
    use super::*;

    #[test]
    fn test_strict_first_read_denies_then_nudges() {
        let (temp, file) = strict_fixture(true, true);
        let first = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("s1")),
            true,
            &[],
        ));
        assert!(is_deny(&first));
        assert!(
            serde_json::from_str::<Value>(&first).unwrap()["hookSpecificOutput"]
                ["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("graphoxide query")
        );
        assert!(temp
            .path()
            .join("graphoxide-out/cache/hook_sessions/s1.denied")
            .is_file());

        let second = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("s1")),
            true,
            &[],
        ));
        assert!(!is_deny(&second));
        assert!(second.contains("MANDATORY"));
    }

    #[test]
    fn test_strict_new_session_denies_again() {
        let (temp, file) = strict_fixture(true, true);
        let _ = run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("sA")),
            true,
            &[],
        );
        let output = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("sB")),
            true,
            &[],
        ));
        assert!(is_deny(&output));
    }

    #[test]
    fn test_fresh_query_stamp_suppresses_deny() {
        let (temp, file) = strict_fixture(true, true);
        let stamp = temp.path().join("graphoxide-out/cache/last_query_stamp");
        fs::create_dir_all(stamp.parent().unwrap()).unwrap();
        fs::write(&stamp, "now").unwrap();
        set_file_mtime(&stamp, FileTime::from_system_time(SystemTime::now())).unwrap();
        let output = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("s1")),
            true,
            &[],
        ));
        assert!(!is_deny(&output));
        assert!(output.contains("MANDATORY"));
    }

    #[test]
    fn test_expired_query_stamp_still_denies() {
        let (temp, file) = strict_fixture(true, true);
        let stamp = temp.path().join("graphoxide-out/cache/last_query_stamp");
        fs::create_dir_all(stamp.parent().unwrap()).unwrap();
        fs::write(&stamp, "old").unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        set_file_mtime(&stamp, FileTime::from_unix_time(now - 10_000, 0)).unwrap();
        let output = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("s1")),
            true,
            &[("GRAPHIFY_HOOK_STRICT_TTL", "1800".to_owned())],
        ));
        assert!(is_deny(&output));
    }

    #[test]
    fn test_soft_mode_never_denies() {
        let (temp, file) = strict_fixture(true, true);
        let output = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("s1")),
            false,
            &[],
        ));
        assert!(!is_deny(&output));
        assert!(output.contains("MANDATORY"));
    }

    #[test]
    fn test_env_forces_strict_on() {
        let (temp, file) = strict_fixture(true, true);
        let output = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("s1")),
            false,
            &[("GRAPHIFY_HOOK_STRICT", "1".to_owned())],
        ));
        assert!(is_deny(&output));
    }

    #[test]
    fn test_env_kills_strict() {
        let (temp, file) = strict_fixture(true, true);
        let output = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("s1")),
            true,
            &[("GRAPHIFY_HOOK_STRICT", "0".to_owned())],
        ));
        assert!(!is_deny(&output));
    }

    #[test]
    fn test_out_of_project_read_silenced() {
        let (temp, _file) = strict_fixture(true, true);
        let payload = json!({
            "session_id":"s1",
            "tool_name":"Read",
            "tool_input":{"file_path":"/somewhere/else/x.py"},
        });
        for strict in [false, true] {
            assert!(
                stdout(&run_hook(temp.path(), "read", &payload, strict, &[]))
                    .trim()
                    .is_empty()
            );
        }
    }

    #[test]
    fn test_stale_graph_softens_never_denies() {
        let (temp, file) = strict_fixture(true, false);
        let output = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("s1")),
            true,
            &[],
        ));
        assert!(!is_deny(&output));
        assert!(output.to_ascii_lowercase().contains("stale"));
        assert!(!output.contains("MANDATORY"));
    }

    #[test]
    fn test_needs_update_flag_softens() {
        let (temp, file) = strict_fixture(true, true);
        fs::write(temp.path().join("graphoxide-out/needs_update"), "1").unwrap();
        let output = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("s1")),
            true,
            &[],
        ));
        assert!(!is_deny(&output));
        assert!(output.to_ascii_lowercase().contains("stale"));
    }

    #[test]
    fn test_glob_never_denies() {
        let (temp, _file) = strict_fixture(true, true);
        let payload = json!({
            "session_id":"s1",
            "tool_name":"Glob",
            "tool_input":{"pattern":"**/*.py","path":temp.path()},
        });
        let output = stdout(&run_hook(temp.path(), "read", &payload, true, &[]));
        assert!(!is_deny(&output));
    }

    #[test]
    fn test_search_never_denies() {
        let (temp, _file) = strict_fixture(true, true);
        let payload = json!({"session_id":"s1","tool_input":{"command":"grep -rn foo ."}});
        let output = stdout(&run_hook(temp.path(), "search", &payload, true, &[]));
        assert!(!is_deny(&output));
        assert!(output.contains("MANDATORY"));
    }

    #[test]
    fn test_no_session_id_never_denies() {
        let (temp, file) = strict_fixture(true, true);
        let output = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, None),
            true,
            &[],
        ));
        assert!(!is_deny(&output));
    }

    #[test]
    fn test_not_indexed_file_not_denied() {
        let (temp, file) = strict_fixture(false, true);
        let output = stdout(&run_hook(
            temp.path(),
            "read",
            &read_payload(&file, Some("s1")),
            true,
            &[],
        ));
        assert!(!is_deny(&output));
    }

    #[test]
    fn test_fail_open_on_malformed_stdin() {
        let (temp, _file) = strict_fixture(true, true);
        let output = run_raw(
            temp.path(),
            &["hook-guard", "read", "--strict"],
            b"{not json",
            &[],
        );
        assert!(output.status.success());
        assert!(stdout(&output).is_empty());
    }

    #[test]
    fn test_strict_enabled_env_precedence() {
        assert!(strict_enabled_with_override(false, Some("1")));
        assert!(!strict_enabled_with_override(true, Some("0")));
        assert!(strict_enabled_with_override(true, None));
        assert!(!strict_enabled_with_override(false, None));
    }

    #[test]
    fn test_install_hook_carries_strict_flag() {
        let executable = Path::new("/opt/graphoxide");
        let soft = claude_pretooluse_hooks(executable, false);
        let strict = claude_pretooluse_hooks(executable, true);
        let read_soft = soft
            .iter()
            .find(|(matcher, _)| *matcher == "Read|Glob")
            .unwrap();
        let read_strict = strict
            .iter()
            .find(|(matcher, _)| *matcher == "Read|Glob")
            .unwrap();
        assert!(read_soft.1.ends_with("hook-guard read"));
        assert!(read_strict.1.ends_with("hook-guard read --strict"));
        for hooks in [&soft, &strict] {
            assert!(hooks
                .iter()
                .find(|(matcher, _)| *matcher == "Bash|Grep")
                .unwrap()
                .1
                .ends_with("hook-guard search"));
        }
    }
}

mod read_hook {
    use super::*;

    fn run(tool_input: Value, temp: &TempDir, graph: bool) -> Output {
        if graph {
            make_graph(temp.path());
        }
        run_hook(
            temp.path(),
            "read",
            &json!({"tool_input":tool_input}),
            false,
            &[],
        )
    }

    #[test]
    fn test_matcher_targets_read_and_glob() {
        let hooks = claude_pretooluse_hooks(Path::new("graphoxide"), false);
        assert!(hooks.iter().any(|(matcher, _)| *matcher == "Read|Glob"));
    }

    #[test]
    fn test_command_has_no_shell_syntax() {
        let hooks = claude_pretooluse_hooks(Path::new("graphoxide"), false);
        let command = &hooks
            .iter()
            .find(|(matcher, _)| *matcher == "Read|Glob")
            .unwrap()
            .1;
        for token in ["$(", "case ", "[ -f", "&&", "||", ";;", "echo '"] {
            assert!(
                !command.contains(token),
                "{token:?} leaked into {command:?}"
            );
        }
        assert!(command.contains("graphoxide") && command.contains("hook-guard read"));
    }

    #[test]
    fn test_silent_without_graph() {
        let temp = TempDir::new().unwrap();
        assert!(
            stdout(&run(json!({"file_path":"src/app.py"}), &temp, false))
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn test_nudges_on_source_read_with_graph() {
        let temp = TempDir::new().unwrap();
        assert!(stdout(&run(json!({"file_path":"src/app.py"}), &temp, true))
            .contains("graphoxide query"));
    }

    #[test]
    fn test_nudge_payload_is_valid_pretooluse_json() {
        let temp = TempDir::new().unwrap();
        let payload: Value =
            serde_json::from_slice(&run(json!({"file_path":"pkg/mod.ts"}), &temp, true).stdout)
                .unwrap();
        assert_eq!(payload["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert!(payload["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("graphoxide query"));
    }

    #[test]
    fn test_silent_on_graphify_out_targets() {
        let temp = TempDir::new().unwrap();
        assert!(stdout(&run(
            json!({"file_path":"graphify-out/GRAPH_REPORT.md"}),
            &temp,
            true,
        ))
        .trim()
        .is_empty());
    }

    #[test]
    fn test_silent_on_non_source_files() {
        let temp = TempDir::new().unwrap();
        for path in ["uv.lock", "logo.png", "data.bin", ".gitignore"] {
            assert!(
                stdout(&run(json!({"file_path":path}), &temp, true))
                    .trim()
                    .is_empty(),
                "{path}"
            );
        }
    }

    #[test]
    fn test_glob_pattern_nudges() {
        let temp = TempDir::new().unwrap();
        assert!(
            stdout(&run(json!({"pattern":"**/*.py","path":"src"}), &temp, true,))
                .contains("graphoxide query")
        );
    }

    #[test]
    fn test_nudges_on_framework_source() {
        let temp = TempDir::new().unwrap();
        for path in [
            "src/components/Hero.astro",
            "src/App.vue",
            "src/Card.svelte",
        ] {
            assert!(
                stdout(&run(json!({"file_path":path}), &temp, true)).contains("graphoxide query"),
                "{path}"
            );
        }
    }

    #[test]
    fn test_astro_glob_nudges() {
        let temp = TempDir::new().unwrap();
        assert!(
            stdout(&run(json!({"pattern":"**/*.astro"}), &temp, true)).contains("graphoxide query")
        );
    }

    #[test]
    fn test_silent_on_json_config() {
        let temp = TempDir::new().unwrap();
        for path in ["package.json", "tsconfig.json", "data.geojson"] {
            assert!(
                stdout(&run(json!({"file_path":path}), &temp, true))
                    .trim()
                    .is_empty(),
                "{path}"
            );
        }
    }

    #[test]
    fn test_nudges_on_multi_dot_source() {
        let temp = TempDir::new().unwrap();
        for path in ["src/a.test.tsx", "lib/foo.min.js"] {
            assert!(
                stdout(&run(json!({"file_path":path}), &temp, true)).contains("graphoxide query"),
                "{path}"
            );
        }
    }

    #[test]
    fn test_windows_path_nudges() {
        let temp = TempDir::new().unwrap();
        assert!(stdout(&run(
            json!({"file_path":r"src\components\app.py"}),
            &temp,
            true,
        ))
        .contains("graphoxide query"));
    }

    #[test]
    fn test_silent_when_extension_is_on_a_directory_segment() {
        let temp = TempDir::new().unwrap();
        assert!(stdout(&run(json!({"file_path":"my.ts/file"}), &temp, true))
            .trim()
            .is_empty());
    }

    #[test]
    fn test_fails_open_on_malformed_stdin() {
        let temp = TempDir::new().unwrap();
        make_graph(temp.path());
        let output = run_raw(
            temp.path(),
            &["hook-guard", "read"],
            b"this is not json",
            &[],
        );
        assert!(output.status.success());
        assert!(stdout(&output).trim().is_empty());
    }

    #[test]
    fn test_never_blocks() {
        let temp = TempDir::new().unwrap();
        let output = run(json!({"file_path":"src/app.py"}), &temp, true);
        let text = stdout(&output);
        assert!(output.status.success());
        assert!(!text.contains("permissionDecision"));
        assert!(!text.contains("deny"));
    }
}

mod search_hook {
    use super::*;

    fn run_command(command: &str, temp: &TempDir, graph: bool) -> Output {
        if graph {
            make_graph(temp.path());
        }
        run_hook(
            temp.path(),
            "search",
            &json!({"tool_input":{"command":command}}),
            false,
            &[],
        )
    }

    fn run_grep(tool_input: Value, temp: &TempDir, graph: bool) -> Output {
        if graph {
            make_graph(temp.path());
        }
        run_hook(
            temp.path(),
            "search",
            &json!({"tool_name":"Grep","tool_input":tool_input}),
            false,
            &[],
        )
    }

    #[test]
    fn test_matcher_targets_bash_and_grep() {
        let hooks = claude_pretooluse_hooks(Path::new("graphoxide"), false);
        assert!(hooks.iter().any(|(matcher, _)| *matcher == "Bash|Grep"));
    }

    #[test]
    fn test_hook_command_has_no_backslashes() {
        let hooks = claude_pretooluse_hooks(Path::new(r"C:\Users\me\graphoxide.EXE"), false);
        assert!(hooks.iter().all(|(_, command)| !command.contains('\\')));
    }

    #[test]
    fn test_command_has_no_shell_syntax() {
        let hooks = claude_pretooluse_hooks(Path::new("graphoxide"), false);
        let command = &hooks
            .iter()
            .find(|(matcher, _)| *matcher == "Bash|Grep")
            .unwrap()
            .1;
        for token in ["$(", "case ", "[ -f", "&&", "||", ";;", "echo '"] {
            assert!(
                !command.contains(token),
                "{token:?} leaked into {command:?}"
            );
        }
        assert!(command.contains("graphoxide") && command.contains("hook-guard search"));
    }

    #[test]
    fn test_nudges_on_search_commands_with_graph() {
        let temp = TempDir::new().unwrap();
        for command in [
            "grep -rn foo .",
            "rg pattern src/",
            "ripgrep thing",
            "find . -name '*.py'",
            "fd bar",
            "ack needle",
            "ag needle",
        ] {
            assert!(stdout(&run_command(command, &temp, true)).contains("graphoxide query"));
        }
    }

    #[test]
    fn test_silent_without_graph() {
        let temp = TempDir::new().unwrap();
        assert!(stdout(&run_command("grep -rn foo .", &temp, false))
            .trim()
            .is_empty());
    }

    #[test]
    fn test_silent_on_non_search_commands() {
        let temp = TempDir::new().unwrap();
        for command in ["ls -la", "git status", "cat README.md", "python app.py"] {
            assert!(
                stdout(&run_command(command, &temp, true)).trim().is_empty(),
                "{command}"
            );
        }
    }

    #[test]
    fn test_nudge_payload_is_valid_pretooluse_json() {
        let temp = TempDir::new().unwrap();
        let payload: Value =
            serde_json::from_slice(&run_command("grep -rn foo .", &temp, true).stdout).unwrap();
        assert_eq!(payload["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert!(payload["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("graphoxide query"));
    }

    #[test]
    fn test_fails_open_on_malformed_stdin() {
        let temp = TempDir::new().unwrap();
        make_graph(temp.path());
        let output = run_raw(temp.path(), &["hook-guard", "search"], b"not json", &[]);
        assert!(output.status.success());
        assert!(stdout(&output).trim().is_empty());
    }

    #[test]
    fn test_never_blocks() {
        let temp = TempDir::new().unwrap();
        let output = run_command("grep -rn foo .", &temp, true);
        let text = stdout(&output);
        assert!(output.status.success());
        assert!(!text.contains("permissionDecision"));
        assert!(!text.contains("deny"));
    }

    #[test]
    fn test_honors_graphify_out_override() {
        let temp = TempDir::new().unwrap();
        let custom = temp.path().join("custom-out");
        fs::create_dir_all(&custom).unwrap();
        fs::write(custom.join("graph.json"), "{}").unwrap();
        let output = run_raw(
            temp.path(),
            &["hook-guard", "search"],
            &serde_json::to_vec(&json!({"tool_input":{"command":"grep -rn foo ."}})).unwrap(),
            &[("GRAPHIFY_OUT", custom.to_string_lossy().into_owned())],
        );
        assert!(stdout(&output).contains("graphoxide query"));
    }

    #[test]
    fn test_grep_tool_input_nudges_with_graph() {
        let temp = TempDir::new().unwrap();
        for tool_input in [
            json!({"pattern":"extract_corpus","path":"."}),
            json!({"pattern":"TODO"}),
            json!({"pattern":"def main","glob":"*.py"}),
            json!({"pattern":"foo","path":"src/","glob":"**/*.ts"}),
        ] {
            assert!(stdout(&run_grep(tool_input, &temp, true)).contains("graphoxide query"));
        }
    }

    #[test]
    fn test_grep_tool_input_silent_without_graph() {
        let temp = TempDir::new().unwrap();
        assert!(
            stdout(&run_grep(json!({"pattern":"foo","path":"."}), &temp, false,))
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn test_grep_tool_nudge_is_valid_pretooluse_json() {
        let temp = TempDir::new().unwrap();
        let payload: Value = serde_json::from_slice(
            &run_grep(json!({"pattern":"foo","path":"."}), &temp, true).stdout,
        )
        .unwrap();
        assert_eq!(payload["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert!(payload["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("graphoxide query"));
    }

    #[test]
    fn test_grep_tool_never_blocks() {
        let temp = TempDir::new().unwrap();
        let output = run_grep(json!({"pattern":"foo","path":"."}), &temp, true);
        let text = stdout(&output);
        assert!(output.status.success());
        assert!(!text.contains("permissionDecision"));
        assert!(!text.contains("deny"));
    }

    #[test]
    fn test_bash_non_search_with_stray_pattern_key_does_not_nudge() {
        let temp = TempDir::new().unwrap();
        make_graph(temp.path());
        let output = run_hook(
            temp.path(),
            "search",
            &json!({"tool_input":{"command":"ls -la","pattern":"x"}}),
            false,
            &[],
        );
        assert!(stdout(&output).trim().is_empty());
    }
}

mod gemini_hook {
    use super::*;

    fn run(temp: &TempDir, graph: bool, environment: &[(&str, String)]) -> Output {
        if graph {
            make_graph(temp.path());
        }
        run_raw(temp.path(), &["hook-guard", "gemini"], b"", environment)
    }

    #[test]
    fn test_matcher_and_command_shape() {
        let (matcher, command) = install_gemini_hook(Path::new("graphoxide"));
        assert_eq!(matcher, "read_file|list_directory");
        assert!(!command.contains("python -c"));
        assert!(command.contains("graphoxide") && command.contains("hook-guard gemini"));
    }

    #[test]
    fn test_allows_and_nudges_with_graph() {
        let temp = TempDir::new().unwrap();
        let payload: Value = serde_json::from_slice(&run(&temp, true, &[]).stdout).unwrap();
        assert_eq!(payload["decision"], "allow");
        assert!(payload["additionalContext"]
            .as_str()
            .unwrap()
            .contains("graphoxide query"));
    }

    #[test]
    fn test_allows_without_nudge_when_no_graph() {
        let temp = TempDir::new().unwrap();
        let payload: Value = serde_json::from_slice(&run(&temp, false, &[]).stdout).unwrap();
        assert_eq!(payload["decision"], "allow");
        assert!(payload.get("additionalContext").is_none());
    }

    #[test]
    fn test_never_blocks() {
        let temp = TempDir::new().unwrap();
        let output = run(&temp, true, &[]);
        let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(output.status.success());
        assert_eq!(payload["decision"], "allow");
    }

    #[test]
    fn test_honors_graphify_out_override() {
        let temp = TempDir::new().unwrap();
        let custom = temp.path().join("custom-out");
        fs::create_dir_all(&custom).unwrap();
        fs::write(custom.join("graph.json"), "{}").unwrap();
        let payload: Value = serde_json::from_slice(
            &run(
                &temp,
                false,
                &[("GRAPHIFY_OUT", custom.to_string_lossy().into_owned())],
            )
            .stdout,
        )
        .unwrap();
        assert!(payload["additionalContext"]
            .as_str()
            .unwrap()
            .contains("graphoxide query"));
    }
}
