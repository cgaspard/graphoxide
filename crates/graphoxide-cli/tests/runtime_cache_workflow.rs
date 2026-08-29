use serde_json::Value;
use std::{
    fs,
    io::{Read as _, Write as _},
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

fn run_full_index(project: &Path, runtime_report: &Path, compute_workers: &str) -> Output {
    let mut command = graphoxide(project);
    command
        .args(["index", ".", "--no-cluster", "--json"])
        .arg("--runtime-report")
        .arg(runtime_report)
        .args(["--compute-workers", compute_workers]);
    command.output().expect("run full graphoxide index")
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

const STRUCTURED_SECRET_SENTINELS: &[&str] = &[
    "JSON_SECRET_SENTINEL_49",
    "TOML_SECRET_SENTINEL_49",
    "XML_SECRET_SENTINEL_49",
    "CSV_SECRET_SENTINEL_49",
    "INI_SECRET_SENTINEL_49",
    "ENV_SECRET_SENTINEL_49",
    "NAMED_JSON_SECRET_SENTINEL_49",
    "MCP_COMMAND_SECRET_SENTINEL_49",
    "MCP_ARGUMENT_SECRET_SENTINEL_49",
    "MCP_ENV_SECRET_SENTINEL_49",
    "ARCHIVE_SECRET_SENTINEL_49",
    "EXCLUDED_ENV_SECRET_SENTINEL_49",
];

const ROTATED_JSON_SECRET_SENTINEL: &str = "JSON_ROTATED_SECRET_SENTINEL_49";
const RETIRED_AST_SECRET_SENTINEL: &str = "RETIRED_AST_SECRET_SENTINEL_49";
const RETIRED_RUNTIME_SECRET_SENTINEL: &str = "RETIRED_RUNTIME_SECRET_SENTINEL_49";
const PRIOR_GRAPH_SECRET_SENTINEL: &str = "PRIOR_GRAPH_SECRET_SENTINEL_49";

fn write_json_secret(project: &Path, secret: &str) {
    fs::write(
        project.join("settings.json"),
        format!(r#"{{"password":"{secret}","mode":"visible-json"}}"#),
    )
    .expect("JSON fixture");
}

fn write_structured_secret_fixture(project: &Path) {
    fs::create_dir_all(project).expect("structured fixture root");
    write_json_secret(project, STRUCTURED_SECRET_SENTINELS[0]);
    fs::write(
        project.join("settings.toml"),
        format!(
            "api_key = \"{}\"\nmode = \"visible-toml\"\n",
            STRUCTURED_SECRET_SENTINELS[1]
        ),
    )
    .expect("TOML fixture");
    fs::write(
        project.join("settings.xml"),
        format!(
            "<settings><password>{}</password><mode>visible-xml</mode></settings>",
            STRUCTURED_SECRET_SENTINELS[2]
        ),
    )
    .expect("XML fixture");
    fs::write(
        project.join("accounts.csv"),
        format!(
            "name,password,mode\napi,{},visible-csv\n",
            STRUCTURED_SECRET_SENTINELS[3]
        ),
    )
    .expect("CSV fixture");
    fs::write(
        project.join("settings.ini"),
        format!(
            "[service]\npassword={}\nmode=visible-ini\n",
            STRUCTURED_SECRET_SENTINELS[4]
        ),
    )
    .expect("INI fixture");
    fs::write(
        project.join("app.env"),
        format!(
            "TOKEN={}\nMODE=visible-env\n",
            STRUCTURED_SECRET_SENTINELS[5]
        ),
    )
    .expect("extractable environment fixture");
    fs::write(
        project.join(".env"),
        format!(
            "TOKEN={}\nMODE=must-not-be-read\n",
            STRUCTURED_SECRET_SENTINELS[11]
        ),
    )
    .expect("excluded environment fixture");
    fs::write(
        project.join("tsconfig.json"),
        format!(
            r#"{{"extends":["Bearer {}","./visible-tsconfig-base.json"],"compilerOptions":{{"strict":true}}}}"#,
            STRUCTURED_SECRET_SENTINELS[6]
        ),
    )
    .expect("named JSON fixture");
    fs::write(
        project.join(".mcp.json"),
        format!(
            r#"{{"mcpServers":{{"private":{{"command":"sk_live_{}","args":["--token","{}"],"env":{{"TOKEN":"{}"}}}}}}}}"#,
            STRUCTURED_SECRET_SENTINELS[7],
            STRUCTURED_SECRET_SENTINELS[8],
            STRUCTURED_SECRET_SENTINELS[9]
        ),
    )
    .expect("MCP fixture");
    let archived_json = format!(
        r#"{{"password":"{}","mode":"visible-archive"}}"#,
        STRUCTURED_SECRET_SENTINELS[10]
    );
    fs::write(
        project.join("settings.zip"),
        zip_bytes(&[("nested/settings.json", archived_json.as_bytes())]),
    )
    .expect("archive fixture");
}

fn assert_structured_fixture_contains_secrets(project: &Path) {
    for (name, secrets) in [
        ("settings.json", &STRUCTURED_SECRET_SENTINELS[0..1]),
        ("settings.toml", &STRUCTURED_SECRET_SENTINELS[1..2]),
        ("settings.xml", &STRUCTURED_SECRET_SENTINELS[2..3]),
        ("accounts.csv", &STRUCTURED_SECRET_SENTINELS[3..4]),
        ("settings.ini", &STRUCTURED_SECRET_SENTINELS[4..5]),
        ("app.env", &STRUCTURED_SECRET_SENTINELS[5..6]),
        (".env", &STRUCTURED_SECRET_SENTINELS[11..12]),
        ("tsconfig.json", &STRUCTURED_SECRET_SENTINELS[6..7]),
        (".mcp.json", &STRUCTURED_SECRET_SENTINELS[7..10]),
    ] {
        let bytes = fs::read(project.join(name)).expect("structured source bytes");
        for secret in secrets {
            assert!(
                contains_bytes(&bytes, secret.as_bytes()),
                "fixture {name} did not contain planted sentinel {secret}"
            );
        }
    }

    let archive_bytes = fs::read(project.join("settings.zip")).expect("archive source bytes");
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(archive_bytes)).expect("open ZIP fixture");
    let mut member = Vec::new();
    archive
        .by_name("nested/settings.json")
        .expect("structured ZIP member")
        .read_to_end(&mut member)
        .expect("read structured ZIP member");
    assert!(
        contains_bytes(&member, STRUCTURED_SECRET_SENTINELS[10].as_bytes()),
        "archive member did not contain its planted sentinel"
    );
}

fn seed_prior_graph(project: &Path) -> Vec<u8> {
    fs::create_dir_all(managed(project, "")).expect("managed output for prior graph");
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "nodes": [{
            "id": "prior-secret-node",
            "label": PRIOR_GRAPH_SECRET_SENTINEL,
            "file_type": "configuration",
            "source_file": "retired/settings.json"
        }],
        "links": []
    }))
    .expect("prior graph bytes");
    fs::write(managed(project, "graph.json"), &bytes).expect("prior graph");
    assert!(contains_bytes(
        &fs::read(managed(project, "graph.json")).expect("read prior graph"),
        PRIOR_GRAPH_SECRET_SENTINEL.as_bytes()
    ));
    bytes
}

