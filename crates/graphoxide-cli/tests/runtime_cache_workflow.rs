use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn graphoxide(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphoxide"));
    command
        .current_dir(project)
        .env_remove("GRAPHOXIDE_FORCE")
        .env_remove("GRAPHIFY_FORCE")
        .env_remove("GRAPHOXIDE_OUT")
        .env_remove("GRAPHIFY_OUT");
    command
}

fn run_index(project: &Path, runtime_report: &Path, extra: &[&str]) -> Output {
    let mut command = graphoxide(project);
    command
        .args(["index", ".", "--no-cluster", "--code-only", "--json"])
        .arg("--runtime-report")
        .arg(runtime_report)
        .args(extra);
    command.output().expect("run graphoxide index")
}

fn run_update(project: &Path, runtime_report: &Path, extra: &[&str]) -> Output {
    let mut command = graphoxide(project);
    command
        .args(["update", ".", "--no-cluster", "--json"])
        .arg("--runtime-report")
        .arg(runtime_report)
        .args(extra);
    command.output().expect("run graphoxide update")
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", output_text(output));
    serde_json::from_slice::<Value>(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "index stdout is not one JSON value: {error}\n{}",
            output_text(output)
        )
    });
}

fn runtime_report(report: &Path) -> Value {
    serde_json::from_slice::<Value>(&fs::read(report).expect("runtime report"))
        .expect("runtime report JSON")
}

fn runtime_cache(report: &Path) -> Value {
    runtime_report(report)["cache"].clone()
}

fn managed(project: &Path, name: &str) -> PathBuf {
    project.join("graphoxide-out").join(name)
}

fn artifact_bytes(project: &Path) -> [Vec<u8>; 3] {
    [
        fs::read(managed(project, "graph.json")).expect("graph"),
        fs::read(managed(project, "manifest.json")).expect("manifest"),
        fs::read(managed(project, "coverage.json")).expect("coverage"),
    ]
}

fn graph_and_coverage_bytes(project: &Path) -> [Vec<u8>; 2] {
    [
        fs::read(managed(project, "graph.json")).expect("graph"),
        fs::read(managed(project, "coverage.json")).expect("coverage"),
    ]
}

fn runtime_artifacts(project: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, output: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, output);
            } else if path.extension().and_then(|value| value.to_str()) == Some("gxa") {
                output.push(path);
            }
        }
    }

    let mut output = Vec::new();
    visit(&managed(project, "cache/runtime-v1"), &mut output);
    output.sort();
    output
}

