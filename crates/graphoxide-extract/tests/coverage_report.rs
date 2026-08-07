use graphoxide_extract::{
    coverage::{audit_coverage, CoverageBoundaryKind, CoverageOptions, CoverageStatus},
    format_registry::FormatCapability,
};
use std::{collections::BTreeSet, fs, path::Path};

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture parent");
    }
    fs::write(path, bytes).expect("write fixture");
}

#[test]
fn reports_registry_capabilities_unknowns_sensitive_and_policy_outcomes() {
    let project = tempfile::tempdir().expect("temporary project");
    write(project.path(), "package.json", b"{}");
    write(project.path(), "events.jsonl", b"{\"event\":1}\n");
    write(project.path(), "service.yaml", b"service: api\n");
    write(project.path(), "document.pdf", b"%PDF-1.7\n");
    write(
        project.path(),
        "report.docx",
        b"coverage-does-not-parse-office",
    );
    write(project.path(), "Cargo.lock", b"[[package]]\n");
    write(project.path(), "package-lock.json", b"{}\n");
    write(project.path(), "opaque.zzz", b"opaque\n");
    write(project.path(), "LICENSE", b"license\n");
    write(
        project.path(),
        "runner",
        b"#!/usr/bin/env python3\nprint('ok')\n",
    );
    write(
        project.path(),
        "notes.gdoc",
        b"{\"url\":\"local-fixture\"}\n",
    );
    write(
        project.path(),
        "secrets/prod.tfvars",
        b"token = \"private\"\n",
    );
    write(project.path(), "secrets/helper.rs", b"fn helper() {}\n");
    write(project.path(), ".env", b"TOKEN=do-not-read\n");

    let report = audit_coverage(project.path(), &CoverageOptions::default()).expect("coverage");
    let file = |path: &str| {
        report
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("missing {path}"))
    };

    assert_eq!(file("package.json").status, CoverageStatus::InventoryOnly);
    assert_eq!(
        file("package.json").format_id.as_deref(),
        Some("package-manifest")
    );
    assert_eq!(
        file("events.jsonl").declared_capability,
        Some(FormatCapability::SemanticFull)
    );
    assert_eq!(file("service.yaml").status, CoverageStatus::Covered);
    assert_eq!(file("document.pdf").status, CoverageStatus::InventoryOnly);
    assert_eq!(file("report.docx").status, CoverageStatus::InventoryOnly);
    assert_eq!(file("Cargo.lock").status, CoverageStatus::ExcludedPolicy);
    assert_eq!(
        file("Cargo.lock").format_id.as_deref(),
        Some("package-manifest")
    );
    assert_eq!(
        file("package-lock.json").status,
        CoverageStatus::ExcludedPolicy
    );
    assert_eq!(file("opaque.zzz").status, CoverageStatus::Unsupported);
    assert_eq!(file("LICENSE").status, CoverageStatus::Unsupported);
    assert_eq!(file("runner").status, CoverageStatus::Covered);
    assert_eq!(file("runner").format_id.as_deref(), Some("source-code"));
    assert_eq!(file("notes.gdoc").status, CoverageStatus::ExcludedPolicy);
    assert_eq!(
        file("notes.gdoc").reason.as_deref(),
        Some("google_workspace_disabled")
    );
    assert_eq!(
        file("secrets/prod.tfvars").status,
        CoverageStatus::ExcludedSensitive
    );
    assert_eq!(file("secrets/helper.rs").status, CoverageStatus::Covered);
    assert_eq!(file(".env").status, CoverageStatus::ExcludedSensitive);
    assert!(!project.path().join("graphoxide-out/converted").exists());
    assert!(report.complete);
    assert_eq!(report.schema_version, 1);
    assert_eq!(report.strict_failure_count(), 0);
}

fn populate_deterministic_tree(root: &Path) {
    write(root, ".gitignore", b"ignored.bin\n");
    write(root, "src/main.rs", b"fn main() {}\n");
    write(root, "unknown", b"no shebang\n");
    write(root, "ignored.bin", b"ignored\n");
    write(root, "target/generated.rs", b"fn generated() {}\n");
    write(
        root,
        "graphoxide-out/memory/note.md",
        b"# retained memory\n",
    );
    write(
        root,
        "graphoxide-out/other/generated.rs",
        b"fn generated() {}\n",
    );
}

