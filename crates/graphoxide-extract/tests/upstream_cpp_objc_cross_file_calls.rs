//! One-to-one executable port of pinned Graphify
//! `tests/test_cpp_objc_cross_file_calls.py`.

use graphoxide_core::{Confidence, Edge, Extraction, Node};
use graphoxide_extract::extract_files;
use graphoxide_graph::build_graph;
use std::{fs, path::PathBuf};
use tempfile::TempDir;

struct NativeFixture {
    root: TempDir,
}

impl NativeFixture {
    fn new() -> Self {
        Self {
            root: TempDir::new().expect("temporary native fixture"),
        }
    }

    fn write(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.root.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create native fixture directory");
        }
        fs::write(&path, source).expect("write native fixture");
        path
    }

    fn extract(&self, files: &[PathBuf]) -> Vec<Extraction> {
        extract_files(files, Some(self.root.path()), true)
            .expect("extract native fixture")
            .extractions
    }
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|result| result.nodes.iter())
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|result| result.edges.iter())
}

fn label<'a>(extractions: &'a [Extraction], id: &str) -> &'a str {
    nodes(extractions)
        .find(|node| node.id == id)
        .map(|node| node.label.as_str())
        .unwrap_or("")
}

fn source<'a>(extractions: &'a [Extraction], id: &str) -> &'a str {
    nodes(extractions)
        .find(|node| node.id == id)
        .map(|node| node.source_file.as_str())
        .unwrap_or("")
}

fn calls_from<'a>(extractions: &'a [Extraction], source_label: &str) -> Vec<&'a Edge> {
    edges(extractions)
        .filter(|edge| {
            edge.relation == "calls" && label(extractions, edge.true_source()) == source_label
        })
        .collect()
}

fn cpp_fixture(fixture: &NativeFixture, main: &str) -> Vec<PathBuf> {
    vec![
        fixture.write("src/Foo.h", "class Foo {\npublic:\n  void bar();\n};\n"),
        fixture.write("src/Foo.cpp", "#include \"Foo.h\"\nvoid Foo::bar() {}\n"),
        fixture.write("src/Main.cpp", main),
    ]
}

fn objc_fixture(fixture: &NativeFixture) -> Vec<PathBuf> {
    vec![
        fixture.write(
            "src/Foo.h",
            "@interface Foo : NSObject\n- (void)doThing;\n@end\n",
        ),
        fixture.write(
            "src/Foo.m",
            "#import \"Foo.h\"\n@implementation Foo\n- (void)doThing {}\n@end\n",
        ),
        fixture.write(
            "src/Bar.m",
            "#import \"Foo.h\"\n@implementation Bar\n- (void)go {\n  Foo *f = [[Foo alloc] init];\n  [f doThing];\n}\n@end\n",
        ),
    ]
}

#[test]
fn test_cpp_cross_file_member_call_connects_with_relative_paths() {
    let fixture = NativeFixture::new();
    let files = cpp_fixture(
        &fixture,
        "#include \"Foo.h\"\nint main() { Foo f; f.bar(); return 0; }\n",
    );
    let result = fixture.extract(&files);
    assert_eq!(nodes(&result).filter(|node| node.label == "Foo").count(), 1);
    let resolved: Vec<_> = calls_from(&result, "main()")
        .into_iter()
        .filter(|edge| label(&result, edge.true_target()) == "bar")
        .collect();
    assert_eq!(resolved.len(), 1, "cross-file Foo::bar call: {resolved:?}");
    assert!(resolved[0].true_target().contains("foo"));
}

#[test]
fn test_cpp_instance_member_call_resolves() {
    let fixture = NativeFixture::new();
    let result = fixture.extract(&cpp_fixture(
        &fixture,
        "#include \"Foo.h\"\nint main() { Foo f; f.bar(); }\n",
    ));
    let calls: Vec<_> = calls_from(&result, "main()")
        .into_iter()
        .filter(|edge| label(&result, edge.true_target()) == "bar")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].confidence, Confidence::Inferred);
}

#[test]
fn test_cpp_pointer_member_call_resolves() {
    let fixture = NativeFixture::new();
    let result = fixture.extract(&cpp_fixture(
        &fixture,
        "#include \"Foo.h\"\nint main() { Foo* f = new Foo(); f->bar(); }\n",
    ));
    let calls: Vec<_> = calls_from(&result, "main()")
        .into_iter()
        .filter(|edge| label(&result, edge.true_target()) == "bar")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].confidence, Confidence::Inferred);
}

#[test]
fn test_cpp_qualified_member_call_is_extracted() {
    let fixture = NativeFixture::new();
    let files = vec![
        fixture.write(
            "src/Foo.h",
            "class Foo {\npublic:\n  static void bar();\n};\n",
        ),
        fixture.write("src/Foo.cpp", "#include \"Foo.h\"\nvoid Foo::bar() {}\n"),
        fixture.write(
            "src/Main.cpp",
            "#include \"Foo.h\"\nint main() { Foo::bar(); }\n",
        ),
    ];
    let result = fixture.extract(&files);
    let calls: Vec<_> = calls_from(&result, "main()")
        .into_iter()
        .filter(|edge| label(&result, edge.true_target()) == "bar")
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "qualified calls: {:?}",
        calls_from(&result, "main()")
    );
    assert_eq!(calls[0].confidence, Confidence::Extracted);
}

