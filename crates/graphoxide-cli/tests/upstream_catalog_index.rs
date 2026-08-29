//! Catalog-aware indexing contracts exercised through the shipped CLI binary.

use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    io::Write as _,
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

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", output_text(output));
    serde_json::from_slice::<Value>(&output.stdout).unwrap_or_else(|error| {
        panic!("index stdout is not JSON: {error}\n{}", output_text(output))
    });
}

fn managed(project: &Path, name: &str) -> PathBuf {
    project.join("graphoxide-out").join(name)
}

fn graph(project: &Path) -> Value {
    serde_json::from_slice(&fs::read(managed(project, "graph.json")).expect("graph bytes"))
        .expect("graph JSON")
}

fn runtime_report(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("runtime report bytes"))
        .expect("runtime report JSON")
}

fn catalog_entry(source_path: &str, source_id: &str, capture_id: &str) -> Value {
    json!({
        "source_id": source_id,
        "capture_id": capture_id,
        "source_path": source_path,
        "sha256": "",
        "captured_at": "2026-08-24T12:34:56Z",
        "accessed_at": "2026-08-24T12:35:56+00:00",
        "updated_at": "2026-08-23T01:02:03.456-06:00",
        "representation": "markdown",
        "source_system": "sharepoint",
        "url": "https://example.test/sites/team/document",
        "location": "Team/Architecture"
    })
}

fn write_catalog(project: &Path, mut entries: Value) {
    for entry in entries.as_array_mut().expect("catalog entries") {
        let source_path = entry["source_path"].as_str().expect("catalog source_path");
        entry["sha256"] = json!(hex::encode(Sha256::digest(
            fs::read(project.join(source_path)).expect("catalog source bytes")
        )));
    }
    let directory = project.join("catalog");
    fs::create_dir_all(&directory).expect("catalog directory");
    fs::write(
        directory.join("catalog.json"),
        serde_json::to_vec(&json!({"version": 1, "entries": entries})).expect("catalog JSON"),
    )
    .expect("catalog file");
}

fn write_catalog_unchecked(project: &Path, entries: Value) {
    let directory = project.join("catalog");
    fs::create_dir_all(&directory).expect("catalog directory");
    fs::write(
        directory.join("catalog.json"),
        serde_json::to_vec(&json!({"version": 1, "entries": entries})).expect("catalog JSON"),
    )
    .expect("catalog file");
}

fn write_v2_catalog(project: &Path) {
    let active = fs::read(project.join("raw/active.rs")).expect("active source bytes");
    let directory = project.join("catalog");
    fs::create_dir_all(&directory).expect("catalog directory");
    fs::write(
        directory.join("catalog.json"),
        serde_json::to_vec(&json!({
            "version": 2,
            "sources": [{
                "source_id": "source-one",
                "source_system": "sharepoint",
                "url": "https://example.invalid/site/page",
                "location": "Site/Library/Folder/Page",
                "active_capture_id": "capture-active"
            }],
            "captures": [
                {
                    "source_id": "source-one",
                    "capture_id": "capture-active",
                    "source_path": "raw/active.rs",
                    "sha256": hex::encode(Sha256::digest(active)),
                    "captured_at": "2026-08-24T12:34:56Z",
                    "accessed_at": "2026-08-24T12:35:56Z",
                    "updated_at": "2026-08-24T12:35:56Z",
                    "representation": "markdown"
                },
                {
                    "source_id": "source-one",
                    "capture_id": "capture-history",
                    "source_path": "raw/history.rs",
                    "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "captured_at": "2026-08-23T12:34:56Z",
                    "accessed_at": "2026-08-23T12:35:56Z",
                    "updated_at": "2026-08-23T12:35:56Z",
                    "representation": "markdown"
                }
            ]
        }))
        .expect("catalog JSON"),
    )
    .expect("catalog file");
}

fn run_index(project: &Path, force: bool, no_cluster: bool, report: &Path) -> Output {
    let mut command = graphoxide(project);
    command
        .args(["index", ".", "--catalog", "catalog", "--json"])
        .arg("--runtime-report")
        .arg(report);
    if force {
        command.arg("--force");
    }
    if no_cluster {
        command.arg("--no-cluster");
    }
    command.output().expect("run catalog-aware index")
}