#[test]
fn reports_are_root_independent_and_boundaries_are_separate() {
    let left = tempfile::tempdir().expect("left project");
    let right = tempfile::tempdir().expect("right project");
    populate_deterministic_tree(left.path());
    populate_deterministic_tree(right.path());

    let left_report = audit_coverage(left.path(), &CoverageOptions::default()).expect("left");
    let right_report = audit_coverage(right.path(), &CoverageOptions::default()).expect("right");
    assert_eq!(
        serde_json::to_vec(&left_report).expect("left JSON"),
        serde_json::to_vec(&right_report).expect("right JSON")
    );
    assert_eq!(left_report.root, ".");
    assert!(left_report.files.iter().any(|file| {
        file.path == "graphoxide-out/memory/note.md" && file.status == CoverageStatus::Covered
    }));
    assert!(!left_report
        .files
        .iter()
        .any(|file| file.path == "ignored.bin" || file.path.starts_with("target/")));
    assert!(left_report.boundaries.iter().any(|boundary| {
        boundary.path == "ignored.bin" && boundary.kind == CoverageBoundaryKind::Ignored
    }));
    assert!(left_report.boundaries.iter().any(|boundary| {
        boundary.path == "target" && boundary.kind == CoverageBoundaryKind::PrunedNoise
    }));
    assert!(left_report
        .files
        .iter()
        .all(|file| !file.path.contains(left.path().to_string_lossy().as_ref())));
}

#[test]
fn a_regular_managed_memory_path_does_not_create_a_walk_error() {
    let project = tempfile::tempdir().expect("temporary project");
    write(project.path(), "main.rs", b"fn main() {}\n");
    write(
        project.path(),
        "graphoxide-out/memory",
        b"not a directory\n",
    );

    let report = audit_coverage(project.path(), &CoverageOptions::default()).expect("coverage");
    assert!(report.complete);
    assert!(report.walk_errors.is_empty());
    assert_eq!(report.summary.walk_errors, 0);
}

#[cfg(unix)]
#[test]
fn symlink_boundaries_and_followed_source_dedup_match_discovery_policy() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().expect("temporary project");
    let outside = tempfile::tempdir().expect("outside directory");
    write(project.path(), "source.rs", b"fn source() {}\n");
    write(outside.path(), "outside.rs", b"fn outside() {}\n");
    symlink("source.rs", project.path().join("alias.rs")).expect("in-root symlink");
    symlink(
        outside.path().join("outside.rs"),
        project.path().join("outside.rs"),
    )
    .expect("out-of-root symlink");

    let default_report =
        audit_coverage(project.path(), &CoverageOptions::default()).expect("default coverage");
    assert_eq!(
        default_report
            .files
            .iter()
            .filter(|file| file.path.ends_with(".rs"))
            .count(),
        1
    );
    assert!(default_report.boundaries.iter().any(|boundary| {
        boundary.path == "alias.rs" && boundary.reason == "symlink_not_followed"
    }));

    let followed = audit_coverage(
        project.path(),
        &CoverageOptions {
            follow_symlinks: true,
            ..CoverageOptions::default()
        },
    )
    .expect("followed coverage");
    assert_eq!(
        followed
            .files
            .iter()
            .filter(|file| file.path.ends_with(".rs"))
            .count(),
        1
    );
    assert!(followed.files.iter().any(|file| file.path == "source.rs"));
    assert!(!followed.files.iter().any(|file| file.path == "alias.rs"));
    assert!(followed.boundaries.iter().any(|boundary| {
        boundary.path == "outside.rs" && boundary.reason == "symlink_target_outside_root"
    }));
}

