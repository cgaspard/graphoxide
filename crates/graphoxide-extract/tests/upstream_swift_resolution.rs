//! One-to-one executable port of the 13 non-analysis cases across pinned
//! Graphify Swift regression modules:
//! - `tests/test_swift_builtin_noise.py`
//! - `tests/test_swift_computed_properties.py`
//! - `tests/test_swift_cross_file_calls.py`
//! - `tests/test_swift_import_resolution.py`

use graphoxide_core::{Confidence, Edge, Extraction, Node};
use graphoxide_extract::{extract, extract_files};
use graphoxide_graph::build_graph;
use std::{collections::BTreeSet, fs, path::PathBuf};
use tempfile::TempDir;

struct SwiftFixture {
    root: TempDir,
}

impl SwiftFixture {
    fn new() -> Self {
        Self {
            root: TempDir::new().expect("temporary Swift fixture"),
        }
    }

    fn write(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.root.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create Swift fixture parent");
        }
        fs::write(&path, source).expect("write Swift fixture");
        path
    }

    fn extract_files(&self, files: &[PathBuf]) -> Vec<Extraction> {
        extract_files(files, Some(self.root.path()), true)
            .expect("extract Swift fixture")
            .extractions
    }
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|result| result.nodes.iter())
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|result| result.edges.iter())
}

fn node_label<'a>(extractions: &'a [Extraction], id: &str) -> &'a str {
    nodes(extractions)
        .find(|node| node.id == id)
        .map(|node| node.label.as_str())
        .unwrap_or("")
}

fn node_source<'a>(extractions: &'a [Extraction], id: &str) -> &'a str {
    nodes(extractions)
        .find(|node| node.id == id)
        .map(|node| node.source_file.as_str())
        .unwrap_or("")
}

fn context(edge: &Edge) -> Option<&str> {
    edge.extra
        .get("context")
        .and_then(serde_json::Value::as_str)
}

fn single_file(source: &str) -> Extraction {
    let fixture = SwiftFixture::new();
    let path = fixture.write("View.swift", source);
    extract(&path).expect("extract one Swift file")
}

fn issue_fixture(fixture: &SwiftFixture) -> Vec<PathBuf> {
    vec![
        fixture.write(
            "src/Models/SessionViewModel.swift",
            "class SessionViewModel {\n    func update() {}\n}\n",
        ),
        fixture.write(
            "src/Services/NetworkService.swift",
            "class NetworkService {\n    func fetch() {}\n}\n",
        ),
        fixture.write(
            "src/Core/SessionType.swift",
            "enum SessionType {\n    static func staticMethod() {}\n}\n",
        ),
        fixture.write(
            "src/Core/Singleton.swift",
            "class Singleton {\n    static let shared = Singleton()\n    func method() {}\n}\n",
        ),
        fixture.write(
            "src/Views/HomeView.swift",
            "class HomeView {\n\
                 \x20   let vm = SessionViewModel()\n\
                 \x20   var svc: NetworkService\n\n\
                 \x20   func go() {\n\
                 \x20       vm.update()\n\
                 \x20       SessionType.staticMethod()\n\
                 \x20       Singleton.shared.method()\n\
                 \x20       self.svc.fetch()\n\
                 \x20   }\n\
                 }\n",
        ),
    ]
}

#[test]
fn test_swift_builtin_receiver_does_not_bind_to_user_symbol() {
    let fixture = SwiftFixture::new();
    let model = fixture.write(
        "Model.swift",
        "class Data {\n    func append(_ s: String) {}\n}\n",
    );
    let uploader = fixture.write(
        "Uploader.swift",
        "class Uploader {\n\
         \x20   let payload: Data = Data()\n\
         \x20   func send() {\n\
         \x20       payload.append(\"x\")\n\
         \x20   }\n\
         }\n",
    );
    let result = fixture.extract_files(&[model, uploader]);
    let data_ids: BTreeSet<_> = nodes(&result)
        .filter(|node| node.label == "Data" && node.source_file.ends_with("Model.swift"))
        .map(|node| node.id.as_str())
        .collect();
    assert!(!data_ids.is_empty(), "the user class Data must still exist");
    for edge in edges(&result).filter(|edge| {
        data_ids.contains(edge.true_target())
            && matches!(edge.relation.as_str(), "calls" | "references")
            && context(edge) == Some("call")
    }) {
        assert!(
            !node_source(&result, edge.true_source()).ends_with("Uploader.swift"),
            "builtin-typed receiver bound to user Data: {edge:?}"
        );
    }
}

