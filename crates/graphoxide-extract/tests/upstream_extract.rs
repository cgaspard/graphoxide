use graphoxide_core::{Confidence, Edge, Extraction, Node};
use graphoxide_extract::{
    detect::{classify_file, FileType},
    extract, extract_files, extract_files_with, extract_project_with_options_and_output,
};
use std::{collections::BTreeSet, fs};
use tempfile::TempDir;

struct Project {
    root: TempDir,
}

impl Project {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("create extraction project"),
        }
    }

    fn path(&self, relative: &str) -> std::path::PathBuf {
        self.root.path().join(relative)
    }

    fn write(&self, relative: &str, contents: &str) -> std::path::PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        fs::write(&path, contents).expect("write extraction fixture");
        path
    }

    fn single(&self, relative: &str) -> Extraction {
        extract(&self.path(relative)).expect("extract one fixture")
    }

    fn all(&self) -> Extraction {
        let parts = extract_project_with_options_and_output(
            self.root.path(),
            true,
            &self.root.path().join("graphoxide-out"),
        )
        .expect("extract project");
        merge(parts)
    }

    fn selected(&self, relatives: &[&str]) -> Extraction {
        let paths = relatives
            .iter()
            .map(|path| self.path(path))
            .collect::<Vec<_>>();
        merge(
            extract_files(&paths, Some(self.root.path()), true)
                .expect("extract selected fixtures")
                .extractions,
        )
    }
}

fn merge(parts: Vec<Extraction>) -> Extraction {
    let mut result = Extraction::default();
    for mut part in parts {
        result.nodes.append(&mut part.nodes);
        result.edges.append(&mut part.edges);
        result.hyperedges.append(&mut part.hyperedges);
    }
    result
}

fn labels(result: &Extraction) -> BTreeSet<&str> {
    result
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect()
}

fn edge_pairs<'a>(result: &'a Extraction, relation: &str) -> BTreeSet<(&'a str, &'a str)> {
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == relation)
        .map(|edge| (edge.true_source(), edge.true_target()))
        .collect()
}

fn node_id(result: &Extraction, label: &str) -> String {
    result
        .nodes
        .iter()
        .find(|node| node.label == label)
        .unwrap_or_else(|| panic!("missing node {label:?}; labels={:?}", labels(result)))
        .id
        .clone()
}

fn sample_bash(project: &Project) {
    project.write("helpers.sh", "#!/usr/bin/env bash\nhelper() { :; }\n");
    project.write(
        "sample.sh",
        "#!/usr/bin/env bash\nset -euo pipefail\nsource ./helpers.sh\n\nbuild() { echo build; }\ntest_suite() { build; }\ndeploy() { build; test_suite; }\n",
    );
}

fn sample_python(project: &Project) {
    project.write(
        "sample.py",
        "class Transformer:\n    def __init__(self, d_model: int):\n        self.d_model = d_model\n\n    def forward(self, x):\n        return x\n",
    );
}

fn sample_calls(project: &Project) {
    project.write(
        "sample_calls.py",
        "def compute_score(data):\n    return sum(data)\n\ndef normalize(value):\n    return value / 100\n\ndef run_analysis(data):\n    score = compute_score(data)\n    compute_score(data)\n    return normalize(score)\n\nclass Analyzer:\n    def process(self, data):\n        return run_analysis(data)\n",
    );
}

#[test]
fn test_make_id_strips_dots_and_underscores() {
    assert_eq!(graphoxide_core::make_id(&["_auth"]), "auth");
    assert_eq!(
        graphoxide_core::make_id(&[".httpx._client"]),
        "httpx_client"
    );
}

#[test]
fn test_make_id_consistent() {
    assert_eq!(
        graphoxide_core::make_id(&["foo", "Bar"]),
        graphoxide_core::make_id(&["foo", "Bar"])
    );
}

#[test]
fn test_make_id_no_leading_trailing_underscores() {
    let id = graphoxide_core::make_id(&["__init__"]);
    assert!(!id.starts_with('_'));
    assert!(!id.ends_with('_'));
}

#[test]
fn test_extract_python_finds_class() {
    let project = Project::new();
    sample_python(&project);
    assert!(labels(&project.single("sample.py")).contains("Transformer"));
}

#[test]
fn test_extract_python_finds_methods() {
    let project = Project::new();
    sample_python(&project);
    let result = project.single("sample.py");
    let found = labels(&result);
    assert!(found.contains(".__init__()"));
    assert!(found.contains(".forward()"));
}

#[test]
fn test_extract_python_no_dangling_edges() {
    let project = Project::new();
    sample_python(&project);
    let result = project.single("sample.py");
    let ids: BTreeSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    assert!(result
        .edges
        .iter()
        .all(|edge| ids.contains(edge.true_source())));
}

#[test]
fn test_structural_edges_are_extracted() {
    let project = Project::new();
    sample_python(&project);
    for edge in project.single("sample.py").edges.iter().filter(|edge| {
        matches!(
            edge.relation.as_str(),
            "contains" | "method" | "inherits" | "imports" | "imports_from"
        )
    }) {
        assert_eq!(edge.confidence, Confidence::Extracted);
    }
}

#[test]
fn test_extract_merges_multiple_files() {
    let project = Project::new();
    project.write("a.py", "def a():\n    return 1\n");
    project.write("b.py", "def b():\n    return a()\n");
    let result = project.all();
    assert!(labels(&result).contains("a()"));
    assert!(labels(&result).contains("b()"));
}

#[test]
fn test_extract_disambiguates_duplicate_symbol_ids_by_source_path() {
    let project = Project::new();
    project.write("apps/api/Program.cs", "class Program { void Run() {} }\n");
    project.write("tools/api/Program.cs", "class Program { void Run() {} }\n");
    let result = project.all();
    let programs: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| node.label == "Program")
        .collect();
    assert_eq!(programs.len(), 2);
    assert_ne!(programs[0].id, programs[1].id);
    let ids: BTreeSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    for edge in result
        .edges
        .iter()
        .filter(|edge| matches!(edge.relation.as_str(), "contains" | "method"))
    {
        assert!(ids.contains(edge.true_source()));
        assert!(ids.contains(edge.true_target()));
    }
}

#[test]
fn test_cpp_unresolved_base_class_stubs_stay_disambiguated_by_file() {
    let project = Project::new();
    project.write("a/Foo.cpp", "class Foo : public Base {};\n");
    project.write("b/Bar.cpp", "class Bar : public Base {};\n");
    let result = project.all();
    let stubs: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| node.label == "Base" && node.source_file.is_empty())
        .collect();
    assert_eq!(stubs.len(), 2);
    assert_ne!(stubs[0].id, stubs[1].id);
}

#[test]
fn test_cross_file_type_annotation_refs_resolve_to_single_node() {
    let project = Project::new();
    project.write(
        "pkg/thing.py",
        "class Thing:\n    def run(self):\n        return 1\n",
    );
    project.write(
        "pkg/a.py",
        "from pkg.thing import Thing\ndef use_a(obj: Thing) -> Thing:\n    return obj\n",
    );
    project.write(
        "pkg/b.py",
        "from pkg.thing import Thing\ndef use_b(obj: Thing) -> Thing:\n    return obj\n",
    );
    let result = project.all();
    let things: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| node.label == "Thing")
        .collect();
    assert_eq!(
        things.len(),
        1,
        "things={:?}",
        things.iter().map(|node| &node.id).collect::<Vec<_>>()
    );
}

#[test]
fn test_go_cross_file_type_refs_resolve_to_single_node() {
    let project = Project::new();
    project.write(
        "pkg/thing.go",
        "package pkg\ntype Thing struct{}\nfunc (t Thing) Run() int { return 1 }\n",
    );
    project.write(
        "pkg/a.go",
        "package pkg\nfunc UseA(obj Thing) Thing { return obj }\n",
    );
    project.write(
        "pkg/b.go",
        "package pkg\nfunc UseB(obj Thing) Thing { return obj }\n",
    );
    let result = project.all();
    assert_eq!(
        result
            .nodes
            .iter()
            .filter(|node| node.label == "Thing")
            .count(),
        1,
        "nodes={:?}",
        result
            .nodes
            .iter()
            .map(|node| (&node.id, &node.label, &node.source_file))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_imported_type_stubs_do_not_collide_across_source_files() {
    let project = Project::new();
    project.write(
        "pkg/a.py",
        "from pathlib import Path\ndef use_a(p: Path):\n    return p\n",
    );
    project.write(
        "pkg/b.py",
        "from pathlib import Path\ndef use_b(p: Path):\n    return p\n",
    );
    let result = project.all();
    let paths: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| node.label == "Path" && node.source_file.is_empty())
        .collect();
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0].id, paths[1].id);
}

#[test]
fn test_origin_file_is_not_serialized_into_extract_output() {
    let project = Project::new();
    project.write(
        "pkg/a.py",
        "from pathlib import Path\ndef use_a(p: Path):\n    return p\n",
    );
    project.write(
        "pkg/b.py",
        "from pathlib import Path\ndef use_b(p: Path):\n    return p\n",
    );
    let result = project.all();
    assert!(result
        .nodes
        .iter()
        .all(|node| !node.extra.contains_key("origin_file")));
    assert!(!serde_json::to_string(&result)
        .unwrap()
        .contains(project.root.path().to_string_lossy().as_ref()));
}

#[test]
fn test_go_imported_type_stubs_do_not_collide_across_source_files() {
    let project = Project::new();
    project.write(
        "a/use_a.go",
        "package a\nimport \"ext\"\nfunc UseA(w ext.Widget) {}\n",
    );
    project.write(
        "b/use_b.go",
        "package b\nimport \"ext\"\nfunc UseB(w ext.Widget) {}\n",
    );
    let result = project.all();
    let widgets: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| node.label == "Widget" && node.source_file.is_empty())
        .collect();
    assert_eq!(
        widgets.len(),
        2,
        "nodes={:?}",
        result
            .nodes
            .iter()
            .map(|node| (&node.id, &node.label, &node.source_file))
            .collect::<Vec<_>>()
    );
    assert_ne!(widgets[0].id, widgets[1].id);
}

#[test]
fn test_extract_updates_raw_call_callers_after_duplicate_id_disambiguation() {
    let project = Project::new();
    project.write(
        "apps/api/Program.cs",
        "class Program { void Run() { SharedHelper(); } }\n",
    );
    project.write("tools/api/Program.cs", "class Program { void Run() {} }\n");
    project.write(
        "shared/Helper.cs",
        "class Helper { void SharedHelper() {} }\n",
    );
    let result = project.all();
    let ids: BTreeSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    for edge in result
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls" && !edge.extra.contains_key("unresolved_call"))
    {
        assert!(ids.contains(edge.true_source()));
        assert!(ids.contains(edge.true_target()));
    }
}

