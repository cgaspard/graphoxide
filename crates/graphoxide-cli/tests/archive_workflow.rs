use flate2::{write::GzEncoder, Compression};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

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

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) -> Value {
    assert!(output.status.success(), "{}", output_text(output));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON object: {error}\n{}",
            output_text(output)
        )
    })
}

fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, value) in entries {
        writer.start_file(*name, options).expect("start ZIP member");
        writer.write_all(value).expect("write ZIP member");
    }
    writer.finish().expect("finish ZIP").into_inner()
}

fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    const BLOCK_BYTES: usize = 512;
    let mut archive = Vec::new();
    for (name, value) in entries {
        assert!(name.len() <= 100);
        let mut header = [0_u8; BLOCK_BYTES];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", value.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum = header.iter().map(|byte| u32::from(*byte)).sum::<u32>();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(value);
        archive.resize(archive.len().div_ceil(BLOCK_BYTES) * BLOCK_BYTES, 0);
    }
    archive.resize(archive.len() + 2 * BLOCK_BYTES, 0);
    archive
}

fn gzip_bytes(payload: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(payload).expect("write GZIP payload");
    encoder.finish().expect("finish GZIP stream")
}

fn managed(project: &Path, name: &str) -> PathBuf {
    project.join("graphoxide-out").join(name)
}

fn graph(project: &Path) -> Value {
    serde_json::from_slice(&fs::read(managed(project, "graph.json")).expect("graph bytes"))
        .expect("graph JSON")
}

fn write_nested_archive(project: &Path, source: &str) {
    let tar = tar_bytes(&[("design/architecture.dot", source.as_bytes())]);
    fs::write(
        project.join("architecture.zip"),
        zip_bytes(&[("nested/design.tar", &tar)]),
    )
    .expect("archive fixture");
}

fn artifact_bytes(project: &Path) -> [Vec<u8>; 3] {
    [
        fs::read(managed(project, "graph.json")).expect("graph"),
        fs::read(managed(project, "manifest.json")).expect("manifest"),
        fs::read(managed(project, "coverage.json")).expect("coverage"),
    ]
}

fn manifest_without_runtime_cache(bytes: &[u8]) -> Value {
    let mut manifest: Value = serde_json::from_slice(bytes).expect("manifest JSON");
    for entry in manifest
        .as_object_mut()
        .expect("manifest object")
        .values_mut()
    {
        entry
            .as_object_mut()
            .expect("manifest entry object")
            .remove("runtime_cache");
    }
    manifest
}

fn manifest_runtime_cache_count(bytes: &[u8]) -> usize {
    serde_json::from_slice::<Value>(bytes)
        .expect("manifest JSON")
        .as_object()
        .expect("manifest object")
        .values()
        .filter(|entry| entry.get("runtime_cache").is_some())
        .count()
}

#[test]
fn default_index_recurses_archives_and_warm_cache_is_worker_deterministic() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    write_nested_archive(
        &project,
        "digraph Platform { gateway -> database [label=queries]; }\n",
    );
    let first_report = fixture.path().join("first-runtime.json");
    let first = graphoxide(&project)
        .args([
            "index",
            ".",
            "--force",
            "--no-cluster",
            "--json",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
        ])
        .arg("--runtime-report")
        .arg(&first_report)
        .output()
        .expect("cold archive index");
    assert_success(&first);

    let indexed = graph(&project);
    let nodes = indexed["nodes"].as_array().expect("nodes");
    assert!(nodes.iter().any(|node| node["label"] == "gateway"));
    assert!(nodes.iter().any(|node| {
        node["source_file"] == "architecture.zip!/nested/design.tar!/design/architecture.dot"
    }));
    assert!(indexed["edges"]
        .as_array()
        .is_some_and(|edges| { edges.iter().any(|edge| edge["relation"] == "flows_to") }));
    assert!(!project.join("nested/design.tar").exists());
    assert!(!project.join("design/architecture.dot").exists());
    let accepted = artifact_bytes(&project);
    assert_eq!(
        manifest_runtime_cache_count(&accepted[1]),
        0,
        "forced extraction must reset runtime-cache authorization"
    );

    fs::remove_file(managed(&project, "graph.json")).expect("remove forced graph only");
    let warm_report = fixture.path().join("warm-runtime.json");
    let warm = graphoxide(&project)
        .args([
            "index",
            ".",
            "--no-cluster",
            "--json",
            "--io-workers",
            "1",
            "--compute-workers",
            "4",
        ])
        .arg("--runtime-report")
        .arg(&warm_report)
        .output()
        .expect("warm archive index");
    assert_success(&warm);
    let warm_artifacts = artifact_bytes(&project);
    assert_eq!(
        warm_artifacts[0], accepted[0],
        "warm graph must remain byte-identical"
    );
    assert_eq!(
        warm_artifacts[2], accepted[2],
        "warm coverage must remain byte-identical"
    );
    assert_eq!(
        manifest_without_runtime_cache(&warm_artifacts[1]),
        manifest_without_runtime_cache(&accepted[1]),
        "the successful warm repair may authorize a runtime-cache artifact but must not change other manifest evidence"
    );
    assert_eq!(
        manifest_runtime_cache_count(&warm_artifacts[1]),
        1,
        "successful warm repair must authorize the persisted archive extraction"
    );
    let report: Value = serde_json::from_slice(&fs::read(warm_report).expect("runtime report"))
        .expect("runtime report JSON");
    assert_eq!(report["cache"]["metadata_hits"], 0);
    assert_eq!(report["cache"]["payload_reads_avoided"], 0);
    assert_eq!(report["cache"]["parses_avoided"], 0);

    fs::remove_file(managed(&project, "graph.json")).expect("remove repaired graph only");
    let second_warm_report = fixture.path().join("second-warm-runtime.json");
    let second_warm = graphoxide(&project)
        .args([
            "index",
            ".",
            "--no-cluster",
            "--json",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
        ])
        .arg("--runtime-report")
        .arg(&second_warm_report)
        .output()
        .expect("second warm archive index");
    assert_success(&second_warm);
    assert_eq!(
        artifact_bytes(&project),
        warm_artifacts,
        "once runtime-cache authorization is repaired, the next warm graph reconstruction must be byte-identical"
    );
    let second_report: Value =
        serde_json::from_slice(&fs::read(second_warm_report).expect("second warm runtime report"))
            .expect("second runtime report JSON");
    assert_eq!(second_report["cache"]["metadata_hits"], 1);
    assert_eq!(second_report["cache"]["payload_reads_avoided"], 1);
    assert_eq!(second_report["cache"]["parses_avoided"], 1);
}

