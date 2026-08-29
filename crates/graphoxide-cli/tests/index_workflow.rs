use graphoxide_cli::{
    build_progress::{BUILD_PROGRESS_NONCE_ENV, BUILD_PROGRESS_PREFIX},
    index::{graph_file_sha256, COVERAGE_ARTIFACT, MAX_INDEX_MANIFEST_BYTES},
    watch::RebuildLockGuard,
};
use graphoxide_extract::coverage::{CoverageReport, CoverageStatus};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::{fs::File, io::Read as _, os::fd::FromRawFd as _};

fn graphoxide(current_directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command.current_dir(current_directory);
    command
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().expect("poll graphoxide child").is_some() {
            return child.wait_with_output().expect("collect graphoxide child");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("collect timed-out graphoxide child");
            panic!(
                "graphoxide child exceeded {timeout:?}\n{}",
                output_text(&output)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
const TERMINAL_RETAIN_LIMIT: usize = 64 * 1024;

#[cfg(unix)]
fn read_terminal(mut terminal: File) -> (Vec<u8>, bool) {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 4096];
    loop {
        match terminal.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let retained = TERMINAL_RETAIN_LIMIT.saturating_sub(output.len()).min(read);
                output.extend_from_slice(&buffer[..retained]);
                truncated |= retained != read;
            }
            // Linux PTY masters report EIO after the final slave descriptor
            // closes; other Unix implementations report a normal EOF.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("read terminal stderr: {error}"),
        }
    }
    (output, truncated)
}

#[cfg(unix)]
fn run_with_terminal_stderr(mut command: Command, timeout: Duration) -> (Output, Vec<u8>) {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: openpty initializes both descriptors on success. Null terminal
    // attributes and window size request the platform defaults.
    let result = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(
        result,
        0,
        "create terminal pair: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: successful openpty returned two newly owned descriptors.
    let terminal = unsafe { File::from_raw_fd(master_fd) };
    // SAFETY: this is the other newly owned descriptor from openpty.
    let child_stderr = unsafe { File::from_raw_fd(slave_fd) };

    command
        .stdout(Stdio::piped())
        .stderr(Stdio::from(child_stderr));
    let child = command
        .spawn()
        .expect("spawn graphoxide with terminal stderr");
    // Command no longer needs to retain any parent-side stdio configuration.
    drop(command);
    let terminal_reader = thread::spawn(move || read_terminal(terminal));
    let output = wait_with_timeout(child, timeout);
    let (terminal_output, truncated) = terminal_reader.join().expect("join terminal stderr reader");
    assert!(
        !truncated,
        "terminal progress exceeded {TERMINAL_RETAIN_LIMIT} bytes"
    );
    (output, terminal_output)
}

#[cfg(unix)]
struct PermissionRestore {
    path: PathBuf,
    mode: u32,
}

#[cfg(unix)]
impl Drop for PermissionRestore {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode));
    }
}

fn managed(output_root: &Path) -> PathBuf {
    output_root.join("graphoxide-out")
}

fn run_build(project: &Path, command: &str, output_root: &Path, extra: &[&str]) -> Output {
    let mut process = graphoxide(project);
    process
        .arg(command)
        .arg(project)
        .arg("--out")
        .arg(output_root)
        .arg("--json");
    process.args(extra);
    process.output().expect("run graphoxide build")
}

#[test]
fn opt_in_progress_is_bounded_source_safe_stderr_and_keeps_json_stdout() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("private-project-name");
    let output_root = fixture.path().join("output");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "pub fn answer() -> u8 { 42 }\n").expect("Rust");

    let output = graphoxide(&project)
        .arg("index")
        .arg(&project)
        .arg("--out")
        .arg(&output_root)
        .arg("--force")
        .arg("--no-cluster")
        .arg("--json")
        .arg("--progress=json")
        .output()
        .expect("index with progress stream");
    assert!(output.status.success(), "{}", output_text(&output));
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("one unchanged JSON report");
    assert_eq!(stdout["build"]["operation"], "index");

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    let events = stderr
        .lines()
        .filter_map(|line| line.strip_prefix(BUILD_PROGRESS_PREFIX))
        .map(|payload| {
            assert!(payload.len() <= 4096, "wire payload exceeded its byte cap");
            assert!(!payload.contains("private-project-name"));
            assert!(!payload.contains("output_path"));
            assert!(!payload.contains("warnings"));
            serde_json::from_str::<Value>(payload).expect("progress JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        events.first().and_then(|event| event["type"].as_str()),
        Some("started")
    );
    assert!(events.iter().any(|event| event["type"] == "phase"));
    assert!(events.iter().any(|event| {
        event["type"] == "phase"
            && event["phase"] == "extracting"
            && event["processed"].as_u64().is_some()
            && event["total"].as_u64().is_some()
    }));
    let phases = events
        .iter()
        .filter(|event| event["type"] == "phase")
        .filter_map(|event| event["phase"].as_str())
        .collect::<Vec<_>>();
    for expected in [
        "auditing",
        "scanning",
        "extracting",
        "building",
        "publishing",
    ] {
        assert!(phases.contains(&expected), "missing {expected}: {phases:?}");
    }
    let phase_rank = |phase: &str| match phase {
        "waiting" => 0,
        "auditing" => 1,
        "scanning" => 2,
        "extracting" => 3,
        "building" => 4,
        "clustering" => 5,
        "publishing" => 6,
        other => panic!("unknown phase {other}"),
    };
    assert!(phases
        .windows(2)
        .all(|pair| { pair[0] == pair[1] || phase_rank(pair[0]) < phase_rank(pair[1]) }));
    let extracting = events
        .iter()
        .filter(|event| event["type"] == "phase" && event["phase"] == "extracting")
        .collect::<Vec<_>>();
    assert!(extracting.windows(2).all(|pair| {
        pair[0]["total"] == pair[1]["total"]
            && pair[0]["processed"].as_u64() <= pair[1]["processed"].as_u64()
    }));
    let run_nonce = events
        .first()
        .and_then(|event| event["run_nonce"].as_str())
        .expect("generated progress nonce");
    assert_eq!(run_nonce.len(), 32);
    assert!(
        run_nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "nonce must be lowercase hexadecimal"
    );
    assert!(events.iter().all(|event| event["run_nonce"] == run_nonce));
    let completed = events
        .iter()
        .find(|event| event["type"] == "completed")
        .expect("successful completion envelope");
    assert_eq!(completed["operation"], "index");
    assert_eq!(completed["mode"], "full");
    assert_eq!(completed["status"], "rebuilt");
    assert_eq!(completed["files"]["indexed"], 1);
    assert!(completed["source_bytes"].as_u64().is_some());

    let default_output = graphoxide(&project)
        .arg("update")
        .arg(&project)
        .arg("--json")
        .env("GRAPHOXIDE_OUT", output_root.join("graphoxide-out"))
        .output()
        .expect("default non-TTY update");
    assert!(
        default_output.status.success(),
        "{}",
        output_text(&default_output)
    );
    assert!(!String::from_utf8_lossy(&default_output.stderr).contains(BUILD_PROGRESS_PREFIX));
    serde_json::from_slice::<Value>(&default_output.stdout).expect("default stdout remains JSON");
}

#[cfg(unix)]
#[test]
fn auto_progress_uses_terminal_stderr_and_preserves_json_stdout() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "pub fn answer() -> u8 { 42 }\n").expect("Rust");

    for (case, progress) in [("default", None), ("explicit", Some("--progress=auto"))] {
        let output_root = fixture.path().join(format!("{case}-output"));
        let mut command = graphoxide(&project);
        command
            .arg("index")
            .arg(&project)
            .arg("--out")
            .arg(&output_root)
            .arg("--force")
            .arg("--no-cluster")
            .arg("--json");
        if let Some(progress) = progress {
            command.arg(progress);
        }

        let (output, terminal_stderr) = run_with_terminal_stderr(command, Duration::from_secs(20));
        let terminal_stderr = String::from_utf8(terminal_stderr).expect("UTF-8 terminal stderr");
        assert!(
            output.status.success(),
            "{case} auto progress failed\n{}\nterminal stderr:\n{terminal_stderr}",
            output_text(&output)
        );
        let stdout: Value =
            serde_json::from_slice(&output.stdout).expect("stdout remains one JSON document");
        assert_eq!(stdout["build"]["operation"], "index", "{case}");
        assert!(
            terminal_stderr.contains("[graphoxide] index full build started"),
            "{case} did not enable human progress on a terminal:\n{terminal_stderr}"
        );
        assert!(
            terminal_stderr.contains("[graphoxide] Scanning inputs"),
            "{case} omitted a human phase update:\n{terminal_stderr}"
        );
        assert!(
            terminal_stderr.contains("[graphoxide] build complete in"),
            "{case} omitted the human terminal state:\n{terminal_stderr}"
        );
        assert!(
            !terminal_stderr.contains(BUILD_PROGRESS_PREFIX),
            "{case} auto mode leaked the JSON protocol onto human stderr:\n{terminal_stderr}"
        );
    }
}