#[cfg(unix)]
#[test]
fn path_encoding_is_lossless_and_safe_for_human_rendering() {
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStringExt as _, fs::symlink},
    };

    let project = tempfile::tempdir().expect("temporary project");
    let malformed_supported = fs::write(
        project.path().join(OsString::from_vec(b"bad\xff".to_vec())),
        b"one",
    )
    .is_ok()
        && fs::write(
            project.path().join(OsString::from_vec(b"bad\xfe".to_vec())),
            b"two",
        )
        .is_ok();
    if malformed_supported {
        symlink(
            OsString::from_vec(b"bad\xff".to_vec()),
            project.path().join("valid-alias.rs"),
        )
        .expect("alias to malformed target");
    }
    write(project.path(), "line\nbreak\tname", b"control");
    write(project.path(), "a\\b", b"backslash");
    write(project.path(), "a/b", b"nested");

    let report = audit_coverage(project.path(), &CoverageOptions::default()).expect("coverage");
    let paths: BTreeSet<_> = report.files.iter().map(|file| file.path.as_str()).collect();
    if malformed_supported {
        assert!(paths.contains("bad%FF"));
        assert!(paths.contains("bad%FE"));
        assert!(report
            .files
            .iter()
            .filter(|file| file.path.starts_with("bad%"))
            .all(|file| file.status == CoverageStatus::ExcludedPolicy
                && file.reason.as_deref() == Some("non_unicode_path")));
    }
    assert!(paths.contains("line%0Abreak%09name"));
    assert!(paths.contains("a%5Cb"));
    assert!(paths.contains("a/b"));
    assert_eq!(paths.len(), report.files.len());
    if malformed_supported {
        let followed = audit_coverage(
            project.path(),
            &CoverageOptions {
                follow_symlinks: true,
                ..CoverageOptions::default()
            },
        )
        .expect("followed coverage");
        let alias = followed
            .files
            .iter()
            .find(|file| file.path == "valid-alias.rs")
            .expect("valid alias outcome");
        assert_eq!(alias.status, CoverageStatus::ExcludedPolicy);
        assert_eq!(alias.reason.as_deref(), Some("non_unicode_source_binding"));
    }
}

#[cfg(unix)]
#[test]
fn sensitive_files_are_classified_before_open_and_unreadable_is_strict() {
    use std::os::unix::fs::PermissionsExt as _;

    let project = tempfile::tempdir().expect("temporary project");
    write(project.path(), ".env", b"SECRET=must-not-be-reported\n");
    write(project.path(), "locked.rs", b"fn locked() {}\n");
    let secret = project.path().join(".env");
    let locked = project.path().join("locked.rs");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).expect("lock secret");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("lock source");

    let report = audit_coverage(project.path(), &CoverageOptions::default()).expect("coverage");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).expect("restore secret");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).expect("restore source");

    let secret = report
        .files
        .iter()
        .find(|file| file.path == ".env")
        .unwrap();
    assert_eq!(secret.status, CoverageStatus::ExcludedSensitive);
    assert!(!serde_json::to_string(&report)
        .expect("JSON")
        .contains("must-not-be-reported"));
    let locked = report
        .files
        .iter()
        .find(|file| file.path == "locked.rs")
        .unwrap();
    // Privileged test runners can still open mode-000 files.
    if locked.status == CoverageStatus::Unreadable {
        assert_eq!(locked.format_id.as_deref(), Some("source-code"));
        assert_eq!(report.strict_failure_count(), 1);
        assert!(!report.complete);
    }
}

#[cfg(unix)]
#[test]
fn unreadable_directory_is_a_root_relative_strict_walk_error() {
    use std::os::unix::fs::PermissionsExt as _;

    let project = tempfile::tempdir().expect("temporary project");
    write(project.path(), "blocked/hidden.rs", b"fn hidden() {}\n");
    let blocked = project.path().join("blocked");
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).expect("lock directory");

    let report = audit_coverage(project.path(), &CoverageOptions::default()).expect("coverage");
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).expect("restore directory");

    // Privileged test runners can still enumerate mode-000 directories.
    if report.summary.walk_errors > 0 {
        assert!(!report.complete);
        assert_eq!(report.strict_failure_count(), report.summary.walk_errors);
        assert_eq!(
            report.walk_errors[0].operation,
            graphoxide_extract::coverage::CoverageOperation::ReadDirectory
        );
        assert_eq!(report.walk_errors[0].path, "blocked");
        assert_eq!(
            report.walk_errors.len() + report.walk_errors_truncated,
            report.summary.walk_errors
        );
    }
}
