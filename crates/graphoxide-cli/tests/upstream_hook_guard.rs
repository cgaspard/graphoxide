use graphoxide_cli::hook_guard::{evaluate, evaluate_with_graph_probe, GuardContext};
use serde_json::{json, Value};
use std::{
    fs, io,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

fn context(temp: &TempDir, graph: bool, output: &str) -> GuardContext {
    if graph {
        let directory = temp.path().join(output);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("graph.json"), "{}").unwrap();
    }
    GuardContext::new(temp.path(), output)
}

fn invoke(mode: &str, payload: Value, temp: &TempDir, graph: bool, output: &str) -> String {
    evaluate(
        mode,
        &serde_json::to_vec(&payload).unwrap(),
        &context(temp, graph, output),
    )
}

fn run_cli(args: &[&str], cwd: &Path, stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(args)
        .current_dir(cwd)
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn test_search_nudges() {
    for command in [
        "grep -rn foo .",
        "pgrep -f server",
        "egrep pattern file",
        "fgrep lit file",
        "ls -la | grep foo",
        "ripgrep thing",
        "rg pattern src/",
        "find . -name '*.py'",
        "fd bar",
        "ack needle",
        "ag needle",
    ] {
        let temp = TempDir::new().unwrap();
        let output = invoke(
            "search",
            json!({"tool_input":{"command":command}}),
            &temp,
            true,
            "graphoxide-out",
        );
        assert!(output.contains("graphoxide query"), "{command:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap()["hookSpecificOutput"]["hookEventName"],
            "PreToolUse"
        );
    }
}

#[test]
fn test_search_silent() {
    for command in [
        "",
        "ls -la",
        "git status",
        "cat README.md",
        "python app.py",
        "cd findings && ls",
        "manage db migrate",
        "echo hello",
    ] {
        let temp = TempDir::new().unwrap();
        assert!(invoke(
            "search",
            json!({"tool_input":{"command":command}}),
            &temp,
            true,
            "graphoxide-out",
        )
        .is_empty());
    }
}

#[test]
fn test_search_silent_without_graph() {
    let temp = TempDir::new().unwrap();
    assert!(invoke(
        "search",
        json!({"tool_input":{"command":"grep x"}}),
        &temp,
        false,
        "graphoxide-out",
    )
    .is_empty());
}

#[test]
fn test_search_missing_command_key() {
    let temp = TempDir::new().unwrap();
    assert!(invoke(
        "search",
        json!({"tool_input":{}}),
        &temp,
        true,
        "graphoxide-out",
    )
    .is_empty());
}

#[test]
fn test_search_non_string_command_is_silent() {
    let temp = TempDir::new().unwrap();
    assert!(invoke(
        "search",
        json!({"tool_input":{"command":123}}),
        &temp,
        true,
        "graphoxide-out",
    )
    .is_empty());
}

#[test]
fn test_search_top_level_command_without_tool_input() {
    let temp = TempDir::new().unwrap();
    assert!(invoke(
        "search",
        json!({"command":"grep x"}),
        &temp,
        true,
        "graphoxide-out",
    )
    .contains("graphoxide query"));
}

#[test]
fn test_search_non_dict_tool_input_is_silent() {
    let temp = TempDir::new().unwrap();
    assert!(invoke(
        "search",
        json!({"tool_input":"grep foo"}),
        &temp,
        true,
        "graphoxide-out",
    )
    .is_empty());
}

#[test]
fn test_read_nudges() {
    let inputs = [
        json!({"file_path":"src/app.py"}),
        json!({"file_path":"pkg/mod.ts"}),
        json!({"file_path":"src/App.vue"}),
        json!({"file_path":"src/Hero.astro"}),
        json!({"file_path":"src/Card.svelte"}),
        json!({"file_path":"SRC/APP.PY"}),
        json!({"file_path":"src/a.test.tsx"}),
        json!({"file_path":"lib/foo.min.js"}),
        json!({"file_path":r"src\components\app.py"}),
        json!({"pattern":"**/*.py","path":"src"}),
        json!({"pattern":"**/*.astro"}),
    ];
    for tool_input in inputs {
        let temp = TempDir::new().unwrap();
        let output = invoke(
            "read",
            json!({"tool_input":tool_input}),
            &temp,
            true,
            "graphoxide-out",
        );
        assert!(output.contains("graphoxide query"), "{tool_input}");
    }
}

#[test]
fn test_read_silent() {
    let inputs = [
        json!({"file_path":"package.json"}),
        json!({"file_path":"tsconfig.json"}),
        json!({"file_path":"data.geojson"}),
        json!({"file_path":"uv.lock"}),
        json!({"file_path":"logo.png"}),
        json!({"file_path":"data.bin"}),
        json!({"file_path":".gitignore"}),
        json!({"file_path":"Makefile"}),
        json!({"file_path":"my.ts/file"}),
        json!({"file_path":"graphoxide-out/GRAPH_REPORT.md"}),
        json!({"file_path":""}),
        json!({}),
    ];
    for tool_input in inputs {
        let temp = TempDir::new().unwrap();
        assert!(
            invoke(
                "read",
                json!({"tool_input":tool_input}),
                &temp,
                true,
                "graphoxide-out",
            )
            .is_empty(),
            "{tool_input}"
        );
    }
}

#[test]
fn test_read_silent_without_graph() {
    let temp = TempDir::new().unwrap();
    assert!(invoke(
        "read",
        json!({"tool_input":{"file_path":"src/app.py"}}),
        &temp,
        false,
        "graphoxide-out",
    )
    .is_empty());
}

#[test]
fn test_read_non_dict_tool_input_is_silent() {
    let temp = TempDir::new().unwrap();
    assert!(invoke(
        "read",
        json!({"tool_input":["src/app.py"]}),
        &temp,
        true,
        "graphoxide-out",
    )
    .is_empty());
}

#[test]
fn test_read_respects_custom_output_dir_name() {
    let temp = TempDir::new().unwrap();
    assert!(invoke(
        "read",
        json!({"tool_input":{"file_path":"build-out/report.py"}}),
        &temp,
        true,
        "build-out",
    )
    .is_empty());
}

#[test]
fn test_read_nudges_source_outside_custom_output_dir() {
    let temp = TempDir::new().unwrap();
    assert!(invoke(
        "read",
        json!({"tool_input":{"file_path":"src/app.py"}}),
        &temp,
        true,
        "build-out",
    )
    .contains("graphoxide query"));
}

#[test]
fn test_fail_open_on_bad_stdin() {
    for mode in ["search", "read"] {
        for raw in [
            b"not json at all".as_slice(),
            b"".as_slice(),
            b"[1,2,3]".as_slice(),
            b"\xff\xfe\0bad".as_slice(),
        ] {
            let temp = TempDir::new().unwrap();
            assert!(evaluate(mode, raw, &context(&temp, true, "graphoxide-out")).is_empty());
        }
    }
}

#[test]
fn test_search_out_path_error_is_swallowed() {
    let temp = TempDir::new().unwrap();
    let payload = serde_json::to_vec(&json!({"tool_input":{"command":"grep x"}})).unwrap();
    let output = evaluate_with_graph_probe(
        "search",
        &payload,
        &context(&temp, true, "graphoxide-out"),
        |_| Err(io::Error::other("boom")),
    );
    assert!(output.is_empty());
}

#[test]
fn test_gemini_allow_with_nudge() {
    let temp = TempDir::new().unwrap();
    let output = evaluate("gemini", b"", &context(&temp, true, "graphoxide-out"));
    let payload: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(payload["decision"], "allow");
    assert!(payload["additionalContext"]
        .as_str()
        .unwrap()
        .contains("graphoxide query"));
}

#[test]
fn test_gemini_allow_without_graph() {
    let temp = TempDir::new().unwrap();
    let output = evaluate("gemini", b"", &context(&temp, false, "graphoxide-out"));
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!({"decision":"allow"})
    );
}