#[test]
fn test_swift_user_receiver_type_still_resolves() {
    let fixture = SwiftFixture::new();
    let engine = fixture.write(
        "Engine.swift",
        "class AudioEngine {\n    func play() {}\n}\n",
    );
    let player = fixture.write(
        "Player.swift",
        "class Player {\n\
         \x20   let engine: AudioEngine = AudioEngine()\n\
         \x20   func start() {\n\
         \x20       engine.play()\n\
         \x20   }\n\
         }\n",
    );
    let result = fixture.extract_files(&[engine, player]);
    assert!(edges(&result).any(|edge| {
        edge.relation == "calls"
            && context(edge) == Some("call")
            && node_label(&result, edge.true_source()).contains("start")
            && node_label(&result, edge.true_target()).contains("play")
    }));
}

#[test]
fn test_computed_property_emits_node_and_walks_body() {
    let result = single_file(
        "struct PlayerScrubber: View {\n\
         \x20   var body: some View {\n\
         \x20       VStack { doTap() }\n\
         \x20   }\n\
         \x20   var toggled: Int { 1 }\n\
         \x20   func doTap() {}\n\
         }\n",
    );
    assert!(result.nodes.iter().any(|node| node.label == ".body"));
    assert!(result.nodes.iter().any(|node| node.label == ".toggled"));
    let body = result
        .nodes
        .iter()
        .find(|node| node.label == ".body")
        .expect("computed body node");
    let do_tap = result
        .nodes
        .iter()
        .find(|node| node.label == ".doTap()")
        .expect("doTap method node");
    assert!(result.edges.iter().any(|edge| {
        edge.relation == "calls" && edge.true_source() == body.id && edge.true_target() == do_tap.id
    }));
}

#[test]
fn test_stored_property_not_emitted_as_member_but_keeps_type_ref() {
    let result = single_file("struct S {\n    var vm: ViewModel\n}\n");
    assert!(result.nodes.iter().all(|node| node.label != ".vm"));
    let reference_targets: BTreeSet<_> = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "references")
        .filter_map(|edge| {
            result
                .nodes
                .iter()
                .find(|node| node.id == edge.true_target())
                .map(|node| node.label.as_str())
        })
        .collect();
    assert!(reference_targets.contains("ViewModel"));
}

#[test]
fn test_observed_property_body_is_walked() {
    let result = single_file(
        "class M {\n\
         \x20   var score: Int = 0 {\n\
         \x20       didSet { react() }\n\
         \x20   }\n\
         \x20   func react() {}\n\
         }\n",
    );
    let score = result
        .nodes
        .iter()
        .find(|node| node.label == ".score")
        .expect("observed property node");
    let react = result
        .nodes
        .iter()
        .find(|node| node.label == ".react()")
        .expect("react method node");
    assert!(result.edges.iter().any(|edge| {
        edge.relation == "calls" && edge.true_source() == score.id && edge.true_target() == react.id
    }));
}

#[test]
fn test_swift_cross_file_member_calls_resolve() {
    let fixture = SwiftFixture::new();
    let result = fixture.extract_files(&issue_fixture(&fixture));
    let triples: BTreeSet<_> = edges(&result)
        .filter(|edge| matches!(edge.relation.as_str(), "calls" | "references"))
        .map(|edge| {
            (
                node_label(&result, edge.true_source()).to_owned(),
                edge.relation.clone(),
                node_label(&result, edge.true_target()).to_owned(),
            )
        })
        .collect();
    for expected in [
        ("HomeView", "calls", "SessionViewModel"),
        (".go()", "calls", ".update()"),
        (".go()", "calls", ".fetch()"),
        (".go()", "calls", ".staticMethod()"),
        (".go()", "calls", ".method()"),
    ] {
        assert!(
            triples.contains(&(
                expected.0.to_owned(),
                expected.1.to_owned(),
                expected.2.to_owned(),
            )),
            "missing Swift call {expected:?}: {triples:?}"
        );
    }
}

