// One explicit integration-test binary avoids a linker invocation per CLI test
// source file. It retains unrelated CLI workflow coverage while the root tests
// cover only the direct, pointer-only knowledgebase workflow.
#[path = "archive_workflow.rs"]
mod archive_workflow;
#[path = "broken_pipe.rs"]
mod broken_pipe;
#[path = "coverage_audit.rs"]
mod coverage_audit;
#[path = "direct_source_regressions.rs"]
mod direct_source_regressions;
#[path = "document_packages_workflow.rs"]
mod document_packages_workflow;
#[path = "enrichment_workflow.rs"]
mod enrichment_workflow;
#[path = "index_workflow.rs"]
mod index_workflow;
#[path = "packaged_artifact_smoke.rs"]
mod packaged_artifact_smoke;
#[path = "pdf_workflow.rs"]
mod pdf_workflow;
#[path = "registry_cli.rs"]
mod registry_cli;
#[path = "runtime_cache_workflow.rs"]
mod runtime_cache_workflow;
#[path = "serve_http_real_socket.rs"]
mod serve_http_real_socket;
#[path = "upstream_antigravity_install.rs"]
mod upstream_antigravity_install;
#[path = "upstream_catalog_index.rs"]
mod upstream_catalog_index;
#[path = "upstream_claude_md.rs"]
mod upstream_claude_md;
#[path = "upstream_cli_export.rs"]
mod upstream_cli_export;
#[path = "upstream_codebuddy_agents.rs"]
mod upstream_codebuddy_agents;
#[path = "upstream_devin.rs"]
mod upstream_devin;
#[path = "upstream_extract_cli.rs"]
mod upstream_extract_cli;
#[path = "upstream_extract_code_only.rs"]
mod upstream_extract_code_only;
#[path = "upstream_google_workspace.rs"]
mod upstream_google_workspace;
#[path = "upstream_home_sandbox.rs"]
mod upstream_home_sandbox;
#[path = "upstream_hook_guard.rs"]
mod upstream_hook_guard;
#[path = "upstream_hooks.rs"]
mod upstream_hooks;
#[path = "upstream_host_hooks.rs"]
mod upstream_host_hooks;
#[path = "upstream_incomplete_build_guard.rs"]
mod upstream_incomplete_build_guard;
#[path = "upstream_incremental.rs"]
mod upstream_incremental;
#[path = "upstream_install.rs"]
mod upstream_install;
#[path = "upstream_install_references_upgrade.rs"]
mod upstream_install_references_upgrade;
#[path = "upstream_install_roundtrip.rs"]
mod upstream_install_roundtrip;
#[path = "upstream_install_strings.rs"]
mod upstream_install_strings;
#[path = "upstream_labeling.rs"]
mod upstream_labeling;
#[path = "upstream_multigraph_diagnostics.rs"]
mod upstream_multigraph_diagnostics;
#[path = "upstream_pipeline.rs"]
mod upstream_pipeline;
#[path = "upstream_reflect_cli.rs"]
mod upstream_reflect_cli;
#[path = "upstream_replace_or_append_section.rs"]
mod upstream_replace_or_append_section;
#[path = "upstream_settings_merge.rs"]
mod upstream_settings_merge;
#[path = "upstream_skill_version_warning.rs"]
mod upstream_skill_version_warning;
#[path = "upstream_transcribe.rs"]
mod upstream_transcribe;
#[path = "upstream_uninstall_scope.rs"]
mod upstream_uninstall_scope;
#[path = "upstream_watch.rs"]
mod upstream_watch;

use std::{fs, path::Path, process::Command};

fn graphoxide() -> Command {
    Command::new(env!("CARGO_BIN_EXE_graphoxide"))
}

fn write_authoring_input(root: &Path) {
    fs::create_dir_all(root.join("config")).expect("create config directory");
    fs::create_dir_all(root.join("providers")).expect("create provider directory");
    fs::write(
        root.join("providers/transport.json"),
        r#"{"version":1,"id":"fixture","protocol":"ollama-native","endpoint":"http://127.0.0.1:1","source_egress_consent":"fixture-consent","models":[{"id":"author","api_model":"fixture-author","label":"Fixture author","capabilities":["structured-output","text-generation"]},{"id":"reviewer","api_model":"fixture-reviewer","label":"Fixture reviewer","capabilities":["structured-output","text-generation"]}]}"#,
    )
    .expect("write provider profile");
    fs::write(
        root.join("config/authoring-input.json"),
        r#"{"provider_profile":"providers/transport.json","author_model":"author","reviewer_model":"reviewer","source_egress_consent":"fixture-consent"}"#,
    )
    .expect("write authoring input");
}

fn init_knowledgebase(root: &Path) {
    let init = graphoxide()
        .args([
            "wiki",
            "init",
            "--authoring-profile",
            "config/authoring-input.json",
        ])
        .current_dir(root)
        .output()
        .expect("run direct knowledgebase initialization");
    assert!(
        init.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&init.stderr)
    );
}

fn init_git(root: &Path) {
    let init = Command::new("git")
        .args(["init"])
        .current_dir(root)
        .output()
        .expect("initialize knowledgebase Git worktree");
    assert!(init.status.success(), "git init failed: {init:?}");
}

#[test]
fn direct_cli_help_exposes_only_the_supported_knowledgebase_commands() {
    let help = graphoxide()
        .args(["wiki", "--help"])
        .output()
        .expect("run knowledgebase command help");
    assert!(
        help.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&help.stderr)
    );
    let help = String::from_utf8(help.stdout).expect("UTF-8 help");
    for command in ["init", "source", "live"] {
        assert!(
            help.contains(&format!("\n  {command}  ")),
            "missing supported command {command:?}: {help}"
        );
    }
    for command in ["search", "publish", "evaluate", "schema"] {
        assert!(
            !help.contains(&format!("\n  {command}  ")),
            "retired command {command:?}: {help}"
        );
    }
}

