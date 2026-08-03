use graphoxide_core::{validate::validate_extraction, Confidence, Extraction};
use graphoxide_extract::cargo_introspect::{introspect_cargo, CargoIntrospectionError};
use std::{collections::BTreeSet, fs, path::Path};
use tempfile::TempDir;

fn write_manifest(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text.trim_start()).unwrap();
}

fn node_ids(result: &Extraction) -> BTreeSet<&str> {
    result.nodes.iter().map(|node| node.id.as_str()).collect()
}

fn has_cargo_edge(result: &Extraction, source: &str, target: &str, source_file: &str) -> bool {
    result.edges.iter().any(|edge| {
        edge.source == source
            && edge.target == target
            && edge.relation == "crate_depends_on"
            && edge.confidence == Confidence::Extracted
            && edge.source_file == source_file
            && edge.extra.get("context").and_then(|value| value.as_str())
                == Some("cargo_dependency")
            && edge.extra.get("weight").and_then(|value| value.as_f64()) == Some(1.0)
            && edge
                .extra
                .get("source_location")
                .and_then(|value| value.as_str())
                == Some("L1")
    })
}

#[test]
fn test_cargo_introspect_workspace_internal_dependency_only() {
    let fixture = TempDir::new().unwrap();
    write_manifest(
        &fixture.path().join("Cargo.toml"),
        r#"
[workspace]
members = ["app", "core"]
"#,
    );
    write_manifest(
        &fixture.path().join("app/Cargo.toml"),
        r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
core = { path = "../core" }
serde = "1"
"#,
    );
    write_manifest(
        &fixture.path().join("core/Cargo.toml"),
        r#"
[package]
name = "core"
version = "0.1.0"
edition = "2021"
"#,
    );

    let result = introspect_cargo(fixture.path()).unwrap();
    assert_eq!(
        node_ids(&result),
        BTreeSet::from(["crate:app", "crate:core"])
    );
    let app = result
        .nodes
        .iter()
        .find(|node| node.id == "crate:app")
        .unwrap();
    assert_eq!(app.label, "app");
    assert_eq!(app.source_file, "app/Cargo.toml");
    assert_eq!(app.source_location.as_deref(), Some("L1"));
    assert!(has_cargo_edge(
        &result,
        "crate:app",
        "crate:core",
        "app/Cargo.toml"
    ));
    assert!(!result.edges.iter().any(|edge| edge.target == "crate:serde"));
    validate_extraction(&result).unwrap();
}

#[test]
fn test_cargo_introspect_malformed_toml_reports_parser_error() {
    let fixture = TempDir::new().unwrap();
    write_manifest(
        &fixture.path().join("Cargo.toml"),
        "[package\nname = \"broken\"\n",
    );
    assert!(matches!(
        introspect_cargo(fixture.path()),
        Err(CargoIntrospectionError::Toml { .. })
    ));
}

#[test]
fn test_cargo_introspect_degenerate_manifests_return_empty_or_skip_bad_deps() {
    let fixture = TempDir::new().unwrap();
    let empty = fixture.path().join("empty");
    write_manifest(&empty.join("Cargo.toml"), "");
    let result = introspect_cargo(&empty).unwrap();
    assert!(result.nodes.is_empty() && result.edges.is_empty());

    let nameless = fixture.path().join("nameless");
    write_manifest(
        &nameless.join("Cargo.toml"),
        "[package]\nversion = \"0.1.0\"\n",
    );
    let result = introspect_cargo(&nameless).unwrap();
    assert!(result.nodes.is_empty() && result.edges.is_empty());

    let scalar = fixture.path().join("scalar-dependencies");
    write_manifest(
        &scalar.join("Cargo.toml"),
        r#"
[package]
name = "app"
version = "0.1.0"

dependencies = "not-a-table"
"#,
    );
    let result = introspect_cargo(&scalar).unwrap();
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(result.nodes[0].id, "crate:app");
    assert_eq!(result.nodes[0].source_file, "Cargo.toml");
    assert!(result.edges.is_empty());
}

#[test]
fn test_cargo_introspect_old_manifest_keeps_internal_path_dep_and_skips_external() {
    let fixture = TempDir::new().unwrap();
    write_manifest(
        &fixture.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"legacy\", \"internal\"]\n",
    );
    write_manifest(
        &fixture.path().join("legacy/Cargo.toml"),
        r#"
[package]
name = "legacy"
version = "0.1.0"

[dependencies]
rand = "0.8"
internal = { path = "../internal" }
"#,
    );
    write_manifest(
        &fixture.path().join("internal/Cargo.toml"),
        "[package]\nname = \"internal\"\nversion = \"0.1.0\"\n",
    );
    let result = introspect_cargo(fixture.path()).unwrap();
    assert_eq!(
        node_ids(&result),
        BTreeSet::from(["crate:internal", "crate:legacy"])
    );
    assert_eq!(result.edges.len(), 1);
    assert!(has_cargo_edge(
        &result,
        "crate:legacy",
        "crate:internal",
        "legacy/Cargo.toml"
    ));
}

