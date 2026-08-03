//! One-to-one executable port of pinned Graphify
//! `tests/test_csharp_partial_classes.py`.

use graphoxide_core::Extraction;
use graphoxide_extract::extract_files;
use std::{collections::BTreeSet, fs, path::PathBuf};
use tempfile::TempDir;

fn corpus(files: &[(&str, &str)]) -> Extraction {
    let temp = TempDir::new().expect("temporary partial-class corpus");
    let mut paths = Vec::<PathBuf>::new();
    for (name, source) in files {
        let path = temp.path().join(name);
        fs::write(&path, source).expect("write C# fixture");
        paths.push(path);
    }
    let extractions = extract_files(&paths, Some(temp.path()), true)
        .expect("extract partial-class corpus")
        .extractions;
    Extraction {
        nodes: extractions
            .iter()
            .flat_map(|extraction| extraction.nodes.iter().cloned())
            .collect(),
        edges: extractions
            .iter()
            .flat_map(|extraction| extraction.edges.iter().cloned())
            .collect(),
        hyperedges: Vec::new(),
    }
}

fn calls(result: &Extraction) -> BTreeSet<(String, String)> {
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls")
        .map(|edge| (edge.true_source().to_owned(), edge.true_target().to_owned()))
        .collect()
}

fn find(result: &Extraction, label: &str, id_contains: &str) -> String {
    result
        .nodes
        .iter()
        .find(|node| node.label == label && node.id.contains(id_contains))
        .unwrap_or_else(|| panic!("missing {label} node containing {id_contains}"))
        .id
        .clone()
}

const PART_A: &str = concat!(
    "namespace App {\n",
    "public partial class Foo { public void Alpha() {} }\n",
    "}\n"
);
const PART_B: &str = concat!(
    "namespace App {\n",
    "public partial class Foo { public void Beta() { Alpha(); } }\n",
    "}\n"
);

#[test]
fn test_partial_halves_merge_to_one_class_node() {
    let result = corpus(&[("FooPartA.cs", PART_A), ("FooPartB.cs", PART_B)]);
    let foos: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| node.label == "Foo")
        .collect();
    assert_eq!(foos.len(), 1, "partial halves must have one class node");
    let methods: BTreeSet<_> = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "method" && edge.true_source() == foos[0].id)
        .map(|edge| edge.true_target())
        .collect();
    let labels: BTreeSet<_> = result
        .nodes
        .iter()
        .filter(|node| methods.contains(node.id.as_str()))
        .map(|node| node.label.as_str())
        .collect();
    assert!(labels.contains(".Alpha()"));
    assert!(labels.contains(".Beta()"));
}

#[test]
fn test_cross_file_caller_resolves_into_both_halves() {
    let result = corpus(&[
        ("FooPartA.cs", PART_A),
        ("FooPartB.cs", PART_B),
        (
            "Caller.cs",
            "namespace App { public class Caller { public void Run(Foo f) { f.Alpha(); f.Beta(); } } }\n",
        ),
    ]);
    let run = find(&result, ".Run()", "caller");
    let edges = calls(&result);
    assert!(edges.contains(&(run.clone(), find(&result, ".Alpha()", "foo"))));
    assert!(edges.contains(&(run, find(&result, ".Beta()", "foo"))));
}

#[test]
fn test_cross_half_unqualified_in_class_call_resolves() {
    let result = corpus(&[("FooPartA.cs", PART_A), ("FooPartB.cs", PART_B)]);
    assert!(calls(&result).contains(&(
        find(&result, ".Beta()", "foo"),
        find(&result, ".Alpha()", "foo")
    )));
}

#[test]
fn test_same_name_different_namespace_not_merged() {
    let result = corpus(&[
        (
            "A.cs",
            "namespace Alpha { public partial class Foo { public void FromA() {} } }\n",
        ),
        (
            "B.cs",
            "namespace Beta { public partial class Foo { public void FromB() {} } }\n",
        ),
    ]);
    assert_eq!(
        result
            .nodes
            .iter()
            .filter(|node| node.label == "Foo")
            .count(),
        2
    );
}

#[test]
fn test_non_partial_same_name_not_merged() {
    let result = corpus(&[
        (
            "A.cs",
            "namespace App { public partial class Foo { public void FromA() {} } }\n",
        ),
        (
            "B.cs",
            "namespace App { public class Foo { public void FromB() {} } }\n",
        ),
    ]);
    assert_eq!(
        result
            .nodes
            .iter()
            .filter(|node| node.label == "Foo")
            .count(),
        2
    );
}

#[test]
fn test_nested_partial_not_merged() {
    let result = corpus(&[
        (
            "A.cs",
            concat!(
                "namespace App { public partial class Outer {\n",
                "public partial class Inner { public void FromA() {} }\n",
                "} }\n"
            ),
        ),
        (
            "B.cs",
            concat!(
                "namespace App { public partial class Outer {\n",
                "public partial class Inner { public void FromB() {} }\n",
                "} }\n"
            ),
        ),
    ]);
    assert_eq!(
        result
            .nodes
            .iter()
            .filter(|node| node.label == "Outer")
            .count(),
        1
    );
    assert_eq!(
        result
            .nodes
            .iter()
            .filter(|node| node.label == "Inner")
            .count(),
        2
    );
}
