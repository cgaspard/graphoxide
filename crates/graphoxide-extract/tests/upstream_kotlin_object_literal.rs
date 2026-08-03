use graphoxide_core::{Edge, Extraction, Node};
use graphoxide_extract::extract_files;
use std::{collections::BTreeSet, fs, path::PathBuf};
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

    fn write(&self, relative: &str, source: &str) {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    fn extract(&self, files: &[&str]) -> Vec<Extraction> {
        let paths = files
            .iter()
            .map(|file| self.root.path().join(file))
            .collect::<Vec<PathBuf>>();
        extract_files(&paths, Some(self.root.path()), true)
            .unwrap()
            .extractions
    }
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|extraction| &extraction.nodes)
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|extraction| &extraction.edges)
}

fn find(extractions: &[Extraction], label: &str, id_contains: &str) -> String {
    nodes(extractions)
        .find(|node| node.label == label && node.id.contains(id_contains))
        .unwrap_or_else(|| panic!("missing {label:?} containing {id_contains:?}"))
        .id
        .clone()
}

fn pairs(extractions: &[Extraction], relation: &str) -> BTreeSet<(String, String)> {
    edges(extractions)
        .filter(|edge| edge.relation == relation)
        .map(|edge| (edge.true_source().to_owned(), edge.true_target().to_owned()))
        .collect()
}

fn registry() -> (Project, Vec<Extraction>) {
    let project = Project::new();
    project.write(
        "Registry.kt",
        concat!(
            "interface EventListener {\n",
            "    fun process(e: Event)\n",
            "}\n",
            "class Event\n",
            "class Registry {\n",
            "    fun register() {\n",
            "        val listener = object : EventListener {\n",
            "            fun process(e: Event) { handleSomething(e) }\n",
            "            fun handleSomething(e: Event) { }\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["Registry.kt"]);
    (project, result)
}

#[test]
fn test_object_literal_members_get_nodes_and_method_edges() {
    let (_project, result) = registry();
    let object = find(&result, "EventListener", "object");
    let process = find(&result, ".process()", "object");
    let handle = find(&result, ".handleSomething()", "object");
    let methods = pairs(&result, "method");
    assert!(methods.contains(&(object.clone(), process)));
    assert!(methods.contains(&(object.clone(), handle)));
    let register = find(&result, ".register()", "registry");
    assert!(pairs(&result, "contains").contains(&(register, object)));
}

#[test]
fn test_object_literal_implements_supertype() {
    let (_project, result) = registry();
    let object = find(&result, "EventListener", "object");
    let interface = nodes(&result)
        .find(|node| node.label == "EventListener" && !node.id.contains("object"))
        .expect("declared EventListener")
        .id
        .clone();
    assert!(pairs(&result, "implements").contains(&(object, interface)));
}

#[test]
fn test_object_literal_member_calls_sibling_member() {
    let (_project, result) = registry();
    let process = find(&result, ".process()", "object");
    let handle = find(&result, ".handleSomething()", "object");
    assert!(pairs(&result, "calls").contains(&(process, handle)));
}

#[test]
fn test_two_object_literals_in_one_function_do_not_collide() {
    let project = Project::new();
    project.write(
        "Make.kt",
        concat!(
            "interface Alpha {\n",
            "    fun one()\n",
            "}\n",
            "interface Beta {\n",
            "    fun two()\n",
            "}\n",
            "class Maker {\n",
            "    fun make() {\n",
            "        val a = object : Alpha {\n",
            "            fun one() { }\n",
            "        }\n",
            "        val b = object : Beta {\n",
            "            fun two() { }\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["Make.kt"]);
    let alpha = find(&result, "Alpha", "object");
    let beta = find(&result, "Beta", "object");
    assert_ne!(alpha, beta);
    let one = find(&result, ".one()", "object");
    let two = find(&result, ".two()", "object");
    let methods = pairs(&result, "method");
    assert!(methods.contains(&(alpha.clone(), one.clone())));
    assert!(methods.contains(&(beta.clone(), two.clone())));
    assert!(!methods.contains(&(alpha, two)));
    assert!(!methods.contains(&(beta, one)));
}

#[test]
fn test_named_object_and_plain_class_unchanged() {
    let project = Project::new();
    project.write(
        "Mix.kt",
        concat!(
            "object Singleton {\n",
            "    fun go() { }\n",
            "}\n",
            "class Plain {\n",
            "    fun run() { go2() }\n",
            "    fun go2() { }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["Mix.kt"]);
    let singleton = find(&result, "Singleton", "");
    let plain = find(&result, "Plain", "");
    let go = find(&result, ".go()", "");
    let run = find(&result, ".run()", "");
    let go2 = find(&result, ".go2()", "");
    let methods = pairs(&result, "method");
    assert!(methods.contains(&(singleton, go)));
    assert!(methods.contains(&(plain.clone(), run.clone())));
    assert!(methods.contains(&(plain, go2.clone())));
    assert!(pairs(&result, "calls").contains(&(run, go2)));
    assert!(!nodes(&result)
        .any(|node| { node.id.contains("object") && node.label.starts_with("object@") }));
}

#[test]
fn kotlin_same_supertype_literals_do_not_fan_out_ambiguous_member_calls() {
    let project = Project::new();
    project.write(
        "Ambiguous.kt",
        concat!(
            "interface Listener {\n",
            "  fun run()\n",
            "}\n",
            "class Maker {\n",
            "  fun make() {\n",
            "    val a = object : Listener {\n",
            "      fun run() { shared() }\n",
            "      fun shared() {}\n",
            "    }\n",
            "    val b = object : Listener {\n",
            "      fun run() { shared() }\n",
            "      fun shared() {}\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["Ambiguous.kt"]);
    let object_ids = nodes(&result)
        .filter(|node| node.label == "Listener" && node.id.contains("object"))
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(object_ids.len(), 2);
    let shared = nodes(&result)
        .filter(|node| node.label == ".shared()")
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(shared.len(), 2);
    assert!(edges(&result)
        .filter(|edge| edge.relation == "calls")
        .all(|edge| !shared.contains(edge.true_target())));
}