fn run_extract(project: &Path, force: bool, no_cluster: bool) -> Output {
    let mut command = graphoxide(project);
    command.args(["extract", ".", "--catalog", "catalog", "--json"]);
    if force {
        command.arg("--force");
    }
    if no_cluster {
        command.arg("--no-cluster");
    }
    command.output().expect("run catalog-aware extract")
}

fn source_nodes<'a>(graph: &'a Value, source_file: &str) -> Vec<&'a Value> {
    graph["nodes"]
        .as_array()
        .expect("graph nodes")
        .iter()
        .filter(|node| node["source_file"] == source_file)
        .collect()
}

fn assert_catalog_source(graph: &Value, source_file: &str, source_id: &str) {
    let nodes = source_nodes(graph, source_file);
    assert!(
        !nodes.is_empty(),
        "expected catalog annotations for {source_file}"
    );
    assert!(
        nodes
            .iter()
            .all(|node| node["catalog"]["source_id"] == source_id),
        "unexpected catalog annotation for {source_file}: {nodes:#?}"
    );
}

fn graph_without_catalog(mut graph: Value) -> Value {
    for node in graph["nodes"].as_array_mut().expect("graph nodes") {
        node.as_object_mut()
            .expect("graph node object")
            .remove("catalog");
    }
    graph
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

#[test]
fn catalog_only_metadata_edits_reannotate_raw_graph_without_touching_manifest_or_sources() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("source directory");
    fs::write(
        project.join("src/lib.rs"),
        "pub fn answer() -> u32 { helper() }\nfn helper() -> u32 { 42 }\n",
    )
    .expect("source");
    write_catalog(
        &project,
        json!([catalog_entry("src/lib.rs", "source-one", "capture-one")]),
    );

    assert_success(&run_index(
        &project,
        true,
        true,
        &fixture.path().join("cold-runtime.json"),
    ));
    assert_success(&run_index(
        &project,
        false,
        true,
        &fixture.path().join("warm-runtime.json"),
    ));
    let before_edit = graph(&project);
    assert_catalog_source(&before_edit, "src/lib.rs", "source-one");
    let manifest_before = fs::read(managed(&project, "manifest.json")).expect("warm manifest");

    write_catalog(
        &project,
        json!([catalog_entry("src/lib.rs", "source-two", "capture-one")]),
    );
    let edited_report = fixture.path().join("catalog-only-runtime.json");
    let edited = run_index(&project, false, true, &edited_report);
    assert_success(&edited);
    let edited_runtime = runtime_report(&edited_report);
    assert_eq!(edited_runtime["work"]["parses"], 0);
    assert_eq!(edited_runtime["io"]["sources_read"], 0);
    assert_eq!(
        fs::read(managed(&project, "manifest.json")).expect("manifest after catalog edit"),
        manifest_before,
        "catalog-only metadata must not change extraction manifest bytes"
    );
    let after_edit = graph(&project);
    assert_catalog_source(&after_edit, "src/lib.rs", "source-two");
    assert_eq!(
        graph_without_catalog(after_edit.clone()),
        graph_without_catalog(before_edit),
        "catalog-only edits must not alter extraction-derived graph data"
    );

    write_catalog(&project, json!([]));
    let empty_report = fixture.path().join("empty-catalog-runtime.json");
    assert_success(&run_index(&project, false, true, &empty_report));
    let empty_runtime = runtime_report(&empty_report);
    assert_eq!(empty_runtime["work"]["parses"], 0);
    assert_eq!(empty_runtime["io"]["sources_read"], 0);
    assert_eq!(
        fs::read(managed(&project, "manifest.json")).expect("manifest after empty catalog"),
        manifest_before,
        "empty catalog must not change extraction manifest bytes"
    );
    let after_empty = graph(&project);
    assert!(after_empty["nodes"]
        .as_array()
        .expect("graph nodes")
        .iter()
        .all(|node| node.get("catalog").is_none()));
    assert_eq!(after_empty, graph_without_catalog(after_edit));
}

