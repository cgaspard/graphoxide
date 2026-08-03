use graphoxide_core::{Edge, Extraction, Node};
use graphoxide_extract::{extract, extract_files};
use graphoxide_graph::build_graph;
use graphoxide_query::affected;
use std::{collections::BTreeSet, fs, path::PathBuf};
use tempfile::TempDir;

const SOURCE: &str = concat!(
    "trait Loggable\n",
    "trait Database\n",
    "trait HasConfig\n",
    "trait BaseTrait\n",
    "\n",
    "class SimpleSelfType {\n",
    "  self: HasConfig =>\n",
    "  def configure(): Unit = ()\n",
    "}\n",
    "class CompoundSelfType {\n",
    "  self: Loggable with Database =>\n",
    "  def run(): Unit = ()\n",
    "}\n",
    "class RefinedSelfType {\n",
    "  self: Loggable { def extra: Int } =>\n",
    "  def refined(): Unit = ()\n",
    "}\n",
    "class BinderOnlySelfType {\n",
    "  self =>\n",
    "  def noop(): Unit = ()\n",
    "}\n",
    "class NoSelfType { def plain(): Unit = () }\n",
    "class SelfTypeWithExtends extends BaseTrait {\n",
    "  self: Loggable =>\n",
    "  def combined(): Unit = ()\n",
    "}\n",
);

fn fixture() -> (TempDir, Extraction) {
    let root = TempDir::new().unwrap();
    let path = root.path().join("SelfTypes.scala");
    fs::write(&path, SOURCE).unwrap();
    let result = extract(&path).unwrap();
    (root, result)
}

fn nodes(extraction: &Extraction) -> impl Iterator<Item = &Node> {
    extraction.nodes.iter()
}

fn edges(extraction: &Extraction) -> impl Iterator<Item = &Edge> {
    extraction.edges.iter()
}

fn id(extraction: &Extraction, label: &str) -> String {
    nodes(extraction)
        .find(|node| node.label == label)
        .unwrap_or_else(|| panic!("missing {label:?}"))
        .id
        .clone()
}

fn pairs(extraction: &Extraction, relation: &str) -> BTreeSet<(String, String)> {
    edges(extraction)
        .filter(|edge| edge.relation == relation)
        .map(|edge| (edge.true_source().to_owned(), edge.true_target().to_owned()))
        .collect()
}

#[test]
fn test_self_type_simple_type_emits_requires() {
    let (_root, result) = fixture();
    assert!(pairs(&result, "requires")
        .contains(&(id(&result, "SimpleSelfType"), id(&result, "HasConfig"))));
}

#[test]
fn test_self_type_compound_with_emits_requires_for_each_member() {
    let (_root, result) = fixture();
    let requires = pairs(&result, "requires");
    let source = id(&result, "CompoundSelfType");
    assert!(requires.contains(&(source.clone(), id(&result, "Loggable"))));
    assert!(requires.contains(&(source, id(&result, "Database"))));
}

#[test]
fn test_self_type_structural_refinement_emits_requires_for_base_only() {
    let (_root, result) = fixture();
    assert!(pairs(&result, "requires")
        .contains(&(id(&result, "RefinedSelfType"), id(&result, "Loggable"))));
    assert!(!nodes(&result).any(|node| node.label.contains("extra")));
}

#[test]
fn test_self_type_binder_only_emits_no_requires_edge() {
    let (_root, result) = fixture();
    let source = id(&result, "BinderOnlySelfType");
    assert!(edges(&result).all(|edge| edge.relation != "requires" || edge.true_source() != source));
}

#[test]
fn test_class_without_self_type_emits_no_requires_edge() {
    let (_root, result) = fixture();
    let source = id(&result, "NoSelfType");
    assert!(edges(&result).all(|edge| edge.relation != "requires" || edge.true_source() != source));
}

#[test]
fn test_self_type_coexists_with_unrelated_extends() {
    let (_root, result) = fixture();
    let source = id(&result, "SelfTypeWithExtends");
    assert!(pairs(&result, "inherits").contains(&(source.clone(), id(&result, "BaseTrait"))));
    assert!(pairs(&result, "requires").contains(&(source, id(&result, "Loggable"))));
}

#[test]
fn test_requires_edges_carry_no_context() {
    let (_root, result) = fixture();
    assert!(edges(&result)
        .filter(|edge| edge.relation == "requires")
        .all(|edge| !edge.extra.contains_key("context")));
}

#[test]
fn test_affected_includes_self_type_dependents() {
    let (_root, result) = fixture();
    let target = id(&result, "HasConfig");
    let graph = build_graph(std::slice::from_ref(&result)).unwrap();
    let output = affected(&graph, &target, 2, &[]);
    assert!(output.contains("SimpleSelfType"), "{output}");
    assert!(output.contains("requires"), "{output}");
}

#[test]
fn scala_ambiguous_self_type_does_not_fan_out() {
    let root = TempDir::new().unwrap();
    for (name, source) in [
        ("a/Logging.scala", "trait Logging\n"),
        ("b/Logging.scala", "trait Logging\n"),
        (
            "consumer/Service.scala",
            "class Service {\n  self: Logging =>\n}\n",
        ),
    ] {
        let path = root.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let paths = [
        "a/Logging.scala",
        "b/Logging.scala",
        "consumer/Service.scala",
    ]
    .into_iter()
    .map(|name| root.path().join(name))
    .collect::<Vec<PathBuf>>();
    let result = extract_files(&paths, Some(root.path()), true)
        .unwrap()
        .extractions;
    let definitions = result
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .filter(|node| node.label == "Logging" && !node.source_file.is_empty())
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(definitions.len(), 2);
    let targets = result
        .iter()
        .flat_map(|extraction| &extraction.edges)
        .filter(|edge| edge.relation == "requires")
        .map(|edge| edge.true_target().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(targets.len(), 1);
    assert!(targets.is_disjoint(&definitions));
}