#[test]
fn test_extract_rewires_unique_inheritance_stub_to_real_definition() {
    let project = Project::new();
    project.write("interfaces.py", "class BookStore:\n    pass\n");
    project.write(
        "services/BookStore.cs",
        "class SqliteBookStore : BookStore { }\n",
    );
    let result = project.all();
    let book = result
        .nodes
        .iter()
        .find(|node| node.label == "BookStore" && node.source_file == "interfaces.py")
        .unwrap();
    // Deliberate safety divergence from the pinned upstream behavior: a bare
    // C# supertype spelling is not evidence that an unrelated Python class is
    // its definition. Keep the C# stub instead of creating a cross-runtime hub.
    assert!(result
        .nodes
        .iter()
        .any(|node| node.label == "BookStore" && node.source_file.is_empty()));
    assert!(result
        .edges
        .iter()
        .filter(|edge| edge.relation == "inherits")
        .all(|edge| edge.true_target() != book.id));
}

#[test]
fn test_extract_keeps_stub_when_multiple_real_definitions_match() {
    let project = Project::new();
    project.write("a/interfaces.py", "class BookStore:\n    pass\n");
    project.write("b/interfaces.py", "class BookStore:\n    pass\n");
    project.write(
        "services/BookStore.cs",
        "class SqliteBookStore : BookStore { }\n",
    );
    let result = project.all();
    assert!(result
        .nodes
        .iter()
        .any(|node| node.label == "BookStore" && node.source_file.is_empty()));
}

#[test]
fn test_extract_does_not_rewire_inheritance_stub_to_same_named_function() {
    let project = Project::new();
    project.write("factory.py", "def BookStore():\n    return object()\n");
    project.write(
        "services/BookStore.cs",
        "class SqliteBookStore : BookStore { }\n",
    );
    let result = project.all();
    assert!(result
        .nodes
        .iter()
        .any(|node| node.label == "BookStore" && node.source_file.is_empty()));
}

#[test]
fn test_extract_does_not_rewire_constructor_method_to_same_named_class() {
    let project = Project::new();
    project.write(
        "Sample.java",
        "class DataProcessor { public DataProcessor() {} }\n",
    );
    let result = project.all();
    assert!(labels(&result).contains(".DataProcessor()"));
    assert!(result
        .edges
        .iter()
        .all(|edge| edge.true_source() != edge.true_target()));
}

#[test]
fn test_collect_files_from_dir() {
    let project = Project::new();
    project.write("a.py", "x = 1\n");
    project.write("b.ts", "export const x = 1\n");
    let files = graphoxide_extract::collect_files(project.root.path()).unwrap();
    assert_eq!(files.len(), 2);
    assert!(files
        .iter()
        .all(|path| classify_file(path) == Some(FileType::Code)));
}

#[test]
fn test_collect_files_skips_hidden() {
    let project = Project::new();
    project.write(".cache/hidden.py", "x = 1\n");
    project.write("visible.py", "x = 1\n");
    let files = graphoxide_extract::collect_files(project.root.path()).unwrap();
    assert_eq!(
        files
            .iter()
            .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("visible.py"))
            .count(),
        1
    );
    assert!(!files
        .iter()
        .any(|path| path.to_string_lossy().contains(".cache")));
}