#[test]
fn test_cpp_this_member_call_resolves_to_enclosing_class() {
    let fixture = NativeFixture::new();
    let files = vec![
        fixture.write(
            "src/Foo.h",
            "class Foo {\npublic:\n  void bar();\n  void baz();\n};\n",
        ),
        fixture.write(
            "src/Foo.cpp",
            "#include \"Foo.h\"\nvoid Foo::bar() {}\nvoid Foo::baz() { this->bar(); }\n",
        ),
    ];
    let result = fixture.extract(&files);
    let calls: Vec<_> = calls_from(&result, "baz")
        .into_iter()
        .filter(|edge| label(&result, edge.true_target()) == "bar")
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "this calls: {:?}",
        calls_from(&result, "baz")
    );
    assert_eq!(calls[0].confidence, Confidence::Extracted);
}

#[test]
fn test_cpp_godnode_guard_ambiguous_and_unknown_receiver() {
    let fixture = NativeFixture::new();
    let files = vec![
        fixture.write("src/A.h", "class A {\npublic:\n  void run();\n};\n"),
        fixture.write("src/A.cpp", "#include \"A.h\"\nvoid A::run() {}\n"),
        fixture.write("src/B.h", "class B {\npublic:\n  void run();\n};\n"),
        fixture.write("src/B.cpp", "#include \"B.h\"\nvoid B::run() {}\n"),
        fixture.write(
            "src/Main.cpp",
            "#include \"A.h\"\n#include \"B.h\"\nint main() { x.run(); A a; a.run(); }\n",
        ),
    ];
    let result = fixture.extract(&files);
    let calls: Vec<_> = calls_from(&result, "main()")
        .into_iter()
        .filter(|edge| label(&result, edge.true_target()) == "run")
        .collect();
    assert_eq!(calls.len(), 1, "ambiguous receiver fanned out: {calls:?}");
    assert!(
        source(&result, calls[0].true_target()).ends_with("A.h"),
        "wrong run target: {:?}; nodes: {:?}",
        calls[0],
        nodes(&result)
            .filter(|node| node.label == "run")
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_cpp_resolved_call_survives_build() {
    let fixture = NativeFixture::new();
    let result = fixture.extract(&cpp_fixture(
        &fixture,
        "#include \"Foo.h\"\nint main() { Foo f; f.bar(); }\n",
    ));
    let graph = build_graph(&result).expect("build C++ graph");
    assert!(graph
        .links
        .iter()
        .any(|edge| { edge.relation == "calls" && edge.confidence == Confidence::Inferred }));
}

#[test]
fn test_cpp_unknown_receiver_emits_no_edge() {
    let fixture = NativeFixture::new();
    let files = vec![
        fixture.write(
            "src/Helper.h",
            "class Helper {\npublic:\n  void help();\n};\n",
        ),
        fixture.write(
            "src/Helper.cpp",
            "#include \"Helper.h\"\nvoid Helper::help() {}\n",
        ),
        fixture.write(
            "src/Main.cpp",
            "#include \"Helper.h\"\nint main() { mystery.help(); }\n",
        ),
    ];
    let result = fixture.extract(&files);
    assert!(calls_from(&result, "main()")
        .into_iter()
        .all(|edge| label(&result, edge.true_target()) != "help"));
}

#[test]
fn test_objc_instance_message_send_resolves() {
    let fixture = NativeFixture::new();
    let result = fixture.extract(&objc_fixture(&fixture));
    let calls: Vec<_> = calls_from(&result, "-go")
        .into_iter()
        .filter(|edge| label(&result, edge.true_target()) == "-doThing")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].confidence, Confidence::Inferred);
}

#[test]
fn test_objc_self_message_send_resolves_to_enclosing_class() {
    let fixture = NativeFixture::new();
    let files = vec![
        fixture.write(
            "src/Foo.h",
            "@interface Foo : NSObject\n- (void)render;\n- (void)setup;\n@end\n",
        ),
        fixture.write(
            "src/Foo.m",
            "#import \"Foo.h\"\n@implementation Foo\n- (void)setup { [self render]; }\n- (void)render {}\n@end\n",
        ),
    ];
    let result = fixture.extract(&files);
    let calls: Vec<_> = calls_from(&result, "-setup")
        .into_iter()
        .filter(|edge| label(&result, edge.true_target()) == "-render")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].confidence, Confidence::Extracted);
}

