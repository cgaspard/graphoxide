use graphoxide_core::{Edge, Extraction, Node};
use graphoxide_extract::extract_project_with_options;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_PROJECT: AtomicU64 = AtomicU64::new(0);

struct Project {
    root: PathBuf,
}

impl Project {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "graphoxide-resolution-{label}-{}-{}",
            std::process::id(),
            NEXT_PROJECT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create project fixture");
        Self { root }
    }

    fn write(&self, relative: &str, source: &str) {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
        fs::write(path, source).expect("write fixture");
    }

    fn extract(&self) -> Vec<Extraction> {
        extract_project_with_options(&self.root, true).expect("extract project")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove project fixture");
    }
}

fn definition<'a>(extractions: &'a [Extraction], label: &str, source_file: &str) -> &'a Node {
    extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .find(|node| node.label.trim_end_matches("()") == label && node.source_file == source_file)
        .unwrap_or_else(|| panic!("missing {label} in {source_file}"))
}

fn resolved_call<'a>(extractions: &'a [Extraction], caller: &Node) -> &'a Edge {
    let calls: Vec<_> = extractions
        .iter()
        .flat_map(|extraction| &extraction.edges)
        .filter(|edge| {
            edge.relation == "calls"
                && edge.true_source() == caller.id
                && !edge.extra.contains_key("unresolved_call")
        })
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "expected exactly one resolved call: {calls:?}"
    );
    calls[0]
}

fn target_source<'a>(extractions: &'a [Extraction], edge: &Edge) -> &'a Path {
    Path::new(
        extractions
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .find(|node| node.id == edge.true_target())
            .unwrap_or_else(|| panic!("missing call target {}", edge.true_target()))
            .source_file
            .as_str(),
    )
}

#[test]
fn production_callers_prefer_production_definitions_over_test_decoys() {
    let project = Project::new("production-preference");
    project.write("src/a/service.js", "export function execute() {}\n");
    project.write("tests/service.js", "export function execute() {}\n");
    project.write(
        "src/caller.js",
        r#"
import { execute } from "./a/service";
import { execute as testExecute } from "../tests/service";
export function run() { execute(); }
"#,
    );

    let extractions = project.extract();
    let caller = definition(&extractions, "run", "src/caller.js");
    assert_eq!(
        target_source(&extractions, resolved_call(&extractions, caller)),
        Path::new("src/a/service.js")
    );
}

#[test]
fn test_callers_prefer_a_definition_in_the_same_test_file() {
    let project = Project::new("test-local-preference");
    project.write("src/service.js", "export function execute() {}\n");
    project.write(
        "tests/service.js",
        r#"
import { execute as productionExecute } from "../src/service";
export function execute() {}
export function run() { execute(); }
"#,
    );

    let extractions = project.extract();
    let caller = definition(&extractions, "run", "tests/service.js");
    assert_eq!(
        target_source(&extractions, resolved_call(&extractions, caller)),
        Path::new("tests/service.js")
    );
}

#[test]
fn path_proximity_breaks_ties_after_path_classification() {
    let project = Project::new("proximity-preference");
    project.write("alpha/service.js", "export function execute() {}\n");
    project.write("beta/service.js", "export function execute() {}\n");
    project.write(
        "alpha/caller.js",
        r#"
import { execute } from "./service";
import { execute as betaExecute } from "../beta/service";
export function run() { execute(); }
"#,
    );

    let extractions = project.extract();
    let caller = definition(&extractions, "run", "alpha/caller.js");
    assert_eq!(
        target_source(&extractions, resolved_call(&extractions, caller)),
        Path::new("alpha/service.js")
    );
}