#[test]
fn explicit_never_is_silent_and_invalid_json_nonce_fails_before_mutation() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output_root = fixture.path().join("output");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "pub fn answer() -> u8 { 42 }\n").expect("Rust");

    let never = graphoxide(&project)
        .arg("extract")
        .arg(&project)
        .arg("--out")
        .arg(&output_root)
        .arg("--force")
        .arg("--progress=never")
        .output()
        .expect("extract with progress disabled");
    assert!(never.status.success(), "{}", output_text(&never));
    assert!(!String::from_utf8_lossy(&never.stderr).contains(BUILD_PROGRESS_PREFIX));

    let rejected_root = fixture.path().join("rejected-output");
    let rejected = graphoxide(&project)
        .arg("index")
        .arg(&project)
        .arg("--out")
        .arg(&rejected_root)
        .arg("--force")
        .arg("--progress=json")
        .env(BUILD_PROGRESS_NONCE_ENV, "source-controlled")
        .output()
        .expect("reject malformed progress nonce");
    assert!(!rejected.status.success(), "{}", output_text(&rejected));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("32 lowercase hexadecimal"));
    assert!(
        !managed(&rejected_root).exists(),
        "nonce validation mutated output"
    );

    let watch_rejected = graphoxide(&project)
        .arg("watch")
        .arg(&project)
        .arg("--progress=json")
        .env(BUILD_PROGRESS_NONCE_ENV, "still-invalid")
        .output()
        .expect("watch rejects nonce before installing watcher");
    assert!(!watch_rejected.status.success());
    assert!(!String::from_utf8_lossy(&watch_rejected.stdout).contains("Watching"));
}

#[test]
fn progress_modes_leave_deterministic_graph_manifest_and_coverage_bytes_identical() {
    const NONCE: &str = "abcdefabcdefabcdefabcdefabcdefab";
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output_root = fixture.path().join("output");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(
        project.join("main.rs"),
        "pub fn answer() -> u8 { helper() }\npub fn helper() -> u8 { 42 }\n",
    )
    .expect("Rust");

    let run = |mode: &str| {
        let mut command = graphoxide(&project);
        command
            .arg("index")
            .arg(&project)
            .arg("--out")
            .arg(&output_root)
            .arg("--force")
            .arg("--no-cluster")
            .arg(format!("--progress={mode}"));
        if mode == "json" {
            command.env(BUILD_PROGRESS_NONCE_ENV, NONCE);
        }
        let output = command.output().expect("index fixture");
        assert!(output.status.success(), "{}", output_text(&output));
        ["graph.json", "manifest.json", COVERAGE_ARTIFACT]
            .map(|name| fs::read(managed(&output_root).join(name)).expect("artifact bytes"))
    };

    let auto = run("auto");
    let never = run("never");
    let json = run("json");
    assert_eq!(auto, never);
    assert_eq!(auto, json);
}

#[cfg(unix)]
#[test]
fn source_filename_cannot_authenticate_a_forged_progress_terminal() {
    use std::os::unix::fs::symlink;

    const RUN_NONCE: &str = "0123456789abcdef0123456789abcdef";
    const FORGED_NONCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output_root = fixture.path().join("output");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "pub fn answer() -> u8 { 42 }\n").expect("Rust");
    let forged = format!(
        "{{\"schema_version\":1,\"run_nonce\":\"{FORGED_NONCE}\",\"type\":\"failed\",\"operation\":\"extract\",\"mode\":\"full\"}}"
    );
    let malicious_name = format!("a\n{BUILD_PROGRESS_PREFIX}{forged}\nz");
    symlink(project.join("main.rs"), project.join(malicious_name))
        .expect("malicious symlink fixture");

    let output = graphoxide(&project)
        .arg("extract")
        .arg(&project)
        .arg("--out")
        .arg(&output_root)
        .arg("--force")
        .arg("--json")
        .arg("--progress=json")
        .env(BUILD_PROGRESS_NONCE_ENV, RUN_NONCE)
        .output()
        .expect("extract with authenticated progress stream");
    assert!(output.status.success(), "{}", output_text(&output));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr
            .lines()
            .any(|line| line == format!("{BUILD_PROGRESS_PREFIX}{forged}")),
        "source-derived forged line should remain observable in ordinary stderr: {stderr}"
    );
    let authenticated = stderr
        .lines()
        .filter_map(|line| line.strip_prefix(BUILD_PROGRESS_PREFIX))
        .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .filter(|event| event["run_nonce"] == RUN_NONCE)
        .collect::<Vec<_>>();
    assert_eq!(
        authenticated.first().map(|event| &event["type"]),
        Some(&Value::from("started"))
    );
    assert!(authenticated
        .iter()
        .any(|event| event["type"] == "completed"));
    assert!(!authenticated.iter().any(|event| event["type"] == "failed"));
}

