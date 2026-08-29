use graphoxide_cli::wiki_materialize::reviewable_draft_sha256;
use graphoxide_extract::registry::{shard_for_source_id, RegistrySourceState};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::{fs, path::Path, process::Command};

const SOURCE_ID: &str = "equipment-guide";
const CAPTURE_ID: &str = "capture-20260827";

fn write_json(path: &Path, value: Value) {
    fs::create_dir_all(path.parent().expect("parent directory")).expect("create parent");
    fs::write(
        path,
        format!(
            "{}\n",
            serde_json::to_string(&value).expect("canonical JSON")
        ),
    )
    .expect("write record");
}

fn registry(root: &Path) {
    registry_for_source(root, "equipment/defaults.md", &"aa".repeat(32));
}

fn registry_for_source(root: &Path, relative_path: &str, sha256: &str) {
    write_json(
        &root.join("registry.json"),
        json!({"catalog_id": "demo-catalog", "version": 1}),
    );
    write_json(
        &root.join("origins/team-docs.json"),
        json!({"kind": "filesystem", "logical_name": "team-docs", "origin_id": "team-docs", "version": 1}),
    );
    let shard = shard_for_source_id(SOURCE_ID);
    write_json(
        &root.join(format!("sources/{shard}/{SOURCE_ID}.json")),
        json!({
            "active_capture_id": CAPTURE_ID,
            "origin_id": "team-docs",
            "relative_path": relative_path,
            "source_id": SOURCE_ID,
            "state": "active",
            "version": 1
        }),
    );
    write_json(
        &root.join(format!("captures/{shard}/{SOURCE_ID}/{CAPTURE_ID}.json")),
        json!({
            "capture_id": CAPTURE_ID,
            "observed_at": "2026-08-27T12:34:56Z",
            "relative_path": relative_path,
            "representation": "markdown",
            "sha256": sha256,
            "source_id": SOURCE_ID,
            "version": 1
        }),
    );
}

#[test]
fn validate_reports_a_registry_summary_without_reading_raw_sources() {
    let fixture = tempfile::tempdir().expect("temporary registry");
    registry(fixture.path());

    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .args(["registry", "validate", "--tree"])
        .arg(fixture.path())
        .arg("--json")
        .output()
        .expect("run registry validate");
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("JSON summary");
    assert_eq!(report["catalog_id"], "demo-catalog");
    assert_eq!(report["sources"], 1);
    assert_eq!(report["active_captures"], 1);
}

#[test]
fn freshness_policy_queues_expired_model_work_from_active_capture_history() {
    let fixture = tempfile::tempdir().expect("temporary registry");
    let tree = fixture.path().join("registry");
    let cache_home = fixture.path().join("cache");
    registry(&tree);
    write_json(
        &tree.join("policies/freshness.json"),
        json!({
            "version": 1,
            "model_stage": "wiki-draft",
            "model_max_age_seconds": 60,
            "source_priorities": {SOURCE_ID: 7}
        }),
    );
    let shard = shard_for_source_id(SOURCE_ID);
    write_json(
        &tree.join(format!(
            "runs/{shard}/{SOURCE_ID}/{CAPTURE_ID}/wiki-draft/wiki-draft-1.json"
        )),
        json!({
            "version": 1,
            "run_id": "wiki-draft-1",
            "source_id": SOURCE_ID,
            "capture_id": CAPTURE_ID,
            "stage": "wiki-draft",
            "status": "succeeded",
            "processor": "ollama-native",
            "started_at": "2026-08-27T12:00:00Z",
            "finished_at": "2026-08-27T12:00:01Z",
            "actor": "codex",
            "agent_run_id": "agent-run-1",
            "model_requested": "llama3.2",
            "model_reported": "llama3.2",
            "profile_digest": "a".repeat(64),
            "prompt_schema_digest": "b".repeat(64),
            "evidence_manifest_digest": "c".repeat(64),
            "output_digest": "d".repeat(64),
            "error_class": null
        }),
    );

    let output = graphoxide()
        .args(["registry", "freshness", "--tree"])
        .arg(&tree)
        .args([
            "--now",
            "2026-08-28T12:00:00Z",
            "--origin-id",
            "team-docs",
            "--json",
        ])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("read policy-driven freshness");
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let freshness: Value = serde_json::from_slice(&output.stdout).expect("freshness JSON");
    assert!(freshness["queued"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["source_id"] == SOURCE_ID
                && item["stage"] == "wiki-draft"
                && item["reason"] == "expired"
                && item["tag_priority"] == 7
        })
    }));
}