#[test]
fn test_swift_cross_file_member_calls_have_correct_confidence_and_resolve() {
    let fixture = SwiftFixture::new();
    let result = fixture.extract_files(&issue_fixture(&fixture));
    let mut inferred = BTreeSet::new();
    let mut extracted = BTreeSet::new();
    for edge in edges(&result).filter(|edge| edge.relation == "calls") {
        let target = node_label(&result, edge.true_target());
        if [".update()", ".fetch()"].contains(&target) {
            assert_eq!(edge.confidence, Confidence::Inferred);
            assert_eq!(edge.extra.get("confidence_score"), Some(&0.8.into()));
            assert!(!node_source(&result, edge.true_target()).is_empty());
            inferred.insert(target.to_owned());
        } else if [".staticMethod()", ".method()"].contains(&target) {
            assert_eq!(edge.confidence, Confidence::Extracted);
            assert_eq!(edge.extra.get("confidence_score"), Some(&1.0.into()));
            assert!(!node_source(&result, edge.true_target()).is_empty());
            extracted.insert(target.to_owned());
        }
    }
    assert_eq!(
        inferred,
        BTreeSet::from([".fetch()".into(), ".update()".into()])
    );
    assert_eq!(
        extracted,
        BTreeSet::from([".method()".into(), ".staticMethod()".into()])
    );

    let graph = build_graph(&result).expect("build Swift graph");
    assert!(
        graph
            .links
            .iter()
            .filter(|edge| edge.relation == "calls")
            .count()
            >= 5
    );
}

#[test]
fn test_swift_ambiguous_type_does_not_over_connect() {
    let fixture = SwiftFixture::new();
    let mut files = Vec::new();
    for subdirectory in ["a", "b", "c"] {
        files.push(fixture.write(
            &format!("src/{subdirectory}/Widget.swift"),
            "class Widget {\n    func update() {}\n}\n",
        ));
    }
    files.push(fixture.write(
        "src/Caller.swift",
        "class Caller {\n\
         \x20   var w: Widget\n\
         \x20   func run() {\n\
         \x20       w.update()\n\
         \x20       unknown.update()\n\
         \x20   }\n\
         }\n",
    ));
    let result = fixture.extract_files(&files);
    let inferred: Vec<_> = edges(&result)
        .filter(|edge| edge.relation == "calls" && edge.confidence == Confidence::Inferred)
        .collect();
    assert!(
        inferred.is_empty(),
        "ambiguous calls over-connected: {inferred:?}"
    );
}

#[test]
fn test_swift_unknown_receiver_emits_no_edge() {
    let fixture = SwiftFixture::new();
    let helper = fixture.write(
        "src/Helper.swift",
        "class Helper {\n    func help() {}\n}\n",
    );
    let caller = fixture.write(
        "src/Caller.swift",
        "class Caller {\n\
         \x20   func run() {\n\
         \x20       mystery.help()\n\
         \x20   }\n\
         }\n",
    );
    let result = fixture.extract_files(&[helper, caller]);
    assert!(!edges(&result).any(|edge| {
        edge.relation == "calls"
            && node_label(&result, edge.true_source()) == ".run()"
            && node_label(&result, edge.true_target()) == ".help()"
    }));
}