#[test]
fn v2_inactive_capture_is_excluded_from_graph_coverage_and_incremental_cache() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("raw")).expect("source directory");
    fs::write(project.join("raw/active.rs"), "pub fn active() {}\n").expect("active source");
    fs::write(project.join("raw/history.rs"), "pub fn historical() {}\n")
        .expect("historical source");
    write_v2_catalog(&project);

    let cold_report = fixture.path().join("cold-runtime.json");
    assert_success(&run_index(&project, true, true, &cold_report));
    let cold = runtime_report(&cold_report);
    assert_eq!(cold["work"]["parses"], 1, "only the active capture parses");
    assert_eq!(
        cold["io"]["sources_read"], 1,
        "only the active capture reads"
    );
    assert!(source_nodes(&graph(&project), "raw/history.rs").is_empty());
    let coverage: Value =
        serde_json::from_slice(&fs::read(managed(&project, "coverage.json")).expect("coverage"))
            .expect("coverage JSON");
    assert!(coverage["files"]
        .as_array()
        .expect("coverage files")
        .iter()
        .all(|file| file["path"] != "raw/history.rs"));
    assert!(
        !fs::read_to_string(managed(&project, "manifest.json"))
            .expect("manifest")
            .contains("raw/history.rs"),
        "inactive capture must not enter the incremental extraction cache"
    );
}

#[test]
fn invalid_catalog_fails_before_graph_or_manifest_publication() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("source directory");
    fs::write(
        project.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("source");
    write_catalog(
        &project,
        json!([catalog_entry("src/lib.rs", "source-one", "capture-one")]),
    );
    assert_success(&run_index(
        &project,
        true,
        true,
        &fixture.path().join("initial-runtime.json"),
    ));
    let accepted_graph = fs::read(managed(&project, "graph.json")).expect("accepted graph");
    let accepted_manifest = fs::read(managed(&project, "manifest.json")).expect("manifest");

    fs::write(
        project.join("catalog/catalog.json"),
        br#"{"version":1,"entries":[{"source_id":"missing-required-fields"}]}"#,
    )
    .expect("invalid catalog");
    let rejected = run_index(
        &project,
        false,
        true,
        &fixture.path().join("rejected-runtime.json"),
    );
    assert!(!rejected.status.success(), "{}", output_text(&rejected));
    assert_eq!(
        fs::read(managed(&project, "graph.json")).expect("graph after rejection"),
        accepted_graph,
        "invalid catalog must not publish graph output"
    );
    assert_eq!(
        fs::read(managed(&project, "manifest.json")).expect("manifest after rejection"),
        accepted_manifest,
        "invalid catalog must not publish manifest output"
    );
}

#[test]
fn unsafe_percent_encoded_catalog_metadata_fails_without_exposing_or_publishing_it() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("source directory");
    fs::write(
        project.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("source");
    write_catalog(
        &project,
        json!([catalog_entry("src/lib.rs", "source-one", "capture-one")]),
    );
    assert_success(&run_index(
        &project,
        true,
        true,
        &fixture.path().join("initial-runtime.json"),
    ));
    let accepted_graph = fs::read(managed(&project, "graph.json")).expect("accepted graph");
    let accepted_manifest = fs::read(managed(&project, "manifest.json")).expect("manifest");

    for (ordinal, location) in [
        "Documents%3B%20X%2DAmz%2DSignature%3DCATALOG_SECRET_SENTINEL".to_owned(),
        "x%FFapi%5Fkey=CATALOG_SECRET_SENTINEL".to_owned(),
        "x%25FFapi%255Fkey=CATALOG_SECRET_SENTINEL".to_owned(),
        "Documents%25ZZ".to_owned(),
    ]
    .into_iter()
    .enumerate()
    {
        let mut record = catalog_entry("src/lib.rs", "source-one", "capture-one");
        record["location"] = json!(location);
        write_catalog(&project, json!([record]));
        let rejected = run_index(
            &project,
            false,
            true,
            &fixture
                .path()
                .join(format!("rejected-{ordinal}-runtime.json")),
        );

        let output = output_text(&rejected);
        assert!(!rejected.status.success(), "{output}");
        assert!(
            !output.contains("CATALOG_SECRET_SENTINEL"),
            "catalog diagnostics must not expose credentials: {output}"
        );
        assert_eq!(
            fs::read(managed(&project, "graph.json")).expect("graph after rejection"),
            accepted_graph,
            "unsafe encoded metadata must not publish graph output"
        );
        assert_eq!(
            fs::read(managed(&project, "manifest.json")).expect("manifest after rejection"),
            accepted_manifest,
            "unsafe encoded metadata must not publish manifest output"
        );
    }
}