#[test]
fn test_collect_files_follows_symlinked_directory() {
    let project = Project::new();
    project.write("real/lib.py", "x = 1\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(project.path("real"), project.path("linked")).unwrap();
    let no = graphoxide_extract::detect::detect(project.root.path(), &Default::default()).unwrap();
    let yes = graphoxide_extract::detect::detect(
        project.root.path(),
        &graphoxide_extract::detect::DetectOptions {
            follow_symlinks: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(no.files.get("code").map(Vec::len), Some(1));
    assert_eq!(yes.files.get("code").map(Vec::len), Some(1));
    assert!(yes.files["code"]
        .iter()
        .any(|path| path.ends_with("real/lib.py")));
    assert!(!yes.files["code"].iter().any(|path| path.contains("linked")));
}

#[test]
fn test_collect_files_skips_out_of_root_symlinked_directory() {
    let project = Project::new();
    let root = project.path("root");
    let outside = project.path("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.py"), "token = 1\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
    let result = graphoxide_extract::detect::detect(
        &root,
        &graphoxide_extract::detect::DetectOptions {
            follow_symlinks: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(result.files.get("code").is_none_or(Vec::is_empty));
}

#[test]
fn test_collect_files_skips_out_of_root_symlinked_file_by_default() {
    let project = Project::new();
    let root = project.path("root");
    let outside = project.write("outside/secret.py", "token = 1\n");
    fs::create_dir_all(&root).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside, root.join("secret.py")).unwrap();
    assert!(graphoxide_extract::collect_files(&root).unwrap().is_empty());
}

#[test]
fn test_collect_files_handles_circular_symlinks() {
    let project = Project::new();
    project.write("pkg/mod.py", "x = 1\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(project.root.path(), project.path("pkg/cycle")).unwrap();
    let result = graphoxide_extract::detect::detect(
        project.root.path(),
        &graphoxide_extract::detect::DetectOptions {
            follow_symlinks: true,
            ..Default::default()
        },
    )
    .unwrap();
    let code = result.files.get("code").cloned().unwrap_or_default();
    assert!(code.iter().any(|path| path.ends_with("pkg/mod.py")));
    assert!(code.len() < 4);
}

#[test]
fn test_collect_files_parity_with_legacy_on_fixtures() {
    test_collect_files_from_dir();
}

#[test]
fn test_collect_files_parity_with_legacy_synthetic() {
    let project = Project::new();
    for (path, text) in [
        ("src/app.py", "x=1"),
        ("src/deep/lib.ts", "export const x=1"),
        (".github/ci.sh", "echo hi"),
        ("node_modules/p/index.js", "x"),
        ("vendored/drop.py", "x"),
        ("vendored/keep.py", "x"),
    ] {
        project.write(path, text);
    }
    project.write(".gitignore", "vendored/*.py\n!vendored/keep.py\n");
    let names: BTreeSet<_> = graphoxide_extract::collect_files(project.root.path())
        .unwrap()
        .into_iter()
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "app.py".into(),
            "ci.sh".into(),
            "keep.py".into(),
            "lib.ts".into()
        ])
    );
}

#[test]
fn test_collect_files_walks_each_directory_once() {
    let project = Project::new();
    project.write("src/a.py", "x=1");
    project.write("node_modules/pkg/index.js", "x");
    let files = graphoxide_extract::collect_files(project.root.path()).unwrap();
    assert_eq!(
        files,
        vec![project.path("src/a.py").canonicalize().unwrap()]
    );
}

#[test]
fn test_no_dangling_edges_on_extract() {
    let project = Project::new();
    sample_calls(&project);
    let result = project.all();
    let ids: BTreeSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    for edge in result.edges.iter().filter(|edge| {
        matches!(edge.relation.as_str(), "contains" | "method" | "inherits")
            || (edge.relation == "calls" && !edge.extra.contains_key("unresolved_call"))
    }) {
        assert!(ids.contains(edge.true_source()));
        assert!(ids.contains(edge.true_target()));
    }
}

#[test]
fn test_calls_edges_emitted() {
    let project = Project::new();
    sample_calls(&project);
    assert!(project
        .single("sample_calls.py")
        .edges
        .iter()
        .any(|edge| edge.relation == "calls"));
}

#[test]
fn test_calls_edges_are_extracted() {
    let project = Project::new();
    sample_calls(&project);
    for edge in project
        .single("sample_calls.py")
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls" && !edge.extra.contains_key("unresolved_call"))
    {
        assert_eq!(edge.confidence, Confidence::Extracted);
        assert_eq!(
            edge.extra.get("weight").and_then(|value| value.as_f64()),
            Some(1.0)
        );
    }
}

#[test]
fn test_python_call_edges_have_call_context() {
    let project = Project::new();
    sample_calls(&project);
    assert!(project
        .single("sample_calls.py")
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls")
        .all(|edge| edge.extra.get("context").and_then(|v| v.as_str()) == Some("call")));
}

#[test]
fn test_calls_no_self_loops() {
    let project = Project::new();
    sample_calls(&project);
    assert!(project
        .single("sample_calls.py")
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls")
        .all(|edge| edge.true_source() != edge.true_target()));
}

fn assert_python_call(source_label: &str, target_label: &str) {
    let project = Project::new();
    sample_calls(&project);
    let result = project.single("sample_calls.py");
    let source = node_id(&result, source_label);
    let target = node_id(&result, target_label);
    assert!(edge_pairs(&result, "calls").contains(&(source.as_str(), target.as_str())));
}

#[test]
fn test_run_analysis_calls_compute_score() {
    assert_python_call("run_analysis()", "compute_score()");
}

#[test]
fn test_run_analysis_calls_normalize() {
    assert_python_call("run_analysis()", "normalize()");
}

#[test]
fn test_method_calls_module_function() {
    assert_python_call(".process()", "run_analysis()");
}

#[test]
fn test_calls_deduplication() {
    let project = Project::new();
    sample_calls(&project);
    let result = project.single("sample_calls.py");
    let calls: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls")
        .map(|edge| (edge.true_source(), edge.true_target()))
        .collect();
    assert_eq!(
        calls.len(),
        calls.iter().copied().collect::<BTreeSet<_>>().len()
    );
}

#[test]
fn test_cross_file_calls_skip_ambiguous_duplicate_labels() {
    let project = Project::new();
    project.write("caller.py", "def run():\n    log()\n");
    project.write("a.py", "def log():\n    return 'a'\n");
    project.write("b.py", "def log():\n    return 'b'\n");
    let result = project.all();
    let run = node_id(&result, "run()");
    assert!(!result.edges.iter().any(|edge| edge.relation == "calls"
        && edge.true_source() == run
        && !edge.extra.contains_key("unresolved_call")));
}

#[test]
fn test_cross_file_call_survives_same_named_test_mock() {
    assert_cross_file_mock_resolution(1);
}

#[test]
fn test_cross_file_call_survives_many_test_mocks() {
    assert_cross_file_mock_resolution(5);
}

fn assert_cross_file_mock_resolution(mock_count: usize) {
    let project = Project::new();
    project.write("src/service.py", "def save():\n    return 'real'\n");
    project.write("src/caller.py", "def run():\n    save()\n");
    for index in 0..mock_count {
        project.write(
            &format!("tests/thing{index}_test.py"),
            "def save():\n    return 'mock'\n",
        );
    }
    let result = project.all();
    let run = node_id(&result, "run()");
    let resolved: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| {
            edge.relation == "calls"
                && edge.true_source() == run
                && !edge.extra.contains_key("unresolved_call")
        })
        .collect();
    assert_eq!(resolved.len(), 1);
    let target = result
        .nodes
        .iter()
        .find(|node| node.id == resolved[0].true_target())
        .unwrap();
    assert_eq!(target.source_file, "src/service.py");
}

#[test]
fn test_cross_file_call_god_node_guard_two_real_defs() {
    let project = Project::new();
    project.write("a/svc.py", "def save():\n    return 'a'\n");
    project.write("b/svc.py", "def save():\n    return 'b'\n");
    project.write("c/caller.py", "def run():\n    save()\n");
    let result = project.all();
    let run = node_id(&result, "run()");
    assert!(!result.edges.iter().any(|edge| edge.relation == "calls"
        && edge.true_source() == run
        && !edge.extra.contains_key("unresolved_call")));
}

#[test]
fn test_extract_generic_surfaces_tree_sitter_version_mismatch_hint() {
    // Tree-sitter grammars are statically linked in Rust, so a Python package
    // version mismatch is unrepresentable. Parsing a supported language must
    // still either succeed or return a contextual Rust error.
    let project = Project::new();
    project.write("x.py", "def x():\n    return 1\n");
    assert!(extract(&project.path("x.py")).is_ok());
}

#[test]
fn test_dispatch_includes_sh_and_json() {
    let project = Project::new();
    let shell = project.write("sample.sh", "#!/bin/sh\nmain() { :; }\n");
    let json = project.write("package.json", r#"{"dependencies":{"serde":"1"}}"#);
    assert_eq!(classify_file(&shell), Some(FileType::Code));
    assert_eq!(classify_file(&json), Some(FileType::Code));
    assert!(labels(&extract(&shell).unwrap()).contains("main()"));
    assert!(labels(&extract(&json).unwrap()).contains("dependencies"));
}

#[test]
fn test_extract_bash_finds_functions() {
    let project = Project::new();
    sample_bash(&project);
    let result = project.single("sample.sh");
    let found = labels(&result);
    assert!(found.contains("build()"));
    assert!(found.contains("test_suite()"));
    assert!(found.contains("deploy()"));
}

#[test]
fn test_extract_bash_emits_defines_edges() {
    let project = Project::new();
    sample_bash(&project);
    assert!(project
        .single("sample.sh")
        .edges
        .iter()
        .any(|edge| edge.relation == "defines"));
}

#[test]
fn test_extract_bash_emits_calls_edges() {
    let project = Project::new();
    sample_bash(&project);
    let result = project.single("sample.sh");
    let calls = edge_pairs(&result, "calls");
    let deploy = node_id(&result, "deploy()");
    let suite = node_id(&result, "test_suite()");
    let build = node_id(&result, "build()");
    assert!(calls.contains(&(deploy.as_str(), build.as_str())));
    assert!(calls.contains(&(deploy.as_str(), suite.as_str())));
    assert!(calls.contains(&(suite.as_str(), build.as_str())));
}

#[test]
fn test_extract_bash_calls_have_extracted_confidence() {
    let project = Project::new();
    sample_bash(&project);
    let result = project.single("sample.sh");
    for edge in result.edges.iter().filter(|edge| edge.relation == "calls") {
        assert_eq!(edge.confidence, Confidence::Extracted);
        assert_eq!(
            edge.extra.get("context").and_then(|v| v.as_str()),
            Some("call")
        );
    }
}

#[test]
fn test_extract_bash_emits_source_imports_from() {
    let project = Project::new();
    project.write("helpers.sh", "# helper\n");
    project.write(
        "deploy.sh",
        "#!/bin/bash\nsource ./helpers.sh\nfoo() { :; }\n",
    );
    let result = project.single("deploy.sh");
    let imports: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "imports_from")
        .collect();
    assert_eq!(imports.len(), 1);
    assert_eq!(
        imports[0].extra.get("context").and_then(|v| v.as_str()),
        Some("import")
    );
}

#[test]
fn test_extract_bash_source_via_variable_path_resolves_to_real_file() {
    let project = Project::new();
    project.write("lib/gpu-discover.sh", "# helper\n");
    project.write(
        "bench.sh",
        "#!/bin/bash\nBENCH_DIR=\"$(cd \"$(dirname \"${BASH_SOURCE[0]}\")\" && pwd)\"\nsource \"${BENCH_DIR}/lib/gpu-discover.sh\"\n",
    );
    let result = project.single("bench.sh");
    let edge = result
        .edges
        .iter()
        .find(|edge| edge.relation == "imports_from")
        .expect("variable source edge");
    assert!(edge.true_target().ends_with("lib_gpu_discover"));
    assert_eq!(edge.confidence, Confidence::Inferred);
}

#[test]
fn test_extract_bash_source_via_variable_path_no_match_emits_no_dead_edge() {
    let project = Project::new();
    project.write(
        "bench.sh",
        "#!/bin/bash\nsource \"${BENCH_DIR}/lib/missing.sh\"\n",
    );
    let result = project.single("bench.sh");
    assert!(!result
        .edges
        .iter()
        .any(|edge| matches!(edge.relation.as_str(), "imports" | "imports_from")));
}

#[test]
fn test_extract_bash_emits_script_invocation_calls() {
    for command in ["./helpers.sh", "bash ./helpers.sh"] {
        let project = Project::new();
        project.write("helpers.sh", "#!/bin/bash\necho helper\n");
        project.write("deploy.sh", &format!("#!/bin/bash\n{command}\n"));
        let result = project.single("deploy.sh");
        let invocation: Vec<_> = result
            .edges
            .iter()
            .filter(|edge| {
                edge.extra.get("context").and_then(|v| v.as_str()) == Some("script_invocation")
            })
            .collect();
        assert_eq!(invocation.len(), 1, "command={command}");
        assert!(invocation[0].true_source().ends_with("deploy_sh__entry"));
        assert!(invocation[0].true_target().ends_with("helpers_sh__entry"));
    }
}

#[test]
fn test_extract_bash_skips_missing_and_shadowed_script_invocations() {
    let project = Project::new();
    project.write("helpers.sh", "#!/bin/bash\necho helper\n");
    project.write(
        "deploy.sh",
        "#!/bin/bash\nbash() { :; }\nbash ./helpers.sh\n./missing.sh\n",
    );
    let result = project.single("deploy.sh");
    assert!(!result.edges.iter().any(
        |edge| edge.extra.get("context").and_then(|v| v.as_str()) == Some("script_invocation")
    ));
}

#[test]
fn test_extract_bash_skips_dynamic_script_invocation() {
    let project = Project::new();
    project.write("helpers.sh", "#!/bin/bash\n");
    project.write("deploy.sh", "#!/bin/bash\nbash \"./$SCRIPT.sh\"\n");
    let result = project.single("deploy.sh");
    assert!(!result.edges.iter().any(
        |edge| edge.extra.get("context").and_then(|v| v.as_str()) == Some("script_invocation")
    ));
}

#[test]
fn test_extract_bash_relative_script_invocation_targets_existing_entrypoint() {
    let project = Project::new();
    project.write("helpers.sh", "#!/bin/bash\necho helper\n");
    project.write("deploy.sh", "#!/bin/bash\n./helpers.sh\n");
    let result = project.all();
    let ids: BTreeSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    let invocation = result
        .edges
        .iter()
        .find(|edge| {
            edge.extra.get("context").and_then(|v| v.as_str()) == Some("script_invocation")
        })
        .expect("script invocation");
    assert!(ids.contains(invocation.true_target()));
}

#[test]
fn test_extract_bash_attributes_script_invocation_to_function() {
    let project = Project::new();
    project.write("helpers.sh", "#!/bin/bash\n");
    project.write(
        "deploy.sh",
        "#!/bin/bash\ndeploy() { bash ./helpers.sh; }\n",
    );
    let result = project.single("deploy.sh");
    let deploy = node_id(&result, "deploy()");
    let invocation = result
        .edges
        .iter()
        .find(|edge| {
            edge.extra.get("context").and_then(|v| v.as_str()) == Some("script_invocation")
        })
        .expect("script invocation");
    assert_eq!(invocation.true_source(), deploy);
}

#[test]
fn test_extract_bash_no_self_loops() {
    let project = Project::new();
    sample_bash(&project);
    assert!(project
        .single("sample.sh")
        .edges
        .iter()
        .all(|edge| edge.true_source() != edge.true_target()));
}

#[test]
fn test_extract_bash_no_dangling_edges() {
    let project = Project::new();
    sample_bash(&project);
    let result = project.single("sample.sh");
    let ids: BTreeSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    for edge in result.edges.iter().filter(|edge| {
        !matches!(
            edge.relation.as_str(),
            "imports" | "imports_from" | "__bash_raw_call"
        )
    }) {
        assert!(ids.contains(edge.true_source()));
        assert!(ids.contains(edge.true_target()));
    }
}

#[test]
fn test_extract_bash_skip_builtins_in_calls() {
    let project = Project::new();
    sample_bash(&project);
    let result = project.single("sample.sh");
    let targets: BTreeSet<_> = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls")
        .map(|edge| edge.true_target())
        .collect();
    for builtin in [
        "echo", "cd", "set", "export", "local", "mkdir", "if", "then",
    ] {
        assert!(!targets
            .iter()
            .any(|target| target.ends_with(&format!("_{builtin}"))));
    }
}

#[test]
fn test_extract_bash_missing_grammar_returns_error() {
    // Rust links the grammar at build time: the equivalent contract is that a
    // Bash extraction cannot silently fall through to another parser.
    let project = Project::new();
    project.write("x.sh", "f() { :; }\n");
    assert!(labels(&project.single("x.sh")).contains("f()"));
}

#[test]
fn test_extract_bash_rejects_command_substitution_as_call() {
    let project = Project::new();
    project.write("x.sh", "build() { :; }\n$(build)\n");
    assert!(edge_pairs(&project.single("x.sh"), "calls").is_empty());
}

#[test]
fn test_extract_bash_process_substitution_not_recorded() {
    let project = Project::new();
    project.write("x.sh", "helper() { :; }\ndiff <(helper) <(helper)\n");
    assert!(edge_pairs(&project.single("x.sh"), "calls").is_empty());
}

#[test]
fn test_extract_bash_shadowing_function_is_recorded() {
    let project = Project::new();
    project.write("x.sh", "install() { :; }\ndeploy() { install; }\n");
    let result = project.single("x.sh");
    let calls = edge_pairs(&result, "calls");
    let deploy = node_id(&result, "deploy()");
    let install = node_id(&result, "install()");
    assert!(calls.contains(&(deploy.as_str(), install.as_str())));
}

#[test]
fn test_extract_bash_creates_entrypoint_node() {
    let project = Project::new();
    project.write("x.sh", "f() { :; }\n");
    let result = project.single("x.sh");
    let file = result
        .nodes
        .iter()
        .find(|node| node.extra.get("type").and_then(|v| v.as_str()) == Some("file"))
        .unwrap();
    let entry = result
        .nodes
        .iter()
        .find(|node| node.extra.get("type").and_then(|v| v.as_str()) == Some("bash_entrypoint"))
        .unwrap();
    assert!(edge_pairs(&result, "contains").contains(&(file.id.as_str(), entry.id.as_str())));
}

#[test]
fn test_extract_bash_top_level_call_attributes_to_entrypoint() {
    let project = Project::new();
    project.write("x.sh", "build() { :; }\nbuild\n");
    let result = project.single("x.sh");
    let entry = result
        .nodes
        .iter()
        .find(|node| node.extra.get("type").and_then(|v| v.as_str()) == Some("bash_entrypoint"))
        .unwrap();
    let build = node_id(&result, "build()");
    assert!(edge_pairs(&result, "calls").contains(&(entry.id.as_str(), build.as_str())));
}

#[test]
fn test_extract_bash_entrypoint_no_collision_with_function_named_script() {
    let project = Project::new();
    project.write("deploy.sh", "function script() { :; }\n");
    let result = project.single("deploy.sh");
    let entry = result
        .nodes
        .iter()
        .find(|node| node.extra.get("type").and_then(|v| v.as_str()) == Some("bash_entrypoint"))
        .unwrap();
    assert_ne!(entry.id, node_id(&result, "script()"));
}

#[test]
fn test_extract_bash_nested_function_calls_recorded() {
    let project = Project::new();
    project.write(
        "nested.sh",
        "do_work() { :; }\nouter() { inner() { do_work; }; inner; }\n",
    );
    let result = project.single("nested.sh");
    let inner = node_id(&result, "inner()");
    let work = node_id(&result, "do_work()");
    assert!(edge_pairs(&result, "calls").contains(&(inner.as_str(), work.as_str())));
}

#[test]
fn test_extract_bash_source_user_defined_emits_calls_not_imports_from() {
    let project = Project::new();
    project.write("helpers.sh", "#!/bin/bash\n");
    project.write("run.sh", "source() { :; }\nsource ./helpers.sh\n");
    let result = project.single("run.sh");
    assert!(!result
        .edges
        .iter()
        .any(|edge| edge.relation == "imports_from"));
    assert!(
        result
            .edges
            .iter()
            .any(|edge| edge.relation == "calls"
                && edge.true_target() == node_id(&result, "source()"))
    );
}

#[test]
fn test_extract_bash_emits_raw_calls_and_bash_sources_for_sourced_calls() {
    let project = Project::new();
    project.write("b.sh", "b_func() { :; }\n");
    project.write("a.sh", "source ./b.sh\nmain() { b_func; }\n");
    let result = project.single("a.sh");
    assert!(result
        .edges
        .iter()
        .any(|edge| edge.relation == "imports_from"));
    assert!(result
        .edges
        .iter()
        .any(|edge| edge.relation == "__bash_raw_call"
            && edge.extra.get("callee").and_then(|v| v.as_str()) == Some("b_func")));
}

#[test]
fn test_extract_bash_call_to_sourced_function_resolves() {
    let project = Project::new();
    project.write("b.sh", "b_func() { :; }\n");
    project.write("a.sh", "source ./b.sh\nmain() { b_func; }\n");
    let result = project.all();
    let main = node_id(&result, "main()");
    let function = node_id(&result, "b_func()");
    assert!(edge_pairs(&result, "calls").contains(&(main.as_str(), function.as_str())));
}

#[test]
fn test_extract_bash_sourced_call_does_not_duplicate_source_edge() {
    let project = Project::new();
    project.write("b.sh", "b_func() { :; }\n");
    project.write("a.sh", "source ./b.sh\nmain() { b_func; }\n");
    let result = project.all();
    assert_eq!(
        result
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports_from")
            .count(),
        1
    );
}

#[test]
fn test_extract_bash_call_to_external_command_stays_unlinked() {
    let project = Project::new();
    project.write("b.sh", "deploy() { :; }\n");
    project.write("a.sh", "main() { deploy; }\n");
    let result = project.all();
    let main = node_id(&result, "main()");
    let deploy = node_id(&result, "deploy()");
    assert!(!edge_pairs(&result, "calls").contains(&(main.as_str(), deploy.as_str())));
}

#[test]
fn test_extract_bash_call_into_extensionless_sourced_lib_resolves() {
    let project = Project::new();
    project.write("mylib", "#!/usr/bin/env bash\nlib_helper() { :; }\n");
    project.write("a.sh", "source ./mylib\nmain() { lib_helper; }\n");
    let result = project.all();
    let main = node_id(&result, "main()");
    let helper = node_id(&result, "lib_helper()");
    assert!(edge_pairs(&result, "calls").contains(&(main.as_str(), helper.as_str())));
}

#[test]
fn test_extract_bash_bare_source_name_resolves_to_sibling() {
    let project = Project::new();
    project.write("lib.sh", "bare_helper() { :; }\n");
    project.write("a.sh", "source lib.sh\nmain() { bare_helper; }\n");
    let result = project.all();
    assert!(result
        .edges
        .iter()
        .any(|edge| edge.relation == "imports_from" && edge.true_target() == "lib"));
    assert!(edge_pairs(&result, "calls").contains(&("a_main", "lib_bare_helper")));
}

#[test]
fn test_extract_bash_bare_source_missing_file_fabricates_nothing() {
    let project = Project::new();
    project.write("a.sh", "source nope.sh\n");
    let result = project.single("a.sh");
    assert!(!result
        .edges
        .iter()
        .any(|edge| edge.relation == "imports_from"));
}

#[test]
fn test_bash_var_sourced_function_call_resolves() {
    let project = Project::new();
    project.write("lib/util.sh", "util_fn() { :; }\n");
    project.write("bench.sh", "BENCH_DIR=\"$(cd \"$(dirname \"${BASH_SOURCE[0]}\")\" && pwd)\"\nsource \"${BENCH_DIR}/lib/util.sh\"\nmain() { util_fn; }\n");
    let result = project.all();
    assert!(edge_pairs(&result, "calls").contains(&("bench_main", "lib_util_util_fn")));
}

#[test]
fn test_extract_bash_source_suffix_guard_mid_path_variable() {
    assert_guarded_source("source \"${ROOT}/lib/${X}.sh\"\n");
}

#[test]
fn test_extract_bash_source_suffix_guard_whole_variable_path() {
    assert_guarded_source("source \"$CONFIG_FILE\"\n");
}

#[test]
fn test_extract_bash_source_suffix_guard_rejects_traversal() {
    assert_guarded_source("source \"${ROOT}/../secret.sh\"\n");
}

fn assert_guarded_source(source: &str) {
    let project = Project::new();
    project.write("lib/x.sh", "x() { :; }\n");
    project.write("secret.sh", "secret() { :; }\n");
    project.write("run.sh", source);
    let result = project.single("run.sh");
    assert!(!result
        .edges
        .iter()
        .any(|edge| matches!(edge.relation.as_str(), "imports" | "imports_from")));
}

#[test]
fn test_extract_bash_var_source_uses_tracked_assignment_base() {
    let project = Project::new();
    project.write("lib/utils.sh", "real_util() { :; }\n");
    project.write("scripts/lib/utils.sh", "decoy_util() { :; }\n");
    project.write("scripts/deploy.sh", "ROOT=\"$(cd \"$(dirname \"${BASH_SOURCE[0]}\")/..\" && pwd)\"\nsource \"${ROOT}/lib/utils.sh\"\n");
    let result = project.single("scripts/deploy.sh");
    let target = result
        .edges
        .iter()
        .find(|edge| edge.relation == "imports_from")
        .expect("tracked source")
        .true_target();
    assert!(target.ends_with("lib_utils"));
    assert!(!target.contains("scripts_lib_utils"));
}

#[test]
fn test_extract_bash_var_source_script_dir_idiom_still_resolves() {
    let project = Project::new();
    project.write("lib/x.sh", "x() { :; }\n");
    project.write(
        "bench.sh",
        "DIR=\"$(cd \"$(dirname \"${BASH_SOURCE[0]}\")\" && pwd)\"\nsource \"${DIR}/lib/x.sh\"\n",
    );
    assert!(project
        .single("bench.sh")
        .edges
        .iter()
        .any(|edge| edge.relation == "imports_from"));
}

#[test]
fn test_extract_bash_var_source_untracked_var_keeps_script_dir_guess() {
    let project = Project::new();
    project.write("lib/y.sh", "y() { :; }\n");
    project.write("run.sh", "source \"${EXTERNAL}/lib/y.sh\"\n");
    assert!(project
        .single("run.sh")
        .edges
        .iter()
        .any(|edge| edge.relation == "imports_from"));
}

fn sample_json(project: &Project) {
    project.write(
        "sample.json",
        r#"{"name":"my-app","version":"1.0","scripts":{"build":"tsc"},"dependencies":{"react":"18","axios":"1"},"devDependencies":{"typescript":"5"}}"#,
    );
}

#[test]
fn test_extract_json_top_level_keys() {
    let project = Project::new();
    sample_json(&project);
    let result = project.single("sample.json");
    let found = labels(&result);
    assert!(found.contains("name"));
    assert!(found.contains("version"));
}

#[test]
fn test_extract_json_nested_contains() {
    let project = Project::new();
    sample_json(&project);
    let result = project.single("sample.json");
    assert!(result.edges.iter().any(|edge| edge.relation == "contains"
        && result
            .nodes
            .iter()
            .any(|node| node.id == edge.target && node.label == "build")));
}

#[test]
fn test_extract_json_dependencies_become_imports() {
    let project = Project::new();
    sample_json(&project);
    let result = project.single("sample.json");
    let imports: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "imports")
        .collect();
    assert!(imports.iter().any(|edge| edge.true_target() == "react"));
    assert!(imports.iter().any(|edge| edge.true_target() == "axios"));
    assert!(imports
        .iter()
        .any(|edge| edge.true_target() == "typescript"));
}

