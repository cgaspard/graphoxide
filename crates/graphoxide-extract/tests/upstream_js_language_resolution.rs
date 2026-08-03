use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
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

    fn write(&self, relative: &str, body: &str) {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn extract(&self) -> Vec<Extraction> {
        extract_project_with_options(self.root.path(), true).unwrap()
    }
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|extraction| &extraction.nodes)
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|extraction| &extraction.edges)
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

fn node_id(extractions: &[Extraction], source_file: &str, label: &str) -> String {
    nodes(extractions)
        .find(|node| node.source_file == source_file && node.label == label)
        .unwrap_or_else(|| panic!("missing {label} in {source_file}"))
        .id
        .clone()
}

fn has_edge(extractions: &[Extraction], source: &str, target: &str, relation: &str) -> bool {
    edges(extractions).any(|edge| {
        edge.true_source() == source && edge.true_target() == target && edge.relation == relation
    })
}

fn call_targets(extractions: &[Extraction], caller: &str) -> Vec<String> {
    let labels = nodes(extractions)
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect::<BTreeMap<_, _>>();
    edges(extractions)
        .filter(|edge| {
            edge.relation == "calls"
                && labels
                    .get(edge.true_source())
                    .is_some_and(|label| label.contains(caller))
        })
        .map(|edge| edge.true_target().to_owned())
        .collect()
}

fn service_project(consumer: &str, extra: Option<(&str, &str)>) -> Vec<Extraction> {
    let project = Project::new();
    project.write(
        "svc.ts",
        "export class Svc {\n  doThing(): number { return 1; }\n}\n",
    );
    project.write("consumer.ts", consumer);
    if let Some((path, body)) = extra {
        project.write(path, body);
    }
    project.extract()
}

#[test]
fn test_local_new_binding_receiver() {
    let result = service_project(
        "import { Svc } from './svc';\nconst s = new Svc();\nexport function usesDirect(): number { return s.doThing(); }\n",
        None,
    );
    let target = node_id(&result, "svc.ts", ".doThing()");
    assert!(call_targets(&result, "usesDirect").contains(&target));
}

#[test]
fn test_closure_over_typed_param_receiver() {
    let result = service_project(
        "import { Svc } from './svc';\nexport function register(svc: Svc): () => number { return () => svc.doThing(); }\n",
        None,
    );
    let target = node_id(&result, "svc.ts", ".doThing()");
    assert!(call_targets(&result, "register").contains(&target));
}

#[test]
fn test_new_binding_resolves_to_correct_class_under_ambiguity() {
    let result = service_project(
        "import { Svc } from './svc';\nconst s = new Svc();\nexport function f(): number { return s.doThing(); }\n",
        Some((
            "cache.ts",
            "export class Cache {\n  doThing(): number { return 2; }\n}\n",
        )),
    );
    let targets = call_targets(&result, "f()");
    assert_eq!(targets, vec![node_id(&result, "svc.ts", ".doThing()")]);
}

#[test]
fn test_untyped_param_receiver_emits_no_edge() {
    let result = service_project(
        "export function g(x): number { return x.doThing(); }\n",
        None,
    );
    let target = node_id(&result, "svc.ts", ".doThing()");
    assert!(!call_targets(&result, "g()").contains(&target));
}

#[test]
fn test_array_typed_receiver_emits_no_edge() {
    let result = service_project(
        "import { Svc } from './svc';\nexport function h(xs: Svc[]): number { return xs[0].doThing(); }\n",
        None,
    );
    let target = node_id(&result, "svc.ts", ".doThing()");
    assert!(!call_targets(&result, "h()").contains(&target));
}

fn import_require_project(
    statement: &str,
    target_path: &str,
    target_body: &str,
) -> Vec<Extraction> {
    let project = Project::new();
    project.write(target_path, target_body);
    project.write("src/main.ts", statement);
    project.extract()
}