#[test]
fn clean_rebuild_uses_persistent_cache_without_reading_source_payload() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let source = "pub fn answer() -> u32 { 42 }\n";
    fs::write(project.join("lib.rs"), source).expect("source");

    let cold_report = fixture.path().join("cold-runtime.json");
    let cold = run_index(&project, &cold_report, &[]);
    assert_success(&cold);
    let cold_sidecar = runtime_report(&cold_report);
    let cold_cache = cold_sidecar["cache"].clone();
    let cold_stdout: Value = serde_json::from_slice(&cold.stdout).expect("cold stdout JSON");
    assert_eq!(cold_stdout["schema_version"], 1);
    assert_eq!(cold_sidecar["schema_version"], 2);
    assert_eq!(cold_sidecar["build"]["schema_version"], 1);
    assert_eq!(cold_sidecar["io"]["sources_selected"], 1);
    assert_eq!(
        cold_sidecar["io"]["source_bytes_selected"],
        source.len() as u64
    );
    assert_eq!(cold_sidecar["io"]["sources_read"], 1);
    assert_eq!(cold_sidecar["io"]["sources_delivered"], 1);
    assert_eq!(cold_sidecar["io"]["source_bytes_avoided"], 0);
    assert_eq!(cold_sidecar["work"]["parses"], 1);
    assert!(cold_sidecar["process"]["peak_rss_source"].is_string());
    assert_eq!(cold_cache["enabled"], true);
    assert_eq!(cold_cache["metadata_hits"], 0);
    assert_eq!(cold_cache["parses_avoided"], 0);
    assert!(cold_cache["misses"]
        .as_u64()
        .is_some_and(|value| value >= 1));
    assert!(cold_cache["stores"]
        .as_u64()
        .is_some_and(|value| value >= 1));
    let accepted = artifact_bytes(&project);

    fs::remove_file(managed(&project, "graph.json")).expect("remove graph only");
    let warm_report = fixture.path().join("warm-runtime.json");
    let warm = run_index(&project, &warm_report, &[]);
    assert_success(&warm);
    let warm_sidecar = runtime_report(&warm_report);
    let warm_cache = warm_sidecar["cache"].clone();
    assert_eq!(warm_cache["metadata_hits"], 1);
    assert_eq!(warm_cache["payload_reads_avoided"], 1);
    assert_eq!(warm_cache["parses_avoided"], 1);
    assert_eq!(warm_cache["runtime_hits"], 0);
    assert_eq!(warm_cache["legacy_hits"], 0);
    assert_eq!(warm_sidecar["io"]["sources_selected"], 1);
    assert_eq!(
        warm_sidecar["io"]["source_bytes_selected"],
        source.len() as u64
    );
    assert_eq!(warm_sidecar["io"]["sources_read"], 0);
    assert_eq!(warm_sidecar["io"]["sources_delivered"], 0);
    assert_eq!(warm_sidecar["io"]["source_bytes_read"], 0);
    assert_eq!(warm_sidecar["io"]["source_bytes_delivered"], 0);
    assert_eq!(
        warm_sidecar["io"]["source_bytes_avoided"],
        source.len() as u64
    );
    assert_eq!(warm_sidecar["io"]["read_failures"], 0);
    assert_eq!(warm_sidecar["work"]["parses"], 0);
    assert!(warm_cache["payload_bytes_read"]
        .as_u64()
        .is_some_and(|value| value > 0));
    assert!(
        warm_cache["artifact_bytes_read"].as_u64().unwrap()
            > warm_cache["payload_bytes_read"].as_u64().unwrap()
    );
    assert!(warm_cache["peak_in_flight_transfer_bytes"]
        .as_u64()
        .is_some_and(|value| value > 0));
    assert_eq!(artifact_bytes(&project), accepted);
}

#[test]
fn recreated_same_bytes_fall_back_to_a_content_hit_without_stale_identity_replay() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let source = project.join("lib.rs");
    let body = "pub fn answer() -> u32 { 42 }\n";
    fs::write(&source, body).expect("source");
    let cold_report = fixture.path().join("cold-runtime.json");
    assert_success(&run_index(&project, &cold_report, &[]));
    let accepted = artifact_bytes(&project);

    fs::remove_file(&source).expect("remove source generation");
    fs::write(&source, body).expect("recreate identical source");
    fs::remove_file(managed(&project, "graph.json")).expect("remove graph only");
    let warm_report = fixture.path().join("warm-runtime.json");
    let warm = run_index(&project, &warm_report, &[]);
    assert_success(&warm);
    let warm_cache = runtime_cache(&warm_report);
    assert_eq!(warm_cache["metadata_hits"], 0);
    assert_eq!(warm_cache["runtime_hits"], 1);
    assert_eq!(warm_cache["payload_reads_avoided"], 0);
    assert_eq!(warm_cache["parses_avoided"], 1);
    let rebuilt = artifact_bytes(&project);
    assert_eq!(rebuilt[0], accepted[0], "graph bytes");
    assert_ne!(
        rebuilt[1], accepted[1],
        "source identity evidence must refresh"
    );
    assert_eq!(rebuilt[2], accepted[2], "coverage bytes");
}

