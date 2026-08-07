use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn graphoxide(current_directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command.current_dir(current_directory);
    command
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 stdout")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("UTF-8 stderr")
}

#[test]
fn coverage_audit_is_deterministic_and_strict_allows_unsupported_files() {
    let project = tempfile::tempdir().expect("temporary project");
    fs::write(project.path().join("main.rs"), "fn main() {}\n").expect("write Rust source");
    fs::write(
        project
            .path()
            .join("payload.definitely-not-a-graphoxide-format"),
        b"unsupported fixture\n",
    )
    .expect("write unsupported source");

    let run = || {
        graphoxide(project.path())
            .args(["audit", "coverage"])
            .arg(project.path())
            .arg("--json")
            .output()
            .expect("run coverage audit")
    };
    let first = run();
    assert!(first.status.success(), "{}", stderr(&first));
    let second = run();
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(first.stdout, second.stdout);

    let rendered = stdout(&first);
    assert!(rendered.starts_with("{\n  \"root\":"), "{rendered}");
    assert!(
        !rendered.contains(project.path().to_string_lossy().as_ref()),
        "coverage output leaked the absolute scan root: {rendered}"
    );
    let report: Value = serde_json::from_str(&rendered).expect("coverage JSON");
    assert_eq!(report["root"], ".");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["complete"], true);
    assert_eq!(report["summary"]["total_files"], 2);
    assert_eq!(report["summary"]["covered"], 1);
    assert_eq!(report["summary"]["unsupported"], 1);
    let files = report["files"].as_array().expect("coverage file outcomes");
    assert_eq!(files[0]["path"], "main.rs");
    assert_eq!(files[0]["status"], "covered");
    assert_eq!(
        files[1]["path"],
        "payload.definitely-not-a-graphoxide-format"
    );
    assert_eq!(files[1]["status"], "unsupported");

    let strict = graphoxide(project.path())
        .args(["audit", "coverage", "--strict"])
        .arg(project.path())
        .arg("--json")
        .output()
        .expect("run strict coverage audit");
    assert!(strict.status.success(), "{}", stderr(&strict));

    let human = graphoxide(project.path())
        .args(["audit", "coverage"])
        .arg(project.path())
        .output()
        .expect("run human coverage audit");
    assert!(human.status.success(), "{}", stderr(&human));
    let human = stdout(&human);
    assert!(human.contains("Coverage audit: \".\""), "{human}");
    assert!(
        human.contains("Status: complete; schema: coverage/v1"),
        "{human}"
    );
    assert!(human.contains("- covered\tpath=\"main.rs\""), "{human}");
    assert!(
        human.contains("- unsupported\tpath=\"payload.definitely-not-a-graphoxide-format\""),
        "{human}"
    );
    assert!(
        !human.contains(project.path().to_string_lossy().as_ref()),
        "coverage output leaked the absolute scan root: {human}"
    );
}

#[test]
fn dot_coverage_still_selects_the_legacy_graph_audit() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let coverage_directory = parent.path().join("coverage");
    fs::create_dir(&coverage_directory).expect("create literal coverage directory");
    fs::write(coverage_directory.join("main.rs"), "fn main() {}\n").expect("write Rust source");

    let output = graphoxide(parent.path())
        .args(["audit", "./coverage", "--json", "--force"])
        .output()
        .expect("run legacy graph audit");
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).expect("legacy audit JSON");
    assert!(report.get("input").is_some(), "{report:#}");
    assert!(report.get("build").is_some(), "{report:#}");
    assert!(report.get("files").is_none(), "{report:#}");
}

#[test]
fn coverage_audit_rejects_the_graph_only_force_flag() {
    let project = tempfile::tempdir().expect("temporary project");
    let output = graphoxide(project.path())
        .args(["audit", "coverage", "--force"])
        .output()
        .expect("run coverage audit");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--force applies to graph extraction audits"),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn human_coverage_output_escapes_control_characters_in_paths() {
    let project = tempfile::tempdir().expect("temporary project");
    fs::write(
        project
            .path()
            .join("line\nbreak\tname.definitely-unsupported"),
        b"unsupported fixture\n",
    )
    .expect("write control-character path");

    let output = graphoxide(project.path())
        .args(["audit", "coverage"])
        .arg(project.path())
        .output()
        .expect("run human coverage audit");
    assert!(output.status.success(), "{}", stderr(&output));
    let rendered = stdout(&output);
    assert!(!rendered.contains("line\nbreak"), "{rendered}");
    assert!(
        rendered.contains(r#"path="line%0Abreak%09name.definitely-unsupported""#),
        "{rendered}"
    );
}

#[cfg(unix)]
#[test]
fn strict_coverage_prints_the_report_before_failing_for_an_unreadable_file() {
    use std::os::unix::fs::PermissionsExt;

    let project = tempfile::tempdir().expect("temporary project");
    let source = project.path().join("locked.rs");
    fs::write(&source, "fn locked() {}\n").expect("write Rust source");
    let mut locked_permissions = fs::metadata(&source)
        .expect("source metadata")
        .permissions();
    locked_permissions.set_mode(0o000);
    fs::set_permissions(&source, locked_permissions).expect("make source unreadable");
    if fs::File::open(&source).is_ok() {
        let mut restored = fs::metadata(&source)
            .expect("source metadata")
            .permissions();
        restored.set_mode(0o600);
        fs::set_permissions(&source, restored).expect("restore source permissions");
        return;
    }

    let output = graphoxide(project.path())
        .args(["audit", "coverage", "--strict"])
        .arg(project.path())
        .arg("--json")
        .output()
        .expect("run strict coverage audit");
    let mut restored = fs::metadata(&source)
        .expect("source metadata")
        .permissions();
    restored.set_mode(0o600);
    fs::set_permissions(&source, restored).expect("restore source permissions");

    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).expect("strict coverage JSON");
    assert_eq!(report["complete"], false);
    assert_eq!(report["summary"]["unreadable"], 1);
    assert_eq!(report["files"][0]["path"], "locked.rs");
    assert_eq!(report["files"][0]["status"], "unreadable");
    assert!(
        stderr(&output).contains("strict coverage audit failed with 1"),
        "{}",
        stderr(&output)
    );
}