#[test]
fn test_extract_json_extends_resolved() {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"extends":"./base.json","compilerOptions":{}}"#,
    );
    let result = project.single("tsconfig.json");
    let ids: BTreeSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    let edge = result
        .edges
        .iter()
        .find(|edge| edge.relation == "extends")
        .expect("extends edge");
    assert!(ids.contains(edge.true_target()));
}

#[test]
fn test_extract_json_import_and_extends_targets_are_real_nodes() {
    let project = Project::new();
    project.write(
        "package.json",
        r#"{"dependencies":{"serde":"1"},"extends":["./base.json"]}"#,
    );
    let result = project.single("package.json");
    let ids: BTreeSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    for edge in result
        .edges
        .iter()
        .filter(|edge| matches!(edge.relation.as_str(), "imports" | "extends"))
    {
        assert!(ids.contains(edge.true_target()));
    }
}

#[test]
fn test_extract_json_large_file_is_a_bounded_structured_diagnostic() {
    let project = Project::new();
    let path = project.path("package.json");
    fs::write(&path, vec![b' '; 1_048_577]).unwrap();
    let result = extract(&path).expect("large generic JSON is a diagnostic, not a crash");
    let root = result.nodes.first().expect("structured diagnostic root");
    assert_eq!(
        root.extra
            .get("structured_format")
            .and_then(serde_json::Value::as_str),
        Some("json")
    );
    assert!(root.extra.contains_key("structured_diagnostics"));
}

