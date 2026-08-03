//! Adversarial corpus tests for global-id collisions across language families.

use graphoxide_core::{normalize_id, Extraction};
use graphoxide_extract::extract_files;
use std::{
    collections::{BTreeSet, HashSet},
    path::Path,
};
use tempfile::TempDir;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/upstream");
const IDENTITY_CORPUS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../parity/corpora/identity-collisions"
);

fn corpus_at(root: &Path, files: &[&str]) -> Extraction {
    let cache = TempDir::new().expect("identity cache");
    let files: Vec<_> = files.iter().map(|name| root.join(name)).collect();
    let chunks = extract_files(&files, Some(cache.path()), true)
        .expect("extract identity corpus")
        .extractions;
    Extraction {
        nodes: chunks
            .iter()
            .flat_map(|chunk| chunk.nodes.iter().cloned())
            .collect(),
        edges: chunks
            .iter()
            .flat_map(|chunk| chunk.edges.iter().cloned())
            .collect(),
        hyperedges: Vec::new(),
    }
}

fn corpus(files: &[&str]) -> Extraction {
    corpus_at(Path::new(FIXTURES), files)
}

fn kind(node: &graphoxide_core::Node) -> Option<&str> {
    node.extra.get("type").and_then(|value| value.as_str())
}

fn normalized_label(node: &graphoxide_core::Node) -> String {
    normalize_id(node.label.trim_start_matches('.').trim_end_matches("()"))
}

#[test]
fn sample_geometry_modules_are_not_a_cross_language_hub() {
    let result = corpus(&["sample.jl", "sample.f90"]);
    let modules: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| kind(node) == Some("module") && normalized_label(node) == "geometry")
        .collect();
    assert_eq!(modules.len(), 2, "Julia/Fortran modules: {modules:?}");
    assert_ne!(modules[0].id, modules[1].id);
    assert!(modules
        .iter()
        .any(|node| node.source_file.ends_with("sample.jl")));
    assert!(modules
        .iter()
        .any(|node| node.source_file.ends_with("sample.f90")));

    for module in modules {
        let incident_languages: HashSet<_> = result
            .edges
            .iter()
            .filter(|edge| {
                matches!(edge.relation.as_str(), "contains" | "method")
                    && (edge.true_source() == module.id || edge.true_target() == module.id)
            })
            .filter_map(|edge| Path::new(&edge.source_file).extension()?.to_str())
            .collect();
        assert!(
            !(incident_languages.contains("jl") && incident_languages.contains("f90")),
            "sample_geometry became a cross-language hub: {incident_languages:?}"
        );
    }
}

#[test]
fn sample_base_and_sample_chattype_system_keep_import_provenance() {
    let result = corpus(&["sample.jl", "sample.m", "sample.ps1", "sample.kt"]);
    let objc_base = result
        .nodes
        .iter()
        .find(|node| node.source_file.ends_with("sample.m") && normalized_label(node) == "base")
        .expect("Objective-C Base protocol");
    let kotlin_system = result
        .nodes
        .iter()
        .find(|node| node.source_file.ends_with("sample.kt") && normalized_label(node) == "system")
        .expect("Kotlin SYSTEM enum case");

    assert!(result
        .edges
        .iter()
        .filter(|edge| {
            edge.source_file.ends_with("sample.jl")
                && matches!(edge.relation.as_str(), "imports" | "imports_from")
        })
        .all(|edge| edge.true_target() != objc_base.id));
    assert!(result
        .edges
        .iter()
        .filter(|edge| {
            edge.source_file.ends_with("sample.ps1")
                && matches!(edge.relation.as_str(), "imports" | "imports_from")
        })
        .all(|edge| edge.true_target() != kotlin_system.id));
}

