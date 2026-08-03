//! Adversarial ports for phantom-edge, case, package-root, and builtin-name bugs.

use graphoxide_core::{Confidence, Edge, Extraction, Node};
use graphoxide_extract::extract_project_with_options;
use graphoxide_graph::build_graph;
use std::{collections::BTreeMap, fs};
use tempfile::TempDir;

struct Project {
    root: TempDir,
}

impl Project {
    fn new() -> Self {
        Self {
            root: TempDir::new().unwrap(),
        }
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    fn extract(&self) -> Vec<Extraction> {
        extract_project_with_options(self.root.path(), true).unwrap()
    }
}

fn project(files: &[(&str, &str)]) -> Vec<Extraction> {
    let project = Project::new();
    for (path, source) in files {
        project.write(path, source);
    }
    project.extract()
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|item| &item.nodes)
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|item| &item.edges)
}

fn bare(label: &str) -> &str {
    label.trim_start_matches('.').trim_end_matches("()")
}

fn ids(extractions: &[Extraction], label: &str) -> Vec<String> {
    nodes(extractions)
        .filter(|node| bare(&node.label) == label)
        .map(|node| node.id.clone())
        .collect()
}

fn id(extractions: &[Extraction], label: &str) -> String {
    let matches = ids(extractions, label);
    assert_eq!(matches.len(), 1, "expected one {label:?}: {matches:?}");
    matches[0].clone()
}

fn file_id(extractions: &[Extraction], source_file: &str) -> String {
    nodes(extractions)
        .find(|node| {
            node.source_file == source_file
                && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("file")
        })
        .unwrap_or_else(|| panic!("missing file node for {source_file}"))
        .id
        .clone()
}

fn call_labels(extractions: &[Extraction]) -> Vec<(String, String, Confidence)> {
    let labels = nodes(extractions)
        .map(|node| (node.id.as_str(), bare(&node.label)))
        .collect::<BTreeMap<_, _>>();
    edges(extractions)
        .filter(|edge| edge.relation == "calls")
        .map(|edge| {
            (
                labels
                    .get(edge.true_source())
                    .copied()
                    .unwrap_or(edge.true_source())
                    .to_owned(),
                labels
                    .get(edge.true_target())
                    .copied()
                    .unwrap_or(edge.true_target())
                    .to_owned(),
                edge.confidence,
            )
        })
        .collect()
}

#[test]
fn test_unresolved_bare_import_is_ref_namespaced() {
    let result = project(&[(
        "frontend/src/SomeChart.tsx",
        "import colors from 'tailwindcss/colors';\nexport const C = colors.blue;\n",
    )]);
    let targets = edges(&result)
        .filter(|edge| edge.relation == "imports_from")
        .map(Edge::true_target)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "import targets: {targets:?}");
    assert!(targets[0].starts_with("ref"), "import targets: {targets:?}");
    assert_ne!(targets[0], "colors");
}

#[test]
fn test_scoped_package_import_is_ref_namespaced() {
    let result = project(&[(
        "src/thing.ts",
        "import util from '@scope/utils';\nexport const x = util;\n",
    )]);
    let targets = edges(&result)
        .filter(|edge| edge.relation == "imports_from")
        .map(Edge::true_target)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "import targets: {targets:?}");
    assert!(targets[0].starts_with("ref"), "import targets: {targets:?}");
    assert_ne!(targets[0], "utils");
}

fn phantom_external_project(count: usize) -> Vec<Extraction> {
    let fixture = Project::new();
    fixture.write(
        "backend/utils/colors.py",
        "def hex_to_rgb(value):\n    return (0, 0, 0)\n",
    );
    for index in 0..count {
        fixture.write(
            &format!("frontend/src/Chart{index}.tsx"),
            &format!(
                "import colors from 'tailwindcss/colors';\nexport const C{index} = colors.blue;\n"
            ),
        );
    }
    fixture.extract()
}

fn assert_no_ts_import_to_python_colors(extractions: &[Extraction]) {
    let graph = build_graph(extractions).unwrap();
    let python_ids = graph
        .nodes
        .iter()
        .filter(|node| node.source_file.ends_with("colors.py"))
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    assert!(!python_ids.is_empty());
    assert!(graph.links.iter().all(|edge| {
        edge.relation != "imports_from"
            || !python_ids.contains(&edge.true_target())
            || !(edge.source_file.ends_with(".ts") || edge.source_file.ends_with(".tsx"))
    }));
}