#[test]
fn test_extract_json_handles_invalid_json_as_a_structured_diagnostic() {
    let project = Project::new();
    let path = project.write("package.json", "{invalid");
    let result = extract(&path).expect("malformed generic JSON is contained");
    let root = result.nodes.first().expect("structured diagnostic root");
    assert!(root.extra.contains_key("structured_diagnostics"));
}

#[test]
fn test_extract_json_no_self_loops() {
    let project = Project::new();
    sample_json(&project);
    assert!(project
        .single("sample.json")
        .edges
        .iter()
        .all(|edge| edge.true_source() != edge.true_target()));
}

#[test]
fn test_extract_json_data_file_is_structurally_indexed() {
    let project = Project::new();
    project.write("records.json", r#"{"users":[{"id":1}]}"#);
    let result = project.single("records.json");
    assert!(result.nodes.iter().any(|node| {
        node.extra
            .get("structured_format")
            .and_then(serde_json::Value::as_str)
            == Some("json")
    }));
}

#[test]
fn test_extract_json_top_level_array_is_structurally_indexed() {
    let project = Project::new();
    project.write("records.json", "[1,2,3]");
    let result = project.single("records.json");
    assert!(result.nodes.iter().any(|node| {
        node.extra
            .get("structured_format")
            .and_then(serde_json::Value::as_str)
            == Some("json")
    }));
}

#[test]
fn test_extract_json_config_by_filename_still_extracted() {
    let project = Project::new();
    project.write("tsconfig.json", r#"{"compilerOptions":{"strict":true}}"#);
    assert!(!project.single("tsconfig.json").nodes.is_empty());
}

#[test]
fn test_extract_json_config_by_key_probe() {
    let project = Project::new();
    project.write("weird-name.json", r#"{"dependencies":{"lodash":"4"}}"#);
    assert!(project
        .single("weird-name.json")
        .edges
        .iter()
        .any(|edge| edge.relation == "imports" && edge.true_target() == "lodash"));
}

#[test]
fn test_extract_bash_via_dispatch() {
    let project = Project::new();
    project.write("foo.bash", "main() { :; }\n");
    assert!(labels(&project.single("foo.bash")).contains("main()"));
}

#[test]
fn test_extract_json_via_dispatch() {
    let project = Project::new();
    project.write("package.json", r#"{"dependencies":{"x":"1"}}"#);
    assert!(labels(&project.single("package.json")).contains("dependencies"));
}

#[test]
fn test_extensionless_shebang_via_dispatch() {
    let project = Project::new();
    project.write("devctl", "#!/usr/bin/env -S bash -eu\nmain() { :; }\n");
    project.write(
        "manage",
        "#!/usr/bin/env python3\ndef main():\n    return 1\n",
    );
    assert!(labels(&project.single("devctl")).contains("main()"));
    assert!(labels(&project.single("manage")).contains("main()"));
}

#[test]
// NOTE: This test name is pinned by the Graphify differential parity contract
// (parity/mappings/extract.json) to the upstream test ID
// `test_extensionless_without_usable_shebang_stays_unsupported`; do not rename
// it. "Unsupported" here means no language extractor claims the file — the file
// still stays unsupported as source, it is now additionally inventoried.
fn test_extensionless_without_usable_shebang_stays_unsupported() {
    // Issue #34: extensionless, no-registered-language files no longer vanish
    // with zero graph facts. Instead each in-scope regular file is emitted as a
    // deterministic bounded inventory node so it is visible as graph evidence.
    // A perl shebang with no registered extractor is *not* "usable" the way a
    // bash/python shebang is: it has no extension and no extractor claims it,
    // so it too becomes an inventory node rather than being dropped.
    let project = Project::new();
    project.write("LICENSE-COPY", "plain text\n");
    project.write("legacy", "#!/usr/bin/env perl\nprint 1;\n");

    for name in ["LICENSE-COPY", "legacy"] {
        let extraction = project.single(name);
        assert_eq!(
            extraction.nodes.len(),
            1,
            "{name} should be one inventory node"
        );
        let node = &extraction.nodes[0];
        assert_eq!(node.file_type, "document");
        assert_eq!(node.extra["type"], "format_inventory");
        assert_eq!(node.extra["format"], "unsupported_file");
        assert_eq!(node.extra["format_capability"], "inventory_only");
        assert_eq!(node.extra["parse_status"], "inventory_only");
        assert!(extraction.edges.is_empty());
    }
}

#[test]
fn test_extract_extensionless_bash_cli_end_to_end() {
    let project = Project::new();
    project.write(
        "devctl",
        "#!/usr/bin/env bash\nhelper() { :; }\nmain() { helper; }\nmain \"$@\"\n",
    );
    let result = project.all();
    let ids: BTreeSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    assert!(ids.contains("devctl_helper"));
    assert!(ids.contains("devctl_main"));
}

#[test]
fn test_extract_bash_node_metadata_is_sanitized() {
    let project = Project::new();
    project.write("x.sh", "main() { :; }\n");
    for node in project.single("x.sh").nodes {
        let serialized = node
            .extra
            .get("metadata")
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(!serialized.contains('<'));
        assert!(!serialized.contains('\0'));
    }
}

fn call_between(result: &Extraction, caller: &str, callee: &str) -> bool {
    result.edges.iter().any(|edge| {
        if edge.relation != "calls" {
            return false;
        }
        let source = result
            .nodes
            .iter()
            .find(|node| node.id == edge.true_source());
        let target = result
            .nodes
            .iter()
            .find(|node| node.id == edge.true_target());
        source.is_some_and(|node| node.label.contains(caller))
            && target.is_some_and(|node| node.label.contains(callee))
    })
}

fn sample_cjs(project: &Project) {
    project.write(
        "foundation.js",
        "function loadFoundation() {}\nfunction validateConfig() {}\n",
    );
    project.write("utils.js", "function log() {}\n");
    project.write("helpers.js", "function helperFn() {}\n");
    project.write(
        "cjs_require.js",
        "const { loadFoundation, validateConfig } = require('./foundation');\nconst utils = require('./utils');\nconst helper = require('./helpers').helperFn;\nfunction runDispatch() { loadFoundation({}); validateConfig({}); utils.log('go'); helper(); }\nmodule.exports = { runDispatch };\n",
    );
}

#[test]
fn test_extract_js_destructured_require_imports_from() {
    let project = Project::new();
    sample_cjs(&project);
    let targets: Vec<_> = project
        .single("cjs_require.js")
        .edges
        .into_iter()
        .filter(|edge| edge.relation == "imports_from")
        .map(|edge| edge.target)
        .collect();
    for expected in ["foundation", "utils", "helpers"] {
        assert!(
            targets.iter().any(|target| target.contains(expected)),
            "targets={targets:?}"
        );
    }
}

#[test]
fn test_extract_js_destructured_require_named_symbols() {
    let project = Project::new();
    sample_cjs(&project);
    let targets: BTreeSet<_> = project
        .single("cjs_require.js")
        .edges
        .into_iter()
        .filter(|edge| edge.relation == "imports")
        .map(|edge| edge.target)
        .collect();
    assert!(targets
        .iter()
        .any(|target| target.ends_with("foundation_loadfoundation")));
    assert!(targets
        .iter()
        .any(|target| target.ends_with("foundation_validateconfig")));
}

#[test]
fn test_extract_js_member_require_emits_property_symbol() {
    let project = Project::new();
    sample_cjs(&project);
    assert!(project.single("cjs_require.js").edges.iter().any(|edge| {
        edge.relation == "imports" && edge.true_target().ends_with("helpers_helperfn")
    }));
}

#[test]
fn test_extract_js_arrow_function_still_extracted() {
    let project = Project::new();
    project.write("arrow.js", "const greet = () => console.log('hi');\n");
    assert!(labels(&project.single("arrow.js")).contains("greet()"));
}

#[test]
fn test_extract_js_this_assigned_methods() {
    let project = Project::new();
    project.write(
        "dao.js",
        "function UserDAO(db) { this.addUser = (name) => name; this.getUser = function(id) { return id; }; }\n",
    );
    let result = project.single("dao.js");
    let owner = node_id(&result, "UserDAO()");
    for method in [".addUser()", ".getUser()"] {
        let target = node_id(&result, method);
        assert!(edge_pairs(&result, "method").contains(&(owner.as_str(), target.as_str())));
    }
}

#[test]
fn test_extract_js_commonjs_exports_assignment() {
    let project = Project::new();
    project.write(
        "mod.js",
        "exports.alpha = (x) => x;\nmodule.exports.beta = function(y) { return y; };\n",
    );
    let result = project.single("mod.js");
    let found = labels(&result);
    assert!(found.contains("alpha()"));
    assert!(found.contains("beta()"));
}

#[test]
fn test_extract_js_prototype_method_assignment() {
    let project = Project::new();
    project.write(
        "proto.js",
        "function Foo() {}\nFoo.prototype.bar = function() { return 1; };\n",
    );
    let result = project.single("proto.js");
    let found = labels(&result);
    assert!(found.contains("Foo()"));
    assert!(found.contains(".bar()"));
}

#[test]
fn test_extract_js_const_function_expression() {
    let project = Project::new();
    project.write(
        "fnexpr.js",
        "const handler = function(req, res) { return res; };\n",
    );
    assert!(labels(&project.single("fnexpr.js")).contains("handler()"));
}

#[test]
fn test_extract_ts_class_arrow_field() {
    let project = Project::new();
    project.write(
        "comp.ts",
        "class Widget { onClick = (e) => e; render() { return null; } }\n",
    );
    let result = project.single("comp.ts");
    let found = labels(&result);
    assert!(found.contains("Widget"));
    assert!(found.contains(".onClick()"));
    assert!(found.contains(".render()"));
}

#[test]
fn test_extract_js_arbitrary_member_assignment_not_captured() {
    let project = Project::new();
    project.write("noise.js", "const obj = {};\nobj.whatever = () => 1;\n");
    let result = project.single("noise.js");
    let found = labels(&result);
    assert!(!found.contains("whatever()"));
    assert!(!found.contains(".whatever()"));
}

#[test]
fn test_cross_file_call_promoted_to_extracted_with_import_evidence() {
    let project = Project::new();
    project.write(
        "caller.js",
        "const { doWork } = require('./lib');\nfunction run() { doWork(); }\n",
    );
    project.write(
        "lib.js",
        "function doWork() { return 1; }\nmodule.exports = { doWork };\n",
    );
    let result = project.all();
    let edge = result
        .edges
        .iter()
        .find(|edge| {
            edge.relation == "calls"
                && result
                    .nodes
                    .iter()
                    .any(|node| node.id == edge.true_source() && node.label == "run()")
                && result
                    .nodes
                    .iter()
                    .any(|node| node.id == edge.true_target() && node.label == "doWork()")
        })
        .expect("import-backed cross-file call");
    assert_eq!(edge.confidence, Confidence::Extracted);
}

#[test]
fn test_js_cross_file_call_without_import_emits_no_edge() {
    let project = Project::new();
    project.write("caller.js", "function run() { doUnique(); }\n");
    project.write("lib.js", "function doUnique() { return 1; }\n");
    assert!(!call_between(&project.all(), "run", "doUnique"));
}

fn sample_barrel(project: &Project) {
    project.write(
        "barrel_reexport.ts",
        "export { readCookie, writeCookie } from './cookieHelpers';\nexport * from './storageHelpers';\nexport { basePathRewrite, getFullUrl } from './urlHelpers';\nexport function localHelper() { return 'local'; }\nexport const LOCAL_CONST = 42;\n",
    );
}

#[test]
fn test_barrel_reexport_emits_re_exports_edges() {
    let project = Project::new();
    sample_barrel(&project);
    let targets: Vec<_> = project
        .single("barrel_reexport.ts")
        .edges
        .into_iter()
        .filter(|edge| edge.relation == "re_exports")
        .map(|edge| edge.target)
        .collect();
    for symbol in ["readcookie", "writecookie", "getfullurl", "basepathrewrite"] {
        assert!(
            targets.iter().any(|target| target.contains(symbol)),
            "targets={targets:?}"
        );
    }
}

#[test]
fn test_barrel_reexport_emits_imports_from() {
    let project = Project::new();
    sample_barrel(&project);
    let targets: Vec<_> = project
        .single("barrel_reexport.ts")
        .edges
        .into_iter()
        .filter(|edge| edge.relation == "imports_from")
        .map(|edge| edge.target)
        .collect();
    for module in ["cookiehelpers", "urlhelpers", "storagehelpers"] {
        assert!(
            targets.iter().any(|target| target.contains(module)),
            "targets={targets:?}"
        );
    }
}

#[test]
fn test_barrel_reexport_context_tagged() {
    let project = Project::new();
    sample_barrel(&project);
    let reexports: Vec<_> = project
        .single("barrel_reexport.ts")
        .edges
        .into_iter()
        .filter(|edge| edge.relation == "re_exports")
        .collect();
    assert!(!reexports.is_empty());
    assert!(reexports.iter().all(|edge| {
        edge.extra.get("context").and_then(|value| value.as_str()) == Some("re-export")
    }));
}

#[test]
fn test_barrel_local_exports_still_extracted() {
    let project = Project::new();
    sample_barrel(&project);
    let result = project.single("barrel_reexport.ts");
    let found = labels(&result);
    assert!(found.contains("localHelper()"));
    assert!(found.contains("barrel_reexport.ts"));
}

#[test]
fn test_barrel_reexport_confidence_extracted() {
    let project = Project::new();
    sample_barrel(&project);
    let result = project.single("barrel_reexport.ts");
    assert!(result
        .edges
        .iter()
        .filter(|edge| edge.relation == "re_exports")
        .all(|edge| edge.confidence == Confidence::Extracted));
}

fn sample_tsx(project: &Project) {
    project.write(
        "sample.tsx",
        "function fmtDate(d: Date): string { return d.toISOString(); }\nfunction fmtCount(n: number): string { return `${n} items`; }\nexport function App() { const now = new Date(); return (<div><span>{fmtDate(now)}</span><span>{fmtCount(42)}</span></div>); }\n",
    );
}

#[test]
fn test_extract_tsx_finds_helpers_and_component() {
    let project = Project::new();
    sample_tsx(&project);
    let result = project.single("sample.tsx");
    let found = labels(&result);
    for label in ["fmtDate()", "fmtCount()", "App()"] {
        assert!(found.contains(label), "labels={found:?}");
    }
}

#[test]
fn test_extract_tsx_jsx_expression_calls_resolve() {
    let project = Project::new();
    sample_tsx(&project);
    let result = project.single("sample.tsx");
    assert!(call_between(&result, "App", "fmtDate"));
    assert!(call_between(&result, "App", "fmtCount"));
}

#[test]
fn test_extract_tsx_uses_tsx_grammar() {
    let project = Project::new();
    sample_tsx(&project);
    assert!(labels(&project.single("sample.tsx")).contains("App()"));
}

#[test]
fn test_semantic_reference_edges_carry_context_and_source() {
    let project = Project::new();
    project.write(
        "Foo.cs",
        "class Foo { Bar Use(Bar value) { return value; } }\n",
    );
    let result = project.single("Foo.cs");
    let edge = result
        .edges
        .iter()
        .find(|edge| {
            edge.relation == "references"
                && edge.extra.get("context").and_then(|value| value.as_str())
                    == Some("parameter_type")
        })
        .expect("parameter reference edge");
    assert_eq!(edge.confidence, Confidence::Extracted);
    assert!(edge.source_file.ends_with("Foo.cs"));
    assert!(edge.extra.contains_key("source_location"));
}

#[test]
fn test_pure_export_no_from_not_treated_as_reexport() {
    let project = Project::new();
    project.write("local.ts", "const x = 1;\nexport { x };\n");
    assert!(project
        .single("local.ts")
        .edges
        .iter()
        .all(|edge| edge.relation != "re_exports"));
}

fn resolved_call<'a>(
    result: &'a Extraction,
    caller: &str,
    callee: &str,
) -> Option<&'a graphoxide_core::Edge> {
    result.edges.iter().find(|edge| {
        edge.relation == "calls"
            && !edge.extra.contains_key("unresolved_call")
            && result
                .nodes
                .iter()
                .any(|node| node.id == edge.true_source() && node.label.contains(caller))
            && result
                .nodes
                .iter()
                .any(|node| node.id == edge.true_target() && node.label.contains(callee))
    })
}

#[test]
fn test_python_qualified_class_method_call_resolves_extracted() {
    let project = Project::new();
    project.write(
        "actions.py",
        "class TaskActions:\n    @staticmethod\n    def approve(pk):\n        return pk\n",
    );
    project.write(
        "viewset.py",
        "from actions import TaskActions\nclass TaskViewSet:\n    def handle(self, request):\n        return TaskActions.approve(request)\n",
    );
    let result = project.selected(&["viewset.py", "actions.py"]);
    let edge = resolved_call(&result, "handle", "approve").expect("qualified class call");
    assert_eq!(edge.confidence, Confidence::Extracted);
}

#[test]
fn test_degenerate_symbol_name_does_not_leak_absolute_id() {
    let project = Project::new();
    project.write(
        "vendor.js",
        "function $(){return 1}\nfunction real(){return 2}\n",
    );
    let result = project.selected(&["vendor.js"]);
    let marker = project.root.path().to_string_lossy();
    assert!(result
        .nodes
        .iter()
        .all(|node| !node.id.contains(marker.as_ref())));
    let found = labels(&result);
    assert!(found.contains("real()"));
    assert!(!found.contains("$()"));
}

#[test]
fn test_out_of_tree_cache_root_keeps_source_file_relative_to_scan_root() {
    let project = Project::new();
    project.write(
        "corpus/src/Data/Database/RepositoryTests/order_repository_tests.py",
        "class OrderRepositoryTests:\n    def test_get(self):\n        return 1\n",
    );
    let scan_root = project.path("corpus");
    let out = project.path("a/b/c/d/out");
    fs::create_dir_all(&out).unwrap();
    let result = merge(
        extract_project_with_options_and_output(&scan_root, true, &out)
            .expect("extract with separate output"),
    );
    let source_files: BTreeSet<_> = result
        .nodes
        .iter()
        .filter(|node| !node.source_file.is_empty())
        .map(|node| node.source_file.as_str())
        .collect();
    assert_eq!(
        source_files,
        BTreeSet::from(["src/Data/Database/RepositoryTests/order_repository_tests.py"])
    );
    assert!(result.nodes.iter().all(|node| {
        !node
            .id
            .contains(project.root.path().to_string_lossy().as_ref())
            && !node
                .source_file
                .contains(project.root.path().to_string_lossy().as_ref())
    }));
}

#[test]
fn test_c_include_out_of_root_target_id_is_portable() {
    let project = Project::new();
    project.write("lib/foo.h", "int foo_compute(int x);\n");
    project.write(
        "app/main.c",
        "#include \"../lib/foo.h\"\nint main(void) { return foo_compute(1); }\n",
    );
    let result = merge(
        extract_files(
            &[project.path("app/main.c")],
            Some(&project.path("app")),
            true,
        )
        .unwrap()
        .extractions,
    );
    let edge = result
        .edges
        .iter()
        .find(|edge| edge.relation == "imports")
        .expect("C include");
    assert_eq!(edge.true_target(), "ext_lib_foo_h");
    let marker = project.root.path().to_string_lossy();
    assert!(!edge.true_target().contains(marker.as_ref()));
    assert!(!edge.extra.contains_key("target_file"));
}

#[test]
fn test_c_include_out_of_root_target_id_is_deterministic_across_checkout_paths() {
    let project = Project::new();
    let build = |base: &str| {
        project.write(&format!("{base}/lib/foo.h"), "int foo_compute(int x);\n");
        project.write(
            &format!("{base}/app/main.c"),
            "#include \"../lib/foo.h\"\nint main(void) { return foo_compute(1); }\n",
        );
        let path = project.path(&format!("{base}/app/main.c"));
        let app = project.path(&format!("{base}/app"));
        let output = extract_files(&[path], Some(&app), true).unwrap();
        merge(output.extractions)
            .edges
            .into_iter()
            .find(|edge| edge.relation == "imports")
            .unwrap()
            .target
    };
    assert_eq!(build("checkout_alice/deeper/nesting"), "ext_lib_foo_h");
    assert_eq!(
        build("checkout_bob_at_a_totally_different_nesting_depth/deeper/nesting"),
        "ext_lib_foo_h"
    );
}

#[test]
fn test_c_include_in_root_same_batch_still_resolves_to_real_node() {
    let project = Project::new();
    project.write("lib/foo.h", "int foo_compute(int x);\n");
    project.write(
        "app/main.c",
        "#include \"../lib/foo.h\"\nint main(void) { return foo_compute(1); }\n",
    );
    let result = project.selected(&["app/main.c", "lib/foo.h"]);
    let header = result
        .nodes
        .iter()
        .find(|node| {
            node.source_file == "lib/foo.h"
                && node.extra.get("type").and_then(|v| v.as_str()) == Some("file")
        })
        .expect("header file node");
    let include = result
        .edges
        .iter()
        .find(|edge| edge.relation == "imports" && edge.source_file == "app/main.c")
        .expect("in-root include");
    assert_eq!(include.true_target(), header.id);
    assert!(!include.true_target().starts_with("ext_"));
}

#[test]
fn test_python_relative_import_out_of_root_target_id_is_portable() {
    let project = Project::new();
    project.write("app/__init__.py", "");
    project.write("lib/__init__.py", "");
    project.write("lib/mod.py", "def compute(x):\n    return x + 1\n");
    project.write(
        "app/main.py",
        "from ..lib.mod import compute\ndef run():\n    return compute(1)\n",
    );
    let result = merge(
        extract_files(
            &[project.path("app/main.py"), project.path("app/__init__.py")],
            Some(&project.path("app")),
            true,
        )
        .unwrap()
        .extractions,
    );
    let imports: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "imports_from")
        .collect();
    assert!(!imports.is_empty());
    assert_eq!(imports[0].true_target(), "ext_lib_mod_py");
    let marker = project.root.path().to_string_lossy();
    assert!(imports
        .iter()
        .all(|edge| !edge.true_target().contains(marker.as_ref())));
}

#[test]
fn test_python_relative_import_underflow_cannot_bind_root_collision() {
    let project = Project::new();
    project.write("foo.py", "def x():\n    return 1\n");
    project.write("pkg/main.py", "from ...foo import x\n");
    project.write("pkg/valid.py", "from ..foo import x\n");

    let result = project.selected(&["foo.py", "pkg/main.py", "pkg/valid.py"]);
    let root_foo = result
        .nodes
        .iter()
        .find(|node| {
            node.source_file == "foo.py"
                && node.extra.get("type").and_then(|value| value.as_str()) == Some("file")
        })
        .expect("root foo file");

    assert!(!result
        .edges
        .iter()
        .any(|edge| edge.source_file == "pkg/main.py" && edge.relation == "imports_from"));
    assert!(result.edges.iter().any(|edge| {
        edge.source_file == "pkg/valid.py"
            && edge.relation == "imports_from"
            && edge.true_target() == root_foo.id
    }));
}

#[test]
fn test_python_module_qualified_call_resolves_extracted() {
    let project = Project::new();
    project.write("mathlib.py", "def compute(x):\n    return x * 2\n");
    project.write(
        "caller.py",
        "import mathlib\ndef use_qualified(n):\n    return mathlib.compute(n)\n",
    );
    let result = project.selected(&["caller.py", "mathlib.py"]);
    let edge = resolved_call(&result, "use_qualified", "compute").expect("module call");
    assert_eq!(edge.confidence, Confidence::Extracted);
}

#[test]
fn test_python_module_qualified_call_requires_the_import() {
    let project = Project::new();
    project.write("mathlib.py", "def compute(x):\n    return x * 2\n");
    project.write("caller.py", "def via_obj(o):\n    return o.compute(3)\n");
    let result = project.selected(&["caller.py", "mathlib.py"]);
    assert!(resolved_call(&result, "via_obj", "compute").is_none());
}

fn sample_python_alias(project: &Project, caller: &str, import: &str, receiver: &str) {
    project.write("pkg/__init__.py", "");
    project.write(
        "pkg/gate.py",
        "def validate(rows):\n    return bool(rows)\n",
    );
    project.write(
        caller,
        &format!("{import}\ndef use_alias(rows):\n    return {receiver}.validate(rows)\n"),
    );
}

#[test]
fn test_python_from_import_alias_module_call_resolves() {
    let project = Project::new();
    sample_python_alias(
        &project,
        "pkg/caller.py",
        "from pkg import gate as m_gate\n",
        "m_gate",
    );
    let result = project.selected(&["pkg/caller.py", "pkg/gate.py", "pkg/__init__.py"]);
    assert!(resolved_call(&result, "use_alias", "validate").is_some());
    assert!(result
        .edges
        .iter()
        .all(|edge| !edge.extra.contains_key("local_alias")));
}

#[test]
fn test_python_import_as_alias_module_call_resolves() {
    let project = Project::new();
    project.write("mathlib.py", "def compute(x):\n    return x * 2\n");
    project.write(
        "caller.py",
        "import mathlib as m\ndef use_aliased_import(n):\n    return m.compute(n)\n",
    );
    let result = project.selected(&["caller.py", "mathlib.py"]);
    assert!(resolved_call(&result, "use_aliased_import", "compute").is_some());
}

#[test]
fn test_python_try_except_from_import_alias_module_call_resolves() {
    let project = Project::new();
    project.write("pkg/__init__.py", "");
    project.write(
        "pkg/gate.py",
        "def validate(rows):\n    return bool(rows)\n",
    );
    project.write(
        "pkg/caller_try.py",
        "try:\n    from pkg import gate as t_gate\nexcept ImportError:\n    t_gate = None\ndef use_try_alias(rows):\n    return t_gate.validate(rows)\n",
    );
    let result = project.selected(&["pkg/caller_try.py", "pkg/gate.py", "pkg/__init__.py"]);
    assert!(resolved_call(&result, "use_try_alias", "validate").is_some());
}

#[test]
fn test_python_dotted_import_alias_module_call_resolves() {
    let project = Project::new();
    sample_python_alias(
        &project,
        "pkg/caller_dotted.py",
        "import pkg.gate as g_alias\n",
        "g_alias",
    );
    let result = project.selected(&["pkg/caller_dotted.py", "pkg/gate.py", "pkg/__init__.py"]);
    assert!(resolved_call(&result, "use_alias", "validate").is_some());
}

#[test]
fn test_python_relative_from_import_alias_module_call_resolves() {
    let project = Project::new();
    sample_python_alias(
        &project,
        "pkg/caller_relative.py",
        "from . import gate as r_gate\n",
        "r_gate",
    );
    let result = project.selected(&["pkg/caller_relative.py", "pkg/gate.py", "pkg/__init__.py"]);
    assert!(resolved_call(&result, "use_alias", "validate").is_some());
}

#[test]
fn test_python_external_aliased_import_fabricates_no_call_edge() {
    let project = Project::new();
    project.write(
        "app.py",
        "import numpy as np\nfrom os import path as p\ndef build(rows):\n    p.join('a', 'b')\n    return np.array(rows)\n",
    );
    let result = project.selected(&["app.py"]);
    assert!(resolved_call(&result, "build", "array").is_none());
    assert!(resolved_call(&result, "build", "join").is_none());
}

#[test]
fn test_python_aliased_call_survives_warm_cache() {
    let project = Project::new();
    sample_python_alias(
        &project,
        "pkg/caller.py",
        "from pkg import gate as m_gate\n",
        "m_gate",
    );
    let paths = [
        project.path("pkg/caller.py"),
        project.path("pkg/gate.py"),
        project.path("pkg/__init__.py"),
    ];
    let cold = merge(
        extract_files(&paths, Some(project.root.path()), false)
            .unwrap()
            .extractions,
    );
    assert!(resolved_call(&cold, "use_alias", "validate").is_some());
    let warm = merge(
        extract_files(&paths, Some(project.root.path()), false)
            .unwrap()
            .extractions,
    );
    assert!(resolved_call(&warm, "use_alias", "validate").is_some());
}

#[test]
fn test_python_qualified_call_resolves_when_method_name_collides_with_caller() {
    let project = Project::new();
    project.write(
        "actions.py",
        "class TaskActions:\n    @staticmethod\n    def approve(pk):\n        return pk\n",
    );
    project.write(
        "viewset.py",
        "from actions import TaskActions\nclass TaskViewSet:\n    def approve(self, request):\n        return TaskActions.approve(request)\n",
    );
    let result = project.selected(&["viewset.py", "actions.py"]);
    let edges: Vec<_> =
        result
            .edges
            .iter()
            .filter(|edge| {
                edge.relation == "calls"
                    && result.nodes.iter().any(|node| {
                        node.id == edge.true_source() && node.source_file == "viewset.py"
                    })
                    && result.nodes.iter().any(|node| {
                        node.id == edge.true_target() && node.source_file == "actions.py"
                    })
            })
            .collect();
    assert_eq!(edges.len(), 1, "edges={edges:?}");
    assert_eq!(edges[0].confidence, Confidence::Extracted);
}

#[test]
fn test_python_instance_member_call_not_overconnected() {
    let project = Project::new();
    project.write(
        "svc.py",
        "class Service:\n    def run(self):\n        return 1\n",
    );
    project.write(
        "worker.py",
        "class Worker:\n    def go(self, obj):\n        return obj.run()\n",
    );
    let result = project.selected(&["worker.py", "svc.py"]);
    assert!(resolved_call(&result, "go", "run").is_none());
}

#[test]
fn test_python_qualified_call_ambiguous_class_bails() {
    let project = Project::new();
    project.write(
        "a.py",
        "class Helper:\n    def do(self):\n        return 1\n",
    );
    project.write(
        "b.py",
        "class Helper:\n    def do(self):\n        return 2\n",
    );
    project.write(
        "caller.py",
        "from a import Helper\nclass C:\n    def f(self):\n        return Helper.do(self)\n",
    );
    let result = project.selected(&["caller.py", "a.py", "b.py"]);
    assert!(resolved_call(&result, ".f()", ".do()").is_none());
}

#[test]
fn test_dart_child_node_ids_are_stem_based() {
    let project = Project::new();
    project.write("mydir/sample.dart", "class MyClass {}\nvoid myFunc() {}\n");
    let result = project.selected(&["mydir/sample.dart"]);
    let file_id = result
        .nodes
        .iter()
        .find(|node| node.label == "sample.dart")
        .expect("Dart file node")
        .id
        .clone();
    for (label, expected) in [
        ("MyClass", "mydir_sample_myclass"),
        ("myFunc()", "mydir_sample_myfunc"),
    ] {
        let id = node_id(&result, label);
        assert_eq!(id, expected);
        assert!(!id.contains('/'));
        assert!(id.starts_with(file_id.as_str()));
    }
}

#[test]
fn test_separator_collision_paths_get_distinct_ids() {
    let project = Project::new();
    project.write("foo/bar_baz.py", "class Widget:\n    pass\n");
    project.write("foo_bar/baz.py", "class Gadget:\n    pass\n");
    let result = project.selected(&["foo/bar_baz.py", "foo_bar/baz.py"]);
    let file_ids: BTreeSet<_> = result
        .nodes
        .iter()
        .filter(|node| node.extra.get("type").and_then(|value| value.as_str()) == Some("file"))
        .map(|node| node.id.as_str())
        .collect();
    assert_eq!(file_ids.len(), 2, "nodes={:?}", result.nodes);
}

#[test]
fn test_non_colliding_path_id_is_not_salted() {
    let project = Project::new();
    project.write("src/auth/session.py", "class Session:\n    pass\n");
    let result = project.selected(&["src/auth/session.py"]);
    let file = result
        .nodes
        .iter()
        .find(|node| node.extra.get("type").and_then(|value| value.as_str()) == Some("file"))
        .unwrap();
    assert_eq!(file.id, "src_auth_session");
}

#[test]
fn test_case_insensitive_suffix_filtering() {
    let project = Project::new();
    project.write("app.PY", "class MyPythonClass:\n    pass\n");
    project.write("script.JS", "function myJSFunction() {}\n");
    project.write("lib.Ts", "export class MyTSClass {}\n");
    let result = project.selected(&["app.PY", "script.JS", "lib.Ts"]);
    let found = labels(&result);
    for label in ["MyPythonClass", "myJSFunction()", "MyTSClass"] {
        assert!(found.contains(label), "labels={found:?}");
    }
}

#[test]
fn test_get_extractor_routes_matlab_m_away_from_objc() {
    let project = Project::new();
    project.write(
        "Foo.m",
        "#import \"Foo.h\"\n@implementation Foo\n- (void)bar {}\n@end\n",
    );
    project.write("solver.m", "function y = solver(x)\n  y = x + 1;\nend\n");
    project.write(
        "Model.m",
        "classdef Model\n  methods\n    function run(obj); end\n  end\nend\n",
    );
    project.write("x.mm", "#import <F/F.h>\n@implementation X\n@end\n");
    assert!(!project.single("Foo.m").nodes.is_empty());
    assert!(project.single("solver.m").nodes.is_empty());
    assert!(project.single("Model.m").nodes.is_empty());
    assert!(!project.single("x.mm").nodes.is_empty());
}

#[test]
fn test_matlab_m_not_extracted_as_garbage() {
    let project = Project::new();
    project.write(
        "controller.m",
        "function u = controller(x)\n  u = -x;\nend\n",
    );
    let output = extract_files(
        &[project.path("controller.m")],
        Some(project.root.path()),
        true,
    )
    .unwrap();
    assert!(merge(output.extractions).nodes.is_empty());
    assert!(!output.warnings.is_empty());
}

fn synthetic_node(id: &str, label: &str, source_file: &str, kind: Option<&str>) -> Node {
    let mut extra = std::collections::BTreeMap::new();
    if let Some(kind) = kind {
        extra.insert("type".into(), kind.into());
    }
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: (!source_file.is_empty()).then(|| "L1".into()),
        community: None,
        extra,
    }
}