#[test]
fn legacy_progress_binds_adaptive_start_to_actual_full_then_incremental_mode() {
    const FIRST_NONCE: &str = "11111111111111111111111111111111";
    const SECOND_NONCE: &str = "22222222222222222222222222222222";
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output = fixture.path().join("output");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "pub fn first() {}\n").expect("initial Rust");

    let run = |run_nonce: &str| {
        graphoxide(&project)
            .arg("update")
            .arg(&project)
            .arg("--legacy-executor")
            .arg("--force")
            .arg("--no-cluster")
            .arg("--json")
            .arg("--progress=json")
            .env("GRAPHOXIDE_OUT", &output)
            .env(BUILD_PROGRESS_NONCE_ENV, run_nonce)
            .output()
            .expect("legacy update with progress")
    };
    let events = |output: &Output, run_nonce: &str| {
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter_map(|line| line.strip_prefix(BUILD_PROGRESS_PREFIX))
            .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
            .filter(|event| event["run_nonce"] == run_nonce)
            .collect::<Vec<_>>()
    };

    let first = run(FIRST_NONCE);
    assert!(first.status.success(), "{}", output_text(&first));
    let first_events = events(&first, FIRST_NONCE);
    assert_eq!(
        first_events.first().map(|event| &event["mode"]),
        Some(&Value::from("adaptive"))
    );
    assert!(first_events
        .iter()
        .any(|event| event["type"] == "completed" && event["mode"] == "full"));

    fs::write(project.join("main.rs"), "pub fn second() {}\n").expect("changed Rust");
    let second = run(SECOND_NONCE);
    assert!(second.status.success(), "{}", output_text(&second));
    let second_events = events(&second, SECOND_NONCE);
    assert_eq!(
        second_events.first().map(|event| &event["mode"]),
        Some(&Value::from("adaptive"))
    );
    assert!(second_events
        .iter()
        .any(|event| event["type"] == "completed" && event["mode"] == "incremental"));
    for events in [&first_events, &second_events] {
        let phases = events
            .iter()
            .filter(|event| event["type"] == "phase")
            .filter_map(|event| event["phase"].as_str())
            .collect::<Vec<_>>();
        assert!(phases.contains(&"scanning"), "{phases:?}");
        assert!(phases.contains(&"extracting"), "{phases:?}");
        assert!(phases.contains(&"building"), "{phases:?}");
        assert!(phases.contains(&"publishing"), "{phases:?}");
        let extracting = events
            .iter()
            .filter(|event| event["type"] == "phase" && event["phase"] == "extracting")
            .collect::<Vec<_>>();
        assert_eq!(
            extracting.first().map(|event| &event["processed"]),
            Some(&Value::from(0))
        );
        assert_eq!(
            extracting.last().map(|event| &event["processed"]),
            extracting.last().map(|event| &event["total"])
        );
    }
}

#[test]
fn isolated_update_reports_full_without_baseline_then_incremental_with_baseline() {
    const FIRST_NONCE: &str = "33333333333333333333333333333333";
    const SECOND_NONCE: &str = "44444444444444444444444444444444";
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output = fixture.path().join("output");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "pub fn first() {}\n").expect("initial Rust");

    let run = |nonce: &str| {
        graphoxide(&project)
            .arg("update")
            .arg(&project)
            .arg("--no-cluster")
            .arg("--json")
            .arg("--progress=json")
            .env("GRAPHOXIDE_OUT", &output)
            .env(BUILD_PROGRESS_NONCE_ENV, nonce)
            .output()
            .expect("isolated update")
    };
    let completed_mode = |output: &Output, nonce: &str| {
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter_map(|line| line.strip_prefix(BUILD_PROGRESS_PREFIX))
            .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
            .find(|event| event["run_nonce"] == nonce && event["type"] == "completed")
            .and_then(|event| event["mode"].as_str().map(str::to_owned))
            .expect("completed mode")
    };

    let first = run(FIRST_NONCE);
    assert!(first.status.success(), "{}", output_text(&first));
    assert_eq!(completed_mode(&first, FIRST_NONCE), "full");

    fs::write(project.join("main.rs"), "pub fn second() {}\n").expect("changed Rust");
    let second = run(SECOND_NONCE);
    assert!(second.status.success(), "{}", output_text(&second));
    assert_eq!(completed_mode(&second, SECOND_NONCE), "incremental");
}

#[test]
fn progress_fails_instead_of_completing_when_post_commit_runtime_report_fails() {
    const NONCE: &str = "55555555555555555555555555555555";
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output = fixture.path().join("output");
    let report_destination = fixture.path().join("report-is-a-directory");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&report_destination).unwrap();
    fs::write(project.join("main.rs"), "pub fn indexed() {}\n").unwrap();

    let result = graphoxide(&project)
        .arg("update")
        .arg(&project)
        .arg("--no-cluster")
        .arg("--runtime-report")
        .arg(&report_destination)
        .arg("--progress=json")
        .env("GRAPHOXIDE_OUT", &output)
        .env(BUILD_PROGRESS_NONCE_ENV, NONCE)
        .output()
        .expect("update with failing report destination");
    assert!(!result.status.success(), "{}", output_text(&result));
    assert!(
        output.join("graph.json").is_file(),
        "graph commit precedes report"
    );
    let event_types = String::from_utf8_lossy(&result.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix(BUILD_PROGRESS_PREFIX))
        .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .filter(|event| event["run_nonce"] == NONCE)
        .filter_map(|event| event["type"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        event_types.contains(&"failed".to_owned()),
        "{event_types:?}"
    );
    assert!(
        !event_types.contains(&"completed".to_owned()),
        "{event_types:?}"
    );
}

fn coverage(output_root: &Path) -> CoverageReport {
    serde_json::from_slice(
        &fs::read(managed(output_root).join(COVERAGE_ARTIFACT)).expect("coverage artifact"),
    )
    .expect("coverage JSON")
}

#[test]
fn semantic_dot_is_identical_after_index_update_and_clean_rebuild() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let clean_project = fixture.path().join("clean-project");
    fs::create_dir_all(&project).expect("project directory");
    let diagram = project.join("architecture.dot");
    fs::write(
        &diagram,
        "digraph Platform { node [shape=box]; api -> database [label=queries]; }\n",
    )
    .expect("initial DOT");

    let indexed = graphoxide(&project)
        .arg("index")
        .arg(&project)
        .arg("--force")
        .arg("--no-cluster")
        .arg("--json")
        .output()
        .expect("index DOT project");
    assert!(indexed.status.success(), "{}", output_text(&indexed));
    let graph_path = project.join("graphoxide-out/graph.json");
    let initial_graph: Value =
        serde_json::from_slice(&fs::read(&graph_path).expect("indexed graph")).expect("graph JSON");
    let nodes = initial_graph["nodes"].as_array().expect("raw graph nodes");
    assert!(nodes.iter().any(|node| {
        node["diagram_format"] == "graphviz"
            && node["parse_status"] == "complete"
            && node["format_capability"] == "semantic_full"
    }));
    assert!(nodes.iter().any(|node| node["label"] == "api"));
    assert!(nodes.iter().any(|node| node["label"] == "database"));
    assert!(initial_graph["edges"].as_array().is_some_and(|edges| {
        edges
            .iter()
            .any(|edge| edge["relation"] == "flows_to" && edge["dot_occurrence_count"] == 1)
    }));

    let changed_source =
        "digraph Platform { node [shape=box]; api -> cache -> database [label=queries]; }\n";
    fs::write(&diagram, changed_source).expect("changed DOT");
    let updated = graphoxide(&project)
        .arg("update")
        .arg(&project)
        .arg("--no-cluster")
        .arg("--json")
        .output()
        .expect("incremental DOT update");
    assert!(updated.status.success(), "{}", output_text(&updated));

    fs::create_dir_all(&clean_project).expect("clean project directory");
    fs::write(clean_project.join("architecture.dot"), changed_source).expect("clean DOT source");
    let clean = graphoxide(&clean_project)
        .arg("update")
        .arg(&clean_project)
        .arg("--force")
        .arg("--no-cluster")
        .arg("--json")
        .output()
        .expect("clean DOT rebuild");
    assert!(clean.status.success(), "{}", output_text(&clean));
    assert_eq!(
        fs::read(&graph_path).expect("updated graph"),
        fs::read(clean_project.join("graphoxide-out/graph.json")).expect("clean graph"),
        "incremental DOT updates must match a clean rebuild byte-for-byte"
    );
}