#[test]
fn foo_extension_folds_but_powershell_foo_does_not() {
    let result = corpus(&[
        "swift_cross_file/Foo.swift",
        "swift_cross_file/Foo+Ext.swift",
        "sample_import.ps1",
    ]);
    let foo = result
        .nodes
        .iter()
        .find(|node| {
            node.source_file.ends_with("Foo.swift")
                && normalized_label(node) == "foo"
                && kind(node) == Some("class")
        })
        .expect("Swift Foo class");
    let two = result
        .nodes
        .iter()
        .find(|node| normalized_label(node) == "two")
        .expect("Swift extension method");
    assert!(result.edges.iter().any(|edge| {
        edge.relation == "method" && edge.true_source() == foo.id && edge.true_target() == two.id
    }));
    assert!(result
        .edges
        .iter()
        .filter(|edge| {
            edge.source_file.ends_with("sample_import.ps1")
                && matches!(edge.relation.as_str(), "imports" | "imports_from")
        })
        .all(|edge| edge.true_target() != foo.id));
}

#[test]
fn logger_and_string_external_references_are_partitioned_by_origin() {
    let logger_result = corpus(&["dynamic_import.ts", "sample.psd1"]);
    let ts_logger_targets: HashSet<_> = logger_result
        .edges
        .iter()
        .filter(|edge| {
            edge.source_file.ends_with("dynamic_import.ts")
                && matches!(edge.relation.as_str(), "imports" | "imports_from")
                && edge.true_target().contains("logger")
        })
        .map(|edge| edge.true_target())
        .collect();
    let ps_logger_targets: HashSet<_> = logger_result
        .edges
        .iter()
        .filter(|edge| {
            edge.source_file.ends_with("sample.psd1")
                && matches!(edge.relation.as_str(), "imports" | "imports_from")
                && edge.true_target().contains("logger")
        })
        .map(|edge| edge.true_target())
        .collect();
    assert!(!ts_logger_targets.is_empty(), "TS logger import missing");
    assert!(!ps_logger_targets.is_empty(), "PSD1 logger import missing");
    assert!(ts_logger_targets.is_disjoint(&ps_logger_targets));

    let string_result = corpus(&["sample.swift", "sample.ps1"]);
    let swift_string_targets: HashSet<_> = string_result
        .edges
        .iter()
        .filter(|edge| {
            edge.source_file.ends_with("sample.swift")
                && edge.relation == "references"
                && string_result
                    .nodes
                    .iter()
                    .any(|node| node.id == edge.true_target() && normalized_label(node) == "string")
        })
        .map(|edge| edge.true_target())
        .collect();
    let ps_string_targets: HashSet<_> = string_result
        .edges
        .iter()
        .filter(|edge| {
            edge.source_file.ends_with("sample.ps1")
                && edge.relation == "references"
                && string_result
                    .nodes
                    .iter()
                    .any(|node| node.id == edge.true_target() && normalized_label(node) == "string")
        })
        .map(|edge| edge.true_target())
        .collect();
    assert!(
        !swift_string_targets.is_empty(),
        "Swift String refs missing"
    );
    assert!(
        !ps_string_targets.is_empty(),
        "PowerShell string refs missing"
    );
    assert!(swift_string_targets.is_disjoint(&ps_string_targets));
}

#[test]
fn objc_swift_bridge_remains_intentional() {
    let result = corpus(&[
        "objc_mixed/Widget.h",
        "objc_mixed/Widget.m",
        "objc_mixed/WidgetExtras.swift",
    ]);
    let widgets: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| normalized_label(node) == "widget" && kind(node) == Some("class"))
        .collect();
    let widget_ids: HashSet<_> = widgets.iter().map(|node| node.id.as_str()).collect();
    assert_eq!(
        widget_ids.len(),
        1,
        "header/implementation Widget identity split: {widgets:?}"
    );
    let describe = result
        .nodes
        .iter()
        .find(|node| normalized_label(node) == "describe")
        .expect("Swift bridge method");
    assert!(result.edges.iter().any(|edge| {
        edge.relation == "method"
            && edge.true_source() == *widget_ids.iter().next().expect("Widget id")
            && edge.true_target() == describe.id
    }));
}