fn synthetic_edge(source: &str, target: &str, relation: &str, source_file: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: std::collections::BTreeMap::new(),
    }
}

#[test]
fn test_rewire_binds_cross_module_function_reference_to_definition() {
    let mut parts = [Extraction {
        nodes: vec![
            synthetic_node("pkg_dep_get_db", "get_db()", "pkg/dep.py", Some("function")),
            synthetic_node("get_db", "get_db()", "", None),
        ],
        edges: vec![synthetic_edge(
            "pkg_ep_route",
            "get_db",
            "references",
            "pkg/ep.py",
        )],
        hyperedges: vec![],
    }];
    graphoxide_extract::resolution::resolve(&mut parts);
    assert_eq!(parts[0].edges[0].true_target(), "pkg_dep_get_db");
    assert!(parts[0].nodes.iter().all(|node| node.id != "get_db"));
}

#[test]
fn test_rewire_does_not_bind_function_reference_across_language() {
    let mut parts = [Extraction {
        nodes: vec![
            synthetic_node("svc_get_db", "get_db()", "svc/main.go", Some("function")),
            synthetic_node("get_db", "get_db()", "", None),
        ],
        edges: vec![synthetic_edge(
            "app_route",
            "get_db",
            "references",
            "app/route.py",
        )],
        hyperedges: vec![],
    }];
    graphoxide_extract::resolution::resolve(&mut parts);
    assert_eq!(parts[0].edges[0].true_target(), "get_db");
}