#[test]
fn force_bypasses_cache_reads_but_preserves_deterministic_artifacts() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project");
    fs::write(project.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").expect("source");
    let cold_report = fixture.path().join("cold-runtime.json");
    assert_success(&run_index(&project, &cold_report, &[]));
    let accepted = artifact_bytes(&project);

    let forced_report = fixture.path().join("forced-runtime.json");
    let forced = run_index(&project, &forced_report, &["--force"]);
    assert_success(&forced);
    let forced_cache = runtime_cache(&forced_report);
    assert_eq!(forced_cache["metadata_hits"], 0);
    assert_eq!(forced_cache["runtime_hits"], 0);
    assert_eq!(forced_cache["legacy_hits"], 0);
    assert_eq!(forced_cache["parses_avoided"], 0);
    assert!(forced_cache["bypasses"]
        .as_u64()
        .is_some_and(|value| value >= 1));
    assert_eq!(artifact_bytes(&project), accepted);
}

#[test]
fn corrupt_runtime_artifact_is_a_safe_miss_and_repairs_for_the_next_run() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project");
    fs::write(project.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").expect("source");
    let cold_report = fixture.path().join("cold-runtime.json");
    assert_success(&run_index(&project, &cold_report, &[]));
    let accepted = artifact_bytes(&project);

    let artifacts = runtime_artifacts(&project);
    assert_eq!(
        artifacts.len(),
        1,
        "single source should create one cache segment"
    );
    let mut corrupt = fs::read(&artifacts[0]).expect("cache segment");
    let last = corrupt.last_mut().expect("non-empty cache segment");
    *last ^= 0x5a;
    fs::write(&artifacts[0], corrupt).expect("corrupt cache payload");
    fs::remove_file(managed(&project, "graph.json")).expect("remove graph only");

    let repaired_report = fixture.path().join("repaired-runtime.json");
    let repaired = run_index(&project, &repaired_report, &[]);
    assert_success(&repaired);
    let repaired_cache = runtime_cache(&repaired_report);
    assert_eq!(repaired_cache["metadata_hits"], 0);
    assert!(repaired_cache["stale_or_corrupt"]
        .as_u64()
        .is_some_and(|value| value >= 1));
    assert_eq!(artifact_bytes(&project), accepted);

    fs::remove_file(managed(&project, "graph.json")).expect("remove graph again");
    let warm_report = fixture.path().join("warm-runtime.json");
    assert_success(&run_index(&project, &warm_report, &[]));
    let warm_cache = runtime_cache(&warm_report);
    assert_eq!(warm_cache["metadata_hits"], 1);
    assert_eq!(warm_cache["parses_avoided"], 1);
    assert_eq!(artifact_bytes(&project), accepted);
}

#[test]
fn same_size_rewrite_with_restored_mtime_cannot_replay_stale_facts() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let source = project.join("service.py");
    fs::write(&source, "def alpha():\n    pass\n").expect("source");
    let cold_report = fixture.path().join("cold-runtime.json");
    assert_success(&run_index(&project, &cold_report, &[]));
    let original_mtime = filetime::FileTime::from_last_modification_time(
        &fs::metadata(&source).expect("source metadata"),
    );

    fs::write(&source, "def bravo():\n    pass\n").expect("same-size rewrite");
    filetime::set_file_mtime(&source, original_mtime).expect("restore mtime");
    let changed_report = fixture.path().join("changed-runtime.json");
    let changed = run_index(&project, &changed_report, &[]);
    assert_success(&changed);
    let changed_cache = runtime_cache(&changed_report);
    assert_eq!(changed_cache["metadata_hits"], 0);
    assert!(changed_cache["misses"]
        .as_u64()
        .is_some_and(|value| value >= 1));
    let graph: Value = serde_json::from_slice(&fs::read(managed(&project, "graph.json")).unwrap())
        .expect("graph JSON");
    let graph_text = serde_json::to_string(&graph).unwrap();
    assert!(graph_text.contains("bravo"));
    assert!(!graph_text.contains("alpha"));
}

