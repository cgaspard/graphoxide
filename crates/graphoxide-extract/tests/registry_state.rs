use graphoxide_extract::registry_state::{
    scan_bound_file, stat_bound_file, Availability, QueueReason, RegistryLocalState,
    ScanDisposition, SourceObservation,
};
use sha2::{Digest as _, Sha256};
use std::path::Path;

fn observation(mtime_ns: i64, sha256: Option<&str>) -> SourceObservation {
    SourceObservation {
        availability: Availability::Available,
        size_bytes: Some(64),
        mtime_ns: Some(mtime_ns),
        ctime_ns: Some(mtime_ns),
        sha256: sha256.map(str::to_owned),
    }
}

#[test]
fn local_state_is_metadata_only_rebuildable_and_uses_the_xdg_layout() {
    let fixture = tempfile::tempdir().expect("temporary cache home");
    let cache = RegistryLocalState::open_in(
        fixture.path(),
        "demo-catalog",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .expect("open local state");

    assert_eq!(
        cache.path(),
        fixture
            .path()
            .join("graphoxide/catalogs/demo-catalog/registry.sqlite3")
    );
    cache
        .bind_origin("team-docs", Path::new("/srv/docs"))
        .expect("bind local origin");
    assert_eq!(
        cache.origin_binding("team-docs").expect("read binding"),
        Some("/srv/docs".to_owned())
    );

    assert_eq!(
        cache
            .scan_disposition(
                "source-a",
                "team-docs",
                "equipment/defaults.yaml",
                &observation(1, None)
            )
            .expect("first scan"),
        ScanDisposition::HashRequired
    );
    cache
        .record_observation(
            "source-a",
            "team-docs",
            "equipment/defaults.yaml",
            &observation(
                1,
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ),
        )
        .expect("record source observation");
    assert_eq!(
        cache
            .scan_disposition(
                "source-a",
                "team-docs",
                "equipment/defaults.yaml",
                &observation(1, None)
            )
            .expect("unchanged scan"),
        ScanDisposition::Unchanged
    );
    assert_eq!(
        cache
            .scan_disposition(
                "source-a",
                "team-docs",
                "equipment/defaults.yaml",
                &observation(2, None)
            )
            .expect("changed stat scan"),
        ScanDisposition::HashRequired
    );

    let reopened = RegistryLocalState::open_in(
        fixture.path(),
        "demo-catalog",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .expect("open state for another registry revision");
    assert_eq!(
        reopened
            .scan_disposition(
                "source-a",
                "team-docs",
                "equipment/defaults.yaml",
                &observation(1, None)
            )
            .expect("revision preserves unchanged source fingerprint"),
        ScanDisposition::Unchanged
    );
    assert_eq!(
        reopened
            .scan_disposition(
                "source-a",
                "team-docs",
                "equipment/renamed.yaml",
                &observation(1, None)
            )
            .expect("path change invalidates cached fingerprint"),
        ScanDisposition::HashRequired
    );
    reopened
        .bind_origin("team-docs", Path::new("/srv/other-docs"))
        .expect("rebind origin");
    assert_eq!(
        reopened
            .scan_disposition(
                "source-a",
                "team-docs",
                "equipment/defaults.yaml",
                &observation(1, None)
            )
            .expect("origin rebind invalidates cached fingerprint"),
        ScanDisposition::HashRequired
    );
    assert_eq!(
        reopened
            .origin_binding("team-docs")
            .expect("preserved binding"),
        Some("/srv/other-docs".to_owned())
    );
}

#[test]
fn queue_order_is_deterministic_and_never_uses_priority_to_skip_changed_work() {
    let fixture = tempfile::tempdir().expect("temporary cache home");
    let cache = RegistryLocalState::open_in(
        fixture.path(),
        "demo-catalog",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .expect("open local state");
    cache
        .enqueue("source-z", "materialize", QueueReason::Expired, 999, 3)
        .expect("queue expired");
    cache
        .enqueue("source-c", "materialize", QueueReason::Changed, 1, 4)
        .expect("queue changed");
    cache
        .enqueue("source-a", "materialize", QueueReason::Manual, 0, 5)
        .expect("queue manual");
    cache
        .enqueue("source-b", "materialize", QueueReason::Changed, 99, 6)
        .expect("queue changed with tag priority");

    let ordered = (0..4)
        .map(|now| {
            let item = cache
                .claim_next("test-runner", now, now + 10)
                .expect("dequeue")
                .expect("queued item");
            cache.complete(&item, "test-runner").expect("complete item");
            item
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ordered
            .iter()
            .map(|item| (item.source_id.as_str(), item.reason))
            .collect::<Vec<_>>(),
        vec![
            ("source-a", QueueReason::Manual),
            ("source-b", QueueReason::Changed),
            ("source-c", QueueReason::Changed),
            ("source-z", QueueReason::Expired),
        ]
    );
}

#[test]
fn scanner_hashes_the_original_source_without_retaining_credential_bearing_bytes() {
    let fixture = tempfile::tempdir().expect("temporary source root");
    let source = fixture.path().join("equipment/defaults.yaml");
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("create source parent");
    let bytes = b"default_username: admin\ndefault_password: fake-only-password\n";
    std::fs::write(&source, bytes).expect("write source");

    let stat = stat_bound_file(fixture.path(), "equipment/defaults.yaml").expect("stat source");
    assert_eq!(stat.availability, Availability::Available);
    assert_eq!(stat.sha256, None);

    let observation =
        scan_bound_file(fixture.path(), "equipment/defaults.yaml").expect("scan original source");
    let expected_sha256 = hex::encode(Sha256::digest(bytes));
    assert_eq!(observation.availability, Availability::Available);
    assert_eq!(observation.size_bytes, Some(bytes.len() as u64));
    assert_eq!(
        observation.sha256.as_deref(),
        Some(expected_sha256.as_str())
    );

    let missing =
        scan_bound_file(fixture.path(), "equipment/missing.yaml").expect("scan missing source");
    assert_eq!(missing.availability, Availability::Missing);
    assert_eq!(missing.sha256, None);
}