#[test]
fn test_rewire_does_not_bind_ambiguous_function_reference() {
    let mut parts = [Extraction {
        nodes: vec![
            synthetic_node("a_get_db", "get_db()", "a.py", Some("function")),
            synthetic_node("b_get_db", "get_db()", "b.py", Some("function")),
            synthetic_node("get_db", "get_db()", "", None),
        ],
        edges: vec![synthetic_edge("c_route", "get_db", "references", "c.py")],
        hyperedges: vec![],
    }];
    graphoxide_extract::resolution::resolve(&mut parts);
    assert_eq!(parts[0].edges[0].true_target(), "get_db");
}

#[test]
fn test_rewire_does_not_bind_supertype_stub_to_function() {
    let mut parts = [Extraction {
        nodes: vec![
            synthetic_node(
                "factory_bookstore",
                "BookStore()",
                "factory.py",
                Some("function"),
            ),
            synthetic_node("bookstore", "BookStore", "", None),
        ],
        edges: vec![synthetic_edge(
            "store_sqlite",
            "bookstore",
            "inherits",
            "store.py",
        )],
        hyperedges: vec![],
    }];
    graphoxide_extract::resolution::resolve(&mut parts);
    assert_eq!(parts[0].edges[0].true_target(), "bookstore");
}

#[test]
fn test_extract_falls_back_to_sequential_when_parallel_returns_false() {
    // Rust has no Python ProcessPool failure mode: the explicit-file API is
    // the deterministic in-process fallback path itself.
    let project = Project::new();
    project.write("a.py", "def a():\n    return 1\n");
    project.write("b.py", "def b():\n    return 2\n");
    let output = extract_files(
        &[project.path("a.py"), project.path("b.py")],
        Some(project.root.path()),
        true,
    )
    .unwrap();
    assert_eq!(output.extractions.len(), 2);
    assert!(labels(&merge(output.extractions)).contains("b()"));
}