#[test]
fn test_cargo_introspect_modern_virtual_and_root_package_workspaces() {
    let fixture = TempDir::new().unwrap();
    let virtual_root = fixture.path().join("virtual");
    write_manifest(
        &virtual_root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/*"]

[workspace.dependencies]
beta = { path = "crates/beta" }
serde = "1"
"#,
    );
    write_manifest(
        &virtual_root.join("crates/alpha/Cargo.toml"),
        r#"
[package]
name = "alpha"
version = "0.1.0"
edition = "2021"

[dependencies]
beta = { workspace = true }
serde = { workspace = true }
"#,
    );
    write_manifest(
        &virtual_root.join("crates/beta/Cargo.toml"),
        "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let result = introspect_cargo(&virtual_root).unwrap();
    assert_eq!(
        node_ids(&result),
        BTreeSet::from(["crate:alpha", "crate:beta"])
    );
    assert_eq!(result.edges.len(), 1);
    assert!(has_cargo_edge(
        &result,
        "crate:alpha",
        "crate:beta",
        "crates/alpha/Cargo.toml"
    ));

    let package_root = fixture.path().join("package-root");
    write_manifest(
        &package_root.join("Cargo.toml"),
        r#"
[package]
name = "root_pkg"
version = "0.1.0"
edition = "2021"

[workspace]
members = ["crates/*"]
"#,
    );
    write_manifest(
        &package_root.join("crates/member/Cargo.toml"),
        r#"
[package]
name = "member"
version = "0.1.0"
edition = "2021"

[dependencies]
root_pkg = { path = "../.." }
"#,
    );
    let result = introspect_cargo(&package_root).unwrap();
    assert_eq!(
        node_ids(&result),
        BTreeSet::from(["crate:member", "crate:root_pkg"])
    );
    assert_eq!(result.edges.len(), 1);
    assert!(has_cargo_edge(
        &result,
        "crate:member",
        "crate:root_pkg",
        "crates/member/Cargo.toml"
    ));
}

#[test]
fn test_cargo_introspect_large_workspace_dependency_chain() {
    let fixture = TempDir::new().unwrap();
    write_manifest(
        &fixture.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    for index in 0..200 {
        let dependency = if index < 199 {
            format!(
                "\n[dependencies]\ncrate_{next:03} = {{ path = \"../crate_{next:03}\" }}\n",
                next = index + 1
            )
        } else {
            String::new()
        };
        write_manifest(
            &fixture
                .path()
                .join(format!("crates/crate_{index:03}/Cargo.toml")),
            &format!(
                "[package]\nname = \"crate_{index:03}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{dependency}"
            ),
        );
    }
    let result = introspect_cargo(fixture.path()).unwrap();
    assert_eq!(result.nodes.len(), 200);
    assert_eq!(result.edges.len(), 199);
    assert!(has_cargo_edge(
        &result,
        "crate:crate_000",
        "crate:crate_001",
        "crates/crate_000/Cargo.toml"
    ));
    assert!(has_cargo_edge(
        &result,
        "crate:crate_198",
        "crate:crate_199",
        "crates/crate_198/Cargo.toml"
    ));
    validate_extraction(&result).unwrap();
}

#[test]
fn test_cargo_introspect_honors_package_rename_on_internal_dep() {
    let fixture = TempDir::new().unwrap();
    write_manifest(
        &fixture.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"storage\"]\n",
    );
    write_manifest(
        &fixture.path().join("app/Cargo.toml"),
        r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
db = { path = "../storage", package = "internal-storage" }
"#,
    );
    write_manifest(
        &fixture.path().join("storage/Cargo.toml"),
        "[package]\nname = \"internal-storage\"\nversion = \"0.1.0\"\n",
    );
    let result = introspect_cargo(fixture.path()).unwrap();
    assert_eq!(
        node_ids(&result),
        BTreeSet::from(["crate:app", "crate:internal-storage"])
    );
    assert!(has_cargo_edge(
        &result,
        "crate:app",
        "crate:internal-storage",
        "app/Cargo.toml"
    ));
}

#[test]
fn test_cargo_introspect_package_rename_falls_through_when_unresolved() {
    let fixture = TempDir::new().unwrap();
    write_manifest(
        &fixture.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\"]\n",
    );
    write_manifest(
        &fixture.path().join("app/Cargo.toml"),
        r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
tokio_rt = { version = "1", package = "tokio" }
"#,
    );
    let result = introspect_cargo(fixture.path()).unwrap();
    assert_eq!(node_ids(&result), BTreeSet::from(["crate:app"]));
    assert!(result.edges.is_empty());
}

#[test]
fn cargo_rejects_parent_member_patterns_and_duplicate_package_names() {
    let fixture = TempDir::new().unwrap();
    write_manifest(
        &fixture.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"../outside\"]\n",
    );
    assert!(matches!(
        introspect_cargo(fixture.path()),
        Err(CargoIntrospectionError::UnsafeMemberPattern(_))
    ));

    write_manifest(
        &fixture.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\n",
    );
    for member in ["a", "b"] {
        write_manifest(
            &fixture.path().join(member).join("Cargo.toml"),
            "[package]\nname = \"duplicate\"\nversion = \"0.1.0\"\n",
        );
    }
    assert!(matches!(
        introspect_cargo(fixture.path()),
        Err(CargoIntrospectionError::DuplicatePackageName { .. })
    ));
}

#[test]
fn cargo_rejects_oversized_manifests_before_parsing() {
    let fixture = TempDir::new().unwrap();
    let oversized = "#".repeat(2 * 1024 * 1024 + 1);
    write_manifest(&fixture.path().join("Cargo.toml"), &oversized);
    assert!(matches!(
        introspect_cargo(fixture.path()),
        Err(CargoIntrospectionError::TooLarge { .. })
    ));
}