#[test]
fn test_gemini_always_allows_even_when_check_throws() {
    let temp = TempDir::new().unwrap();
    let output = evaluate_with_graph_probe(
        "gemini",
        b"",
        &context(&temp, true, "graphoxide-out"),
        |_| Err(io::Error::other("boom")),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!({"decision":"allow"})
    );
}

#[test]
fn test_dispatch_missing_mode_exits_zero_silent() {
    let temp = TempDir::new().unwrap();
    let output = run_cli(&["hook-guard"], temp.path(), b"{}");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn test_dispatch_unknown_mode_exits_zero_silent() {
    let temp = TempDir::new().unwrap();
    let output = run_cli(&["hook-guard", "bogus"], temp.path(), b"{}");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn test_dispatch_always_exits_zero() {
    for (args, stdin) in [
        (
            &["hook-guard", "search"][..],
            br#"{"tool_input":{"command":"grep x"}}"#.as_slice(),
        ),
        (
            &["hook-guard", "read"][..],
            br#"{"tool_input":{"file_path":"a.py"}}"#.as_slice(),
        ),
        (&["hook-guard", "gemini"][..], b"".as_slice()),
    ] {
        let temp = TempDir::new().unwrap();
        context(&temp, true, "graphoxide-out");
        let output = run_cli(args, temp.path(), stdin);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_read_nudge_em_dash_survives_utf8() {
    let temp = TempDir::new().unwrap();
    context(&temp, true, "graphoxide-out");
    let output = run_cli(
        &["hook-guard", "read"],
        temp.path(),
        br#"{"tool_input":{"file_path":"src/app.py"}}"#,
    );
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let payload: Value = serde_json::from_str(&text).unwrap();
    assert!(payload["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .contains('—'));
}