#[test]
fn test_import_require_relative_emits_file_edge() {
    let result = import_require_project(
        "import legacy = require('./legacy');\nconst n = legacy.foo();\n",
        "src/legacy.ts",
        "export function foo(): number { return 1; }\n",
    );
    assert!(has_edge(
        &result,
        &file_id(&result, "src/main.ts"),
        &file_id(&result, "src/legacy.ts"),
        "imports_from",
    ));
}

#[test]
fn test_import_require_single_quotes() {
    let result = import_require_project(
        "import util = require('./util');\nexport const x = util.V;\n",
        "src/util.ts",
        "export const V = 1;\n",
    );
    assert!(has_edge(
        &result,
        &file_id(&result, "src/main.ts"),
        &file_id(&result, "src/util.ts"),
        "imports_from",
    ));
}

#[test]
fn test_import_require_bare_module_targets_ref_stub() {
    let project = Project::new();
    project.write(
        "src/io.ts",
        "import fs = require('fs');\nexport const data = fs.readFileSync('x');\n",
    );
    let result = project.extract();
    let source = file_id(&result, "src/io.ts");
    let targets = edges(&result)
        .filter(|edge| edge.true_source() == source && edge.relation == "imports_from")
        .map(Edge::true_target)
        .collect::<Vec<_>>();
    assert!(!targets.is_empty());
    assert!(targets.iter().any(|target| target.starts_with("ref")));
    assert!(!targets.contains(&make_id(&["fs"]).as_str()));
}

#[test]
fn test_import_require_parity_with_namespace_import() {
    let project = Project::new();
    project.write("a/dep.ts", "export function f() {}\n");
    project.write(
        "a/via_require.ts",
        "import dep = require('./dep');\ndep.f();\n",
    );
    project.write("a/via_esm.ts", "import * as dep from './dep';\ndep.f();\n");
    let result = project.extract();
    let outgoing = |source_file: &str| {
        let source = file_id(&result, source_file);
        let mut values = edges(&result)
            .filter(|edge| edge.true_source() == source && edge.relation != "contains")
            .map(|edge| (edge.true_target().to_owned(), edge.relation.clone()))
            .collect::<Vec<_>>();
        values.sort();
        values
    };
    assert_eq!(outgoing("a/via_require.ts"), outgoing("a/via_esm.ts"));
}

#[test]
fn test_esm_imports_unaffected() {
    let project = Project::new();
    project.write("src/bar.ts", "export class Bar {}\n");
    project.write(
        "src/app.ts",
        "import { Bar } from './bar';\nexport const b = new Bar();\n",
    );
    let result = project.extract();
    let source = file_id(&result, "src/app.ts");
    assert!(has_edge(
        &result,
        &source,
        &file_id(&result, "src/bar.ts"),
        "imports_from",
    ));
    assert!(edges(&result).any(|edge| edge.true_source() == source && edge.relation == "imports"));
}

fn assert_no_import_self_loop(relative: &str, source: &str) {
    let project = Project::new();
    project.write(relative, source);
    let result = project.extract();
    assert!(edges(&result).all(|edge| {
        !matches!(
            edge.relation.as_str(),
            "imports" | "imports_from" | "re_exports"
        ) || edge.true_source() != edge.true_target()
    }));
    let graph = build_graph(&result).unwrap();
    assert!(graph.links.iter().all(|edge| {
        !matches!(
            edge.relation.as_str(),
            "imports" | "imports_from" | "re_exports"
        ) || edge.true_source() != edge.true_target()
    }));
}

#[test]
fn test_python_external_import_matching_current_basename_has_no_self_loop() {
    for (path, source) in [
        ("src/contracting/stdlib/builtins.py", "import builtins\n"),
        (
            "playground/services/contracting.py",
            "from contracting import constants\n",
        ),
    ] {
        assert_no_import_self_loop(path, source);
    }
}