#[test]
fn archive_update_replaces_nested_facts_and_migrates_prior_ast_schema() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    let clean = fixture.path().join("clean");
    fs::create_dir(&project).expect("project");
    fs::create_dir(&clean).expect("clean project");
    write_nested_archive(&project, "digraph Old { old_api -> old_db; }\n");

    let initial = graphoxide(&project)
        .args(["index", ".", "--force", "--no-cluster", "--json"])
        .output()
        .expect("initial archive index");
    assert_success(&initial);

    let manifest_path = managed(&project, "manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).expect("manifest"))
        .expect("manifest JSON");
    for entry in manifest
        .as_object_mut()
        .expect("manifest object")
        .values_mut()
    {
        entry["ast_version"] = graphoxide_extract::cache::AST_CACHE_VERSION
            .saturating_sub(1)
            .into();
    }
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("manifest bytes"),
    )
    .expect("seed prior schema");

    let current_source = "digraph Current { gateway -> cache -> database; }\n";
    write_nested_archive(&project, current_source);
    let updated = graphoxide(&project)
        .args(["update", ".", "--no-cluster", "--json"])
        .output()
        .expect("archive update");
    assert_success(&updated);
    let updated_graph = graph(&project);
    let rendered = serde_json::to_string(&updated_graph).expect("render graph");
    assert!(rendered.contains("gateway"));
    assert!(rendered.contains("database"));
    assert!(!rendered.contains("old_api"));
    assert!(!rendered.contains("old_db"));
    let migrated: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("migrated manifest"))
            .expect("migrated manifest JSON");
    assert!(migrated
        .as_object()
        .expect("manifest object")
        .values()
        .all(|entry| {
            entry["ast_version"].as_u64()
                == Some(u64::from(graphoxide_extract::cache::AST_CACHE_VERSION))
        }));

    write_nested_archive(&clean, current_source);
    let rebuilt = graphoxide(&clean)
        .args(["update", ".", "--force", "--no-cluster", "--json"])
        .output()
        .expect("clean archive rebuild");
    assert_success(&rebuilt);
    assert_eq!(
        fs::read(managed(&project, "graph.json")).expect("updated graph"),
        fs::read(managed(&clean, "graph.json")).expect("clean graph"),
        "incremental archive facts must equal a clean isolated rebuild"
    );
}

#[test]
fn default_clustered_index_recurses_a_single_gzip_stream() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir(&project).expect("project");
    let tar = tar_bytes(&[(
        "design/architecture.dot",
        b"digraph Runtime { gateway -> database [label=queries]; }\n",
    )]);
    fs::write(project.join("bundle.tar.gz"), gzip_bytes(&tar)).expect("GZIP fixture");

    let indexed = graphoxide(&project)
        .args(["index", ".", "--force", "--json"])
        .output()
        .expect("default clustered archive index");
    assert_success(&indexed);

    let accepted = graph(&project);
    let nodes = accepted["nodes"].as_array().expect("clustered graph nodes");
    assert!(nodes.iter().any(|node| node["label"] == "gateway"));
    assert!(nodes.iter().any(|node| {
        node["source_file"] == "bundle.tar.gz!/bundle.tar!/design/architecture.dot"
    }));
    assert!(accepted["links"]
        .as_array()
        .is_some_and(|links| { links.iter().any(|edge| edge["relation"] == "flows_to") }));
    assert!(!project.join("bundle.tar").exists());
    assert!(!project.join("design/architecture.dot").exists());
}