#[test]
fn graph_audit_surfaces_incomplete_semantic_dot_parses() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project directory");
    let diagram = project.join("architecture.dot");
    fs::write(&diagram, "digraph Broken { good -> ; retained -> node; }\n").expect("malformed DOT");

    let report = graphoxide(&project)
        .arg("audit")
        .arg(&project)
        .arg("--json")
        .arg("--force")
        .output()
        .expect("audit malformed DOT");
    assert!(report.status.success(), "{}", output_text(&report));
    let report_json: Value = serde_json::from_slice(&report.stdout).expect("audit JSON");
    assert!(report_json["strict_violations"]
        .as_u64()
        .is_some_and(|value| value > 0));
    assert!(report_json["findings"].as_array().is_some_and(|findings| {
        findings.iter().any(|finding| {
            finding["severity"] == "error"
                && finding["code"] == "semantic_parse_incomplete"
                && finding["source_file"] == "architecture.dot"
        })
    }));

    let strict = graphoxide(&project)
        .arg("audit")
        .arg(&project)
        .arg("--json")
        .arg("--strict")
        .arg("--force")
        .output()
        .expect("strict audit malformed DOT");
    assert!(
        !strict.status.success(),
        "strict audit must reject recovered DOT"
    );
    let _: Value = serde_json::from_slice(&strict.stdout).expect("strict audit JSON");

    fs::write(&diagram, "digraph Valid { good -> retained; }\n").expect("valid DOT");
    let valid = graphoxide(&project)
        .arg("audit")
        .arg(&project)
        .arg("--json")
        .arg("--strict")
        .arg("--force")
        .output()
        .expect("strict audit valid DOT");
    assert!(valid.status.success(), "{}", output_text(&valid));
    let valid_json: Value = serde_json::from_slice(&valid.stdout).expect("valid audit JSON");
    assert_eq!(valid_json["strict_violations"], 0);
}

#[test]
fn clustered_index_accepts_bounded_repeated_dot_edges() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project directory");
    let mut source = String::from("digraph Repeated {\n");
    for _ in 0..10_000 {
        source.push_str("  source -> target;\n");
    }
    source.push_str("}\n");
    fs::write(project.join("repeated.dot"), source).expect("repeated DOT fixture");

    let indexed = graphoxide(&project)
        .arg("index")
        .arg(&project)
        .arg("--force")
        .arg("--json")
        .output()
        .expect("cluster and index bounded DOT project");
    assert!(indexed.status.success(), "{}", output_text(&indexed));

    let graph: Value = serde_json::from_slice(
        &fs::read(project.join("graphoxide-out/graph.json")).expect("clustered graph"),
    )
    .expect("clustered graph JSON");
    let nodes = graph["nodes"].as_array().expect("graph nodes");
    let root = nodes
        .iter()
        .find(|node| node["diagram_format"] == "graphviz" && node["type"] == "diagram")
        .expect("DOT root");
    assert_eq!(root["parse_status"], "partial");
    assert!(root["dot_diagnostics"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item["code"] == "dot_edge_metadata_limit")
    }));
    assert!(graph["links"]
        .as_array()
        .is_some_and(|edges| { edges.iter().any(|edge| edge["relation"] == "flows_to") }));
}

#[test]
fn index_and_extract_reject_nonexistent_source_roots_without_creating_paths() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    for command in ["index", "extract"] {
        let missing = fixture.path().join(format!("missing-{command}"));
        let output_root = fixture.path().join(format!("output-{command}"));
        let result = graphoxide(fixture.path())
            .arg(command)
            .arg(&missing)
            .arg("--out")
            .arg(&output_root)
            .arg("--json")
            .output()
            .expect("run build with missing source root");

        assert!(
            !result.status.success(),
            "{command} must reject missing root"
        );
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("must already exist and be a directory"),
            "{command}: {}",
            output_text(&result)
        );
        assert!(
            !missing.exists(),
            "{command} must not create its missing source root"
        );
        assert!(
            !output_root.exists(),
            "{command} must not create output state after source validation fails"
        );
    }
}

#[test]
fn index_is_graph_equivalent_to_extract_and_emits_associated_mixed_coverage() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let extract_root = fixture.path().join("extract-output");
    let index_root = fixture.path().join("index-output");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "pub fn answer() -> u8 { 42 }\n").expect("Rust");
    fs::write(project.join("README.md"), "# Project\n").expect("Markdown");
    fs::write(project.join("package.json"), "{\"name\":\"fixture\"}\n").expect("manifest");
    let sentinel = fixture.path().join("payload-executed");
    fs::write(
        project.join("payload.unknown-format"),
        format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
    )
    .expect("unknown executable-looking payload");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            project.join("payload.unknown-format"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("make unknown fixture executable");
    }
    fs::write(project.join(".env"), "TOKEN=never-read\n").expect("sensitive");

    let extract = run_build(&project, "extract", &extract_root, &["--force"]);
    assert!(extract.status.success(), "{}", output_text(&extract));
    assert!(
        !managed(&extract_root).join(COVERAGE_ARTIFACT).exists(),
        "legacy extract must not gain an index-only artifact"
    );

    let index = run_build(&project, "index", &index_root, &["--force"]);
    assert!(index.status.success(), "{}", output_text(&index));
    let stdout: Value = serde_json::from_slice(&index.stdout).expect("one index JSON document");
    assert_eq!(stdout["schema_version"], 1);
    assert_eq!(stdout["build"]["operation"], "index");
    assert_eq!(stdout["coverage"]["complete"], true);
    assert_eq!(
        stdout["coverage"]["path"],
        managed(&index_root)
            .join(COVERAGE_ARTIFACT)
            .to_string_lossy()
            .as_ref()
    );

    assert_eq!(
        fs::read(managed(&extract_root).join("graph.json")).unwrap(),
        fs::read(managed(&index_root).join("graph.json")).unwrap(),
        "index and extract must publish identical graph bytes"
    );
    assert_eq!(
        fs::read(managed(&extract_root).join("manifest.json")).unwrap(),
        fs::read(managed(&index_root).join("manifest.json")).unwrap(),
        "index and extract must publish identical manifest bytes"
    );

    let report = coverage(&index_root);
    assert!(report.complete);
    assert_eq!(report.root, ".");
    let outcome = |path: &str| {
        report
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("missing coverage outcome for {path}"))
    };
    assert_eq!(outcome("main.rs").status, CoverageStatus::Covered);
    assert_eq!(outcome("README.md").status, CoverageStatus::Covered);
    assert_eq!(
        outcome("package.json").status,
        CoverageStatus::InventoryOnly
    );
    assert_eq!(
        outcome("payload.unknown-format").status,
        CoverageStatus::Unsupported
    );
    assert_eq!(outcome(".env").status, CoverageStatus::ExcludedSensitive);
    let graph_path = managed(&index_root).join("graph.json");
    let association = report.graph.expect("graph association");
    assert_eq!(association.path, "graph.json");
    assert_eq!(
        association.sha256,
        graph_file_sha256(&graph_path).expect("exact graph digest")
    );
    assert_eq!(stdout["coverage"]["graph_sha256"], association.sha256);
    assert!(
        !sentinel.exists(),
        "indexing must never execute an unknown payload"
    );
    let serialized = fs::read_to_string(managed(&index_root).join(COVERAGE_ARTIFACT)).unwrap();
    assert!(
        !serialized.contains(project.to_string_lossy().as_ref()),
        "coverage artifact must not leak its absolute root"
    );
}

