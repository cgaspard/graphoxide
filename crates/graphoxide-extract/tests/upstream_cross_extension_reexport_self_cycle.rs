use graphoxide_core::{Extraction, Node};
use graphoxide_extract::extract_project_with_options;
use graphoxide_graph::{build_graph, find_import_cycles};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path};
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

    fn standard() -> Self {
        let project = Self::new();
        project.write("foo.mjs", "export const N = 1;\n");
        project.write("foo.ts", "export { N } from './foo.mjs';\n");
        project
    }

    fn extract(&self) -> Vec<Extraction> {
        extract_project_with_options(self.root.path(), true).unwrap()
    }
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|extraction| &extraction.nodes)
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

fn source_by_id(extractions: &[Extraction]) -> BTreeMap<String, String> {
    nodes(extractions)
        .map(|node| (node.id.clone(), node.source_file.clone()))
        .collect()
}

#[test]
fn test_cross_ext_reexport_emits_no_self_loop() {
    let result = Project::standard().extract();
    assert!(result.iter().flat_map(|value| &value.edges).all(|edge| {
        !matches!(edge.relation.as_str(), "imports_from" | "re_exports")
            || edge.true_source() != edge.true_target()
    }));
}

#[test]
fn test_cross_ext_reexport_target_is_the_sibling_node() {
    let result = Project::standard().extract();
    let typescript = file_id(&result, "foo.ts");
    let module = file_id(&result, "foo.mjs");
    assert_eq!(typescript, "foo_ts_foo");
    assert_eq!(module, "foo_mjs_foo");
    let sources = source_by_id(&result);
    let edges = result
        .iter()
        .flat_map(|value| &value.edges)
        .filter(|edge| {
            edge.true_source() == typescript
                && matches!(edge.relation.as_str(), "imports_from" | "re_exports")
        })
        .collect::<Vec<_>>();
    assert!(!edges.is_empty());
    assert!(edges.iter().all(|edge| {
        sources
            .get(edge.true_target())
            .is_some_and(|source| source == "foo.mjs")
    }));
}

#[test]
fn test_cross_ext_reexport_no_phantom_import_cycle() {
    let graph = build_graph(&Project::standard().extract()).unwrap();
    assert!(find_import_cycles(&graph, 5, 20).is_empty());
}

#[test]
fn test_same_basename_three_colliding_siblings_reexport_selects_named_variant() {
    let project = Project::standard();
    project.write("foo.cjs", "module.exports.M = 2;\n");
    let result = project.extract();
    let typescript = file_id(&result, "foo.ts");
    let mjs = file_id(&result, "foo.mjs");
    let cjs = file_id(&result, "foo.cjs");
    assert_ne!(mjs, cjs);
    assert_ne!(cjs, typescript);
    let sources = source_by_id(&result);
    let targets = result
        .iter()
        .flat_map(|value| &value.edges)
        .filter(|edge| {
            edge.true_source() == typescript
                && matches!(edge.relation.as_str(), "imports_from" | "re_exports")
        })
        .map(|edge| edge.true_target())
        .collect::<Vec<_>>();
    assert!(!targets.is_empty());
    assert!(targets.iter().all(|target| sources
        .get(*target)
        .is_some_and(|source| source == "foo.mjs")));
}

#[test]
fn test_disambiguation_strips_transient_target_file_hint() {
    let result = Project::standard().extract();
    assert!(result
        .iter()
        .flat_map(|value| &value.edges)
        .all(|edge| !edge.extra.contains_key("target_file")));
}

#[test]
fn test_target_file_hint_stripped_even_without_a_collision() {
    let project = Project::new();
    project.write("util.ts", "export const helper = 1;\n");
    project.write("main.ts", "import { helper } from './util';\n");
    let result = project.extract();
    assert!(result
        .iter()
        .flat_map(|value| &value.edges)
        .all(|edge| !edge.extra.contains_key("target_file")));
}

#[test]
fn test_graph_json_has_no_target_file_and_no_absolute_path() {
    let project = Project::standard();
    let graph = build_graph(&project.extract()).unwrap();
    let raw = serde_json::to_string(&graph).unwrap();
    assert!(!raw.contains("target_file"));
    assert!(!raw.contains(&project.root.path().to_string_lossy().to_string()));
}

fn graph_links_at(parent: &Path, directory: &str) -> Vec<serde_json::Value> {
    let root = parent.join(directory);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("foo.mjs"), "export const N = 1;\n").unwrap();
    fs::write(root.join("foo.ts"), "export { N } from './foo.mjs';\n").unwrap();
    let extractions = extract_project_with_options(&root, true).unwrap();
    let graph = build_graph(&extractions).unwrap();
    let mut links = serde_json::to_value(graph).unwrap()["links"]
        .as_array()
        .unwrap()
        .clone();
    links.sort_by_key(serde_json::Value::to_string);
    links
}

#[test]
fn test_graph_json_is_checkout_location_independent() {
    let parent = TempDir::new().unwrap();
    assert_eq!(
        graph_links_at(parent.path(), "loc_a"),
        graph_links_at(parent.path(), "loc_bbbb_longer")
    );
}

#[test]
fn test_build_drops_persisted_target_file_from_a_pre_fix_graph() {
    let legacy: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"foo_ts_foo","label":"foo.ts","source_file":"foo.ts","file_type":"code","type":"file"},
            {"id":"foo_mjs_foo","label":"foo.mjs","source_file":"foo.mjs","file_type":"code","type":"file"}
        ],
        "edges": [{
            "source":"foo_ts_foo","target":"foo_mjs_foo","relation":"imports_from",
            "context":"re-export","confidence":"EXTRACTED","source_file":"foo.ts",
            "target_file":"/some/other/checkout/foo.mjs","weight":1.0
        }]
    }))
    .unwrap();
    let graph = build_graph(&[legacy]).unwrap();
    assert_eq!(graph.links.len(), 1);
    assert!(!graph.links[0].extra.contains_key("target_file"));
}

#[test]
fn test_target_file_hint_never_written_to_the_ast_cache() {
    let project = Project::standard();
    project.extract();
    let cache = project.root.path().join("graphoxide-out/cache/ast");
    if cache.exists() {
        for entry in walk_files(&cache) {
            assert!(!fs::read_to_string(entry).unwrap().contains("target_file"));
        }
    }
}

fn walk_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).unwrap().filter_map(Result::ok) {
            if entry.path().is_dir() {
                pending.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    files
}
