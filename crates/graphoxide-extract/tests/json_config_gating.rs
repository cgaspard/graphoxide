use graphoxide_core::Extraction;
use graphoxide_extract::{extract, extract_project_with_options_and_output};
use graphoxide_graph::build_graph;
use std::{fs, path::Path};

fn extract_json(root: &Path, name: &str, contents: &str) -> Extraction {
    let path = root.join(name);
    fs::write(&path, contents).unwrap_or_else(|error| panic!("write {name}: {error}"));
    extract(&path).unwrap_or_else(|error| panic!("extract {name}: {error}"))
}

#[test]
fn ordinary_json_shapes_are_skipped_by_the_public_dispatcher() {
    let temp = tempfile::tempdir().expect("create JSON gating fixture");
    let cases = [
        (
            "extraction.json",
            r#"{"nodes":[{"id":"n1","label":"one"}],"edges":[],"input_tokens":12}"#,
        ),
        ("records.json", r#"[{"id":1},{"id":2}]"#),
        ("scalar.json", r#""ordinary data""#),
        (
            "nested-config-key.json",
            r#"{"metadata":{"dependencies":{"lodash":"4"}},"rows":[]}"#,
        ),
        (
            "wrong-case-key.json",
            r#"{"Dependencies":{"lodash":"4"},"rows":[]}"#,
        ),
        (
            "config-looking-name.json",
            r#"{"generation":{"target":"model"},"cases":[1,2]}"#,
        ),
    ];

    for (name, contents) in cases {
        let result = extract_json(temp.path(), name, contents);
        assert!(result.nodes.is_empty(), "data JSON emitted nodes: {name}");
        assert!(result.edges.is_empty(), "data JSON emitted edges: {name}");
    }
}

#[test]
fn recognized_json_config_filenames_match_the_upstream_gate() {
    let temp = tempfile::tempdir().expect("create JSON filename fixture");
    let exact_names = [
        "package.json",
        "tsconfig.json",
        "jsconfig.json",
        "composer.json",
        "deno.json",
        "bower.json",
        "manifest.json",
        "app.json",
        "now.json",
        "vercel.json",
        "angular.json",
        "nest-cli.json",
        "biome.json",
        "renovate.json",
        ".babelrc.json",
        ".eslintrc.json",
        ".prettierrc.json",
        "babel.config.json",
    ];
    let compound_names = [
        "frontend.eslintrc.json",
        "frontend.prettierrc.json",
        "frontend.babelrc.json",
        "api.tsconfig.json",
        "api.jsconfig.json",
    ];

    for name in exact_names.into_iter().chain(compound_names) {
        let result = extract_json(temp.path(), name, "{}");
        assert!(
            result.nodes.iter().any(|node| node.label == name),
            "recognized config filename was skipped: {name}"
        );
    }

    let upper = extract_json(temp.path(), "TSCONFIG.JSON", "{}");
    assert!(
        upper.nodes.iter().any(|node| node.label == "TSCONFIG.JSON"),
        "the upstream filename gate is case-insensitive"
    );
}

#[test]
fn jsonc_editor_files_do_not_abort_extraction_before_the_config_gate() {
    let temp = tempfile::tempdir().expect("create JSONC editor fixture");
    let cases = [
        (
            "tasks.json",
            r#"{
                // VS Code tasks are JSONC even though the suffix is .json.
                "version": "2.0.0",
                "tasks": [
                    { "label": "build", "type": "shell", "command": "cargo build", },
                ],
            }"#,
        ),
        (
            "launch.json",
            r#"{
                /* Block comments are valid in VS Code configuration. */
                "version": "0.2.0",
                "configurations": [
                    { "name": "Run", "type": "lldb", "request": "launch", },
                ],
            }"#,
        ),
    ];

    for (name, contents) in cases {
        let result = extract_json(temp.path(), name, contents);
        assert!(
            result.nodes.is_empty(),
            "editor JSONC unexpectedly emitted nodes: {name}"
        );
        assert!(
            result.edges.is_empty(),
            "editor JSONC unexpectedly emitted edges: {name}"
        );
    }
}

#[test]
fn recognized_json_config_accepts_comments_and_trailing_commas() {
    let temp = tempfile::tempdir().expect("create recognized JSONC fixture");
    let result = extract_json(
        temp.path(),
        "tsconfig.json",
        r#"{
            // TypeScript configuration uses JSONC.
            "compilerOptions": {
                "strict": true,
                "baseUrl": ".",
            },
        }"#,
    );

    assert!(result
        .nodes
        .iter()
        .any(|node| node.label == "compilerOptions"));
    assert!(result.nodes.iter().any(|node| node.label == "strict"));
}

#[test]
fn recognized_top_level_keys_match_the_upstream_gate_exactly() {
    let temp = tempfile::tempdir().expect("create JSON key fixture");
    let keys = [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
        "bundleDependencies",
        "bundledDependencies",
        "extends",
        "$ref",
        "$schema",
        "compilerOptions",
    ];

    for (index, key) in keys.into_iter().enumerate() {
        let name = format!("arbitrary-{index}.json");
        let contents = format!(r#"{{"{key}":{{}}}}"#);
        let result = extract_json(temp.path(), &name, &contents);
        assert!(
            result.nodes.iter().any(|node| node.label == key),
            "recognized top-level key was skipped: {key}"
        );
    }
}

#[test]
fn recognized_dependency_survives_corpus_resolution_as_an_import_target() {
    let temp = tempfile::tempdir().expect("create JSON dependency fixture");
    let root = temp.path().join("corpus");
    fs::create_dir(&root).expect("create corpus root");
    fs::write(
        root.join("weird-name.json"),
        r#"{"dependencies":{"lodash":"4"}}"#,
    )
    .expect("write config");

    let chunks =
        extract_project_with_options_and_output(&root, true, &temp.path().join("managed-output"))
            .expect("extract config corpus");
    let graph = build_graph(&chunks).expect("build config graph");

    let dependency = graph
        .nodes
        .iter()
        .find(|node| node.id == "lodash")
        .expect("dependency concept node must survive corpus resolution");
    assert_eq!(dependency.label, "lodash");
    assert_eq!(dependency.file_type, "concept");
    assert_eq!(dependency.source_file, "weird-name.json");
    assert!(graph.links.iter().any(|edge| {
        edge.relation == "imports"
            && edge.true_source() == "weird_name_dependencies_lodash"
            && edge.true_target() == "lodash"
    }));
}

#[test]
fn recognized_config_nodes_and_edges_keep_the_declaring_key_line() {
    let temp = tempfile::tempdir().expect("create JSON location fixture");
    let result = extract_json(
        temp.path(),
        "weird-name.json",
        "{\n  \"dependencies\": {\n    \"lodash\": \"4\"\n  }\n}\n",
    );

    let dependencies = result
        .nodes
        .iter()
        .find(|node| node.id.ends_with("_dependencies"))
        .expect("dependencies key node");
    let lodash_key = result
        .nodes
        .iter()
        .find(|node| node.id.ends_with("_dependencies_lodash"))
        .expect("lodash key node");
    assert_eq!(dependencies.source_location.as_deref(), Some("L2"));
    assert_eq!(lodash_key.source_location.as_deref(), Some("L3"));
    assert!(result.edges.iter().any(|edge| {
        edge.relation == "imports"
            && edge
                .extra
                .get("source_location")
                .and_then(|line| line.as_str())
                == Some("L3")
    }));
}