#[test]
fn code_only_raw_index_matches_extract_and_is_deterministic() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let extract_root = fixture.path().join("extract-output");
    let index_root = fixture.path().join("index-output");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "pub fn main_fact() {}\n").expect("Rust");
    fs::write(project.join("README.md"), "# Deliberately excluded\n").expect("Markdown");

    let extract = run_build(
        &project,
        "extract",
        &extract_root,
        &["--force", "--no-cluster", "--code-only"],
    );
    assert!(extract.status.success(), "{}", output_text(&extract));
    let first = run_build(
        &project,
        "index",
        &index_root,
        &["--force", "--no-cluster", "--code-only"],
    );
    assert!(first.status.success(), "{}", output_text(&first));
    assert_eq!(
        fs::read(managed(&extract_root).join("graph.json")).unwrap(),
        fs::read(managed(&index_root).join("graph.json")).unwrap()
    );
    let report = coverage(&index_root);
    let document = report
        .files
        .iter()
        .find(|file| file.path == "README.md")
        .expect("README coverage");
    assert_eq!(document.status, CoverageStatus::ExcludedPolicy);
    assert_eq!(document.reason.as_deref(), Some("code_only"));
    let graph_before = fs::read(managed(&index_root).join("graph.json")).unwrap();
    let coverage_before = fs::read(managed(&index_root).join(COVERAGE_ARTIFACT)).unwrap();

    let second = run_build(
        &project,
        "index",
        &index_root,
        &["--no-cluster", "--code-only"],
    );
    assert!(second.status.success(), "{}", output_text(&second));
    assert_eq!(
        fs::read(managed(&index_root).join("graph.json")).unwrap(),
        graph_before,
        "unchanged incremental index must retain deterministic graph bytes"
    );
    assert_eq!(
        fs::read(managed(&index_root).join(COVERAGE_ARTIFACT)).unwrap(),
        coverage_before,
        "unchanged incremental index must retain deterministic coverage bytes"
    );
}

#[test]
fn index_coverage_uses_effective_persisted_excludes_and_gitignore_policy() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output_root = fixture.path().join("output");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "fn main() {}\n").expect("main");
    fs::write(project.join("ignored.rs"), "fn ignored() {}\n").expect("ignored");
    fs::write(project.join("excluded.rs"), "fn excluded() {}\n").expect("excluded");
    fs::write(project.join(".gitignore"), "ignored.rs\n").expect("gitignore");

    let first = run_build(
        &project,
        "index",
        &output_root,
        &["--force", "--exclude", "excluded.rs"],
    );
    assert!(first.status.success(), "{}", output_text(&first));
    let first_report = coverage(&output_root);
    assert!(first_report
        .boundaries
        .iter()
        .any(|boundary| boundary.path == "ignored.rs" && boundary.reason == "ignore_rule"));
    assert!(first_report
        .boundaries
        .iter()
        .any(|boundary| boundary.path == "excluded.rs" && boundary.reason == "ignore_rule"));

    let second = run_build(&project, "index", &output_root, &["--no-gitignore"]);
    assert!(second.status.success(), "{}", output_text(&second));
    let second_report = coverage(&output_root);
    assert!(second_report
        .files
        .iter()
        .any(|file| file.path == "ignored.rs" && file.status == CoverageStatus::Covered));
    assert!(second_report
        .boundaries
        .iter()
        .any(|boundary| boundary.path == "excluded.rs" && boundary.reason == "ignore_rule"));
}

#[test]
fn runtime_report_cannot_overwrite_coverage_and_is_rejected_before_mutation() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output = project.join("graphoxide-out");
    fs::create_dir_all(&output).expect("managed output");
    fs::write(project.join("main.rs"), "fn main() {}\n").expect("source");
    let coverage_path = output.join(COVERAGE_ARTIFACT);
    fs::write(&coverage_path, b"previous accepted coverage\n").expect("coverage");

    let result = graphoxide(&project)
        .arg("index")
        .arg(".")
        .arg("--runtime-report")
        .arg("graphoxide-out/coverage.json")
        .arg("--json")
        .output()
        .expect("run colliding index");

    assert!(!result.status.success(), "collision must fail");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("collides with managed build artifact"),
        "{}",
        output_text(&result)
    );
    assert_eq!(
        fs::read(&coverage_path).unwrap(),
        b"previous accepted coverage\n"
    );
    assert!(!output.join("graph.json").exists());
    assert!(!output.join("manifest.json").exists());
    assert!(
        !output.join(".rebuild.lock").exists(),
        "collision validation must run before lock creation"
    );
}