#[test]
fn test_no_phantom_edge_from_tsx_to_unrelated_python_file() {
    assert_no_ts_import_to_python_colors(&phantom_external_project(1));
}

#[test]
fn test_multiple_tsx_files_do_not_all_alias_onto_one_python_file() {
    assert_no_ts_import_to_python_colors(&phantom_external_project(3));
}

#[test]
fn test_unimported_cross_package_call_emits_no_edge() {
    let result = project(&[
        (
            "pkg-a/src/index.ts",
            "declare function validate(x: number): boolean;\nexport function run(x: number): boolean { return validate(x); }\n",
        ),
        (
            "pkg-b/src/index.ts",
            "export function validate(name: string): boolean { return name.length > 0; }\n",
        ),
    ]);
    assert!(!call_labels(&result)
        .iter()
        .any(|(source, target, _)| source == "run" && target == "validate"));
}

#[test]
fn test_many_files_do_not_collapse_onto_one_export() {
    let fixture = Project::new();
    fixture.write(
        "proto/index.ts",
        "export function encode(x: string): string { return x; }\n",
    );
    for index in 0..4 {
        fixture.write(
            &format!("svc{index}/index.ts"),
            &format!(
                "declare function encode(x: string): string;\nexport function use{index}(x: string) {{ return encode(x); }}\n"
            ),
        );
    }
    let result = fixture.extract();
    assert!(!call_labels(&result)
        .iter()
        .any(|(source, target, _)| source.starts_with("use") && target == "encode"));
}

#[test]
fn test_imported_cross_file_call_still_resolves() {
    let result = project(&[
        (
            "a.ts",
            "import { validate } from './b';\nexport function run(x: number) { return validate(x); }\n",
        ),
        (
            "b.ts",
            "export function validate(name: string): boolean { return name.length > 0; }\n",
        ),
    ]);
    assert!(call_labels(&result)
        .iter()
        .any(|(source, target, confidence)| {
            source == "run" && target == "validate" && *confidence == Confidence::Extracted
        }));
}

#[test]
fn test_same_file_call_unaffected() {
    let result = project(&[(
        "s.ts",
        "function helper() { return 1; }\nexport function main() { return helper(); }\n",
    )]);
    assert!(call_labels(&result)
        .iter()
        .any(|(source, target, _)| source == "main" && target == "helper"));
}

#[test]
fn test_non_js_single_candidate_cross_file_still_resolves() {
    let result = project(&[
        ("helper.rb", "def transform(data)\n  data.upcase\nend\n"),
        ("main.rb", "def handle(v)\n  transform(v)\nend\n"),
    ]);
    assert!(call_labels(&result)
        .iter()
        .any(|(source, target, _)| source == "handle" && target == "transform"));
}

#[test]
fn test_python_path_does_not_resolve_to_shell_path() {
    let result = project(&[
        ("run.sh", "export PATH=/usr/local/bin:$PATH\n"),
        (
            "mod.py",
            "from pathlib import Path\ndef load(p: Path) -> Path:\n    return Path(p)\ndef other():\n    return load(Path('x'))\n",
        ),
    ]);
    let path = id(&result, "PATH");
    let labels = nodes(&result)
        .map(|node| (node.id.as_str(), bare(&node.label)))
        .collect::<BTreeMap<_, _>>();
    assert!(edges(&result).all(|edge| {
        edge.true_target() != path
            || !matches!(labels.get(edge.true_source()), Some(&"load" | &"other"))
    }));
    assert!(
        edges(&result)
            .filter(|edge| edge.true_target() == path)
            .count()
            <= 1
    );
}

#[test]
fn test_case_sensitive_cross_file_ref_respects_case() {
    let result = project(&[
        ("consts.rs", "pub const PATH: &str = \"/x\";\n"),
        ("use.rs", "struct Wrap(Path);\n"),
    ]);
    let path = id(&result, "PATH");
    let wrap = id(&result, "Wrap");
    assert!(!edges(&result).any(|edge| {
        edge.true_source() == wrap && edge.true_target() == path && edge.relation == "references"
    }));
}