#[test]
fn discovery_lists_every_safe_file_but_tracks_only_on_explicit_acceptance() {
    let fixture = tempfile::tempdir().expect("temporary registry");
    let tree = fixture.path().join("registry");
    let raw = fixture.path().join("raw");
    registry(&tree);
    fs::create_dir_all(raw.join("docs")).expect("create docs");
    fs::create_dir_all(raw.join(".git")).expect("create git metadata");
    fs::write(raw.join("docs/guide.md"), "# Guide\n").expect("write markdown");
    fs::write(raw.join("blob.unknown"), [0_u8, 1, 2]).expect("write opaque file");
    fs::write(raw.join(".git/config"), "private metadata").expect("write ignored metadata");
    let cache_home = fixture.path().join("cache");
    let bind = graphoxide()
        .args(["registry", "origin", "bind", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--local-root"])
        .arg(&raw)
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("bind origin");
    assert!(
        bind.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&bind.stderr)
    );

    let listed = graphoxide()
        .args(["registry", "discover", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--max-files", "1", "--json"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("list candidates");
    assert!(
        listed.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: Value = serde_json::from_slice(&listed.stdout).expect("candidate JSON");
    assert_eq!(listed["accepted"], false);
    assert_eq!(listed["candidates"].as_array().map(Vec::len), Some(1));
    assert!(listed["candidates"]
        .as_array()
        .is_some_and(|candidates| candidates.iter().all(|candidate| {
            candidate["relative_path"]
                .as_str()
                .is_some_and(|path| !path.starts_with(".git/"))
        })));
    assert_eq!(
        graphoxide_extract::registry::RegistrySnapshot::load(&tree)
            .expect("unmodified registry")
            .sources()
            .len(),
        1
    );

    let accepted = graphoxide()
        .args(["registry", "discover", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--accept-discovered"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("accept candidates");
    assert!(
        accepted.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let snapshot =
        graphoxide_extract::registry::RegistrySnapshot::load(&tree).expect("accepted registry");
    assert_eq!(snapshot.sources().len(), 3);
    assert!(snapshot
        .sources()
        .values()
        .filter(|source| source.source_id != SOURCE_ID)
        .all(|source| {
            source.state == RegistrySourceState::PendingVerification
                && source.active_capture_id.is_none()
        }));
}

fn graphoxide() -> Command {
    Command::new(env!("CARGO_BIN_EXE_graphoxide"))
}

fn commit_registry(tree: &Path) -> String {
    for args in [
        vec!["init"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Graphoxide Test",
            "-c",
            "user.email=graphoxide-test@example.invalid",
            "commit",
            "-m",
            "registry",
        ],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(tree)
            .args(args)
            .output()
            .expect("run git for registry fixture");
        assert!(
            output.status.success(),
            "git fixture stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("read registry fixture revision");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("Git revision is UTF-8")
        .trim()
        .to_owned()
}

fn commit_registry_changes(tree: &Path, message: &str) -> String {
    for args in [
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Graphoxide Test",
            "-c",
            "user.email=graphoxide-test@example.invalid",
            "commit",
            "-m",
            message,
        ],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(tree)
            .args(args)
            .output()
            .expect("commit registry fixture changes");
        assert!(
            output.status.success(),
            "git fixture stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("read updated registry fixture revision");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("Git revision is UTF-8")
        .trim()
        .to_owned()
}

fn write_materialize_graph(
    path: &Path,
    capture_id: &str,
    sha256: &str,
    observed_at: &str,
    text: &str,
) {
    write_json(
        path,
        json!({
            "directed": false,
            "multigraph": false,
            "nodes": [{
                "id": "equipment-defaults",
                "label": "Equipment defaults",
                "file_type": "document",
                "source_file": "equipment/defaults.md",
                "source_location": "L1",
                "type": "document_heading",
                "structured_text": text,
                "catalog": {
                    "source_id": SOURCE_ID,
                    "capture_id": capture_id,
                    "source_path": "equipment/defaults.md",
                    "sha256": sha256,
                    "captured_at": observed_at,
                    "accessed_at": observed_at,
                    "updated_at": observed_at,
                    "representation": "markdown",
                    "source_system": "registry-filesystem",
                    "url": "local-registry:team-docs",
                    "location": "equipment/defaults.md"
                }
            }],
            "links": []
        }),
    );
}

#[test]
fn lifecycle_commands_track_metadata_and_keep_capture_history() {
    let fixture = tempfile::tempdir().expect("temporary registry");
    let tree = fixture.path().join("registry");

    let output = graphoxide()
        .args(["registry", "init", "--tree"])
        .arg(&tree)
        .args(["--catalog-id", "demo-catalog"])
        .output()
        .expect("initialize registry");
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = graphoxide()
        .args(["registry", "origin", "add", "--tree"])
        .arg(&tree)
        .args([
            "--origin-id",
            "team-docs",
            "--kind",
            "filesystem",
            "--logical-name",
            "team-docs",
        ])
        .output()
        .expect("add origin");
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = graphoxide()
        .args(["registry", "track", "--tree"])
        .arg(&tree)
        .args([
            "--origin-id",
            "team-docs",
            "--source-id",
            SOURCE_ID,
            "--relative-path",
            "equipment/defaults.md",
        ])
        .output()
        .expect("track source");
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let snapshot = graphoxide_extract::registry::RegistrySnapshot::load(&tree)
        .expect("tracked registry stays valid");
    assert!(snapshot.active_captures().is_empty());
    assert_eq!(
        snapshot.sources()[SOURCE_ID].state,
        RegistrySourceState::PendingVerification
    );

    let output = graphoxide()
        .args(["registry", "retire", "--tree"])
        .arg(&tree)
        .args(["--source-id", SOURCE_ID])
        .output()
        .expect("retire source");
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn publish_rename_restore_and_resolve_preserve_capture_history() {
    let fixture = tempfile::tempdir().expect("temporary registry fixture");
    let tree = fixture.path().join("registry");
    let raw_root = fixture.path().join("raw");
    let cache_home = fixture.path().join("cache");
    let original = raw_root.join("equipment/defaults.yaml");
    fs::create_dir_all(original.parent().expect("raw parent")).expect("create raw parent");
    fs::write(
        &original,
        "default_username: admin\ndefault_password: fake-only-password\n",
    )
    .expect("write raw source");

    for args in [
        vec![
            "registry",
            "init",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--catalog-id",
            "demo-catalog",
        ],
        vec![
            "registry",
            "origin",
            "add",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--origin-id",
            "team-docs",
            "--kind",
            "filesystem",
            "--logical-name",
            "team-docs",
        ],
        vec![
            "registry",
            "track",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--origin-id",
            "team-docs",
            "--source-id",
            SOURCE_ID,
            "--relative-path",
            "equipment/defaults.yaml",
        ],
        vec![
            "registry",
            "origin",
            "bind",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--origin-id",
            "team-docs",
            "--local-root",
            raw_root.to_str().expect("UTF-8 raw root"),
        ],
        vec![
            "registry",
            "scan",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--origin-id",
            "team-docs",
            "--mode",
            "changed",
        ],
    ] {
        let output = graphoxide()
            .args(args)
            .env("XDG_CACHE_HOME", &cache_home)
            .output()
            .expect("set up registry lifecycle");
        assert!(
            output.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let listed = graphoxide()
        .args(["registry", "list", "--tree"])
        .arg(&tree)
        .args([
            "--origin-id",
            "team-docs",
            "--format",
            "yaml",
            "--min-size-bytes",
            "1",
            "--json",
        ])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("list registry metadata");
    assert!(
        listed.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: Value = serde_json::from_slice(&listed.stdout).expect("list JSON");
    assert_eq!(listed["sources"][0]["source_id"], SOURCE_ID);
    assert_eq!(listed["sources"][0]["format"], "yaml");
    assert!(listed["sources"][0]["size_bytes"].as_u64().is_some());

    let freshness = graphoxide()
        .args(["registry", "freshness", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--json"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("read registry freshness");
    assert!(
        freshness.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&freshness.stderr)
    );
    let freshness: Value = serde_json::from_slice(&freshness.stdout).expect("freshness JSON");
    assert!(freshness["queued"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item["source_id"] == SOURCE_ID && item["stage"] == "extract")
    }));

    let publish = graphoxide()
        .args(["registry", "publish", "--tree"])
        .arg(&tree)
        .args([
            "--origin-id",
            "team-docs",
            "--from-local-state",
            "--observed-at",
            "2026-08-27T12:34:56Z",
        ])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("publish first capture");
    assert!(
        publish.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&publish.stderr)
    );
    let first = graphoxide_extract::registry::RegistrySnapshot::load(&tree)
        .expect("published registry stays valid");
    let first_capture = first.sources()[SOURCE_ID]
        .active_capture_id
        .clone()
        .expect("first active capture");
    assert_eq!(first.captures().len(), 1);

    let rename = graphoxide()
        .args(["registry", "rename", "--tree"])
        .arg(&tree)
        .args([
            "--source-id",
            SOURCE_ID,
            "--relative-path",
            "equipment/renamed-defaults.yaml",
        ])
        .output()
        .expect("rename source head");
    assert!(
        rename.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&rename.stderr)
    );
    let renamed = graphoxide_extract::registry::RegistrySnapshot::load(&tree)
        .expect("renamed registry stays valid");
    assert_eq!(
        renamed.sources()[SOURCE_ID].state,
        RegistrySourceState::PendingVerification
    );
    assert_eq!(renamed.captures().len(), 1);
    assert!(renamed.sources()[SOURCE_ID].active_capture_id.is_none());

    let moved = raw_root.join("equipment/renamed-defaults.yaml");
    fs::rename(&original, &moved).expect("move raw source without copying it");
    for args in [
        vec![
            "registry",
            "scan",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--origin-id",
            "team-docs",
            "--mode",
            "changed",
        ],
        vec![
            "registry",
            "publish",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--origin-id",
            "team-docs",
            "--from-local-state",
            "--observed-at",
            "2026-08-27T12:35:56Z",
        ],
    ] {
        let output = graphoxide()
            .args(args)
            .env("XDG_CACHE_HOME", &cache_home)
            .output()
            .expect("scan and publish renamed source");
        assert!(
            output.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let republished = graphoxide_extract::registry::RegistrySnapshot::load(&tree)
        .expect("republished registry stays valid");
    let second_capture = republished.sources()[SOURCE_ID]
        .active_capture_id
        .clone()
        .expect("renamed active capture");
    assert_ne!(first_capture, second_capture);
    assert_eq!(republished.captures().len(), 2);
    assert_eq!(
        republished.sources()[SOURCE_ID].relative_path,
        "equipment/renamed-defaults.yaml"
    );

    let run_input = fixture.path().join("wiki-run.json");
    write_json(
        &run_input,
        json!({
            "version": 1,
            "run_id": "wiki-draft-1",
            "source_id": SOURCE_ID,
            "capture_id": second_capture,
            "stage": "wiki-draft",
            "status": "succeeded",
            "processor": "ollama-native",
            "started_at": "2026-08-27T12:36:00Z",
            "finished_at": "2026-08-27T12:36:01Z",
            "actor": "codex",
            "agent_run_id": "agent-run-1",
            "model_requested": "llama3.2",
            "model_reported": "llama3.2",
            "profile_digest": "a".repeat(64),
            "prompt_schema_digest": "b".repeat(64),
            "evidence_manifest_digest": "c".repeat(64),
            "output_digest": "d".repeat(64),
            "provider_request_id": "req-wiki-1",
            "input_tokens": 123,
            "output_tokens": 45,
            "cost_microunits": 1720,
            "latency_ms": 1000,
            "retry_count": 0,
            "error_class": null
        }),
    );
    let record_run = graphoxide()
        .args(["registry", "run", "record", "--tree"])
        .arg(&tree)
        .args(["--input"])
        .arg(&run_input)
        .output()
        .expect("record a provenance run");
    assert!(
        record_run.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&record_run.stderr)
    );
    assert!(tree
        .join(format!(
            "runs/{}/{}/{}/wiki-draft/wiki-draft-1.json",
            shard_for_source_id(SOURCE_ID),
            SOURCE_ID,
            second_capture,
        ))
        .is_file());

    let listed = graphoxide()
        .args(["registry", "list", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--json"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("list processed source metadata");
    assert!(
        listed.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: Value = serde_json::from_slice(&listed.stdout).expect("list JSON");
    let last_run = &listed["sources"][0]["last_runs"][0];
    assert_eq!(last_run["capture_id"], second_capture);
    assert_eq!(last_run["actor"], "codex");
    assert_eq!(last_run["model_reported"], "llama3.2");
    assert_eq!(last_run["provider_request_id"], "req-wiki-1");
    assert_eq!(last_run["input_tokens"], 123);
    assert_eq!(last_run["retry_count"], 0);

    let review_input = fixture.path().join("wiki-review.json");
    write_json(
        &review_input,
        json!({
            "version": 1,
            "review_id": "wiki-review-1",
            "decision": "approved",
            "reviewer": "wiki-reviewer",
            "reviewed_at": "2026-08-27T12:36:02Z",
            "plan_sha256": "e".repeat(64),
            "capture_set_sha256": "f".repeat(64),
            "draft_sha256": "0".repeat(64)
        }),
    );
    let record_review = graphoxide()
        .args(["registry", "review", "record", "--tree"])
        .arg(&tree)
        .args(["--input"])
        .arg(&review_input)
        .output()
        .expect("record an immutable review");
    assert!(
        record_review.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&record_review.stderr)
    );
    assert!(tree.join("reviews/wiki-review-1.json").is_file());
    assert_eq!(
        graphoxide_extract::registry::RegistrySnapshot::load(&tree)
            .expect("recorded review keeps registry valid")
            .reviews()
            .len(),
        1
    );

    let retire = graphoxide()
        .args(["registry", "retire", "--tree"])
        .arg(&tree)
        .args(["--source-id", SOURCE_ID])
        .output()
        .expect("retire source");
    assert!(retire.status.success());
    let restore = graphoxide()
        .args(["registry", "restore", "--tree"])
        .arg(&tree)
        .args(["--source-id", SOURCE_ID, "--capture-id", &second_capture])
        .output()
        .expect("restore source");
    assert!(
        restore.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    let resolve = graphoxide()
        .args(["registry", "resolve-source", "--tree"])
        .arg(&tree)
        .args(["--source-id", SOURCE_ID, "--choose", &first_capture])
        .output()
        .expect("resolve source head");
    assert!(
        resolve.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&resolve.stderr)
    );
    let resolved = graphoxide_extract::registry::RegistrySnapshot::load(&tree)
        .expect("resolved registry stays valid");
    assert_eq!(
        resolved.sources()[SOURCE_ID].active_capture_id.as_deref(),
        Some(first_capture.as_str())
    );
    assert_eq!(resolved.captures().len(), 2);
    let registry_text = fs::read_to_string(tree.join("registry.json")).expect("registry header");
    assert!(!registry_text.contains("fake-only-password"));
}

#[test]
fn changed_scan_hashes_only_the_first_pending_source_observation() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let tree = fixture.path().join("registry");
    let raw_root = fixture.path().join("raw");
    let cache_home = fixture.path().join("cache");
    let source = raw_root.join("equipment/defaults.yaml");
    fs::create_dir_all(source.parent().expect("source parent")).expect("create source parent");
    fs::write(
        &source,
        "default_username: admin\ndefault_password: fake-only-password\n",
    )
    .expect("write source");

    for args in [
        vec![
            "registry",
            "init",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--catalog-id",
            "demo-catalog",
        ],
        vec![
            "registry",
            "origin",
            "add",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--origin-id",
            "team-docs",
            "--kind",
            "filesystem",
            "--logical-name",
            "team-docs",
        ],
        vec![
            "registry",
            "track",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--origin-id",
            "team-docs",
            "--source-id",
            SOURCE_ID,
            "--relative-path",
            "equipment/defaults.yaml",
        ],
        vec![
            "registry",
            "origin",
            "bind",
            "--tree",
            tree.to_str().expect("UTF-8 tree"),
            "--origin-id",
            "team-docs",
            "--local-root",
            raw_root.to_str().expect("UTF-8 raw root"),
        ],
    ] {
        let output = graphoxide()
            .args(args)
            .env("XDG_CACHE_HOME", &cache_home)
            .output()
            .expect("set up registry scan");
        assert!(
            output.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let first = graphoxide()
        .args(["registry", "scan", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--mode", "changed", "--json"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("run first changed scan");
    assert!(
        first.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).expect("first scan JSON");
    assert_eq!(first["hashed"], 1);
    assert_eq!(first["queued"], 1);

    let second = graphoxide()
        .args(["registry", "scan", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--mode", "changed", "--json"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("run second changed scan");
    assert!(
        second.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second: Value = serde_json::from_slice(&second.stdout).expect("second scan JSON");
    assert_eq!(second["hashed"], 0);
    assert_eq!(second["unchanged"], 1);

    let snapshot = graphoxide_extract::registry::RegistrySnapshot::load(&tree)
        .expect("scan must not mutate Git registry");
    assert!(snapshot.active_captures().is_empty());
}

#[test]
fn index_binds_only_active_registry_sources_and_preserves_catalog_citation() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let tree = fixture.path().join("registry");
    let raw_root = fixture.path().join("raw");
    let cache_home = fixture.path().join("cache");
    let tracked = raw_root.join("equipment/defaults.md");
    fs::create_dir_all(tracked.parent().expect("source parent")).expect("create source parent");
    let bytes =
        b"# Equipment defaults\n\ndefault_username: admin\ndefault_password: fake-only-password\n";
    fs::write(&tracked, bytes).expect("write tracked source");
    fs::write(raw_root.join("untracked.md"), "# Must not be indexed\n")
        .expect("write untracked source");
    registry_for_source(
        &tree,
        "equipment/defaults.md",
        &hex::encode(Sha256::digest(bytes)),
    );

    let bind = graphoxide()
        .args(["registry", "origin", "bind", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--local-root"])
        .arg(&raw_root)
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("bind origin");
    assert!(
        bind.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&bind.stderr)
    );

    let index = graphoxide()
        .arg("index")
        .arg(&raw_root)
        .args([
            "--registry",
            tree.to_str().expect("UTF-8 registry"),
            "--registry-origin",
            "team-docs",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
            "--progress",
            "never",
        ])
        .env("XDG_CACHE_HOME", &cache_home)
        .env("RAYON_NUM_THREADS", "1")
        .env("TOKIO_WORKER_THREADS", "1")
        .output()
        .expect("index bound registry origin");
    assert!(
        index.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&index.stderr)
    );

    let graph: Value = serde_json::from_slice(
        &fs::read(raw_root.join("graphoxide-out/graph.json")).expect("read graph"),
    )
    .expect("graph JSON");
    let nodes = graph["nodes"].as_array().expect("graph nodes");
    assert!(nodes.iter().any(|node| {
        node["source_file"] == "equipment/defaults.md" && node["catalog"]["source_id"] == SOURCE_ID
    }));
    assert!(!nodes
        .iter()
        .any(|node| node["source_file"] == "untracked.md"));
    let build_config: Value = serde_json::from_slice(
        &fs::read(raw_root.join("graphoxide-out/.graphoxide_build.json"))
            .expect("read managed build config"),
    )
    .expect("managed build config JSON");
    assert_eq!(
        build_config["registry_binding"]["catalog_id"],
        "demo-catalog"
    );
    assert_eq!(build_config["registry_binding"]["origin_id"], "team-docs");
    assert!(build_config["registry_binding"]["tree_sha256"]
        .as_str()
        .is_some_and(|digest| digest.len() == 64));
    assert!(
        !build_config
            .to_string()
            .contains(&raw_root.display().to_string()),
        "the managed config must retain registry identity, not a local raw-source location"
    );

    let unbound_update = graphoxide()
        .arg("update")
        .arg(&raw_root)
        .args([
            "--no-cluster",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
            "--progress",
            "never",
        ])
        .env("XDG_CACHE_HOME", &cache_home)
        .env("RAYON_NUM_THREADS", "1")
        .env("TOKIO_WORKER_THREADS", "1")
        .output()
        .expect("refuse unbound update after registry-bound index");
    assert!(
        !unbound_update.status.success(),
        "an unbound update must not replace an indexed registry-bound graph"
    );
    assert!(
        String::from_utf8_lossy(&unbound_update.stderr).contains("registry-bound"),
        "stderr:\n{}",
        String::from_utf8_lossy(&unbound_update.stderr)
    );
}

#[test]
fn update_binds_registry_sources_and_rejects_an_unrecorded_change() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let tree = fixture.path().join("registry");
    let raw_root = fixture.path().join("raw");
    let cache_home = fixture.path().join("cache");
    let tracked = raw_root.join("equipment/defaults.md");
    fs::create_dir_all(tracked.parent().expect("source parent")).expect("create source parent");
    let bytes =
        b"# Equipment defaults\n\ndefault_username: admin\ndefault_password: fake-only-password\n";
    fs::write(&tracked, bytes).expect("write tracked source");
    fs::write(raw_root.join("untracked.md"), "# Must not be indexed\n")
        .expect("write untracked source");
    registry_for_source(
        &tree,
        "equipment/defaults.md",
        &hex::encode(Sha256::digest(bytes)),
    );

    let bind = graphoxide()
        .args(["registry", "origin", "bind", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--local-root"])
        .arg(&raw_root)
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("bind origin");
    assert!(
        bind.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&bind.stderr)
    );

    let first = graphoxide()
        .arg("update")
        .arg(&raw_root)
        .args([
            "--registry",
            tree.to_str().expect("UTF-8 registry"),
            "--registry-origin",
            "team-docs",
            "--no-cluster",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
            "--progress",
            "never",
        ])
        .env("XDG_CACHE_HOME", &cache_home)
        .env("RAYON_NUM_THREADS", "1")
        .env("TOKIO_WORKER_THREADS", "1")
        .output()
        .expect("update bound registry origin");
    assert!(
        first.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let graph_path = raw_root.join("graphoxide-out/graph.json");
    let accepted_graph = fs::read(&graph_path).expect("accepted bound graph");
    let graph: Value = serde_json::from_slice(&accepted_graph).expect("graph JSON");
    let nodes = graph["nodes"].as_array().expect("graph nodes");
    assert!(nodes.iter().any(|node| {
        node["source_file"] == "equipment/defaults.md" && node["catalog"]["source_id"] == SOURCE_ID
    }));
    assert!(!nodes
        .iter()
        .any(|node| node["source_file"] == "untracked.md"));

    let unbound = graphoxide()
        .arg("update")
        .arg(&raw_root)
        .args([
            "--no-cluster",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
            "--progress",
            "never",
        ])
        .env("XDG_CACHE_HOME", &cache_home)
        .env("RAYON_NUM_THREADS", "1")
        .env("TOKIO_WORKER_THREADS", "1")
        .output()
        .expect("refuse an unbound update after a registry-bound graph");
    assert!(
        !unbound.status.success(),
        "an unbound update must not discard persisted registry provenance"
    );
    assert!(
        String::from_utf8_lossy(&unbound.stderr).contains("registry-bound"),
        "stderr:\n{}",
        String::from_utf8_lossy(&unbound.stderr)
    );
    assert_eq!(
        fs::read(&graph_path).expect("graph after unbound update"),
        accepted_graph,
        "an unbound update must not replace the accepted registry-bound graph"
    );

    fs::write(
        &tracked,
        "# Equipment defaults\n\ndefault_username: admin\ndefault_password: changed-without-capture\n",
    )
    .expect("change raw source without registry capture");
    let rejected = graphoxide()
        .arg("update")
        .arg(&raw_root)
        .args([
            "--registry",
            tree.to_str().expect("UTF-8 registry"),
            "--registry-origin",
            "team-docs",
            "--no-cluster",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
            "--progress",
            "never",
        ])
        .env("XDG_CACHE_HOME", &cache_home)
        .env("RAYON_NUM_THREADS", "1")
        .env("TOKIO_WORKER_THREADS", "1")
        .output()
        .expect("reject changed registry source");
    assert!(
        !rejected.status.success(),
        "registry mismatch must reject publication"
    );
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("catalog sha256"),
        "stderr:\n{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(
        fs::read(&graph_path).expect("graph after rejected update"),
        accepted_graph,
        "a changed but uncaptured source must not overwrite the accepted graph"
    );
}

#[test]
fn materialize_publishes_source_ready_pages_from_a_pinned_registry_commit() {
    let fixture = tempfile::tempdir().expect("temporary fixture");
    let tree = fixture.path().join("registry");
    let raw_root = fixture.path().join("raw");
    let cache_home = fixture.path().join("cache");
    let source = raw_root.join("equipment/defaults.md");
    fs::create_dir_all(source.parent().expect("source parent")).expect("create source parent");
    let bytes =
        b"# Equipment defaults\n\ndefault_username: admin\ndefault_password: fake-only-password\n";
    fs::write(&source, bytes).expect("write tracked source");
    registry_for_source(
        &tree,
        "equipment/defaults.md",
        &hex::encode(Sha256::digest(bytes)),
    );
    let revision = commit_registry(&tree);

    let bind = graphoxide()
        .args(["registry", "origin", "bind", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--local-root"])
        .arg(&raw_root)
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("bind materialization origin");
    assert!(
        bind.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&bind.stderr)
    );

    let index = graphoxide()
        .arg("index")
        .arg(&raw_root)
        .args([
            "--registry",
            tree.to_str().expect("UTF-8 registry"),
            "--registry-origin",
            "team-docs",
            "--io-workers",
            "1",
            "--compute-workers",
            "1",
            "--progress",
            "never",
        ])
        .env("XDG_CACHE_HOME", &cache_home)
        .env("RAYON_NUM_THREADS", "1")
        .env("TOKIO_WORKER_THREADS", "1")
        .output()
        .expect("index bound registry origin");
    assert!(
        index.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&index.stderr)
    );

    let materialize_graph = raw_root.join("materialize-graph.json");
    write_materialize_graph(
        &materialize_graph,
        CAPTURE_ID,
        &hex::encode(Sha256::digest(bytes)),
        "2026-08-27T12:34:56Z",
        "default_username: admin\ndefault_password: fake-only-password",
    );

    let plan = raw_root.join("wiki-plan.json");
    write_json(
        &plan,
        json!({
            "version": 1,
            "domains": [{"id": "equipment", "title": "Equipment", "slug": "equipment"}],
            "sources": [{
                "id": format!("{SOURCE_ID}#{CAPTURE_ID}"),
                "title": "Equipment defaults",
                "slug": "equipment-defaults",
                "domain": "equipment",
                "coverage": "partial"
            }],
            "articles": [{
                "id": "defaults",
                "title": "Equipment defaults overview",
                "slug": "defaults",
                "domain": "equipment",
                "article_type": "reference",
                "sources": [format!("{SOURCE_ID}#{CAPTURE_ID}")],
                "aliases": [],
                "related": []
            }]
        }),
    );
    let output = fixture.path().join("wiki");
    let materialize = graphoxide()
        .args([
            "wiki",
            "materialize",
            "--registry-repo",
            tree.to_str().expect("UTF-8 registry"),
            "--registry-rev",
            &revision,
            "--origin",
            "team-docs",
            "--graph",
        ])
        .arg(&materialize_graph)
        .args(["--plan"])
        .arg(&plan)
        .args(["--output"])
        .arg(&output)
        .args(["--agent-jobs", "1", "--progress", "jsonl"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("materialize live wiki");
    assert!(
        materialize.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&materialize.stderr)
    );
    assert!(String::from_utf8_lossy(&materialize.stderr).contains("\"event\":\"source-ready\""));
    assert!(output.join("sources/equipment-defaults.md").is_file());
    assert!(output.join("wiki-manifest.json").is_file());
    assert!(output.join("search.json").is_file());
    assert!(output.join("llms.txt").is_file());
    let manifest: Value = serde_json::from_slice(
        &fs::read(output.join("wiki-manifest.json")).expect("read manifest"),
    )
    .expect("parse manifest");
    assert_eq!(manifest["registry"]["git_commit"], revision);
    assert_eq!(manifest["sources"][0]["state"], "source-ready");
    assert!(manifest["pages"].as_array().is_some_and(|pages| {
        pages.iter().any(|page| {
            page["path"] == "equipment/defaults--defaults.md" && page["state"] == "source-ready"
        })
    }));
    let references = fs::read_dir(output.join("references"))
        .expect("read reference directory")
        .map(|entry| {
            fs::read_to_string(entry.expect("reference entry").path()).expect("read reference")
        })
        .collect::<Vec<_>>();
    assert!(references
        .iter()
        .any(|reference| reference.contains("fake-only-password")));

    let article_path = output.join("equipment/defaults--defaults.md");
    let article = fs::read_to_string(&article_path).expect("read canonical article");
    let evidence_id = references
        .iter()
        .flat_map(|reference| reference.lines())
        .find_map(|line| line.strip_prefix("- Evidence block: `"))
        .and_then(|line| line.strip_suffix('`'))
        .expect("canonical reference evidence ID");
    let synthesis = format!(
        "\n## Generated details\n\nThe configured default username is documented.\n\nEvidence blocks: `{evidence_id}`\n"
    );
    let draft_marker = format!(
        "\n<!-- graphoxide-draft sha256={} -->\n{synthesis}",
        hex::encode(Sha256::digest(synthesis.as_bytes()))
    );
    let heading_end = article
        .find("# Equipment defaults overview\n")
        .expect("canonical article heading")
        + "# Equipment defaults overview\n".len();
    let mut draft = article;
    draft.insert_str(heading_end, &draft_marker);
    let drafts = fixture.path().join("drafts");
    fs::create_dir_all(drafts.join("equipment")).expect("create canonical draft directory");
    fs::write(drafts.join("equipment/defaults--defaults.md"), draft)
        .expect("write canonical draft");

    let materialize_draft = graphoxide()
        .args([
            "wiki",
            "materialize",
            "--registry-repo",
            tree.to_str().expect("UTF-8 registry"),
            "--registry-rev",
            &revision,
            "--origin",
            "team-docs",
            "--graph",
        ])
        .arg(&materialize_graph)
        .args(["--plan"])
        .arg(&plan)
        .args(["--output"])
        .arg(&output)
        .args(["--drafts"])
        .arg(&drafts)
        .args(["--agent-jobs", "1", "--progress", "never"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("materialize canonical draft");
    assert!(
        materialize_draft.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&materialize_draft.stderr)
    );
    let drafted_manifest: Value = serde_json::from_slice(
        &fs::read(output.join("wiki-manifest.json")).expect("read drafted manifest"),
    )
    .expect("parse drafted manifest");
    assert!(drafted_manifest["pages"].as_array().is_some_and(|pages| {
        pages.iter().any(|page| {
            page["path"] == "equipment/defaults--defaults.md" && page["state"] == "draft-ready"
        })
    }));
    assert!(fs::read_to_string(&article_path)
        .expect("read materialized canonical draft")
        .contains("<!-- graphoxide-draft sha256="));

    let capture_set_sha256 = hex::encode(Sha256::digest(
        format!(
            "{SOURCE_ID}\t{CAPTURE_ID}\t{}\n",
            hex::encode(Sha256::digest(bytes))
        )
        .as_bytes(),
    ));
    write_json(
        &tree.join("reviews/equipment-defaults-approved.json"),
        json!({
            "version": 1,
            "review_id": "equipment-defaults-approved",
            "decision": "approved",
            "reviewer": "wiki-reviewer",
            "reviewed_at": "2026-08-27T12:40:00Z",
            "plan_sha256": hex::encode(Sha256::digest(fs::read(&plan).expect("read reviewed plan"))),
            "capture_set_sha256": capture_set_sha256,
            "draft_sha256": reviewable_draft_sha256(&fs::read_to_string(&article_path).expect("read reviewed draft"))
        }),
    );
    let reviewed_revision = commit_registry_changes(&tree, "approve canonical draft");
    let reviewed = graphoxide()
        .args([
            "wiki",
            "materialize",
            "--registry-repo",
            tree.to_str().expect("UTF-8 registry"),
            "--registry-rev",
            &reviewed_revision,
            "--origin",
            "team-docs",
            "--graph",
        ])
        .arg(&materialize_graph)
        .args(["--plan"])
        .arg(&plan)
        .args(["--output"])
        .arg(&output)
        .args(["--drafts"])
        .arg(&drafts)
        .args(["--agent-jobs", "1", "--progress", "never"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("materialize approved canonical draft");
    assert!(
        reviewed.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&reviewed.stderr)
    );
    let reviewed_manifest: Value = serde_json::from_slice(
        &fs::read(output.join("wiki-manifest.json")).expect("read reviewed manifest"),
    )
    .expect("parse reviewed manifest");
    assert!(reviewed_manifest["pages"].as_array().is_some_and(|pages| {
        pages.iter().any(|page| {
            page["path"] == "equipment/defaults--defaults.md" && page["state"] == "reviewed-ready"
        })
    }));

    fs::remove_file(&source).expect("remove locally unavailable source");
    let unavailable = graphoxide()
        .args(["registry", "scan", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "team-docs", "--mode", "changed", "--json"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("scan unavailable source");
    assert!(
        unavailable.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&unavailable.stderr)
    );
    let unavailable: Value = serde_json::from_slice(&unavailable.stdout).expect("scan JSON");
    assert_eq!(unavailable["missing"], 1);

    let stale = graphoxide()
        .args([
            "wiki",
            "materialize",
            "--registry-repo",
            tree.to_str().expect("UTF-8 registry"),
            "--registry-rev",
            &reviewed_revision,
            "--origin",
            "team-docs",
            "--graph",
        ])
        .arg(&materialize_graph)
        .args(["--plan"])
        .arg(&plan)
        .args(["--output"])
        .arg(&output)
        .args(["--agent-jobs", "1", "--progress", "never"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("materialize unavailable source");
    assert!(
        stale.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&stale.stderr)
    );
    let stale: Value = serde_json::from_slice(
        &fs::read(output.join("wiki-manifest.json")).expect("read stale manifest"),
    )
    .expect("parse stale manifest");
    assert_eq!(stale["sources"][0]["state"], "stale");
    assert!(stale["pages"]
        .as_array()
        .is_some_and(|pages| { pages.iter().any(|page| page["state"] == "stale") }));

    let next_capture = "capture-20260828";
    let next_bytes =
        b"# Equipment defaults\n\ndefault_username: admin\ndefault_password: next-fake-password\n";
    fs::write(&source, next_bytes).expect("update tracked source");
    let next_sha256 = hex::encode(Sha256::digest(next_bytes));
    let shard = shard_for_source_id(SOURCE_ID);
    write_json(
        &tree.join(format!("sources/{shard}/{SOURCE_ID}.json")),
        json!({
            "active_capture_id": next_capture,
            "origin_id": "team-docs",
            "relative_path": "equipment/defaults.md",
            "source_id": SOURCE_ID,
            "state": "active",
            "version": 1
        }),
    );
    write_json(
        &tree.join(format!("captures/{shard}/{SOURCE_ID}/{next_capture}.json")),
        json!({
            "capture_id": next_capture,
            "observed_at": "2026-08-28T12:34:56Z",
            "relative_path": "equipment/defaults.md",
            "representation": "markdown",
            "sha256": next_sha256,
            "source_id": SOURCE_ID,
            "version": 1
        }),
    );
    let next_revision = commit_registry_changes(&tree, "updated capture");
    write_materialize_graph(
        &materialize_graph,
        next_capture,
        &next_sha256,
        "2026-08-28T12:34:56Z",
        "default_username: admin\ndefault_password: next-fake-password",
    );
    write_json(
        &plan,
        json!({
            "version": 1,
            "domains": [{"id": "equipment", "title": "Equipment", "slug": "equipment"}],
            "sources": [{
                "id": format!("{SOURCE_ID}#{next_capture}"),
                "title": "Equipment defaults",
                "slug": "equipment-defaults",
                "domain": "equipment",
                "coverage": "partial"
            }],
            "articles": [{
                "id": "defaults",
                "title": "Equipment defaults overview",
                "slug": "defaults",
                "domain": "equipment",
                "article_type": "reference",
                "sources": [format!("{SOURCE_ID}#{next_capture}")],
                "aliases": [],
                "related": []
            }]
        }),
    );
    let rematerialize = graphoxide()
        .args([
            "wiki",
            "materialize",
            "--registry-repo",
            tree.to_str().expect("UTF-8 registry"),
            "--registry-rev",
            &next_revision,
            "--origin",
            "team-docs",
            "--graph",
        ])
        .arg(&materialize_graph)
        .args(["--plan"])
        .arg(&plan)
        .args(["--output"])
        .arg(&output)
        .args(["--agent-jobs", "1", "--progress", "never"])
        .env("XDG_CACHE_HOME", &cache_home)
        .output()
        .expect("rematerialize changed capture");
    assert!(
        rematerialize.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&rematerialize.stderr)
    );
    let current_references = fs::read_dir(output.join("references"))
        .expect("read current reference directory")
        .map(|entry| {
            fs::read_to_string(entry.expect("reference entry").path()).expect("read reference")
        })
        .collect::<Vec<_>>();
    assert_eq!(current_references.len(), 1);
    assert!(current_references[0].contains("next-fake-password"));
    let manifest: Value = serde_json::from_slice(
        &fs::read(output.join("wiki-manifest.json")).expect("read updated manifest"),
    )
    .expect("parse updated manifest");
    assert!(manifest["historical"]
        .as_array()
        .is_some_and(|pages| !pages.is_empty()));
    let historical = manifest["historical"]
        .as_array()
        .expect("historical page list")
        .iter()
        .find(|page| {
            page["path"]
                .as_str()
                .is_some_and(|path| path.starts_with("references/"))
        })
        .and_then(|page| page["archived_path"].as_str())
        .expect("historical path");
    assert!(fs::read_to_string(output.join(historical))
        .expect("read archived page")
        .contains("fake-only-password"));
}