#[cfg(unix)]
#[test]
fn dangling_runtime_report_symlink_cannot_capture_first_coverage_publish() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output = project.join("graphoxide-out");
    fs::create_dir_all(&output).expect("managed output");
    fs::write(project.join("main.rs"), "fn main() {}\n").expect("source");
    let runtime_report = output.join("runtime.json");
    symlink(COVERAGE_ARTIFACT, &runtime_report).expect("dangling runtime-report symlink");
    assert!(!output.join(COVERAGE_ARTIFACT).exists());

    let result = graphoxide(&project)
        .arg("index")
        .arg(".")
        .arg("--runtime-report")
        .arg("graphoxide-out/runtime.json")
        .arg("--json")
        .output()
        .expect("run symlinked index");

    assert!(!result.status.success(), "symlink must fail");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("symlink or reparse point"),
        "{}",
        output_text(&result)
    );
    assert!(runtime_report
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
    for artifact in [
        "graph.json",
        "manifest.json",
        COVERAGE_ARTIFACT,
        ".rebuild.lock",
    ] {
        assert!(
            !output.join(artifact).exists(),
            "preflight must not create {artifact}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_index_build_config_is_rejected_before_read_or_publication() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output = project.join("graphoxide-out");
    let external = fixture.path().join("external-config.json");
    fs::create_dir_all(&output).expect("managed output");
    fs::write(project.join("main.rs"), "fn main() {}\n").expect("source");
    fs::write(&external, b"external accepted config\n").expect("external config");
    let config = output.join(".graphoxide_build.json");
    symlink(&external, &config).expect("config symlink");

    let result = graphoxide(&project)
        .args(["index", ".", "--force", "--json"])
        .output()
        .expect("run index with unsafe config");

    assert!(!result.status.success(), "unsafe config must fail");
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("refusing unsafe index build config destination"),
        "{}",
        output_text(&result)
    );
    assert_eq!(fs::read(&external).unwrap(), b"external accepted config\n");
    assert!(config.symlink_metadata().unwrap().file_type().is_symlink());
    for artifact in [
        "graph.json",
        "manifest.json",
        COVERAGE_ARTIFACT,
        ".rebuild.lock",
    ] {
        assert!(
            !output.join(artifact).exists(),
            "preflight must not create {artifact}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_prior_index_artifacts_are_rejected_before_read_or_publication() {
    use std::os::unix::fs::symlink;

    for artifact in ["graph.json", "manifest.json", COVERAGE_ARTIFACT] {
        let fixture = tempfile::tempdir().expect("temporary fixture");
        let project = fixture.path().join("project");
        let output = project.join("graphoxide-out");
        let external = fixture.path().join(format!("external-{artifact}"));
        fs::create_dir_all(&output).expect("managed output");
        fs::write(project.join("main.rs"), "fn main() {}\n").expect("source");
        fs::write(&external, b"external private bytes\n").expect("external artifact");
        let managed_artifact = output.join(artifact);
        symlink(&external, &managed_artifact).expect("managed artifact symlink");

        let result = graphoxide(&project)
            .args(["index", ".", "--force", "--json"])
            .output()
            .expect("run index with unsafe prior artifact");

        assert!(!result.status.success(), "{artifact} symlink must fail");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("unsafe prior index artifact"),
            "{artifact}: {}",
            output_text(&result)
        );
        assert_eq!(fs::read(&external).unwrap(), b"external private bytes\n");
        assert!(managed_artifact
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(
            !output.join(".rebuild.lock").exists(),
            "preflight must precede lock creation"
        );
    }
}

#[test]
fn oversized_prior_manifest_is_rejected_before_read_or_publication() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output = project.join("graphoxide-out");
    fs::create_dir_all(&output).expect("managed output");
    fs::write(project.join("main.rs"), "fn main() {}\n").expect("source");
    let manifest = output.join("manifest.json");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest)
        .expect("manifest")
        .set_len(MAX_INDEX_MANIFEST_BYTES + 1)
        .expect("oversized sparse manifest");

    let result = graphoxide(&project)
        .args(["index", ".", "--force", "--json"])
        .output()
        .expect("run index with oversized manifest");

    assert!(!result.status.success(), "oversized manifest must fail");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("oversized index manifest"),
        "{}",
        output_text(&result)
    );
    assert_eq!(
        fs::metadata(&manifest).unwrap().len(),
        MAX_INDEX_MANIFEST_BYTES + 1
    );
    for artifact in ["graph.json", COVERAGE_ARTIFACT, ".rebuild.lock"] {
        assert!(
            !output.join(artifact).exists(),
            "preflight must not create {artifact}"
        );
    }
}

#[test]
fn incremental_index_handles_add_change_delete_and_unknown_only_changes() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let index_root = fixture.path().join("index-output");
    let fresh_root = fixture.path().join("fresh-output");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("a.rs"), "pub fn a() -> u8 { 1 }\n").expect("a source");
    fs::write(project.join("b.rs"), "pub fn b() -> u8 { 2 }\n").expect("b source");
    fs::write(project.join("first.unknown"), "first\n").expect("unknown source");

    let initial = run_build(&project, "index", &index_root, &["--force", "--no-cluster"]);
    assert!(initial.status.success(), "{}", output_text(&initial));

    fs::write(project.join("a.rs"), "pub fn a() -> u8 { 42 }\n").expect("change a");
    fs::remove_file(project.join("b.rs")).expect("delete b");
    fs::write(project.join("c.rs"), "pub fn c() -> u8 { 3 }\n").expect("add c");
    let incremental = run_build(&project, "index", &index_root, &["--no-cluster"]);
    assert!(
        incremental.status.success(),
        "{}",
        output_text(&incremental)
    );

    let fresh = run_build(
        &project,
        "extract",
        &fresh_root,
        &["--force", "--no-cluster"],
    );
    assert!(fresh.status.success(), "{}", output_text(&fresh));
    assert_eq!(
        fs::read(managed(&index_root).join("graph.json")).unwrap(),
        fs::read(managed(&fresh_root).join("graph.json")).unwrap(),
        "incremental add/change/delete must equal a fresh graph"
    );
    let report = coverage(&index_root);
    assert!(report
        .files
        .iter()
        .any(|file| file.path == "a.rs" && file.status == CoverageStatus::Covered));
    assert!(report
        .files
        .iter()
        .any(|file| file.path == "c.rs" && file.status == CoverageStatus::Covered));
    assert!(!report.files.iter().any(|file| file.path == "b.rs"));
    assert!(report.files.iter().any(|file| {
        file.path == "first.unknown" && file.status == CoverageStatus::Unsupported
    }));

    let graph_before_unknown_change = fs::read(managed(&index_root).join("graph.json")).unwrap();
    let coverage_before_unknown_change =
        fs::read(managed(&index_root).join(COVERAGE_ARTIFACT)).unwrap();
    fs::write(project.join("second.unknown"), "second\n").expect("second unknown source");
    let unknown_only = run_build(&project, "index", &index_root, &["--no-cluster"]);
    assert!(
        unknown_only.status.success(),
        "{}",
        output_text(&unknown_only)
    );
    assert_eq!(
        fs::read(managed(&index_root).join("graph.json")).unwrap(),
        graph_before_unknown_change,
        "an unknown-only change must not perturb semantic graph bytes"
    );
    assert_ne!(
        fs::read(managed(&index_root).join(COVERAGE_ARTIFACT)).unwrap(),
        coverage_before_unknown_change,
        "an unknown-only change must still refresh coverage inventory"
    );
    assert!(coverage(&index_root).files.iter().any(|file| {
        file.path == "second.unknown" && file.status == CoverageStatus::Unsupported
    }));
}

#[test]
fn graph_and_coverage_are_identical_across_runtime_worker_counts() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let serial_root = fixture.path().join("serial-output");
    let parallel_root = fixture.path().join("parallel-output");
    fs::create_dir_all(project.join("src")).expect("project directory");
    for index in 0..12 {
        fs::write(
            project.join(format!("src/module_{index}.rs")),
            format!("pub fn value_{index}() -> usize {{ {index} }}\n"),
        )
        .expect("Rust fixture");
    }
    fs::write(project.join("README.md"), "# deterministic\n").expect("document fixture");
    fs::write(project.join("payload.unknown"), "opaque\n").expect("unknown fixture");

    let serial = run_build(
        &project,
        "index",
        &serial_root,
        &[
            "--force",
            "--io-backend",
            "threaded",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
        ],
    );
    assert!(serial.status.success(), "{}", output_text(&serial));
    let parallel = run_build(
        &project,
        "index",
        &parallel_root,
        &[
            "--force",
            "--io-backend",
            "threaded",
            "--io-workers",
            "2",
            "--compute-workers",
            "3",
        ],
    );
    assert!(parallel.status.success(), "{}", output_text(&parallel));

    for artifact in ["graph.json", "manifest.json", COVERAGE_ARTIFACT] {
        assert_eq!(
            fs::read(managed(&serial_root).join(artifact)).unwrap(),
            fs::read(managed(&parallel_root).join(artifact)).unwrap(),
            "{artifact} must not depend on runtime worker counts"
        );
    }
}