#[test]
fn parser_allowance_change_invalidates_manifest_and_cache_evidence() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project");
    fs::write(project.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").expect("source");
    let cold_report = fixture.path().join("cold-runtime.json");
    assert_success(&run_index(
        &project,
        &cold_report,
        &[
            "--memory-budget-bytes",
            "67108864",
            "--compute-workers",
            "1",
        ],
    ));

    let changed_report = fixture.path().join("changed-runtime.json");
    assert_success(&run_index(
        &project,
        &changed_report,
        &[
            "--memory-budget-bytes",
            "134217728",
            "--compute-workers",
            "1",
        ],
    ));
    let changed_cache = runtime_cache(&changed_report);
    assert_eq!(changed_cache["metadata_hits"], 0);
    assert!(changed_cache["misses"]
        .as_u64()
        .is_some_and(|value| value >= 1));

    fs::remove_file(managed(&project, "graph.json")).expect("remove graph only");
    let warm_report = fixture.path().join("warm-runtime.json");
    assert_success(&run_index(
        &project,
        &warm_report,
        &[
            "--memory-budget-bytes",
            "134217728",
            "--compute-workers",
            "1",
        ],
    ));
    let warm_cache = runtime_cache(&warm_report);
    assert_eq!(warm_cache["metadata_hits"], 1);
    assert_eq!(warm_cache["parses_avoided"], 1);
}

#[test]
fn moved_and_deleted_sources_cannot_replay_prior_cache_facts() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project");
    fs::write(project.join("old.py"), "def moved_symbol():\n    pass\n").expect("old source");
    fs::write(
        project.join("deleted.py"),
        "def deleted_symbol():\n    pass\n",
    )
    .expect("deleted source");
    let cold_report = fixture.path().join("cold-runtime.json");
    assert_success(&run_index(&project, &cold_report, &[]));

    fs::rename(project.join("old.py"), project.join("moved.py")).expect("move source");
    fs::remove_file(project.join("deleted.py")).expect("delete source");
    let changed_report = fixture.path().join("changed-runtime.json");
    assert_success(&run_index(&project, &changed_report, &[]));
    let changed_cache = runtime_cache(&changed_report);
    assert_eq!(changed_cache["metadata_hits"], 0);
    let accepted = artifact_bytes(&project);
    let graph_text = String::from_utf8_lossy(&accepted[0]);
    assert!(graph_text.contains("moved.py"));
    assert!(graph_text.contains("moved_symbol"));
    assert!(!graph_text.contains("old.py"));
    assert!(!graph_text.contains("deleted.py"));
    assert!(!graph_text.contains("deleted_symbol"));

    fs::remove_file(managed(&project, "graph.json")).expect("remove graph only");
    let warm_report = fixture.path().join("warm-runtime.json");
    assert_success(&run_index(&project, &warm_report, &[]));
    let warm_cache = runtime_cache(&warm_report);
    assert_eq!(warm_cache["metadata_hits"], 1);
    assert_eq!(warm_cache["payload_reads_avoided"], 1);
    assert_eq!(artifact_bytes(&project), accepted);
}

#[test]
fn worker_count_does_not_change_cold_or_warm_graph_and_coverage() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let one_worker = fixture.path().join("one-worker");
    let many_workers = fixture.path().join("many-workers");
    for project in [&one_worker, &many_workers] {
        fs::create_dir_all(project).expect("project");
        fs::write(project.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").expect("Rust source");
        fs::write(
            project.join("service.py"),
            "def service():\n    return 42\n",
        )
        .expect("Python source");
    }

    let one_report = fixture.path().join("one-runtime.json");
    assert_success(&run_index(
        &one_worker,
        &one_report,
        &["--compute-workers", "1"],
    ));
    let many_report = fixture.path().join("many-runtime.json");
    assert_success(&run_index(
        &many_workers,
        &many_report,
        &["--compute-workers", "4"],
    ));
    assert_eq!(
        graph_and_coverage_bytes(&one_worker),
        graph_and_coverage_bytes(&many_workers)
    );

    for (project, report, workers) in [
        (&one_worker, fixture.path().join("one-warm.json"), "1"),
        (&many_workers, fixture.path().join("many-warm.json"), "4"),
    ] {
        fs::remove_file(managed(project, "graph.json")).expect("remove graph only");
        assert_success(&run_index(
            project,
            &report,
            &["--compute-workers", workers],
        ));
        let cache = runtime_cache(&report);
        assert_eq!(cache["metadata_hits"], 2);
        assert_eq!(cache["payload_reads_avoided"], 2);
    }
    assert_eq!(
        graph_and_coverage_bytes(&one_worker),
        graph_and_coverage_bytes(&many_workers)
    );
}