#[test]
fn test_case_sensitive_same_file_value_and_type_keep_distinct_ids() {
    let result = project(&[(
        "lib.rs",
        "pub const PATH: &str = \"/x\";\npub struct Path;\n",
    )]);
    let upper = id(&result, "PATH");
    let mixed = id(&result, "Path");
    assert_ne!(upper, mixed);
    assert_eq!(
        nodes(&result)
            .find(|node| node.id == upper)
            .and_then(|node| node.extra.get("type"))
            .and_then(serde_json::Value::as_str),
        Some("variable")
    );
    assert_eq!(
        nodes(&result)
            .find(|node| node.id == mixed)
            .and_then(|node| node.extra.get("type"))
            .and_then(serde_json::Value::as_str),
        Some("class")
    );
    let graph = build_graph(&result).unwrap();
    assert!(graph.nodes.iter().any(|node| node.id == upper));
    assert!(graph.nodes.iter().any(|node| node.id == mixed));
}

#[test]
fn test_exact_case_cross_file_still_resolves() {
    let result = project(&[
        ("h.py", "def helper():\n    return 1\n"),
        (
            "m.py",
            "from h import helper\ndef go():\n    return helper()\n",
        ),
    ]);
    assert!(call_labels(&result)
        .iter()
        .any(|(source, target, _)| source == "go" && target == "helper"));
}

#[test]
fn test_php_case_insensitive_resolution_preserved() {
    let result = project(&[
        ("lib.php", "<?php\nfunction Greet() { return 1; }\n"),
        ("main.php", "<?php\nfunction run() { return greet(); }\n"),
    ]);
    assert!(call_labels(&result)
        .iter()
        .any(|(source, target, _)| source == "run" && target == "Greet"));
}

const SRC_LAYOUT_FILES: &[(&str, &str)] = &[
    (
        "mypkg/__init__.py",
        "from mypkg.core import Engine\n",
    ),
    ("mypkg/core.py", "class Engine:\n    pass\n"),
    ("mypkg/helpers.py", "def helper():\n    return 1\n"),
    (
        "mypkg/app.py",
        "from mypkg.core import Engine\nimport mypkg.helpers\n\ndef run():\n    return mypkg.helpers.helper()\n",
    ),
];

fn write_src_layout(project: &Project, prefix: &str) {
    for (path, source) in SRC_LAYOUT_FILES {
        let path = if prefix.is_empty() {
            (*path).to_owned()
        } else {
            format!("{prefix}/{path}")
        };
        project.write(&path, source);
    }
}

#[test]
fn test_resolve_python_module_path_walks_up_to_src_package_root() {
    let fixture = Project::new();
    fixture.write("src/mypkg/core.py", "class Engine: pass\n");
    fixture.write("src/mypkg/app.py", "from mypkg.core import Engine\n");
    fixture.write("flat/mod.py", "x = 1\n");
    fixture.write("flat/app.py", "import flat.mod\n");
    let result = fixture.extract();
    let app = file_id(&result, "src/mypkg/app.py");
    let core = id(&result, "Engine");
    assert!(edges(&result).any(|edge| {
        edge.true_source() == app
            && edge.true_target() == core
            && matches!(edge.relation.as_str(), "imports" | "imports_from")
    }));
    let flat_app = file_id(&result, "flat/app.py");
    let flat_mod = file_id(&result, "flat/mod.py");
    assert!(edges(&result).any(|edge| {
        edge.true_source() == flat_app
            && edge.true_target() == flat_mod
            && matches!(edge.relation.as_str(), "imports" | "imports_from")
    }));
}