#[test]
fn oversized_percent_encoded_catalog_metadata_fails_before_decoding_or_publication() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("source directory");
    fs::write(
        project.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("source");
    write_catalog(
        &project,
        json!([catalog_entry("src/lib.rs", "source-one", "capture-one")]),
    );
    assert_success(&run_index(
        &project,
        true,
        true,
        &fixture.path().join("initial-runtime.json"),
    ));
    let accepted_graph = fs::read(managed(&project, "graph.json")).expect("accepted graph");
    let accepted_manifest = fs::read(managed(&project, "manifest.json")).expect("manifest");

    let mut oversized = catalog_entry("src/lib.rs", "source-one", "capture-one");
    oversized["location"] = Value::String("%ZZ".repeat(32 * 1024));
    write_catalog(&project, json!([oversized]));
    let rejected = run_index(
        &project,
        false,
        true,
        &fixture.path().join("rejected-runtime.json"),
    );

    assert!(!rejected.status.success(), "{}", output_text(&rejected));
    assert!(
        output_text(&rejected).contains("catalog entry metadata exceeds the 65536-byte limit"),
        "{}",
        output_text(&rejected)
    );
    assert_eq!(
        fs::read(managed(&project, "graph.json")).expect("graph after rejection"),
        accepted_graph,
        "oversized percent-encoded metadata must not publish graph output"
    );
    assert_eq!(
        fs::read(managed(&project, "manifest.json")).expect("manifest after rejection"),
        accepted_manifest,
        "oversized percent-encoded metadata must not publish manifest output"
    );
}

#[test]
fn embedded_catalog_tokens_fail_without_exposing_or_publishing_raw_or_clustered_outputs() {
    for no_cluster in [true, false] {
        let fixture = tempfile::tempdir().expect("temporary fixture");
        let project = fixture.path().join("project");
        fs::create_dir_all(project.join("src")).expect("source directory");
        fs::write(
            project.join("src/lib.rs"),
            "pub fn answer() -> u32 { helper() }\nfn helper() -> u32 { 42 }\n",
        )
        .expect("source");
        write_catalog(
            &project,
            json!([catalog_entry("src/lib.rs", "source-one", "capture-one")]),
        );
        assert_success(&run_index(
            &project,
            true,
            no_cluster,
            &fixture.path().join("initial-runtime.json"),
        ));
        let accepted_graph = fs::read(managed(&project, "graph.json")).expect("accepted graph");
        let accepted_manifest =
            fs::read(managed(&project, "manifest.json")).expect("accepted manifest");

        for (ordinal, (field, value)) in [
            ("source_path", "Documents/sk-live-CATALOG_SECRET_SENTINEL"),
            ("location", "Documents/sk-live-CATALOG_SECRET_SENTINEL"),
            (
                "source_path",
                "Documents/sk-abcdefghijklmnop1234-CATALOG_SECRET_SENTINEL",
            ),
            ("location", "Documents%2Fsk%2DCATALOG_SECRET_SENTINEL"),
        ]
        .into_iter()
        .enumerate()
        {
            let mut record = catalog_entry("src/lib.rs", "source-one", "capture-one");
            record["sha256"] = json!(hex::encode(Sha256::digest(
                fs::read(project.join("src/lib.rs")).expect("catalog source bytes")
            )));
            record[field] = json!(value);
            write_catalog_unchecked(&project, json!([record]));

            let rejected = run_index(
                &project,
                false,
                no_cluster,
                &fixture
                    .path()
                    .join(format!("rejected-{no_cluster}-{ordinal}-runtime.json")),
            );
            let output = output_text(&rejected);
            assert!(!rejected.status.success(), "{output}");
            assert!(
                !output.contains("CATALOG_SECRET_SENTINEL"),
                "catalog diagnostics must not expose credentials: {output}"
            );
            assert_eq!(
                fs::read(managed(&project, "graph.json")).expect("graph after rejection"),
                accepted_graph,
                "unsafe catalog metadata must not publish graph output"
            );
            assert_eq!(
                fs::read(managed(&project, "manifest.json")).expect("manifest after rejection"),
                accepted_manifest,
                "unsafe catalog metadata must not publish manifest output"
            );
        }
    }
}