#[test]
fn warm_update_reports_the_accepted_graph_shape_with_metadata_cache_hits() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project");
    fs::write(
        project.join("lib.rs"),
        "pub fn answer() -> u32 { helper() }\npub fn helper() -> u32 { 42 }\n",
    )
    .expect("source");

    let cold_report = fixture.path().join("cold-runtime.json");
    assert_success(&run_index(&project, &cold_report, &[]));
    let accepted_graph = fs::read(managed(&project, "graph.json")).expect("accepted graph");
    let accepted: Value = serde_json::from_slice(&accepted_graph).expect("accepted graph JSON");
    let accepted_nodes = accepted["nodes"].as_array().expect("graph nodes").len() as u64;
    let accepted_edges = accepted
        .get("links")
        .or_else(|| accepted.get("edges"))
        .and_then(Value::as_array)
        .expect("graph edges")
        .len() as u64;
    assert!(accepted_nodes > 0);

    let warm_report = fixture.path().join("warm-update-runtime.json");
    let warm = run_update(&project, &warm_report, &[]);
    assert_success(&warm);
    let stdout: Value = serde_json::from_slice(&warm.stdout).expect("update JSON report");
    let sidecar: Value =
        serde_json::from_slice(&fs::read(&warm_report).expect("warm update runtime report"))
            .expect("warm update runtime report JSON");

    assert_eq!(stdout["status"], "unchanged");
    assert_eq!(stdout["graph"]["nodes"], accepted_nodes);
    assert_eq!(stdout["graph"]["edges"], accepted_edges);
    assert_eq!(sidecar["build"]["graph"], stdout["graph"]);
    assert_eq!(sidecar["cache"]["metadata_hits"], 1);
    assert_eq!(sidecar["cache"]["payload_reads_avoided"], 1);
    assert_eq!(sidecar["cache"]["parses_avoided"], 1);
    assert_eq!(
        fs::read(managed(&project, "graph.json")).expect("unchanged graph"),
        accepted_graph
    );
}

#[cfg(unix)]
#[test]
fn unsafe_runtime_cache_path_fails_open_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project");
    fs::write(project.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").expect("source");
    let cold_report = fixture.path().join("cold-runtime.json");
    assert_success(&run_index(&project, &cold_report, &[]));
    let accepted = artifact_bytes(&project);

    let runtime_root = managed(&project, "cache/runtime-v1");
    fs::rename(&runtime_root, fixture.path().join("saved-runtime-v1"))
        .expect("move accepted cache aside");
    let outside = fixture.path().join("outside");
    fs::create_dir(&outside).expect("outside target");
    symlink(&outside, &runtime_root).expect("unsafe cache link");
    fs::remove_file(managed(&project, "graph.json")).expect("remove graph only");

    let disabled_report = fixture.path().join("disabled-runtime.json");
    let disabled = run_index(&project, &disabled_report, &[]);
    assert_success(&disabled);
    let disabled_cache = runtime_cache(&disabled_report);
    assert_eq!(disabled_cache["enabled"], false);
    assert_eq!(disabled_cache["metadata_hits"], 0);
    assert_eq!(disabled_cache["runtime_hits"], 0);
    assert!(disabled_cache["probe_failures"]
        .as_u64()
        .is_some_and(|value| value >= 1));
    assert_eq!(artifact_bytes(&project), accepted);
    assert_eq!(fs::read_dir(&outside).expect("outside target").count(), 0);
}
