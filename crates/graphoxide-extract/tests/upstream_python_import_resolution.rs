use graphoxide_core::Extraction;
use graphoxide_extract::extract_files;
use std::{collections::BTreeMap, fs, path::PathBuf};
use tempfile::TempDir;

fn fixture(files: &[(&str, &str)]) -> Extraction {
    let temp = TempDir::new().unwrap();
    let mut paths = Vec::<PathBuf>::new();
    for (relative, source) in files {
        let path = temp.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        paths.push(path);
    }
    let result = extract_files(&paths, Some(temp.path()), true).unwrap();
    let mut merged = Extraction::default();
    for extraction in result.extractions {
        merged.nodes.extend(extraction.nodes);
        merged.edges.extend(extraction.edges);
        merged.hyperedges.extend(extraction.hyperedges);
    }
    merged
}

fn node_id(result: &Extraction, label: &str, source_file: &str) -> String {
    let matches = result
        .nodes
        .iter()
        .filter(|node| node.label == label && node.source_file == source_file)
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "nodes for {label:?} in {source_file:?}");
    matches[0].clone()
}

fn has_edge(result: &Extraction, source: &str, target: &str, relation: &str) -> bool {
    result
        .edges
        .iter()
        .any(|edge| edge.source == source && edge.target == target && edge.relation == relation)
}

#[test]
fn test_python_package_reexport_resolves_import_and_call_to_origin_symbol() {
    let result = fixture(&[
        ("pkg/foo.py", "def Foo():\n    return 1\n"),
        ("pkg/__init__.py", "from .foo import Foo as PublicFoo\n"),
        (
            "app.py",
            "from pkg import PublicFoo\n\ndef X():\n    return PublicFoo()\n",
        ),
    ]);
    let origin_file = node_id(&result, "foo.py", "pkg/foo.py");
    let barrel_file = node_id(&result, "__init__.py", "pkg/__init__.py");
    let consumer_file = node_id(&result, "app.py", "app.py");
    let origin_symbol = node_id(&result, "Foo()", "pkg/foo.py");
    let consumer_symbol = node_id(&result, "X()", "app.py");
    assert!(has_edge(&result, &barrel_file, &origin_file, "re_exports"));
    assert!(has_edge(&result, &consumer_file, &origin_symbol, "imports"));
    assert!(has_edge(&result, &consumer_symbol, &origin_symbol, "calls"));
}

#[test]
fn test_python_parameter_return_and_generic_contexts() {
    let result = fixture(&[
        (
            "pkg/model.py",
            "class Payload:\n    pass\n\nclass Result:\n    pass\n",
        ),
        (
            "pkg/service.py",
            "from .model import Payload, Result\n\ndef process(item: Payload) -> Result:\n    return Result()\n\ndef process_many(items: list[Payload]) -> Result:\n    return Result()\n",
        ),
    ]);
    let labels = result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect::<BTreeMap<_, _>>();
    let pairs = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "references")
        .map(|edge| {
            (
                labels
                    .get(edge.source.as_str())
                    .copied()
                    .unwrap_or(&edge.source),
                labels
                    .get(edge.target.as_str())
                    .copied()
                    .unwrap_or(&edge.target),
                edge.extra.get("context").and_then(|value| value.as_str()),
            )
        })
        .collect::<Vec<_>>();
    assert!(pairs.contains(&("process()", "Payload", Some("parameter_type"))));
    assert!(pairs.contains(&("process()", "Result", Some("return_type"))));
    assert!(pairs.contains(&("process_many()", "Payload", Some("generic_arg"))));
}