#[derive(Debug)]
struct RetiredCacheFixture {
    ast_artifact: PathBuf,
    runtime_catalog: PathBuf,
    runtime_artifact: PathBuf,
}

fn seed_retired_secret_cache(project: &Path) -> RetiredCacheFixture {
    let ast_directory = managed(project, "cache/ast/v29");
    fs::create_dir_all(&ast_directory).expect("retired AST directory");
    let ast_artifact = ast_directory.join(format!("{}.json", "a".repeat(64)));
    fs::write(&ast_artifact, RETIRED_AST_SECRET_SENTINEL).expect("retired AST artifact");
    assert_eq!(
        fs::read(&ast_artifact).expect("read retired AST artifact"),
        RETIRED_AST_SECRET_SENTINEL.as_bytes()
    );

    let runtime_shard = managed(project, "cache/runtime-v1/shards/00");
    fs::create_dir_all(&runtime_shard).expect("retired runtime shard");
    let runtime_catalog = runtime_shard.join("catalog.gxi");
    let runtime_artifact = runtime_shard.join("active-0.gxa");
    fs::write(&runtime_catalog, RETIRED_RUNTIME_SECRET_SENTINEL).expect("retired runtime catalog");
    fs::write(&runtime_artifact, RETIRED_RUNTIME_SECRET_SENTINEL)
        .expect("retired runtime artifact");
    assert_eq!(
        fs::read(&runtime_catalog).expect("read retired runtime catalog"),
        RETIRED_RUNTIME_SECRET_SENTINEL.as_bytes()
    );
    assert_eq!(
        fs::read(&runtime_artifact).expect("read retired runtime artifact"),
        RETIRED_RUNTIME_SECRET_SENTINEL.as_bytes()
    );

    RetiredCacheFixture {
        ast_artifact,
        runtime_catalog,
        runtime_artifact,
    }
}