#[cfg(unix)]
#[test]
fn incomplete_index_requires_allow_partial_and_reports_it_truthfully() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output_root = fixture.path().join("output");
    let output = managed(&output_root);
    fs::create_dir_all(&project).expect("project directory");
    fs::create_dir_all(&output).expect("managed output");
    fs::write(project.join("main.rs"), "fn main() {}\n").expect("readable source");
    let locked = project.join("locked.rs");
    fs::write(&locked, "fn locked() {}\n").expect("locked source");
    let original_mode = fs::metadata(&locked)
        .expect("locked source metadata")
        .permissions()
        .mode();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
        .expect("make source unreadable");
    let _restore = PermissionRestore {
        path: locked.clone(),
        mode: original_mode,
    };
    if fs::File::open(&locked).is_ok() {
        return;
    }

    fs::write(output.join("graph.json"), b"previous graph\n").expect("seed graph");
    fs::write(output.join("manifest.json"), b"previous manifest\n").expect("seed manifest");
    fs::write(output.join(COVERAGE_ARTIFACT), b"previous coverage\n").expect("seed coverage");
    let before = ["graph.json", "manifest.json", COVERAGE_ARTIFACT]
        .map(|artifact| fs::read(output.join(artifact)).unwrap());

    let refused = run_build(
        &project,
        "index",
        &output_root,
        &["--force", "--no-cluster"],
    );
    assert!(!refused.status.success(), "{}", output_text(&refused));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("incomplete coverage"),
        "{}",
        output_text(&refused)
    );
    for (artifact, expected) in ["graph.json", "manifest.json", COVERAGE_ARTIFACT]
        .into_iter()
        .zip(before)
    {
        assert_eq!(
            fs::read(output.join(artifact)).unwrap(),
            expected,
            "refused incomplete index changed {artifact}"
        );
    }

    let allowed = run_build(
        &project,
        "index",
        &output_root,
        &["--force", "--no-cluster", "--allow-partial"],
    );
    assert!(allowed.status.success(), "{}", output_text(&allowed));
    let stdout: Value = serde_json::from_slice(&allowed.stdout)
        .unwrap_or_else(|error| panic!("index JSON: {error}\n{}", output_text(&allowed)));
    assert_eq!(stdout["coverage"]["complete"], false);
    let report = coverage(&output_root);
    assert!(!report.complete);
    assert!(report
        .files
        .iter()
        .any(|file| { file.path == "locked.rs" && file.status == CoverageStatus::Unreadable }));

    let human = graphoxide(&project)
        .arg("index")
        .arg(&project)
        .arg("--out")
        .arg(&output_root)
        .arg("--force")
        .arg("--no-cluster")
        .arg("--allow-partial")
        .output()
        .expect("run human partial index");
    assert!(human.status.success(), "{}", output_text(&human));
    assert!(
        String::from_utf8_lossy(&human.stdout)
            .contains("Wrote associated coverage (incomplete) to"),
        "{}",
        output_text(&human)
    );
}

#[test]
fn extraction_failure_requires_partial_and_reclassifies_coverage() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let allowed_output = fixture.path().join("allowed-output");
    let refused_output = fixture.path().join("refused-output");
    let large_name = "e\u{301}.zip";
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "fn main() {}\n").expect("normal source");
    fs::File::create(project.join(large_name))
        .expect("large source")
        .set_len(64 * 1024 * 1024 + 1)
        .expect("large source length");

    let allowed = run_build(
        &project,
        "index",
        &allowed_output,
        &["--force", "--no-cluster", "--allow-partial"],
    );
    assert!(allowed.status.success(), "{}", output_text(&allowed));
    let stdout: Value = serde_json::from_slice(&allowed.stdout)
        .unwrap_or_else(|error| panic!("index JSON: {error}\n{}", output_text(&allowed)));
    assert_eq!(stdout["coverage"]["complete"], false);
    let report = coverage(&allowed_output);
    assert!(!report.complete);
    let large = report
        .files
        .iter()
        .find(|file| file.path == large_name)
        .expect("large source coverage");
    assert_eq!(large.status.as_str(), "extraction_failed");
    assert_eq!(large.reason.as_deref(), Some("extraction_failed"));
    let serialized = fs::read_to_string(managed(&allowed_output).join(COVERAGE_ARTIFACT)).unwrap();
    assert!(
        !serialized.contains(project.to_string_lossy().as_ref()),
        "coverage artifact leaked its absolute root"
    );
    assert!(!serialized.contains("TooLarge"));
    assert!(!serialized.contains("67108865"));

    let refused = run_build(
        &project,
        "index",
        &refused_output,
        &["--force", "--no-cluster"],
    );
    assert!(!refused.status.success(), "{}", output_text(&refused));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("incomplete coverage"),
        "{}",
        output_text(&refused)
    );
    for artifact in ["graph.json", "manifest.json", COVERAGE_ARTIFACT] {
        assert!(
            !managed(&refused_output).join(artifact).exists(),
            "refused partial index published {artifact}"
        );
    }
}

#[cfg(unix)]
#[test]
fn explicit_never_is_silent_while_waiting_for_the_build_lock() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output_root = fixture.path().join("output");
    let output = managed(&output_root);
    fs::create_dir_all(&project).expect("project directory");
    fs::create_dir_all(&output).expect("managed output");
    fs::write(project.join("main.rs"), "fn main() {}\n").expect("source");
    let guard = RebuildLockGuard::acquire(&output, false)
        .expect("lock acquisition")
        .expect("uncontended lock");

    let mut child = graphoxide(&project)
        .arg("index")
        .arg(&project)
        .arg("--out")
        .arg(&output_root)
        .arg("--force")
        .arg("--json")
        .arg("--progress=never")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn waiting index");
    thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().expect("poll contended child").is_none(),
        "index unexpectedly completed while the rebuild lock was held"
    );
    assert!(!output.join("graph.json").exists());
    drop(guard);
    let result = wait_with_timeout(child, Duration::from_secs(5));

    assert!(result.status.success(), "{}", output_text(&result));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!stderr.contains("waiting for the rebuild lock at"));
    assert!(!stderr.contains(BUILD_PROGRESS_PREFIX));
    assert!(output.join("graph.json").is_file());
}