#[test]
fn test_extract_parallel_returns_false_on_broken_pool() {
    // The injectable in-process boundary must surface a backend failure as a
    // concise error, without a subprocess traceback or poisoned cache entry.
    let project = Project::new();
    project.write("broken.py", "def broken():\n    return 1\n");
    let error = extract_files_with(
        &[project.path("broken.py")],
        Some(project.root.path()),
        true,
        |_path, _relative| anyhow::bail!("broken extraction backend"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("extract broken.py"));
    assert!(error
        .chain()
        .any(|cause| cause.to_string().contains("broken extraction backend")));
}

#[test]
fn test_extract_parallel_skips_pool_when_max_workers_is_one() {
    let project = Project::new();
    project.write("one.py", "def one():\n    return 1\n");
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let output = extract_files_with(
        &[project.path("one.py")],
        Some(project.root.path()),
        true,
        |path, _relative| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            extract(path)
        },
    )
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(output.extractions.len(), 1);
}

#[test]
fn test_extract_parallel_still_spawns_pool_for_multiple_workers() {
    let project = Project::new();
    for name in ["a", "b", "c", "d"] {
        project.write(
            &format!("{name}.py"),
            &format!("def {name}():\n    return 1\n"),
        );
    }
    // Project extraction is backed by rayon; this regression test exercises
    // the multi-file branch without depending on host CPU count.
    let parts =
        extract_project_with_options_and_output(project.root.path(), true, &project.path("out"))
            .unwrap();
    assert_eq!(parts.len(), 4);
}

#[test]
fn test_extract_warns_on_code_files_with_no_ast_extractor() {
    let project = Project::new();
    project.write("analysis.R", "f <- function(x) x + 1\n");
    project.write("helper.r", "g <- function(y) y * 2\n");
    project.write("main.py", "def main():\n    return 1\n");
    let output = extract_files(
        &[
            project.path("analysis.R"),
            project.path("helper.r"),
            project.path("main.py"),
        ],
        Some(project.root.path()),
        true,
    )
    .unwrap();
    let warning = output.warnings.join("\n");
    assert!(warning.contains("no AST extractor"));
    assert!(warning.contains(".r (2)"));
    assert!(warning.contains("#1689"));
    assert!(labels(&merge(output.extractions)).contains("main()"));
}

#[test]
fn test_extract_no_warning_when_all_code_has_extractors() {
    let project = Project::new();
    project.write("a.py", "def a():\n    return 1\n");
    let output = extract_files(&[project.path("a.py")], Some(project.root.path()), true).unwrap();
    assert!(output
        .warnings
        .iter()
        .all(|warning| !warning.contains("no AST extractor")));
}

#[test]
fn test_extract_warns_when_sql_extra_missing() {
    // The Rust port statically links SQL support, so the Python optional-extra
    // failure is eliminated. Keep the original regression identity and assert
    // that SQL files cannot silently disappear in this build.
    let project = Project::new();
    project.write("schema.sql", "CREATE TABLE users (id INT);\n");
    project.write("views.sql", "CREATE VIEW v AS SELECT * FROM users;\n");
    project.write("main.py", "def main():\n    return 1\n");
    let output = extract_files(
        &[
            project.path("schema.sql"),
            project.path("views.sql"),
            project.path("main.py"),
        ],
        Some(project.root.path()),
        true,
    )
    .unwrap();
    let result = merge(output.extractions);
    assert!(result
        .nodes
        .iter()
        .any(|node| node.source_file == "schema.sql"));
    assert!(result
        .nodes
        .iter()
        .any(|node| node.source_file == "views.sql"));
    assert!(output.warnings.is_empty(), "warnings={:?}", output.warnings);
}

#[test]
fn test_extract_no_missing_dep_warning_when_sql_installed() {
    let project = Project::new();
    project.write("schema.sql", "CREATE TABLE users (id INT);\n");
    let output = extract_files(
        &[project.path("schema.sql")],
        Some(project.root.path()),
        true,
    )
    .unwrap();
    assert!(!merge(output.extractions).nodes.is_empty());
    assert!(output
        .warnings
        .iter()
        .all(|warning| !warning.contains("#1745")));
}

#[test]
fn test_extract_progress_final_line_uses_consistent_denominator() {
    let project = Project::new();
    let mut paths = Vec::new();
    for index in 0..100 {
        paths.push(project.write(
            &format!("m{index}.py"),
            &format!("def f{index}():\n    return {index}\n"),
        ));
    }
    for index in 0..5 {
        paths.push(project.write(
            &format!("s{index}.r"),
            &format!("g{index} <- function(x) x\n"),
        ));
    }
    let output = extract_files(&paths, Some(project.root.path()), true).unwrap();
    let completed = output
        .extractions
        .iter()
        .filter(|extraction| !extraction.nodes.is_empty())
        .count();
    assert_eq!(completed, 100);
    assert!(output.warnings.join("\n").contains(".r (5)"));
}