#[test]
fn catalog_reservation_rejects_an_insufficient_runtime_budget_before_publication() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("source directory");
    fs::write(
        project.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("source");
    write_catalog(
        &project,
        json!([catalog_entry("src/lib.rs", "source-one", "capture-one")]),
    );

    let rejected = graphoxide(&project)
        .args([
            "index",
            ".",
            "--catalog",
            "catalog",
            "--memory-budget-bytes",
            "1048576",
            "--json",
        ])
        .output()
        .expect("run constrained catalog index");

    assert!(!rejected.status.success(), "{}", output_text(&rejected));
    assert!(
        output_text(&rejected).contains("catalog reserves 201326592 bytes"),
        "{}",
        output_text(&rejected)
    );
    assert!(
        !managed(&project, "graph.json").exists() && !managed(&project, "manifest.json").exists(),
        "an under-budget catalog must fail before publication"
    );
}

#[test]
fn extract_applies_catalog_annotations_and_rejects_digest_mismatch_before_publication() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("source directory");
    fs::write(
        project.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("source");
    write_catalog(
        &project,
        json!([catalog_entry("src/lib.rs", "source-one", "capture-one")]),
    );

    assert_success(&run_extract(&project, true, true));
    assert_catalog_source(&graph(&project), "src/lib.rs", "source-one");
    let accepted_graph = fs::read(managed(&project, "graph.json")).expect("accepted graph");
    let accepted_manifest = fs::read(managed(&project, "manifest.json")).expect("manifest");

    let mut mismatched = catalog_entry("src/lib.rs", "source-one", "capture-one");
    mismatched["sha256"] = Value::String("0".repeat(64));
    write_catalog_unchecked(&project, json!([mismatched]));
    let rejected = run_extract(&project, false, true);
    assert!(!rejected.status.success(), "{}", output_text(&rejected));
    assert!(
        output_text(&rejected).contains("catalog sha256 does not match source_path"),
        "{}",
        output_text(&rejected)
    );
    assert_eq!(
        fs::read(managed(&project, "graph.json")).expect("graph after rejection"),
        accepted_graph,
        "digest mismatch must not publish extract graph output"
    );
    assert_eq!(
        fs::read(managed(&project, "manifest.json")).expect("manifest after rejection"),
        accepted_manifest,
        "digest mismatch must not publish extract manifest output"
    );
}

#[test]
fn source_digest_mismatch_blocks_raw_and_clustered_index_publication() {
    for no_cluster in [true, false] {
        let fixture = tempfile::tempdir().expect("temporary fixture");
        let project = fixture.path().join("project");
        fs::create_dir_all(project.join("src")).expect("source directory");
        fs::write(
            project.join("src/lib.rs"),
            "pub fn answer() -> u32 { helper() }\nfn helper() -> u32 { 42 }\n",
        )
        .expect("source");
        write_catalog(
            &project,
            json!([catalog_entry("src/lib.rs", "source-one", "capture-one")]),
        );
        assert_success(&run_index(
            &project,
            true,
            no_cluster,
            &fixture.path().join("initial-runtime.json"),
        ));
        let accepted_graph = fs::read(managed(&project, "graph.json")).expect("accepted graph");
        let accepted_manifest = fs::read(managed(&project, "manifest.json")).expect("manifest");

        fs::write(
            project.join("src/lib.rs"),
            "pub fn answer() -> u32 { replacement() }\nfn replacement() -> u32 { 7 }\n",
        )
        .expect("mutate catalog source");
        let rejected = run_index(
            &project,
            false,
            no_cluster,
            &fixture.path().join("rejected-runtime.json"),
        );

        assert!(!rejected.status.success(), "{}", output_text(&rejected));
        assert!(
            output_text(&rejected).contains("catalog sha256 does not match source_path"),
            "{}",
            output_text(&rejected)
        );
        assert_eq!(
            fs::read(managed(&project, "graph.json")).expect("graph after rejection"),
            accepted_graph,
            "source digest mismatch must not publish index graph output"
        );
        assert_eq!(
            fs::read(managed(&project, "manifest.json")).expect("manifest after rejection"),
            accepted_manifest,
            "source digest mismatch must not publish index manifest output"
        );
    }
}