#[test]
fn test_objc_godnode_guard_ambiguous_selector() {
    let fixture = NativeFixture::new();
    let files = vec![
        fixture.write(
            "src/A.h",
            "@interface A : NSObject\n- (void)doStuff;\n@end\n",
        ),
        fixture.write(
            "src/A.m",
            "#import \"A.h\"\n@implementation A\n- (void)doStuff {}\n@end\n",
        ),
        fixture.write(
            "src/B.h",
            "@interface B : NSObject\n- (void)doStuff;\n@end\n",
        ),
        fixture.write(
            "src/B.m",
            "#import \"B.h\"\n@implementation B\n- (void)doStuff {}\n@end\n",
        ),
        fixture.write(
            "src/C.m",
            "#import \"A.h\"\n#import \"B.h\"\n@implementation C\n- (void)go { [thing doStuff]; }\n@end\n",
        ),
    ];
    let result = fixture.extract(&files);
    assert!(calls_from(&result, "-go").is_empty());
}

#[test]
fn test_objc_resolved_calls_survive_build() {
    let fixture = NativeFixture::new();
    let result = fixture.extract(&objc_fixture(&fixture));
    let graph = build_graph(&result).expect("build Objective-C graph");
    assert!(graph
        .links
        .iter()
        .any(|edge| { edge.relation == "calls" && edge.confidence == Confidence::Inferred }));
}

// Additional adversarial guards around the exact upstream surface. These keep
// the receiver evidence rules honest when declarations are shadowed, out of
// scope, absent on the inferred type, or ambiguous across the corpus.

#[test]
fn adversarial_cpp_shadowed_receiver_does_not_reuse_stale_type() {
    let fixture = NativeFixture::new();
    let result = fixture.extract(&cpp_fixture(
        &fixture,
        "#include \"Foo.h\"\nint main() { Foo f; { auto f = mystery; f.bar(); } }\n",
    ));
    assert!(calls_from(&result, "main()")
        .into_iter()
        .all(|edge| label(&result, edge.true_target()) != "bar"));
}

#[test]
fn adversarial_cpp_out_of_scope_receiver_does_not_resolve() {
    let fixture = NativeFixture::new();
    let result = fixture.extract(&cpp_fixture(
        &fixture,
        "#include \"Foo.h\"\nint main() { { Foo f; } f.bar(); }\n",
    ));
    assert!(calls_from(&result, "main()")
        .into_iter()
        .all(|edge| label(&result, edge.true_target()) != "bar"));
}

#[test]
fn adversarial_cpp_method_absent_on_receiver_type_emits_no_edge() {
    let fixture = NativeFixture::new();
    let files = vec![
        fixture.write("src/Foo.h", "class Foo { public: void bar(); };\n"),
        fixture.write("src/Foo.cpp", "void Foo::bar() {}\n"),
        fixture.write("src/Bar.h", "class Bar {};\n"),
        fixture.write("src/Main.cpp", "int main() { Bar b; b.bar(); }\n"),
    ];
    let result = fixture.extract(&files);
    assert!(calls_from(&result, "main()")
        .into_iter()
        .all(|edge| label(&result, edge.true_target()) != "bar"));
}

#[test]
fn adversarial_cpp_duplicate_receiver_type_is_ambiguous() {
    let fixture = NativeFixture::new();
    let files = vec![
        fixture.write("a/Foo.h", "class Foo { public: void run(); };\n"),
        fixture.write("b/Foo.h", "class Foo { public: void run(); };\n"),
        fixture.write("Main.cpp", "int main() { Foo f; f.run(); }\n"),
    ];
    let result = fixture.extract(&files);
    assert!(calls_from(&result, "main()")
        .into_iter()
        .all(|edge| label(&result, edge.true_target()) != "run"));
}

#[test]
fn adversarial_objc_typed_receiver_disambiguates_same_selector() {
    let fixture = NativeFixture::new();
    let files = vec![
        fixture.write(
            "src/A.h",
            "@interface A : NSObject\n- (void)doStuff;\n@end\n",
        ),
        fixture.write(
            "src/A.m",
            "#import \"A.h\"\n@implementation A\n- (void)doStuff {}\n@end\n",
        ),
        fixture.write(
            "src/B.h",
            "@interface B : NSObject\n- (void)doStuff;\n@end\n",
        ),
        fixture.write(
            "src/B.m",
            "#import \"B.h\"\n@implementation B\n- (void)doStuff {}\n@end\n",
        ),
        fixture.write(
            "src/C.m",
            "@implementation C\n- (void)go { A *thing; [thing doStuff]; }\n@end\n",
        ),
    ];
    let result = fixture.extract(&files);
    let calls: Vec<_> = calls_from(&result, "-go")
        .into_iter()
        .filter(|edge| label(&result, edge.true_target()) == "-doStuff")
        .collect();
    assert_eq!(calls.len(), 1);
    assert!(source(&result, calls[0].true_target()).ends_with("A.h"));
}

#[test]
fn adversarial_objc_unknown_receiver_does_not_bind_unique_selector() {
    let fixture = NativeFixture::new();
    let files = vec![
        fixture.write(
            "src/A.h",
            "@interface A : NSObject\n- (void)doStuff;\n@end\n",
        ),
        fixture.write(
            "src/C.m",
            "@implementation C\n- (void)go { [thing doStuff]; }\n@end\n",
        ),
    ];
    let result = fixture.extract(&files);
    assert!(calls_from(&result, "-go").is_empty());
}