#[test]
fn pyi_and_cuda_cpp_interop_resolve_to_real_definitions() {
    let result = corpus_at(
        Path::new(IDENTITY_CORPUS),
        &[
            "contracts.pyi",
            "python_consumer.py",
            "helper.cuh",
            "native_consumer.cpp",
        ],
    );
    let contracts: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| normalized_label(node) == "contract")
        .collect();
    assert_eq!(contracts.len(), 1, "Contract nodes: {contracts:?}");
    assert_eq!(contracts[0].source_file, "contracts.pyi");
    assert!(result.edges.iter().any(|edge| {
        edge.relation == "references"
            && edge.source_file == "python_consumer.py"
            && edge.true_target() == contracts[0].id
    }));

    let native_helper = result
        .nodes
        .iter()
        .find(|node| normalized_label(node) == "native_helper")
        .expect("CUDA header function");
    assert_eq!(native_helper.source_file, "helper.cuh");
    assert!(result.edges.iter().any(|edge| {
        edge.relation == "calls"
            && edge.source_file == "native_consumer.cpp"
            && edge.true_target() == native_helper.id
    }));
}

#[test]
fn reusable_identity_corpus_is_unique_portable_and_order_invariant() {
    let names = [
        "Child.cs",
        "consumer.ps1",
        "consumer.ts",
        "contracts.pyi",
        "Foo+Extension.swift",
        "Foo.swift",
        "geometry.f90",
        "geometry.jl",
        "helper.cuh",
        "interfaces.py",
        "logger.ts",
        "modules.psd1",
        "native_consumer.cpp",
        "python_consumer.py",
    ];
    let forward = corpus_at(Path::new(IDENTITY_CORPUS), &names);
    let mut reversed = names;
    reversed.reverse();
    let backward = corpus_at(Path::new(IDENTITY_CORPUS), &reversed);

    let node_facts = |result: &Extraction| {
        result
            .nodes
            .iter()
            .map(|node| {
                (
                    node.id.clone(),
                    node.label.clone(),
                    node.source_file.clone(),
                    kind(node).unwrap_or("").to_owned(),
                )
            })
            .collect::<BTreeSet<_>>()
    };
    let edge_facts = |result: &Extraction| {
        result
            .edges
            .iter()
            .map(|edge| {
                (
                    edge.true_source().to_owned(),
                    edge.true_target().to_owned(),
                    edge.relation.clone(),
                    edge.source_file.clone(),
                )
            })
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(node_facts(&forward), node_facts(&backward));
    assert_eq!(edge_facts(&forward), edge_facts(&backward));

    let ids: BTreeSet<_> = forward.nodes.iter().map(|node| node.id.as_str()).collect();
    assert_eq!(ids.len(), forward.nodes.len(), "duplicate node IDs");
    assert!(forward
        .nodes
        .iter()
        .all(|node| node.source_file.is_empty() || !Path::new(&node.source_file).is_absolute()));
    assert!(forward
        .edges
        .iter()
        .all(|edge| edge.source_file.is_empty() || !Path::new(&edge.source_file).is_absolute()));

    let swift_foo = forward
        .nodes
        .iter()
        .find(|node| {
            node.source_file == "Foo.swift"
                && normalized_label(node) == "foo"
                && kind(node) == Some("class")
        })
        .expect("Swift Foo class");
    assert!(forward
        .edges
        .iter()
        .filter(|edge| {
            edge.source_file == "consumer.ps1"
                && matches!(edge.relation.as_str(), "imports" | "imports_from")
        })
        .all(|edge| edge.true_target() != swift_foo.id));

    let python_base = forward
        .nodes
        .iter()
        .find(|node| node.source_file == "interfaces.py" && normalized_label(node) == "base")
        .expect("Python Base definition");
    assert!(forward
        .edges
        .iter()
        .filter(|edge| edge.relation == "inherits" && edge.source_file == "Child.cs")
        .all(|edge| edge.true_target() != python_base.id));
    assert!(forward
        .nodes
        .iter()
        .any(|node| node.label == "Base" && node.source_file.is_empty()));
}