fn import_facts(extractions: &[Extraction]) -> Vec<(String, String, String)> {
    let mut values = edges(extractions)
        .filter(|edge| matches!(edge.relation.as_str(), "imports" | "imports_from"))
        .map(|edge| {
            (
                edge.relation.clone(),
                edge.true_source().trim_start_matches("src_").to_owned(),
                edge.true_target().trim_start_matches("src_").to_owned(),
            )
        })
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

#[test]
fn test_import_edges_identical_from_root_or_src() {
    let direct = Project::new();
    write_src_layout(&direct, "");
    let direct_result = direct.extract();
    let nested = Project::new();
    write_src_layout(&nested, "src");
    let nested_result = nested.extract();
    assert!(!import_facts(&direct_result).is_empty());
    assert_eq!(import_facts(&nested_result), import_facts(&direct_result));
    let app = file_id(&nested_result, "src/mypkg/app.py");
    assert!(edges(&nested_result).any(|edge| {
        edge.true_source() == app
            && matches!(edge.relation.as_str(), "imports" | "imports_from")
            && edge.true_target().starts_with("src_mypkg_core")
    }));
}

#[test]
fn test_package_reexport_cleanup_preserves_unresolved_external_import() {
    let result = project(&[
        (
            "pkg/__init__.py",
            "from pkg.core import Engine\nfrom thirdparty.widgets import Widget\n",
        ),
        ("pkg/core.py", "class Engine:\n    pass\n"),
    ]);
    let package = file_id(&result, "pkg/__init__.py");
    assert!(edges(&result).any(|edge| {
        edge.true_source() == package
            && edge.relation == "imports_from"
            && edge.true_target() == "thirdparty_widgets"
    }));
    assert!(!edges(&result).any(|edge| {
        edge.true_source() == package
            && edge.relation == "imports_from"
            && edge.true_target() == "pkg_core"
    }));
}

#[test]
fn test_ambiguous_package_alias_is_not_repointed() {
    let result = project(&[
        ("a/src/pkg/__init__.py", ""),
        ("b/src/pkg/__init__.py", ""),
        ("a/src/pkg/mod.py", "def f():\n    return 1\n"),
        ("b/src/pkg/mod.py", "def f():\n    return 2\n"),
        ("a/src/pkg/app.py", "import pkg.mod\n"),
    ]);
    let app = file_id(&result, "a/src/pkg/app.py");
    let targets = edges(&result)
        .filter(|edge| {
            edge.true_source() == app
                && matches!(edge.relation.as_str(), "imports" | "imports_from")
        })
        .map(Edge::true_target)
        .collect::<Vec<_>>();
    assert!(!targets.contains(&"a_src_pkg_mod"));
    assert!(!targets.contains(&"b_src_pkg_mod"));
}

#[test]
fn test_non_python_import_edge_is_not_repointed() {
    let mut result = project(&[
        ("src/pkg/__init__.py", ""),
        ("src/pkg/mod.py", "def f():\n    return 1\n"),
    ]);
    result[0].nodes.push(Node {
        id: "app_cs".into(),
        label: "app.cs".into(),
        file_type: "code".into(),
        source_file: "app.cs".into(),
        source_location: Some("L1".into()),
        community: None,
        extra: BTreeMap::from([("type".into(), "file".into())]),
    });
    result[0].edges.push(Edge {
        source: "app_cs".into(),
        target: "pkg_mod".into(),
        relation: "imports".into(),
        confidence: Confidence::Extracted,
        source_file: "app.cs".into(),
        extra: BTreeMap::new(),
    });
    let graph = build_graph(&result).unwrap();
    assert!(!graph.links.iter().any(|edge| {
        edge.true_source() == "app_cs"
            && edge.true_target() == "src_pkg_mod"
            && matches!(edge.relation.as_str(), "imports" | "imports_from")
    }));
}

#[test]
fn test_builtin_date_type_ref_does_not_bind_to_user_date() {
    let result = project(&[
        ("model.ts", "export class DATE { value: string = ''; }\n"),
        (
            "a.ts",
            "export function parse(x: Date): number { return x.getTime(); }\n",
        ),
        (
            "b.ts",
            "export function fmt(w: Date): string { return w.toISOString(); }\n",
        ),
    ]);
    let date = id(&result, "DATE");
    assert!(edges(&result)
        .filter(|edge| edge.true_target() == date)
        .all(|edge| edge.relation != "references"));
    assert!(
        edges(&result)
            .filter(|edge| edge.true_source() == date || edge.true_target() == date)
            .count()
            <= 1
    );
}

#[test]
fn test_nonbuiltin_receiver_type_still_resolves() {
    let result = project(&[
        (
            "svc.ts",
            "export class PaymentClient { charge(n: number): boolean { return true; } }\n",
        ),
        (
            "order.ts",
            "import { PaymentClient } from './svc';\nexport class Order { constructor(private client: PaymentClient) {} pay(): boolean { return this.client.charge(1); } }\n",
        ),
    ]);
    let charge = id(&result, "charge");
    assert!(edges(&result).any(|edge| edge.true_target() == charge));
}

#[test]
fn test_builtin_static_call_does_not_bind_to_user_symbol() {
    let result = project(&[
        (
            "format.ts",
            "const DATE = new Intl.DateTimeFormat('en-US', {});\nexport function fmt(x: number): string { return DATE.format(x); }\n",
        ),
        (
            "svc.ts",
            "export class Svc { expiry(d: Date): Date { return d; } stamp(): number { return Date.now(); } when(): string { return new Date().toISOString(); } }\n",
        ),
    ]);
    let date = id(&result, "DATE");
    assert!(edges(&result)
        .filter(|edge| edge.true_target() == date && edge.relation == "references")
        .all(|edge| edge.source_file == "format.ts"));
}

fn ids_in_file(extractions: &[Extraction], source_file: &str, label: &str) -> Vec<String> {
    nodes(extractions)
        .filter(|node| node.source_file == source_file && bare(&node.label) == label)
        .map(|node| node.id.clone())
        .collect()
}

fn go_builtin_shadow_project(extra: &str) -> Vec<Extraction> {
    project(&[
        (
            "history.go",
            &format!(
                "package main\n\ntype metricHistory struct {{ samples []int }}\n\nfunc (h *metricHistory) append(v int) {{\n    h.samples = append(h.samples, v)\n}}\n{extra}"
            ),
        ),
        (
            "worker.go",
            "package main\n\nfunc collect(values []int) []int {\n    out := []int{}\n    for _, v := range values { out = append(out, v) }\n    return out\n}\n",
        ),
    ])
}

#[test]
fn test_builtin_append_does_not_bind_to_user_method() {
    let result = go_builtin_shadow_project("");
    let methods = ids_in_file(&result, "history.go", "append");
    assert!(!methods.is_empty());
    let workers = nodes(&result)
        .filter(|node| node.source_file == "worker.go")
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    assert!(edges(&result).all(|edge| {
        !methods.iter().any(|target| target == edge.true_target())
            || !workers.contains(&edge.true_source())
    }));
}

#[test]
fn test_user_method_node_survives_the_filter() {
    let result = go_builtin_shadow_project("");
    assert!(!ids_in_file(&result, "history.go", "append").is_empty());
}

#[test]
fn test_non_builtin_cross_file_call_still_resolves() {
    let result = project(&[
        (
            "engine.go",
            "package main\n\nfunc process(v int) int { return v * 2 }\n",
        ),
        (
            "runner.go",
            "package main\n\nfunc run(v int) int { return process(v) }\n",
        ),
    ]);
    let targets = ids_in_file(&result, "engine.go", "process");
    let sources = ids_in_file(&result, "runner.go", "run");
    assert!(edges(&result).any(|edge| {
        sources.iter().any(|source| source == edge.true_source())
            && targets.iter().any(|target| target == edge.true_target())
    }));
}

#[test]
fn test_builtin_append_does_not_bind_in_file() {
    let result =
        go_builtin_shadow_project("\nfunc widen(xs []int) []int {\n    return append(xs, 0)\n}\n");
    let methods = ids_in_file(&result, "history.go", "append");
    let widen = ids_in_file(&result, "history.go", "widen");
    assert!(!methods.is_empty() && !widen.is_empty());
    assert!(edges(&result).all(|edge| {
        !widen.iter().any(|source| source == edge.true_source())
            || !methods.iter().any(|target| target == edge.true_target())
    }));
}

#[test]
fn test_go_selector_call_to_shadowing_method_survives() {
    let result =
        go_builtin_shadow_project("\nfunc record(h *metricHistory, v int) {\n    h.append(v)\n}\n");
    let methods = ids_in_file(&result, "history.go", "append");
    let record = ids_in_file(&result, "history.go", "record");
    assert!(edges(&result).any(|edge| {
        record.iter().any(|source| source == edge.true_source())
            && methods.iter().any(|target| target == edge.true_target())
    }));
}

#[test]
fn test_rust_in_file_type_new_edge_survives() {
    let result = project(&[(
        "lib.rs",
        "pub struct Widget { n: i32 }\nimpl Widget { pub fn new(n: i32) -> Widget { Widget { n } } }\npub fn build() -> Widget { Widget::new(3) }\n",
    )]);
    let new = ids_in_file(&result, "lib.rs", "new");
    let build = ids_in_file(&result, "lib.rs", "build");
    assert!(!new.is_empty() && !build.is_empty());
    assert!(edges(&result).any(|edge| {
        build.iter().any(|source| source == edge.true_source())
            && new.iter().any(|target| target == edge.true_target())
    }));
}