#[cfg(unix)]
#[test]
fn sigint_while_waiting_for_lock_preserves_all_seeded_artifacts() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output_root = fixture.path().join("output");
    let output = managed(&output_root);
    fs::create_dir_all(&project).expect("project directory");
    fs::create_dir_all(&output).expect("managed output");
    fs::write(project.join("main.rs"), "fn main() {}\n").expect("source");
    fs::write(output.join("graph.json"), b"previous graph\n").expect("seed graph");
    fs::write(output.join("manifest.json"), b"previous manifest\n").expect("seed manifest");
    fs::write(output.join(COVERAGE_ARTIFACT), b"previous coverage\n").expect("seed coverage");
    let before = ["graph.json", "manifest.json", COVERAGE_ARTIFACT]
        .map(|artifact| fs::read(output.join(artifact)).unwrap());
    let guard = RebuildLockGuard::acquire(&output, false)
        .expect("lock acquisition")
        .expect("uncontended lock");

    let mut child = graphoxide(&project)
        .arg("index")
        .arg(&project)
        .arg("--out")
        .arg(&output_root)
        .arg("--force")
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn waiting index");
    let child_stderr = child.stderr.take().expect("piped child stderr");
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let stderr_reader = thread::spawn(move || {
        let mut reader = BufReader::new(child_stderr);
        let mut all = Vec::new();
        let mut announced = false;
        loop {
            let mut line = Vec::new();
            let read = reader
                .read_until(b'\n', &mut line)
                .expect("read child stderr");
            if read == 0 {
                break;
            }
            if !announced && String::from_utf8_lossy(&line).contains("waiting for the rebuild lock")
            {
                let _ = ready_sender.send(());
                announced = true;
            }
            all.extend_from_slice(&line);
        }
        all
    });
    ready_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("index must announce lock wait after its Ctrl-C handler is installed");
    let signal = Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .output()
        .expect("send SIGINT");
    assert!(signal.status.success(), "failed to send SIGINT: {signal:?}");
    let mut result = wait_with_timeout(child, Duration::from_secs(5));
    result.stderr = stderr_reader.join().expect("join stderr reader");
    drop(guard);

    assert!(!result.status.success(), "SIGINT must cancel index");
    use std::os::unix::process::ExitStatusExt as _;
    assert_eq!(
        result.status.signal(),
        None,
        "the handler must convert SIGINT into cooperative cancellation: {}",
        output_text(&result)
    );
    assert_eq!(result.status.code(), Some(1), "{}", output_text(&result));
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("project build cancelled while waiting for the rebuild lock"),
        "{}",
        output_text(&result)
    );
    for (artifact, expected) in ["graph.json", "manifest.json", COVERAGE_ARTIFACT]
        .into_iter()
        .zip(before)
    {
        assert_eq!(
            fs::read(output.join(artifact)).unwrap(),
            expected,
            "{artifact}"
        );
    }
    let unexpected_temporaries = fs::read_dir(&output)
        .expect("managed output entries")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp") || name.contains(".coverage."))
        .collect::<Vec<_>>();
    assert!(
        unexpected_temporaries.is_empty(),
        "cancelled index left temporary files: {unexpected_temporaries:?}"
    );
}

#[test]
fn index_waits_for_the_managed_rebuild_lock() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output_root = fixture.path().join("output");
    let output = managed(&output_root);
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "fn main() {}\n").expect("source");
    let guard = RebuildLockGuard::acquire(&output, false)
        .expect("lock acquisition")
        .expect("uncontended lock");

    let mut child = graphoxide(&project)
        .arg("index")
        .arg(&project)
        .arg("--out")
        .arg(&output_root)
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn blocked index");
    thread::sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().expect("poll child").is_none(),
        "index must wait rather than race a cooperating publisher"
    );
    drop(guard);
    let result = wait_with_timeout(child, Duration::from_secs(20));
    assert!(result.status.success(), "{}", output_text(&result));
    let report = coverage(&output_root);
    assert_eq!(
        report.graph.expect("association").sha256,
        graph_file_sha256(&output.join("graph.json")).expect("graph digest")
    );
}

#[test]
fn extract_waits_for_the_same_lock_and_cannot_race_index_publication() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output_root = fixture.path().join("output");
    let output = managed(&output_root);
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "fn first() {}\n").expect("source");

    let index = run_build(&project, "index", &output_root, &["--force"]);
    assert!(index.status.success(), "{}", output_text(&index));
    let graph_before = fs::read(output.join("graph.json")).expect("accepted graph");
    let coverage_before = coverage(&output_root);
    fs::write(project.join("main.rs"), "fn second() {}\n").expect("changed source");

    let guard = RebuildLockGuard::acquire(&output, false)
        .expect("lock acquisition")
        .expect("uncontended lock");
    let mut child = graphoxide(&project)
        .arg("extract")
        .arg(&project)
        .arg("--out")
        .arg(&output_root)
        .arg("--force")
        .arg("--legacy-executor")
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn blocked extract");
    thread::sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().expect("poll child").is_none(),
        "extract must honor the same publication lock as index"
    );
    assert_eq!(
        fs::read(output.join("graph.json")).unwrap(),
        graph_before,
        "a waiting extract cannot mutate index artifacts"
    );

    drop(guard);
    let result = wait_with_timeout(child, Duration::from_secs(20));
    assert!(result.status.success(), "{}", output_text(&result));
    assert_ne!(fs::read(output.join("graph.json")).unwrap(), graph_before);
    assert_ne!(
        graph_file_sha256(&output.join("graph.json")).expect("changed graph digest"),
        coverage_before.graph.expect("prior association").sha256,
        "coverage from the prior index remains detectably stale after extract"
    );
}

#[test]
fn isolated_update_waits_for_the_same_lock_and_cannot_race_index_publication() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output = project.join("graphoxide-out");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("main.rs"), "fn first() {}\n").expect("source");

    let index = graphoxide(&project)
        .args(["index", ".", "--force", "--json"])
        .output()
        .expect("initial index");
    assert!(index.status.success(), "{}", output_text(&index));
    let graph_before = fs::read(output.join("graph.json")).expect("accepted graph");
    let coverage_before: CoverageReport = serde_json::from_slice(
        &fs::read(output.join(COVERAGE_ARTIFACT)).expect("accepted coverage"),
    )
    .expect("coverage JSON");
    fs::write(project.join("main.rs"), "fn second() {}\n").expect("changed source");

    let guard = RebuildLockGuard::acquire(&output, false)
        .expect("lock acquisition")
        .expect("uncontended lock");
    let mut child = graphoxide(&project)
        .args(["update", ".", "--force", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn blocked isolated update");
    thread::sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().expect("poll child").is_none(),
        "isolated update must honor the same publication lock as index"
    );
    assert_eq!(
        fs::read(output.join("graph.json")).unwrap(),
        graph_before,
        "a waiting update cannot mutate index artifacts"
    );

    drop(guard);
    let result = wait_with_timeout(child, Duration::from_secs(20));
    assert!(result.status.success(), "{}", output_text(&result));
    assert_ne!(fs::read(output.join("graph.json")).unwrap(), graph_before);
    assert_ne!(
        graph_file_sha256(&output.join("graph.json")).expect("changed graph digest"),
        coverage_before.graph.expect("prior association").sha256,
        "coverage from the prior index remains detectably stale after update"
    );
}

#[test]
fn cluster_only_waits_for_the_same_lock_before_read_modify_write() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let output = project.join("graphoxide-out");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(
        project.join("main.rs"),
        "fn first() {}\nfn second() { first(); }\n",
    )
    .expect("source");
    let index = graphoxide(&project)
        .args(["index", ".", "--force", "--json"])
        .output()
        .expect("initial index");
    assert!(index.status.success(), "{}", output_text(&index));
    let graph_before = fs::read(output.join("graph.json")).expect("accepted graph");

    let guard = RebuildLockGuard::acquire(&output, false)
        .expect("lock acquisition")
        .expect("uncontended lock");
    let mut child = graphoxide(&project)
        .args(["cluster-only", ".", "--no-label", "--no-viz"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn blocked cluster-only");
    thread::sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().expect("poll child").is_none(),
        "cluster-only must honor the managed graph publication lock"
    );
    assert_eq!(
        fs::read(output.join("graph.json")).unwrap(),
        graph_before,
        "a waiting graph mutator cannot replace the accepted graph"
    );

    drop(guard);
    let result = wait_with_timeout(child, Duration::from_secs(20));
    assert!(result.status.success(), "{}", output_text(&result));
}