#[test]
fn oversized_replicated_catalog_annotation_fails_before_publication() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("source directory");
    let mut source = String::new();
    for index in 0..2_200 {
        source.push_str(&format!("pub fn symbol_{index}() -> u32 {{ {index} }}\n"));
    }
    fs::write(project.join("src/lib.rs"), source).expect("source");
    write_catalog(
        &project,
        json!([catalog_entry("src/lib.rs", "source-one", "capture-one")]),
    );
    assert_success(&run_index(
        &project,
        true,
        true,
        &fixture.path().join("initial-runtime.json"),
    ));
    let accepted_graph = fs::read(managed(&project, "graph.json")).expect("accepted graph");
    let accepted_manifest = fs::read(managed(&project, "manifest.json")).expect("manifest");

    let mut oversized = catalog_entry("src/lib.rs", "source-one", "capture-one");
    oversized["location"] = Value::String("x".repeat(60 * 1024));
    write_catalog(&project, json!([oversized]));
    let rejected = run_index(
        &project,
        false,
        true,
        &fixture.path().join("rejected-runtime.json"),
    );
    assert!(!rejected.status.success(), "{}", output_text(&rejected));
    assert_eq!(
        fs::read(managed(&project, "graph.json")).expect("graph after rejection"),
        accepted_graph,
        "oversized replicated catalog annotations must not publish graph output"
    );
    assert_eq!(
        fs::read(managed(&project, "manifest.json")).expect("manifest after rejection"),
        accepted_manifest,
        "oversized replicated catalog annotations must not publish manifest output"
    );
}

#[test]
fn catalog_annotations_survive_clustered_graph_construction() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("source directory");
    fs::write(
        project.join("src/lib.rs"),
        "pub fn answer() -> u32 { helper() }\npub fn helper() -> u32 { 42 }\n",
    )
    .expect("source");
    write_catalog(
        &project,
        json!([catalog_entry("src/lib.rs", "source-one", "capture-one")]),
    );

    assert_success(&run_index(
        &project,
        true,
        false,
        &fixture.path().join("clustered-runtime.json"),
    ));
    let indexed = graph(&project);
    let annotated = source_nodes(&indexed, "src/lib.rs");
    assert!(
        !annotated.is_empty(),
        "clustered graph lost catalog annotations"
    );
    assert!(annotated
        .iter()
        .all(|node| node["catalog"]["capture_id"] == "capture-one"));
}

#[test]
fn archive_member_nodes_inherit_the_outer_source_catalog_annotation() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(
        project.join("architecture.zip"),
        zip_bytes(&[(
            "design/architecture.dot",
            b"digraph Architecture { gateway -> database; }\n",
        )]),
    )
    .expect("archive source");
    write_catalog(
        &project,
        json!([catalog_entry(
            "architecture.zip",
            "archive-source",
            "capture-one",
        )]),
    );

    assert_success(&run_index(
        &project,
        true,
        true,
        &fixture.path().join("archive-runtime.json"),
    ));
    let indexed = graph(&project);
    let members = indexed["nodes"]
        .as_array()
        .expect("graph nodes")
        .iter()
        .filter(|node| {
            node["source_file"]
                .as_str()
                .is_some_and(|source| source.starts_with("architecture.zip!/"))
        })
        .collect::<Vec<_>>();
    assert!(!members.is_empty(), "container member nodes");
    for member in members {
        assert_eq!(member["catalog"]["source_path"], "architecture.zip");
        assert_eq!(member["catalog"]["source_id"], "archive-source");
    }
}