#[test]
fn test_rust_import_matching_current_basename_has_no_self_loop() {
    for (path, source) in [
        (
            "packages/compiler/src/fixture.rs",
            "mod tests { use crate::fixture::Fixture; }\n",
        ),
        (
            "packages/zk/src/poseidon.rs",
            "use ark_crypto_primitives::sponge::poseidon::{PoseidonConfig, PoseidonSponge};\n",
        ),
    ] {
        assert_no_import_self_loop(path, source);
    }
}

#[test]
fn test_recursive_call_self_loop_is_preserved() {
    let node = Node {
        id: "module_recurse".into(),
        label: "recurse()".into(),
        file_type: "code".into(),
        source_file: "module.py".into(),
        source_location: Some("L1".into()),
        community: None,
        extra: BTreeMap::from([("_origin".into(), "ast".into())]),
    };
    let edge = Edge {
        source: node.id.clone(),
        target: node.id.clone(),
        relation: "calls".into(),
        confidence: Confidence::Extracted,
        source_file: node.source_file.clone(),
        extra: BTreeMap::from([("source_location".into(), "L2".into())]),
    };
    let graph = build_graph(&[Extraction {
        nodes: vec![node],
        edges: vec![edge],
        hyperedges: Vec::new(),
    }])
    .unwrap();
    assert!(graph.links.iter().any(|edge| {
        edge.true_source() == "module_recurse"
            && edge.true_target() == "module_recurse"
            && edge.relation == "calls"
    }));
}

fn exported_scalar_result(suffix: &str) -> Vec<Extraction> {
    let project = Project::new();
    project.write(
        &format!("constants{suffix}"),
        r#"
export const NUMBER = 42;
export const STRING = "value";
export const BOOLEAN = true;
export const TEMPLATE = `value-${NUMBER}`;
export const MEMBER = process.env.VALUE;
export const LOGICAL = process.env.VALUE ?? "fallback";
export const TERNARY = BOOLEAN ? "yes" : "no";

const internalScalar = 1;
function helper() {
  const localScalar = 2;
}
"#,
    );
    project.extract()
}

#[test]
fn test_exported_scalar_bindings_emit_nodes() {
    for suffix in [".js", ".ts"] {
        let result = exported_scalar_result(suffix);
        let labels = nodes(&result)
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>();
        for expected in [
            "NUMBER", "STRING", "BOOLEAN", "TEMPLATE", "MEMBER", "LOGICAL", "TERNARY",
        ] {
            assert!(
                labels.contains(&expected),
                "missing {expected} for {suffix}"
            );
        }
        assert!(!labels.contains(&"internalScalar"));
        assert!(!labels.contains(&"localScalar"));
    }
}

#[test]
fn test_exported_scalar_fix_skips_unsupported_binding_patterns() {
    let project = Project::new();
    project.write(
        "patterns.ts",
        "const config = { source: 1 };\nconst items = [1];\nexport const { source: renamed } = config;\nexport const [first] = items;\nexport const $ = 1;\nexport const _ = 2;\n",
    );
    let result = project.extract();
    let labels = nodes(&result)
        .map(|node| node.label.as_str())
        .collect::<Vec<_>>();
    assert!(!labels.contains(&"$"));
    assert!(!labels.contains(&"_"));
    assert!(labels
        .iter()
        .all(|label| !label.contains("renamed") && !label.contains("first")));
    assert!(edges(&result).all(|edge| edge.true_source() != edge.true_target()));
}

#[test]
fn test_exported_scalar_binding_satisfies_named_import_target() {
    let project = Project::new();
    project.write(
        "constants.ts",
        "export const A_PREFIX = process.env.A_PREFIX ?? 'X>';\nexport const A_MAX = Number(process.env.A_MAX || 10);\n",
    );
    project.write(
        "consumer.ts",
        "import { A_PREFIX, A_MAX } from './constants';\n",
    );
    let result = project.extract();
    let ids = nodes(&result)
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let targets = edges(&result)
        .filter(|edge| edge.relation == "imports")
        .map(Edge::true_target)
        .collect::<Vec<_>>();
    assert!(!targets.is_empty());
    assert!(targets.iter().all(|target| ids.contains(target)));
}
