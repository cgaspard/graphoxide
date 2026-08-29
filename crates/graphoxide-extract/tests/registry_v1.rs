use graphoxide_extract::registry::{
    activate_capture, add_origin, append_capture_and_activate, initialize_tree, retire_source,
    shard_for_source_id, RegistryCapture, RegistryOrigin, RegistrySnapshot, RegistrySourceState,
};
use serde_json::json;
use std::{fs, path::Path};

const SOURCE_ID: &str = "equipment-guide";
const CAPTURE_ID: &str = "capture-20260827";

fn write_json(path: &Path, value: serde_json::Value) {
    fs::create_dir_all(path.parent().expect("parent directory")).expect("create parent");
    fs::write(
        path,
        format!(
            "{}\n",
            serde_json::to_string(&value).expect("canonical JSON")
        ),
    )
    .expect("write registry record");
}

fn write_valid_registry(root: &Path) {
    write_json(
        &root.join("registry.json"),
        json!({"catalog_id": "demo-catalog", "version": 1}),
    );
    write_json(
        &root.join("origins/team-docs.json"),
        json!({
            "kind": "filesystem",
            "logical_name": "team-docs",
            "origin_id": "team-docs",
            "version": 1
        }),
    );
    let shard = shard_for_source_id(SOURCE_ID);
    write_json(
        &root.join(format!("sources/{shard}/{SOURCE_ID}.json")),
        json!({
            "active_capture_id": CAPTURE_ID,
            "origin_id": "team-docs",
            "relative_path": "equipment/defaults.md",
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
            "relative_path": "equipment/defaults.md",
            "representation": "markdown",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_id": SOURCE_ID,
            "version": 1
        }),
    );
}

#[test]
fn loads_canonical_metadata_only_registry_with_capture_closure() {
    let fixture = tempfile::tempdir().expect("temporary registry");
    write_valid_registry(fixture.path());

    let snapshot = RegistrySnapshot::load(fixture.path()).expect("valid registry");

    assert_eq!(snapshot.catalog_id(), "demo-catalog");
    assert_eq!(snapshot.sources().len(), 1);
    assert_eq!(snapshot.captures().len(), 1);
    assert_eq!(snapshot.active_captures().len(), 1);
    assert_eq!(
        snapshot.active_captures()[0].relative_path(),
        "equipment/defaults.md"
    );
}

#[test]
fn rejects_noncanonical_json_and_wrong_source_shards() {
    let fixture = tempfile::tempdir().expect("temporary registry");
    write_valid_registry(fixture.path());

    fs::write(
        fixture.path().join("registry.json"),
        "{\"version\":1, \"catalog_id\":\"demo-catalog\"}\n",
    )
    .expect("write noncanonical registry");
    let error = RegistrySnapshot::load(fixture.path()).expect_err("noncanonical JSON rejected");
    assert!(format!("{error:#}").contains("canonical JSON"));

    write_valid_registry(fixture.path());
    let shard = shard_for_source_id(SOURCE_ID);
    let source = fixture
        .path()
        .join(format!("sources/{shard}/{SOURCE_ID}.json"));
    let wrong = fixture.path().join(format!("sources/ff/{SOURCE_ID}.json"));
    fs::create_dir_all(wrong.parent().expect("wrong shard parent")).expect("create wrong shard");
    fs::rename(source, wrong).expect("move source to wrong shard");

    let error = RegistrySnapshot::load(fixture.path()).expect_err("wrong shard rejected");
    assert!(format!("{error:#}").contains("source shard"));
}

#[test]
fn rejects_duplicate_json_keys_before_schema_deserialization() {
    let fixture = tempfile::tempdir().expect("temporary registry");
    write_valid_registry(fixture.path());
    fs::write(
        fixture.path().join("registry.json"),
        "{\"catalog_id\":\"demo-catalog\",\"catalog_id\":\"other\",\"version\":1}\n",
    )
    .expect("write duplicate registry key");

    let error = RegistrySnapshot::load(fixture.path()).expect_err("duplicate key rejected");
    assert!(format!("{error:#}").contains("duplicate JSON object key"));
}

#[test]
fn lifecycle_keeps_an_interrupted_first_track_valid_and_preserves_history() {
    let fixture = tempfile::tempdir().expect("temporary registry");
    let root = fixture.path().join("registry");
    initialize_tree(&root, "demo-catalog").expect("initialize registry");
    add_origin(
        &root,
        RegistryOrigin {
            version: 1,
            origin_id: "team-docs".to_owned(),
            kind: "filesystem".to_owned(),
            logical_name: "team-docs".to_owned(),
        },
    )
    .expect("add origin");
    let capture = RegistryCapture {
        version: 1,
        capture_id: CAPTURE_ID.to_owned(),
        source_id: SOURCE_ID.to_owned(),
        relative_path: "equipment/defaults.md".to_owned(),
        sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        observed_at: "2026-08-27T12:34:56Z".to_owned(),
        representation: "markdown".to_owned(),
    };
    let active = append_capture_and_activate(&root, capture.clone(), Some("team-docs"))
        .expect("append first capture");
    assert_eq!(active.active_captures().len(), 1);

    let retired = retire_source(&root, SOURCE_ID).expect("retire source");
    assert_eq!(retired.active_captures().len(), 0);
    assert_eq!(
        retired.sources()[SOURCE_ID].state,
        RegistrySourceState::Retired
    );

    let restored = activate_capture(&root, SOURCE_ID, CAPTURE_ID).expect("restore capture");
    assert_eq!(
        restored.active_captures()[0].capture().sha256,
        capture.sha256
    );
}
