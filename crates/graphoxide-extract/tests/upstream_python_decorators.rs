use graphoxide_core::{make_id, Edge, Extraction};
use graphoxide_extract::extract_files;
use std::{fs, path::PathBuf};
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

fn symbol(file: &str, name: &str) -> String {
    let stem = std::path::Path::new(file)
        .with_extension("")
        .to_string_lossy()
        .into_owned();
    make_id(&[&stem, name])
}

fn method(file: &str, class: &str, name: &str) -> String {
    make_id(&[&symbol(file, class), name])
}

fn decorator_edges<'a>(result: &'a Extraction, owner: &str) -> Vec<&'a Edge> {
    result
        .edges
        .iter()
        .filter(|edge| {
            edge.source == owner
                && edge.relation == "references"
                && edge.extra.get("context").and_then(|value| value.as_str()) == Some("decorator")
        })
        .collect()
}

fn decorator_targets(result: &Extraction, owner: &str) -> std::collections::BTreeSet<String> {
    decorator_edges(result, owner)
        .into_iter()
        .map(|edge| edge.target.clone())
        .collect()
}

#[test]
fn test_module_level_function_decorator() {
    let result = fixture(&[(
        "pkg/consumer.py",
        "from deco import my_decorator\n\n@my_decorator\ndef business_logic():\n    pass\n",
    )]);
    assert!(
        decorator_targets(&result, &symbol("pkg/consumer.py", "business_logic"))
            .contains(&make_id(&["my_decorator"]))
    );
}

#[test]
fn test_same_file_decorator_resolves_to_local_definition() {
    let result = fixture(&[(
        "pkg/local.py",
        "def my_decorator(fn):\n    return fn\n\n@my_decorator\ndef business_logic():\n    pass\n",
    )]);
    let targets = decorator_targets(&result, &symbol("pkg/local.py", "business_logic"));
    assert!(
        targets.contains(&symbol("pkg/local.py", "my_decorator")),
        "decorator targets: {targets:?}"
    );
}

#[test]
fn test_decorator_with_arguments() {
    let result = fixture(&[(
        "pkg/args.py",
        "from deco import retry\n\n@retry(times=3)\ndef flaky():\n    pass\n",
    )]);
    assert!(
        decorator_targets(&result, &symbol("pkg/args.py", "flaky")).contains(&make_id(&["retry"]))
    );
}

#[test]
fn test_attribute_decorator_targets_the_symbol_not_the_module() {
    let result = fixture(&[(
        "pkg/web.py",
        "import app\n\n@app.route(\"/\")\ndef index():\n    pass\n",
    )]);
    let targets = decorator_targets(&result, &symbol("pkg/web.py", "index"));
    assert!(targets.contains(&make_id(&["route"])));
    assert!(!targets.contains(&make_id(&["app"])));
}

#[test]
fn test_stacked_decorators_all_emit() {
    let result = fixture(&[(
        "pkg/stack.py",
        "from deco import a, b, c\n\n@a\n@b\n@c\ndef target():\n    pass\n",
    )]);
    let targets = decorator_targets(&result, &symbol("pkg/stack.py", "target"));
    for name in ["a", "b", "c"] {
        assert!(targets.contains(&make_id(&[name])), "missing @{name}");
    }
}

#[test]
fn test_decorated_method_owner_is_class_qualified() {
    let result = fixture(&[(
        "pkg/svc.py",
        "from deco import traced\n\nclass Service:\n    @traced\n    def handle(self):\n        pass\n",
    )]);
    assert!(
        decorator_targets(&result, &method("pkg/svc.py", "Service", "handle"))
            .contains(&make_id(&["traced"]))
    );
}

#[test]
fn test_property_still_class_qualified() {
    let result = fixture(&[(
        "pkg/prop.py",
        "class Config:\n    @property\n    def name(self):\n        return 1\n",
    )]);
    assert!(result
        .nodes
        .iter()
        .any(|node| node.id == method("pkg/prop.py", "Config", "name")));
}

#[test]
fn test_decorated_class() {
    let result = fixture(&[(
        "pkg/model.py",
        "from registry import register_model\n\n@register_model\nclass Point:\n    x: int\n",
    )]);
    assert!(decorator_targets(&result, &symbol("pkg/model.py", "Point"))
        .contains(&make_id(&["register_model"])));
}

#[test]
fn test_stdlib_class_decorator_emits_no_edge() {
    let result = fixture(&[(
        "pkg/dc.py",
        "from dataclasses import dataclass\n\n@dataclass\nclass Point:\n    x: int\n",
    )]);
    assert!(decorator_edges(&result, &symbol("pkg/dc.py", "Point")).is_empty());
    assert!(!result
        .nodes
        .iter()
        .any(|node| node.id == make_id(&["dataclass"])));
}

#[test]
fn test_builtin_method_decorators_emit_no_edge_or_stub() {
    let result = fixture(&[(
        "pkg/builtins.py",
        "class Config:\n    @property\n    def name(self):\n        return 1\n\n    @staticmethod\n    def make():\n        return Config()\n",
    )]);
    assert!(decorator_edges(&result, &method("pkg/builtins.py", "Config", "name")).is_empty());
    assert!(decorator_edges(&result, &method("pkg/builtins.py", "Config", "make")).is_empty());
    for name in ["property", "staticmethod"] {
        assert!(!result.nodes.iter().any(|node| node.id == make_id(&[name])));
    }
}

#[test]
fn test_functools_wraps_does_not_rewire_onto_local_wraps() {
    let result = fixture(&[
        (
            "pkg/gift.py",
            "def wraps(thing):\n    return thing\n",
        ),
        (
            "pkg/util.py",
            "import functools\n\ndef logged(fn):\n    @functools.wraps(fn)\n    def inner(*args, **kwargs):\n        return fn(*args, **kwargs)\n    return inner\n",
        ),
    ]);
    let local_wraps = symbol("pkg/gift.py", "wraps");
    assert!(!result.edges.iter().any(|edge| {
        edge.target == local_wraps
            && edge.relation == "references"
            && edge.extra.get("context").and_then(|value| value.as_str()) == Some("decorator")
    }));
}

#[test]
fn test_undecorated_function_emits_no_decorator_edge() {
    let result = fixture(&[("pkg/plain.py", "def plain():\n    pass\n")]);
    assert!(decorator_edges(&result, &symbol("pkg/plain.py", "plain")).is_empty());
}
