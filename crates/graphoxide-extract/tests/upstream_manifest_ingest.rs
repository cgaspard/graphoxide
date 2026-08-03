use graphoxide_extract::{
    detect::{classify_file, FileType},
    extract_files,
    manifest_ingest::{extract_package_manifest, is_package_manifest_path},
};
use graphoxide_graph::build_graph;
use std::{collections::BTreeSet, fs, path::Path};
use tempfile::TempDir;

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn package_node(result: &graphoxide_core::Extraction) -> &graphoxide_core::Node {
    result
        .nodes
        .iter()
        .find(|node| node.extra.get("type").and_then(|value| value.as_str()) == Some("package"))
        .expect("package node")
}

fn dependency_targets(result: &graphoxide_core::Extraction) -> BTreeSet<&str> {
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == "depends_on")
        .map(|edge| edge.target.as_str())
        .collect()
}

#[test]
fn test_manifests_classify_as_code_not_document() {
    let fixture = TempDir::new().unwrap();
    for name in ["apm.yml", "pyproject.toml", "go.mod", "pom.xml"] {
        let path = fixture.path().join(name);
        write(&path, "x");
        assert!(is_package_manifest_path(&path));
        assert_eq!(classify_file(&path), Some(FileType::Code), "{name}");
    }
    let generic_yaml = fixture.path().join("config.yaml");
    write(&generic_yaml, "a: 1");
    assert_eq!(classify_file(&generic_yaml), Some(FileType::Document));
}

#[test]
fn test_apm_parses_name_and_deps() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("apm.yml");
    write(
        &path,
        "name: my-pkg\nversion: 1.2.3\ndependencies:\n  - dep-a\n  - dep-b\n",
    );
    let result = extract_package_manifest(&path);
    let package = package_node(&result);
    assert_eq!(package.label, "my-pkg");
    assert_eq!(
        package
            .extra
            .get("version")
            .and_then(|value| value.as_str()),
        Some("1.2.3")
    );
    let dependencies = dependency_targets(&result);
    assert!(dependencies.is_superset(&BTreeSet::from(["pkg_dep_a", "pkg_dep_b"])));
}

#[test]
fn test_pyproject_parses_pep508_deps() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("pyproject.toml");
    write(
        &path,
        r#"[project]
name = "cool-lib"
version = "0.1"
dependencies = ["requests>=2.0", "rich[jupyter]==13.0", "tomli; python_version<'3.11'"]
"#,
    );
    let result = extract_package_manifest(&path);
    assert_eq!(package_node(&result).label, "cool-lib");
    assert!(dependency_targets(&result).is_superset(&BTreeSet::from([
        "pkg_requests",
        "pkg_rich",
        "pkg_tomli"
    ])));
}

#[test]
fn test_gomod_parses_module_and_requires() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("go.mod");
    write(
        &path,
        "module example.com/me/app\n\ngo 1.22\n\nrequire (\n\tgithub.com/x/y v1.2.3\n\tgithub.com/a/b v0.4.0\n)\n",
    );
    let result = extract_package_manifest(&path);
    assert_eq!(package_node(&result).label, "example.com/me/app");
    let dependencies = dependency_targets(&result);
    assert!(dependencies.contains("pkg_github_com_x_y"));
    assert!(dependencies.contains("pkg_github_com_a_b"));
}

#[test]
fn test_pom_parses_artifact_and_deps() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("pom.xml");
    write(
        &path,
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <groupId>com.acme</groupId>
  <artifactId>widget</artifactId>
  <version>2.0</version>
  <dependencies>
    <dependency><groupId>org.lib</groupId><artifactId>core</artifactId></dependency>
  </dependencies>
</project>
"#,
    );
    let result = extract_package_manifest(&path);
    assert_eq!(package_node(&result).label, "com.acme:widget");
    assert!(dependency_targets(&result).contains("pkg_org_lib_core"));
}

#[test]
fn test_apm_dependency_collapses_to_single_canonical_node() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("packages");
    write(
        &base.join("core/apm.yml"),
        "name: coding-standards-core\nversion: 1.0.4\n",
    );
    write(
        &base.join("csharp/apm.yml"),
        "name: coding-standards-csharp\ndependencies:\n  - coding-standards-core\n",
    );
    write(
        &base.join("python/apm.yml"),
        "name: coding-standards-python\ndependencies:\n  coding-standards-core: \">=1.0\"\n",
    );
    let files = [
        base.join("core/apm.yml"),
        base.join("csharp/apm.yml"),
        base.join("python/apm.yml"),
    ];
    let extracted = extract_files(&files, Some(fixture.path()), true).unwrap();
    let package_nodes = extracted
        .extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .filter(|node| node.label == "coding-standards-core")
        .collect::<Vec<_>>();
    assert_eq!(package_nodes.len(), 1);
    assert_eq!(package_nodes[0].id, "pkg_coding_standards_core");
    assert!(!package_nodes[0].source_file.is_empty());

    let graph = build_graph(&extracted.extractions).unwrap();
    assert_eq!(
        graph
            .nodes
            .iter()
            .filter(|node| node.label == "coding-standards-core")
            .count(),
        1
    );
    assert_eq!(
        graph
            .links
            .iter()
            .filter(|edge| edge.relation == "depends_on")
            .count(),
        2
    );
}

#[test]
fn test_external_dependency_edge_pruned_not_orphaned() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("apm.yml");
    write(&path, "name: leaf\ndependencies:\n  - some-external-pkg\n");
    let extracted = extract_files(&[path], Some(fixture.path()), true).unwrap();
    assert_eq!(extracted.extractions[0].edges.len(), 1);
    let graph = build_graph(&extracted.extractions).unwrap();
    assert!(graph
        .nodes
        .iter()
        .all(|node| node.id != "pkg_some_external_pkg"));
    assert!(graph.nodes.iter().any(|node| node.label == "leaf"));
}

#[test]
fn test_malformed_manifest_does_not_crash() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("pom.xml");
    write(&path, "<project><not closed");
    let result = extract_package_manifest(&path);
    assert!(result.nodes.is_empty() && result.edges.is_empty());
}

#[test]
fn manifest_ingest_rejects_oversize_and_ambiguous_yaml() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("apm.yml");
    write(&path, &"x".repeat(2_000_001));
    let result = extract_package_manifest(&path);
    assert!(result.nodes.is_empty() && result.edges.is_empty());

    write(
        &path,
        "name: leaf\ndependencies:\n\t- hidden-by-invalid-tab\n",
    );
    let result = extract_package_manifest(&path);
    assert!(result.nodes.is_empty() && result.edges.is_empty());
}

#[test]
fn manifest_ingest_deduplicates_dependencies_and_skips_self_edges() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("apm.yml");
    write(
        &path,
        "name: leaf\ndependencies:\n  - leaf\n  - dep\n  - dep\n",
    );
    let result = extract_package_manifest(&path);
    assert_eq!(result.edges.len(), 1);
    assert_eq!(result.edges[0].source, "pkg_leaf");
    assert_eq!(result.edges[0].target, "pkg_dep");
}