#[test]
fn test_deferred_singleton_local_var_resolves() {
    let fixture = SwiftFixture::new();
    let manager = fixture.write(
        "src/NetworkManager.swift",
        "class NetworkManager {\n\
         \x20   static let shared = NetworkManager()\n\
         \x20   func fetchData() {}\n\
         \x20   func isLoading() -> Bool { return false }\n\
         }\n",
    );
    let controller = fixture.write(
        "src/ViewController.swift",
        "class ViewControllerA {\n\
         \x20   func loadIfNeeded() {\n\
         \x20       let manager = NetworkManager.shared\n\
         \x20       if manager.isLoading() { return }\n\
         \x20       manager.fetchData()\n\
         \x20   }\n\
         \x20   func makeFresh() {\n\
         \x20       let m = NetworkManager()\n\
         \x20       m.fetchData()\n\
         \x20   }\n\
         }\n",
    );
    let result = fixture.extract_files(&[manager, controller]);
    let calls: BTreeSet<_> = edges(&result)
        .filter(|edge| edge.relation == "calls")
        .map(|edge| {
            (
                node_label(&result, edge.true_source()).to_owned(),
                node_label(&result, edge.true_target()).to_owned(),
            )
        })
        .collect();
    for expected in [
        ("loadIfNeeded", "fetchData"),
        ("loadIfNeeded", "isLoading"),
        ("makeFresh", "fetchData"),
    ] {
        assert!(
            calls
                .iter()
                .any(|(source, target)| source.contains(expected.0) && target.contains(expected.1)),
            "missing deferred Swift call {expected:?}: {calls:?}"
        );
    }
}

#[test]
fn test_swift_import_resolves_to_module_node() {
    let fixture = SwiftFixture::new();
    let core = fixture.write(
        "Sources/CoreKit/CoreKit.swift",
        "public struct CoreKit {}\n",
    );
    let feature = fixture.write(
        "Sources/FeatureKit/FeatureKit.swift",
        "import CoreKit\n\npublic struct FeatureKit {}\n",
    );
    let result = fixture.extract_files(&[core, feature]);
    let ids: BTreeSet<_> = nodes(&result).map(|node| node.id.as_str()).collect();
    let imports: Vec<_> = edges(&result)
        .filter(|edge| edge.relation == "imports")
        .collect();
    assert!(!imports.is_empty());
    assert!(imports.iter().all(|edge| ids.contains(edge.true_target())));
    assert!(nodes(&result).any(|node| {
        node.label == "CoreKit"
            && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("module")
    }));
}

#[test]
fn test_swift_same_module_imported_twice_collapses_to_one_node() {
    let fixture = SwiftFixture::new();
    let files = [
        fixture.write(
            "Sources/CoreKit/CoreKit.swift",
            "public struct CoreKit {}\n",
        ),
        fixture.write(
            "Sources/AKit/AKit.swift",
            "import CoreKit\n\npublic struct AKit {}\n",
        ),
        fixture.write(
            "Sources/BKit/BKit.swift",
            "import CoreKit\n\npublic struct BKit {}\n",
        ),
    ];
    let result = fixture.extract_files(&files);
    let module_ids: BTreeSet<_> = nodes(&result)
        .filter(|node| {
            node.label == "CoreKit"
                && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("module")
        })
        .map(|node| node.id.as_str())
        .collect();
    assert_eq!(module_ids.len(), 1);
    let import_targets: BTreeSet<_> = edges(&result)
        .filter(|edge| edge.relation == "imports")
        .map(Edge::true_target)
        .collect();
    assert_eq!(import_targets, module_ids);
}

#[test]
fn test_swift_import_edges_survive_build() {
    let fixture = SwiftFixture::new();
    let files = [
        fixture.write(
            "Sources/CoreKit/CoreKit.swift",
            "public struct CoreKit {}\n",
        ),
        fixture.write("Sources/AKit/AKit.swift", "import CoreKit\n"),
        fixture.write("Sources/BKit/BKit.swift", "import CoreKit\n"),
    ];
    let result = fixture.extract_files(&files);
    let graph = build_graph(&result).expect("build Swift import graph");
    let imports: Vec<_> = graph
        .links
        .iter()
        .filter(|edge| edge.relation == "imports")
        .collect();
    assert_eq!(imports.len(), 2);
    assert_eq!(
        imports
            .iter()
            .map(|edge| edge.true_target())
            .collect::<BTreeSet<_>>()
            .len(),
        1
    );
}