#[cfg(unix)]
#[test]
fn direct_cli_parses_and_dispatches_with_a_one_mebibyte_process_stack() {
    for args in [["wiki", "--help"], ["formats", "--json"]] {
        // Exercise the small main-thread stack available on Windows even on
        // Unix CI. A fresh shell changes only its own limit; macOS rejects
        // changing RLIMIT_STACK in a child forked from a Rust test thread.
        let output = Command::new("/bin/sh")
            .args([
                "-c",
                "ulimit -s 1024 && exec \"$@\"",
                "graphoxide-small-stack",
            ])
            .arg(env!("CARGO_BIN_EXE_graphoxide"))
            .args(args)
            .output()
            .expect("run CLI with a small process stack");
        assert!(
            output.status.success(),
            "{args:?} failed with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        if args[0] == "formats" {
            serde_json::from_slice::<serde_json::Value>(&output.stdout)
                .expect("command dispatch produces valid format capabilities");
        } else {
            assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
        }
    }
}

#[test]
fn direct_cli_initializes_a_pointer_only_knowledgebase() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let root = fixture.path().join("knowledgebase");
    fs::create_dir_all(&root).expect("create knowledgebase root");
    init_git(&root);
    write_authoring_input(&root);

    init_knowledgebase(&root);

    let index: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join("sources/index.json")).expect("read source index"),
    )
    .expect("parse source index");
    assert_eq!(index["schema"], "graphoxide.source-index");
    assert_eq!(index["sources"], serde_json::json!([]));
    assert!(root.join("taxonomy/policy.json").is_file());
    assert!(root.join("config/authoring-profile.json").is_file());
}

#[test]
fn direct_cli_never_prints_a_knowledgebase_filesystem_path() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let root = fixture.path().join("knowledgebase");
    fs::create_dir_all(&root).expect("create knowledgebase root");
    init_git(&root);
    write_authoring_input(&root);

    let init = graphoxide()
        .args([
            "wiki",
            "init",
            "--authoring-profile",
            "config/authoring-input.json",
        ])
        .current_dir(&root)
        .output()
        .expect("run direct knowledgebase initialization");
    assert!(
        init.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let root = root.display().to_string();
    assert!(
        !String::from_utf8_lossy(&init.stdout).contains(&root),
        "initialization must not disclose the knowledgebase path"
    );

    let nested = fixture.path().join("knowledgebase/nested");
    fs::create_dir_all(&nested).expect("create nested directory");
    let status = graphoxide()
        .args(["wiki", "source", "status"])
        .current_dir(nested)
        .output()
        .expect("run nested direct knowledgebase command");
    assert!(!status.status.success(), "nested root must be rejected");
    assert!(
        !String::from_utf8_lossy(&status.stderr).contains(&root),
        "rejection must not disclose the knowledgebase path"
    );
}

#[test]
fn direct_cli_source_add_rolls_back_when_authoring_fails_without_retaining_raw_content() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let root = fixture.path().join("knowledgebase");
    let external = fixture.path().join("external.md");
    let raw_content = "source content must never be retained in the knowledgebase";
    fs::create_dir_all(&root).expect("create knowledgebase root");
    fs::write(&external, raw_content).expect("write external document");
    init_git(&root);
    write_authoring_input(&root);
    init_knowledgebase(&root);

    let add = graphoxide()
        .args(["wiki", "source", "add"])
        .arg(&external)
        .current_dir(&root)
        .output()
        .expect("run direct source add");

    assert!(
        !add.status.success(),
        "unavailable authoring must abort admission"
    );
    let index = fs::read_to_string(root.join("sources/index.json")).expect("read source index");
    assert!(!index.contains(raw_content));
    assert!(!index.contains(&external.display().to_string()));
    assert_eq!(
        graphoxide_cli::wiki_source::source_status(&root).expect("read source status"),
        Vec::new()
    );
}

#[test]
fn direct_cli_reports_and_retires_a_pointer_without_reading_or_storing_its_body() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let root = fixture.path().join("knowledgebase");
    let external = fixture.path().join("external.md");
    let raw_content = "transient source body only";
    fs::create_dir_all(&root).expect("create knowledgebase root");
    fs::write(&external, raw_content).expect("write external document");
    init_git(&root);
    write_authoring_input(&root);
    init_knowledgebase(&root);
    let source = graphoxide_cli::wiki_source::admit_sources(&root, [&external])
        .expect("admit pointer for lifecycle test")
        .sources()
        .first()
        .expect("one pointer")
        .clone();

    let status = graphoxide()
        .args(["wiki", "source", "status", "--json"])
        .current_dir(&root)
        .output()
        .expect("run direct source status");
    assert!(
        status.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_text = String::from_utf8(status.stdout).expect("UTF-8 status");
    assert!(status_text.contains(&source.source_id));
    assert!(!status_text.contains(raw_content));
    assert!(!status_text.contains(&external.display().to_string()));

    let retire = graphoxide()
        .args(["wiki", "source", "retire", &source.source_id])
        .current_dir(&root)
        .output()
        .expect("run direct source retire");
    assert!(
        retire.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&retire.stderr)
    );
    assert!(graphoxide_cli::wiki_source::source_status(&root)
        .expect("read source status")
        .is_empty());
}