fn assert_retired_cache_purged(fixture: &RetiredCacheFixture) {
    assert!(!fixture.ast_artifact.exists(), "{fixture:?}");
    assert!(!fixture.runtime_catalog.exists(), "{fixture:?}");
    assert!(!fixture.runtime_artifact.exists(), "{fixture:?}");
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn assert_bytes_exclude_secrets(label: &str, bytes: &[u8], secrets: &[&str]) {
    for secret in secrets {
        assert!(
            !contains_bytes(bytes, secret.as_bytes()),
            "{label} retained planted secret {secret}"
        );
    }
}

fn assert_output_excludes_secrets(output: &Output, runtime_report_path: &Path, secrets: &[&str]) {
    assert_bytes_exclude_secrets("CLI stdout", &output.stdout, secrets);
    assert_bytes_exclude_secrets("CLI stderr", &output.stderr, secrets);
    assert_bytes_exclude_secrets(
        "runtime report",
        &fs::read(runtime_report_path).expect("runtime report bytes"),
        secrets,
    );
}

fn assert_managed_tree_excludes_secrets(project: &Path, secrets: &[&str]) {
    fn visit(path: &Path, secrets: &[&str]) {
        let entries = fs::read_dir(path).unwrap_or_else(|error| {
            panic!("read managed output directory {}: {error}", path.display())
        });
        for entry in entries {
            let entry = entry.expect("managed output entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("managed output file type");
            if file_type.is_dir() {
                visit(&path, secrets);
            } else if file_type.is_file() {
                assert_bytes_exclude_secrets(
                    &format!("managed artifact {}", path.display()),
                    &fs::read(&path).expect("managed artifact bytes"),
                    secrets,
                );
            }
        }
    }

    visit(&managed(project, ""), secrets);
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
    visit(&managed(project, "cache/runtime-v2"), &mut output);
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
fn force_resets_cache_trust_then_normal_and_warm_runs_repair_deterministically() {
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
    let forced_artifacts = artifact_bytes(&project);
    assert_eq!(forced_artifacts[0], accepted[0], "graph bytes");
    assert_eq!(forced_artifacts[2], accepted[2], "coverage bytes");
    assert_ne!(
        forced_artifacts[1], accepted[1],
        "force deliberately removes runtime-cache authorization"
    );
    assert_eq!(
        manifest_without_runtime_cache(&forced_artifacts[1]),
        manifest_without_runtime_cache(&accepted[1]),
        "force may change only runtime-cache authorization"
    );
    let forced_manifest: Value =
        serde_json::from_slice(&forced_artifacts[1]).expect("forced manifest JSON");
    assert!(
        forced_manifest
            .as_object()
            .expect("forced manifest object")
            .values()
            .all(|entry| entry.get("runtime_cache").is_none()),
        "force cannot authorize any runtime artifact"
    );

    let repaired_report = fixture.path().join("repaired-runtime.json");
    assert_success(&run_index(&project, &repaired_report, &[]));
    let repaired_cache = runtime_cache(&repaired_report);
    assert_eq!(repaired_cache["metadata_hits"], 0);
    assert_eq!(repaired_cache["runtime_hits"], 0);
    assert_eq!(repaired_cache["parses_avoided"], 0);
    assert!(repaired_cache["stores"]
        .as_u64()
        .is_some_and(|value| value >= 1));
    let repaired_artifacts = artifact_bytes(&project);
    assert_eq!(repaired_artifacts[0], accepted[0], "repaired graph bytes");
    assert_eq!(
        repaired_artifacts[2], accepted[2],
        "repaired coverage bytes"
    );
    assert_eq!(
        manifest_without_runtime_cache(&repaired_artifacts[1]),
        manifest_without_runtime_cache(&forced_artifacts[1]),
        "repair may change only runtime-cache authorization"
    );

    let warm_report = fixture.path().join("warm-runtime.json");
    assert_success(&run_index(&project, &warm_report, &[]));
    let warm_cache = runtime_cache(&warm_report);
    assert_eq!(warm_cache["metadata_hits"], 1);
    assert_eq!(warm_cache["parses_avoided"], 1);
    assert_eq!(
        artifact_bytes(&project),
        repaired_artifacts,
        "the repaired authorization and graph must be byte-stable when warm"
    );
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
fn structured_source_content_survives_cold_warm_and_updated_artifacts_across_workers() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let one_worker = fixture.path().join("one-worker");
    let many_workers = fixture.path().join("many-workers");
    for project in [&one_worker, &many_workers] {
        write_structured_secret_fixture(project);
        assert_structured_fixture_contains_secrets(project);
    }

    let source_secrets = [
        &STRUCTURED_SECRET_SENTINELS[..7],
        &STRUCTURED_SECRET_SENTINELS[10..11],
    ]
    .concat();
    let mut withheld_secrets = STRUCTURED_SECRET_SENTINELS[7..10].to_vec();
    withheld_secrets.extend([
        STRUCTURED_SECRET_SENTINELS[11],
        RETIRED_AST_SECRET_SENTINEL,
        RETIRED_RUNTIME_SECRET_SENTINEL,
        PRIOR_GRAPH_SECRET_SENTINEL,
    ]);
    let mut output_secrets = source_secrets.clone();
    output_secrets.extend(withheld_secrets.iter().copied());
    output_secrets.push(ROTATED_JSON_SECRET_SENTINEL);
    let mut cold_withheld_secrets = withheld_secrets.clone();
    cold_withheld_secrets.push(ROTATED_JSON_SECRET_SENTINEL);

    for (project, workers, report_name) in [
        (&one_worker, "1", "one-cold.json"),
        (&many_workers, "4", "many-cold.json"),
    ] {
        seed_prior_graph(project);
        let retired = seed_retired_secret_cache(project);
        let report = fixture.path().join(report_name);
        let cold = run_full_index(project, &report, workers);
        assert_success(&cold);
        assert_retired_cache_purged(&retired);
        assert_output_excludes_secrets(&cold, &report, &output_secrets);
        assert_managed_tree_excludes_secrets(project, &cold_withheld_secrets);
        let cold_stdout: Value = serde_json::from_slice(&cold.stdout).expect("cold stdout JSON");
        assert_eq!(
            cold_stdout["build"]["files"]["sensitive"], 1,
            "literal .env must be reported as sensitive-before-open"
        );
        assert_eq!(
            runtime_report(&report)["build"]["files"]["sensitive"],
            1,
            "runtime report lost the literal .env exclusion outcome"
        );

        let graph = fs::read(managed(project, "graph.json")).expect("cold graph");
        let graph_value: Value = serde_json::from_slice(&graph).expect("cold graph JSON");
        let nodes = graph_value["nodes"].as_array().expect("cold graph nodes");
        let graph_text = String::from_utf8_lossy(&graph);
        for secret in &source_secrets {
            assert!(
                graph_text.contains(secret),
                "cold graph dropped source knowledge {secret}"
            );
        }
        for (source, safe_marker, route) in [
            ("settings.json", "visible-json", "JSON"),
            ("settings.toml", "visible-toml", "TOML"),
            ("settings.xml", "visible-xml", "XML"),
            ("accounts.csv", "visible-csv", "CSV"),
            ("settings.ini", "visible-ini", "INI"),
            ("app.env", "visible-env", "environment assignment"),
            (
                "tsconfig.json",
                "./visible-tsconfig-base.json",
                "named JSON configuration",
            ),
            (
                "settings.zip!/nested/settings.json",
                "visible-archive",
                "archived JSON member",
            ),
        ] {
            assert!(
                nodes.iter().any(|node| {
                    node["source_file"] == source
                        && serde_json::to_string(node)
                            .expect("serialize graph node")
                            .contains(safe_marker)
                }),
                "cold graph did not preserve {route} safe marker {safe_marker} at {source}"
            );
        }
        assert!(
            nodes.iter().any(|node| {
                node["source_file"] == ".mcp.json"
                    && node["metadata"]["mcp_kind"] == "mcp_server"
                    && node["label"] == "private"
            }),
            "cold graph did not take the MCP-specific server route"
        );
        assert!(
            nodes.iter().any(|node| {
                node["source_file"] == "settings.zip!/nested/settings.json"
                    && node["structured_value"] == STRUCTURED_SECRET_SENTINELS[10]
            }),
            "archived JSON member dropped its source knowledge"
        );
        let stderr = String::from_utf8_lossy(&cold.stderr);
        assert!(
            stderr.contains("skipped as potentially sensitive") && stderr.contains(".env"),
            "literal .env was not excluded before payload extraction: {stderr}"
        );
    }
    assert_eq!(
        graph_and_coverage_bytes(&one_worker),
        graph_and_coverage_bytes(&many_workers),
        "cold structured graph changed with worker count"
    );
    let accepted = graph_and_coverage_bytes(&one_worker);

    for (project, workers, report_name) in [
        (&one_worker, "1", "one-warm.json"),
        (&many_workers, "4", "many-warm.json"),
    ] {
        fs::remove_file(managed(project, "graph.json")).expect("remove graph before warm build");
        let report = fixture.path().join(report_name);
        let warm = run_full_index(project, &report, workers);
        assert_success(&warm);
        let cache = runtime_cache(&report);
        let warm_hits = cache["metadata_hits"].as_u64().unwrap_or_default()
            + cache["runtime_hits"].as_u64().unwrap_or_default();
        assert!(
            warm_hits >= 9,
            "warm structured build did not hit every physical input cache: {cache}"
        );
        assert!(
            cache["parses_avoided"]
                .as_u64()
                .is_some_and(|value| value >= 9),
            "warm structured build reparsed cached inputs: {cache}"
        );
        assert_output_excludes_secrets(&warm, &report, &output_secrets);
        assert_managed_tree_excludes_secrets(project, &cold_withheld_secrets);
        assert_eq!(graph_and_coverage_bytes(project), accepted);
    }

    for (project, workers, report_name) in [
        (&one_worker, "1", "one-update.json"),
        (&many_workers, "4", "many-update.json"),
    ] {
        write_json_secret(project, ROTATED_JSON_SECRET_SENTINEL);
        assert!(contains_bytes(
            &fs::read(project.join("settings.json")).expect("rotated JSON source"),
            ROTATED_JSON_SECRET_SENTINEL.as_bytes()
        ));
        let retired = seed_retired_secret_cache(project);
        let report = fixture.path().join(report_name);
        let update = run_update(project, &report, &["--compute-workers", workers]);
        assert_success(&update);
        assert_retired_cache_purged(&retired);
        let cache = runtime_cache(&report);
        assert!(
            cache["misses"].as_u64().is_some_and(|value| value >= 1),
            "rotated secret was not re-extracted: {cache}"
        );
        assert!(
            cache["metadata_hits"].as_u64().unwrap_or_default()
                + cache["runtime_hits"].as_u64().unwrap_or_default()
                >= 6,
            "unchanged inputs selected for the incremental pass did not remain warm: {cache}"
        );
        assert_output_excludes_secrets(&update, &report, &output_secrets);
        assert_managed_tree_excludes_secrets(project, &withheld_secrets);
        let updated_graph: Value = serde_json::from_slice(
            &fs::read(managed(project, "graph.json")).expect("updated graph bytes"),
        )
        .expect("updated graph JSON");
        let updated_nodes = updated_graph["nodes"].as_array().expect("updated nodes");
        assert!(
            updated_nodes.iter().any(|node| {
                node["source_file"] == "settings.json" && node["structured_value"] == "visible-json"
            }),
            "updated JSON route dropped its safe value"
        );
        assert!(
            updated_nodes.iter().any(|node| {
                node["source_file"] == "settings.json"
                    && node["structured_value"] == ROTATED_JSON_SECRET_SENTINEL
            }),
            "updated JSON route dropped the current source value"
        );
    }
    assert_eq!(
        graph_and_coverage_bytes(&one_worker),
        graph_and_coverage_bytes(&many_workers),
        "updated structured graph changed with worker count"
    );
}

#[test]
fn unsafe_pre_redaction_cache_aborts_before_graph_publication() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project");
    fs::write(project.join("lib.rs"), "pub fn safe() {}\n").expect("source");
    let legacy_directory = managed(&project, "cache/ast/v29");
    fs::create_dir_all(&legacy_directory).expect("legacy cache directory");
    let valid_artifact = legacy_directory.join(format!("{}.json", "b".repeat(64)));
    fs::write(&valid_artifact, RETIRED_AST_SECRET_SENTINEL).expect("legacy artifact");
    fs::write(legacy_directory.join("unexpected.txt"), b"unexpected")
        .expect("unexpected legacy entry");
    let prior_graph = seed_prior_graph(&project);

    let report = fixture.path().join("rejected-runtime.json");
    let rejected = run_full_index(&project, &report, "1");
    assert!(
        !rejected.status.success(),
        "unexpected success: {}",
        output_text(&rejected)
    );
    assert_eq!(
        fs::read(managed(&project, "graph.json")).expect("prior graph remains"),
        prior_graph,
        "migration failure changed the prior accepted graph"
    );
    assert_eq!(
        fs::read(&valid_artifact).expect("preflight preserves valid artifact"),
        RETIRED_AST_SECRET_SENTINEL.as_bytes()
    );
    assert!(
        !report.exists(),
        "migration failure published a runtime report"
    );
    assert_bytes_exclude_secrets(
        "migration failure stdout",
        &rejected.stdout,
        &[RETIRED_AST_SECRET_SENTINEL, PRIOR_GRAPH_SECRET_SENTINEL],
    );
    assert_bytes_exclude_secrets(
        "migration failure stderr",
        &rejected.stderr,
        &[RETIRED_AST_SECRET_SENTINEL, PRIOR_GRAPH_SECRET_SENTINEL],
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

    let runtime_root = managed(&project, "cache/runtime-v2");
    fs::rename(&runtime_root, fixture.path().join("saved-runtime-v2"))
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
    let repaired = artifact_bytes(&project);
    assert_eq!(repaired[0], accepted[0], "graph remains deterministic");
    assert_eq!(repaired[2], accepted[2], "coverage remains deterministic");
    assert_eq!(
        manifest_without_runtime_cache(&repaired[1]),
        manifest_without_runtime_cache(&accepted[1]),
        "cache authorization is the only intentional manifest difference"
    );
    let repaired_manifest: Value =
        serde_json::from_slice(&repaired[1]).expect("repaired manifest JSON");
    assert!(
        repaired_manifest
            .as_object()
            .expect("manifest object")
            .values()
            .all(|entry| entry.get("runtime_cache").is_none()),
        "failed cache persistence must not authorize the unsafe artifact path"
    );
    assert_eq!(fs::read_dir(&outside).expect("outside target").count(), 0);

    let second_report = fixture.path().join("second-disabled-runtime.json");
    let second = run_index(&project, &second_report, &[]);
    assert_success(&second);
    assert_eq!(
        artifact_bytes(&project),
        repaired,
        "a later run must not replay or reauthorize the unsafe cache target"
    );
    assert_eq!(fs::read_dir(&outside).expect("outside target").count(), 0);
}
