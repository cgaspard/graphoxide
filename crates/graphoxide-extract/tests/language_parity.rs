//! Executable port of upstream Graphify `tests/test_languages.py`.
//!
//! Every upstream pytest node has a distinct Rust test ID in this target. The
//! machine-readable one-to-one inventory lives at
//! `parity/source-maps/test_languages.mapping.json`.

use graphoxide_core::{Confidence, Edge, Extraction, Node};
use graphoxide_extract::{detect, extract, extract_project_with_options};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/upstream");
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempFixture {
    root: PathBuf,
}

impl TempFixture {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "graphoxide-language-parity-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create language parity fixture");
        Self { root }
    }

    fn write(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        fs::write(&path, source).expect("write fixture source");
        path
    }

    fn extract_project(&self) -> Extraction {
        let chunks = extract_project_with_options(&self.root, true).expect("extract project");
        Extraction {
            nodes: chunks
                .iter()
                .flat_map(|chunk| chunk.nodes.iter().cloned())
                .collect(),
            edges: chunks
                .iter()
                .flat_map(|chunk| chunk.edges.iter().cloned())
                .collect(),
            hyperedges: chunks
                .iter()
                .flat_map(|chunk| chunk.hyperedges.iter().cloned())
                .collect(),
        }
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove language parity fixture");
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(FIXTURES).join(name)
}

fn extract_fixture(name: &str) -> Extraction {
    let path = fixture(name);
    extract(&path).unwrap_or_else(|error| panic!("extract {name}: {error:#}"))
}

fn labels(result: &Extraction) -> Vec<&str> {
    result
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect()
}

fn normalize_symbol_label(label: &str) -> &str {
    label
        .trim_matches(|character| matches!(character, '(' | ')'))
        .trim_start_matches('.')
}

fn node_labels(result: &Extraction) -> HashMap<&str, &str> {
    result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect()
}

fn edge_pairs(
    result: &Extraction,
    relation: &str,
    context: Option<&str>,
) -> BTreeSet<(String, String)> {
    let labels = node_labels(result);
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == relation)
        .filter(|edge| context.is_none_or(|expected| edge_context(edge) == Some(expected)))
        .map(|edge| {
            (
                normalize_symbol_label(
                    labels
                        .get(edge.true_source())
                        .copied()
                        .unwrap_or(edge.true_source()),
                )
                .to_owned(),
                normalize_symbol_label(
                    labels
                        .get(edge.true_target())
                        .copied()
                        .unwrap_or(edge.true_target()),
                )
                .to_owned(),
            )
        })
        .collect()
}

fn edge_context(edge: &Edge) -> Option<&str> {
    edge.extra.get("context").and_then(|value| value.as_str())
}

fn edges<'a>(result: &'a Extraction, relations: &[&str]) -> Vec<&'a Edge> {
    result
        .edges
        .iter()
        .filter(|edge| relations.contains(&edge.relation.as_str()))
        .collect()
}

fn node_by_label<'a>(result: &'a Extraction, label: &str) -> &'a Node {
    result
        .nodes
        .iter()
        .find(|node| node.label == label || normalize_symbol_label(&node.label) == label)
        .unwrap_or_else(|| panic!("missing node label {label:?}; labels={:?}", labels(result)))
}

fn owned_method_id(result: &Extraction, owner_label: &str, method_label: &str) -> String {
    let owner = node_by_label(result, owner_label);
    let target = result
        .edges
        .iter()
        .find(|edge| {
            edge.relation == "method"
                && edge.true_source() == owner.id
                && result.nodes.iter().any(|node| {
                    node.id == edge.true_target()
                        && normalize_symbol_label(&node.label) == method_label
                })
        })
        .unwrap_or_else(|| panic!("missing {owner_label}::{method_label}"))
        .true_target();
    target.to_owned()
}

fn assert_labels_contain(result: &Extraction, expected: &[&str]) {
    let actual = labels(result);
    for label in expected {
        assert!(
            actual.iter().any(|candidate| candidate.contains(label)),
            "missing label fragment {label:?}; labels={actual:?}"
        );
    }
}

fn assert_relation(result: &Extraction, relation: &str) {
    assert!(
        result.edges.iter().any(|edge| edge.relation == relation),
        "missing {relation:?} relation; edges={:?}",
        result.edges
    );
}

fn assert_context(result: &Extraction, relations: &[&str], context: &str) {
    let matching = edges(result, relations);
    assert!(!matching.is_empty(), "missing {relations:?} edges");
    assert!(
        matching
            .iter()
            .all(|edge| edge_context(edge) == Some(context)),
        "not every {relations:?} edge has context={context:?}: {matching:?}"
    );
}

fn assert_edge(result: &Extraction, relation: &str, source: &str, target: &str) {
    assert!(
        edge_pairs(result, relation, None).contains(&(source.to_owned(), target.to_owned())),
        "missing {source:?} -[{relation}]-> {target:?}; pairs={:?}",
        edge_pairs(result, relation, None)
    );
}

fn assert_context_edge(
    result: &Extraction,
    relation: &str,
    context: &str,
    source: &str,
    target: &str,
) {
    assert!(
        edge_pairs(result, relation, Some(context))
            .contains(&(source.to_owned(), target.to_owned())),
        "missing {source:?} -[{relation}; {context}]-> {target:?}; pairs={:?}",
        edge_pairs(result, relation, Some(context))
    );
}

fn assert_no_dangling_sources(result: &Extraction) {
    let ids: HashSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    for edge in &result.edges {
        assert!(
            ids.contains(edge.true_source()),
            "dangling edge source: {edge:?}"
        );
    }
}

fn assert_no_dangling_edges(result: &Extraction) {
    let ids: HashSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    for edge in &result.edges {
        assert!(
            ids.contains(edge.true_source()),
            "dangling source: {edge:?}"
        );
        assert!(
            ids.contains(edge.true_target()),
            "dangling target: {edge:?}"
        );
    }
}

macro_rules! fixture_label_tests {
    ($fixture:literal; $( $name:ident => [$($label:literal),+ $(,)?] ),+ $(,)?) => {
        $(
            #[test]
            fn $name() {
                let result = extract_fixture($fixture);
                assert_labels_contain(&result, &[$($label),+]);
            }
        )+
    };
}

mod java_c_cpp_ruby_csharp {
    use super::*;

    #[test]
    fn test_java_no_error() {
        extract_fixture("sample.java");
    }

    fixture_label_tests!("sample.java";
        test_java_finds_class => ["DataProcessor"],
        test_java_finds_interface => ["Processor"],
        test_java_finds_methods => ["addItem", "process"],
    );

    #[test]
    fn test_java_finds_imports() {
        assert_relation(&extract_fixture("sample.java"), "imports");
    }

    #[test]
    fn test_java_import_edges_have_import_context() {
        assert_context(
            &extract_fixture("sample.java"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_java_no_dangling_edges() {
        assert_no_dangling_sources(&extract_fixture("sample.java"));
    }

    #[test]
    fn test_java_enum_constants_have_case_of_edge() {
        let result = extract_fixture("sample.java");
        assert_labels_contain(&result, &["OK", "GAME_DONE"]);
        assert_edge(&result, "case_of", "ErrorCode", "OK");
        assert_edge(&result, "case_of", "ErrorCode", "GAME_DONE");
    }

    #[test]
    fn test_c_no_error() {
        extract_fixture("sample.c");
    }

    fixture_label_tests!("sample.c";
        test_c_finds_functions => ["process", "main"],
    );

    #[test]
    fn test_c_finds_includes() {
        assert_relation(&extract_fixture("sample.c"), "imports");
    }

    #[test]
    fn test_c_emits_calls() {
        assert_relation(&extract_fixture("sample.c"), "calls");
    }

    #[test]
    fn test_c_calls_are_extracted() {
        let result = extract_fixture("sample.c");
        for edge in result.edges.iter().filter(|edge| edge.relation == "calls") {
            assert_eq!(edge.confidence, Confidence::Extracted);
        }
    }

    #[test]
    fn test_c_import_edges_have_import_context() {
        assert_context(
            &extract_fixture("sample.c"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_c_parameter_and_return_type_contexts() {
        let result = extract_fixture("sample.c");
        assert_context_edge(
            &result,
            "references",
            "parameter_type",
            "make_rect",
            "Rectangle",
        );
        assert_context_edge(
            &result,
            "references",
            "return_type",
            "make_rect",
            "Rectangle",
        );
    }

    #[test]
    fn test_c_call_edges_have_call_context() {
        assert_context(&extract_fixture("sample.c"), &["calls"], "call");
    }

    #[test]
    fn test_cpp_no_error() {
        extract_fixture("sample.cpp");
    }

    fixture_label_tests!("sample.cpp";
        test_cpp_finds_class => ["HttpClient"],
        test_cpp_finds_methods => ["HttpClient"],
    );

    #[test]
    fn test_cpp_finds_includes() {
        assert_relation(&extract_fixture("sample.cpp"), "imports");
    }

    #[test]
    fn test_cpp_import_edges_have_import_context() {
        assert_context(
            &extract_fixture("sample.cpp"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_cpp_method_parameter_and_return_type_contexts() {
        let result = extract_fixture("sample.cpp");
        assert_context_edge(&result, "references", "parameter_type", "get", "string");
        assert_context_edge(&result, "references", "return_type", "get", "string");
    }

    #[test]
    fn test_cpp_field_and_template_argument_contexts() {
        let result = extract_fixture("sample.cpp");
        assert_context_edge(&result, "references", "field", "HttpClient", "string");
        assert_context_edge(&result, "references", "field", "HttpClient", "vector");
        assert_context_edge(&result, "references", "generic_arg", "HttpClient", "string");
    }

    #[test]
    fn test_cpp_class_inherits_edge() {
        assert_edge(
            &extract_fixture("sample.cpp"),
            "inherits",
            "AuthedHttpClient",
            "HttpClient",
        );
    }

    #[test]
    fn test_cpp_struct_inherits_edge() {
        assert_edge(
            &extract_fixture("sample.cpp"),
            "inherits",
            "RetryingHttpClient",
            "HttpClient",
        );
    }

    #[test]
    fn test_cpp_generic_parents_include_type_argument_references() {
        let result = extract_fixture("sample.cpp");
        assert_edge(&result, "inherits", "PooledClient", "Connection");
        assert_context_edge(
            &result,
            "references",
            "generic_arg",
            "PooledClient",
            "HttpClient",
        );
    }

    #[test]
    fn test_cuda_no_error() {
        extract_fixture("sample.cu");
    }

    fixture_label_tests!("sample.cu";
        test_cuda_finds_kernel_and_device_functions => ["saxpy", "dot"],
        test_cuda_finds_struct => ["Vec3"],
    );

    #[test]
    fn test_cuda_finds_includes() {
        assert_relation(&extract_fixture("sample.cu"), "imports");
    }

    #[test]
    fn test_cuda_host_call_edges() {
        let result = extract_fixture("sample.cu");
        assert_edge(&result, "calls", "host_norm", "dot");
        assert_edge(&result, "calls", "main", "host_norm");
    }

    #[test]
    fn test_metal_is_code_extension() {
        assert!(detect::is_supported_path(Path::new("shader.metal")));
    }

    #[test]
    fn test_metal_no_error() {
        extract_fixture("sample.metal");
    }

    fixture_label_tests!("sample.metal";
        test_metal_finds_kernel_function_and_struct => ["Vec3", "dot3", "saxpy"],
    );

    #[test]
    fn test_ruby_no_error() {
        extract_fixture("sample.rb");
    }

    fixture_label_tests!("sample.rb";
        test_ruby_finds_class => ["ApiClient"],
        test_ruby_finds_methods => ["get", "post"],
        test_ruby_finds_function => ["parse_response"],
    );

    #[test]
    fn test_ruby_inherits_edge() {
        assert_edge(
            &extract_fixture("sample.rb"),
            "inherits",
            "TimeoutApiClient",
            "ApiClient",
        );
    }

    #[test]
    fn test_csharp_no_error() {
        extract_fixture("sample.cs");
    }

    fixture_label_tests!("sample.cs";
        test_csharp_finds_class => ["DataProcessor"],
        test_csharp_finds_interface => ["IProcessor"],
        test_csharp_finds_methods => ["Process"],
    );

    #[test]
    fn test_csharp_finds_usings() {
        assert_relation(&extract_fixture("sample.cs"), "imports");
    }

    #[test]
    fn test_csharp_inherits_edge() {
        assert_relation(&extract_fixture("sample.cs"), "inherits");
    }

    #[test]
    fn test_csharp_implements_iprocessor() {
        assert_edge(
            &extract_fixture("sample.cs"),
            "implements",
            "DataProcessor",
            "IProcessor",
        );
    }

    #[test]
    fn test_csharp_splits_inherits_and_implements_edges() {
        let result = extract_fixture("sample.cs");
        assert_edge(&result, "inherits", "DataProcessor", "Processor");
        assert_edge(&result, "implements", "DataProcessor", "IProcessor");
    }

    #[test]
    fn test_csharp_parameter_return_and_generic_contexts() {
        let result = extract_fixture("sample.cs");
        assert_context_edge(
            &result,
            "references",
            "parameter_type",
            "Build",
            "HttpClient",
        );
        assert_context_edge(&result, "references", "return_type", "Build", "Result");
        assert_context_edge(
            &result,
            "references",
            "generic_arg",
            "Build",
            "DataProcessor",
        );
    }

    #[test]
    fn test_java_normalizes_inherits_and_implements() {
        let result = extract_fixture("sample.java");
        assert_edge(&result, "inherits", "DataProcessor", "BaseProcessor");
        assert_edge(&result, "implements", "DataProcessor", "Processor");
    }

    #[test]
    fn test_java_generic_parents_include_type_argument_references() {
        let temp = TempFixture::new("java-generic-parents");
        let source = temp.write(
            "GenericParents.java",
            "class Dependency {}\ninterface Event {}\nclass Base<T> {}\ninterface Handler<T> {}\ninterface DerivedHandler extends Handler<Event> {}\nclass Service extends Base<Dependency> implements Handler<Event> {}\n",
        );
        let result = extract(&source).expect("extract Java generic parents");
        assert_edge(&result, "inherits", "Service", "Base");
        assert_edge(&result, "implements", "Service", "Handler");
        assert_context_edge(
            &result,
            "references",
            "generic_arg",
            "Service",
            "Dependency",
        );
        assert_context_edge(&result, "references", "generic_arg", "Service", "Event");
        assert_edge(&result, "inherits", "DerivedHandler", "Handler");
        assert_context_edge(
            &result,
            "references",
            "generic_arg",
            "DerivedHandler",
            "Event",
        );
    }

    #[test]
    fn test_java_type_parameters_do_not_emit_references() {
        let temp = TempFixture::new("java-type-parameters");
        let source = temp.write(
            "TypeParameters.java",
            "class Payload {}\nclass Base<X> {}\nclass Box<T> extends Base<T> {\n T value;\n List<T> values;\n <U> U convert(T input, List<U> mapped, List<Payload> retained) { return null; }\n <V> Box(V value) {}\n}\n",
        );
        let result = extract(&source).expect("extract Java type parameters");
        let refs = edge_pairs(&result, "references", None);
        assert!(refs
            .iter()
            .all(|(_, target)| !["T", "U", "V"].contains(&target.as_str())));
        assert!(result.nodes.iter().all(|node| {
            !["T", "U", "V"].contains(&node.label.as_str()) || !node.source_file.is_empty()
        }));
        assert_edge(&result, "inherits", "Box", "Base");
        assert_context_edge(&result, "references", "generic_arg", "convert", "Payload");
    }

    #[test]
    fn test_java_parameter_return_generic_and_attribute_contexts() {
        let result = extract_fixture("sample.java");
        assert_context_edge(
            &result,
            "references",
            "parameter_type",
            "build",
            "HttpClient",
        );
        assert_context_edge(&result, "references", "return_type", "build", "Result");
        assert_context_edge(
            &result,
            "references",
            "generic_arg",
            "build",
            "DataProcessor",
        );
        assert_context_edge(&result, "references", "attribute", "build", "Override");
    }

    #[test]
    fn test_java_field_type_references_have_field_context() {
        let temp = TempFixture::new("java-fields");
        let source = temp.write(
            "Fields.java",
            "class PaymentGateway {}\nclass Handler {}\nclass CheckoutService { PaymentGateway gateway; List<Handler> handlers; }\n",
        );
        let result = extract(&source).expect("extract Java fields");
        assert_context_edge(
            &result,
            "references",
            "field",
            "CheckoutService",
            "PaymentGateway",
        );
        assert_context_edge(
            &result,
            "references",
            "generic_arg",
            "CheckoutService",
            "Handler",
        );
    }

    #[test]
    fn test_java_record_component_type_references() {
        let temp = TempFixture::new("java-record-components");
        let source = temp.write(
            "RecordComponents.java",
            "class Payload {}\nclass Item {}\nclass Attachment {}\nrecord Order(Payload payload, List<Item> items, int count, Attachment... attachments) {}\n",
        );
        let result = extract(&source).expect("extract Java record components");
        assert_context_edge(&result, "references", "field", "Order", "Payload");
        assert!(!edge_pairs(&result, "references", None).contains(&("Order".into(), "List".into())));
        assert_context_edge(&result, "references", "generic_arg", "Order", "Item");
        assert_context_edge(&result, "references", "field", "Order", "Attachment");
    }

    #[test]
    fn test_java_record_components_skip_type_parameters() {
        let temp = TempFixture::new("java-generic-record");
        let source = temp.write(
            "GenericRecord.java",
            "class Payload {}\nclass Box<X> {}\nrecord Batch<T>(T value, Box<T> boxed, Box<Payload> retained) {}\n",
        );
        let result = extract(&source).expect("extract generic Java record");
        assert!(!edge_pairs(&result, "references", None).contains(&("Batch".into(), "T".into())));
        assert!(result
            .nodes
            .iter()
            .all(|node| node.label != "T" || !node.source_file.is_empty()));
        assert_context_edge(&result, "references", "field", "Batch", "Box");
        assert_context_edge(&result, "references", "generic_arg", "Batch", "Payload");
    }

    #[test]
    fn test_java_type_annotations_have_attribute_context() {
        let temp = TempFixture::new("java-type-annotations");
        let source = temp.write(
            "TypeAnnotations.java",
            "@Service\n@Entity(name = \"checkout\")\nclass CheckoutService {}\n",
        );
        let result = extract(&source).expect("extract Java type annotations");
        assert_context_edge(
            &result,
            "references",
            "attribute",
            "CheckoutService",
            "Service",
        );
        assert_context_edge(
            &result,
            "references",
            "attribute",
            "CheckoutService",
            "Entity",
        );
    }

    #[test]
    fn test_java_enum_and_annotation_declarations_are_type_nodes() {
        let temp = TempFixture::new("java-type-declarations");
        let source = temp.write(
            "TypeDeclarations.java",
            "enum PaymentStatus { PENDING, PAID }\n@interface Audited {}\nclass Order { PaymentStatus status; }\n@Audited class CheckoutService {}\n",
        );
        let result = extract(&source).expect("extract Java type declarations");
        assert_edge(
            &result,
            "contains",
            "TypeDeclarations.java",
            "PaymentStatus",
        );
        assert_edge(&result, "contains", "TypeDeclarations.java", "Audited");
        assert_context_edge(&result, "references", "field", "Order", "PaymentStatus");
        assert_context_edge(
            &result,
            "references",
            "attribute",
            "CheckoutService",
            "Audited",
        );
        for label in ["PaymentStatus", "Audited"] {
            assert_eq!(
                node_by_label(&result, label).source_file,
                source.to_string_lossy()
            );
        }
    }

    #[test]
    fn test_nested_types_contained_by_enclosing_type() {
        let temp = TempFixture::new("nested-types");
        let java = temp.write(
            "Outer.java",
            "class Outer {\n class Inner { void m() {} }\n}\n",
        );
        let result = extract(&java).expect("extract nested Java type");
        assert_edge(&result, "contains", "Outer.java", "Outer");
        assert_edge(&result, "contains", "Outer", "Inner");
        assert!(
            !edge_pairs(&result, "contains", None).contains(&("Outer.java".into(), "Inner".into()))
        );

        let scala = temp.write(
            "Outer.scala",
            "class Outer {\n class Inner\n object Obj\n}\n",
        );
        let result = extract(&scala).expect("extract nested Scala types");
        assert_edge(&result, "contains", "Outer.scala", "Outer");
        assert_edge(&result, "contains", "Outer", "Inner");
        assert_edge(&result, "contains", "Outer", "Obj");
        assert!(!edge_pairs(&result, "contains", None)
            .contains(&("Outer.scala".into(), "Inner".into())));
    }

    #[test]
    fn test_csharp_nested_type_gets_containment_edge() {
        let temp = TempFixture::new("csharp-nested-types");
        let source = temp.write(
            "N.cs",
            "namespace N {\n class Outer {\n class Inner {}\n }\n}\n",
        );
        let result = extract(&source).expect("extract nested C# type");
        assert_edge(&result, "contains", "Outer", "Inner");
        assert!(!edge_pairs(&result, "contains", None).contains(&("N.cs".into(), "Inner".into())));
    }

    #[test]
    fn test_csharp_field_type_references_have_field_context() {
        let result = extract_fixture("sample.cs");
        assert_context_edge(
            &result,
            "references",
            "field",
            "DataProcessor",
            "HttpClient",
        );
    }

    #[test]
    fn test_csharp_property_type_references_have_field_context() {
        let result = extract_fixture("sample.cs");
        assert_context_edge(&result, "references", "field", "DataProcessor", "Processor");
        assert_context_edge(&result, "references", "field", "DataProcessor", "List");
        assert_context_edge(
            &result,
            "references",
            "generic_arg",
            "DataProcessor",
            "Processor",
        );
    }

    #[test]
    fn test_csharp_call_edges_have_call_context() {
        let result = extract_fixture("sample.cs");
        assert_context_edge(&result, "calls", "call", "Process", "Validate");
    }

    #[test]
    fn test_csharp_import_edges_have_import_context() {
        assert_context(&extract_fixture("sample.cs"), &["imports"], "import");
    }
}

mod kotlin_scala_php {
    use super::*;

    #[test]
    fn test_kotlin_no_error() {
        extract_fixture("sample.kt");
    }

    fixture_label_tests!("sample.kt";
        test_kotlin_finds_class => ["HttpClient"],
        test_kotlin_finds_data_class => ["Config"],
        test_kotlin_finds_methods => ["get", "post"],
        test_kotlin_finds_function => ["createClient"],
    );

    #[test]
    fn test_kotlin_enum_entries_have_case_of_edge() {
        let result = extract_fixture("sample.kt");
        assert_labels_contain(&result, &["NORMAL", "GROUP", "SYSTEM"]);
        assert_edge(&result, "case_of", "ChatType", "NORMAL");
        assert_edge(&result, "case_of", "ChatType", "SYSTEM");
    }

    #[test]
    fn test_kotlin_emits_in_file_calls() {
        let result = extract_fixture("sample.kt");
        assert_edge(&result, "calls", "get", "buildRequest");
        assert_edge(&result, "calls", "post", "buildRequest");
        assert_edge(&result, "calls", "createClient", "Config");
        assert_edge(&result, "calls", "createClient", "HttpClient");
    }

    #[test]
    fn test_kotlin_splits_inherits_and_implements() {
        let result = extract_fixture("sample.kt");
        assert_edge(&result, "inherits", "DataProcessor", "BaseProcessor");
        assert_edge(&result, "implements", "DataProcessor", "Loggable");
    }

    #[test]
    fn test_kotlin_interface_delegation_emits_implements() {
        assert_edge(
            &extract_fixture("sample.kt"),
            "implements",
            "LoggingList",
            "MutableList",
        );
    }

    #[test]
    fn test_kotlin_parameter_return_generic_and_field_contexts() {
        let result = extract_fixture("sample.kt");
        assert_context_edge(
            &result,
            "references",
            "parameter_type",
            "run",
            "DataProcessor",
        );
        assert_context_edge(&result, "references", "return_type", "run", "Result");
        assert_context_edge(&result, "references", "generic_arg", "run", "DataProcessor");
        assert_context_edge(&result, "references", "field", "DataProcessor", "Result");
    }

    #[test]
    fn test_kotlin_builtin_types_not_emitted_as_references() {
        let result = extract_fixture("sample.kt");
        let targets: HashSet<_> = edge_pairs(&result, "references", None)
            .into_iter()
            .map(|(_, target)| target)
            .collect();
        for builtin in ["String", "Int"] {
            assert!(!targets.contains(builtin));
        }
    }

    #[test]
    fn test_kotlin_user_types_still_emit_references() {
        let result = extract_fixture("sample.kt");
        assert_context_edge(&result, "references", "field", "DataProcessor", "Result");
        assert_context_edge(
            &result,
            "references",
            "parameter_type",
            "run",
            "DataProcessor",
        );
    }

    #[test]
    fn test_scala_no_error() {
        extract_fixture("sample.scala");
    }

    fixture_label_tests!("sample.scala";
        test_scala_finds_class => ["HttpClient"],
        test_scala_finds_object => ["HttpClientFactory"],
        test_scala_finds_methods => ["get", "post"],
    );

    #[test]
    fn test_scala_import_edges_have_import_context() {
        assert_context(
            &extract_fixture("sample.scala"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_scala_splits_inherits_and_mixes_in() {
        let result = extract_fixture("sample.scala");
        assert_edge(&result, "inherits", "HttpClient", "BaseClient");
        assert_edge(&result, "mixes_in", "HttpClient", "Loggable");
    }

    #[test]
    fn test_scala_constructor_parameter_field_context() {
        assert_context_edge(
            &extract_fixture("sample.scala"),
            "references",
            "field",
            "HttpClient",
            "Config",
        );
    }

    #[test]
    fn test_scala_val_definition_field_context() {
        assert_context_edge(
            &extract_fixture("sample.scala"),
            "references",
            "field",
            "HttpClient",
            "Config",
        );
    }

    #[test]
    fn test_scala_var_definition_field_context() {
        assert_context_edge(
            &extract_fixture("sample.scala"),
            "references",
            "field",
            "HttpClient",
            "BaseClient",
        );
    }

    #[test]
    fn test_scala_method_return_type_context() {
        assert_context_edge(
            &extract_fixture("sample.scala"),
            "references",
            "return_type",
            "create",
            "HttpClient",
        );
    }

    #[test]
    fn test_scala_call_edges_have_call_context() {
        assert_context(&extract_fixture("sample.scala"), &["calls"], "call");
    }

    #[test]
    fn test_scala_expression_body_call_direction_follows_the_lexical_caller() {
        let temp = TempFixture::new("scala-expression-call-direction");
        let path = temp.write(
            "Worker.scala",
            "class Worker {\n  def process(value: String): String = value.trim\n}\n\nclass Runner extends Worker {\n  def execute(value: String): String = process(value)\n}\n",
        );
        let result = extract(&path).expect("extract Scala expression bodies");
        let calls = edge_pairs(&result, "calls", None);
        assert!(calls.contains(&("execute".into(), "process".into())));
        assert!(!calls.contains(&("process".into(), "execute".into())));
    }

    #[test]
    fn test_scala_multiline_expression_body_stops_at_the_next_declaration() {
        let temp = TempFixture::new("scala-multiline-expression-call-direction");
        let path = temp.write(
            "Worker.scala",
            "class Worker {\n  def process(value: String): String = value.trim\n}\n\nclass Runner extends Worker {\n  def execute(value: String): String =\n    process(value)\n\n  def unrelated(value: String): String = value\n}\n",
        );
        let result = extract(&path).expect("extract multiline Scala expression body");
        let calls = edge_pairs(&result, "calls", None);
        assert!(calls.contains(&("execute".into(), "process".into())));
        assert!(!calls.contains(&("process".into(), "execute".into())));
        assert!(!calls.contains(&("unrelated".into(), "process".into())));
    }

    #[test]
    fn test_kotlin_multiline_expression_body_stops_at_the_next_declaration() {
        let temp = TempFixture::new("kotlin-multiline-expression-call-direction");
        let path = temp.write(
            "Worker.kt",
            "fun process(value: String): String = value.trim()\n\nfun execute(value: String): String =\n    process(value)\n\nfun unrelated(value: String): String = value\n",
        );
        let result = extract(&path).expect("extract multiline Kotlin expression body");
        let calls = edge_pairs(&result, "calls", None);
        assert!(calls.contains(&("execute".into(), "process".into())));
        assert!(!calls.contains(&("process".into(), "execute".into())));
        assert!(!calls.contains(&("unrelated".into(), "process".into())));
    }

    #[test]
    fn test_jvm_member_call_without_receiver_type_does_not_bind_by_name() {
        let temp = TempFixture::new("jvm-untyped-member-call");
        temp.write(
            "Foreign.kt",
            "class Foreign {\n  fun ping(): String = \"foreign\"\n}\n",
        );
        temp.write(
            "Use.kt",
            "fun run(value: String): String {\n  return value.ping()\n}\n",
        );
        let result = temp.extract_project();
        assert!(!edge_pairs(&result, "calls", None).contains(&("run".into(), "ping".into())));
    }

    #[test]
    fn test_kotlin_super_call_resolves_only_to_the_parent_method() {
        let temp = TempFixture::new("kotlin-super-call");
        let path = temp.write(
            "Worker.kt",
            concat!(
                "open class Worker {\n",
                "  open fun process(value: String): String = value.trim()\n",
                "}\n",
                "class Runner : Worker() {\n",
                "  override fun process(value: String): String = super.process(value)\n",
                "}\n",
            ),
        );
        let result = extract(&path).expect("extract Kotlin super call");
        let worker_process = owned_method_id(&result, "Worker", "process");
        let runner_process = owned_method_id(&result, "Runner", "process");
        assert!(result.edges.iter().any(|edge| {
            edge.relation == "calls"
                && edge.true_source() == runner_process
                && edge.true_target() == worker_process
        }));
    }

    #[test]
    fn test_scala_this_call_resolves_through_the_nearest_parent() {
        let temp = TempFixture::new("scala-this-call");
        let path = temp.write(
            "Worker.scala",
            concat!(
                "class Worker {\n",
                "  def process(value: String): String = value.trim\n",
                "}\n",
                "class Runner extends Worker {\n",
                "  def execute(value: String): String = this.process(value)\n",
                "}\n",
            ),
        );
        let result = extract(&path).expect("extract Scala this call");
        let worker_process = owned_method_id(&result, "Worker", "process");
        let runner_execute = owned_method_id(&result, "Runner", "execute");
        assert!(result.edges.iter().any(|edge| {
            edge.relation == "calls"
                && edge.true_source() == runner_execute
                && edge.true_target() == worker_process
        }));
    }

    #[test]
    fn test_php_no_error() {
        extract_fixture("sample.php");
    }

    fixture_label_tests!("sample.php";
        test_php_finds_class => ["ApiClient"],
        test_php_finds_methods => ["get", "post"],
        test_php_finds_function => ["parseResponse"],
    );

    #[test]
    fn test_php_finds_imports() {
        assert_relation(&extract_fixture("sample.php"), "imports");
    }

    #[test]
    fn test_php_import_edges_have_import_context() {
        assert_context(
            &extract_fixture("sample.php"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_php_call_edges_have_call_context() {
        assert_context(&extract_fixture("sample.php"), &["calls"], "call");
    }

    #[test]
    fn test_php_finds_static_property_access() {
        assert_relation(
            &extract_fixture("sample_php_static_prop.php"),
            "uses_static_prop",
        );
    }

    #[test]
    fn test_php_static_prop_target_is_holding_class() {
        let result = extract_fixture("sample_php_static_prop.php");
        assert!(edge_pairs(&result, "uses_static_prop", None)
            .iter()
            .any(|(_, target)| target.contains("DefaultPalette")));
    }

    #[test]
    fn test_php_finds_config_helper_call() {
        assert_relation(&extract_fixture("sample_php_config.php"), "uses_config");
    }

    #[test]
    fn test_php_config_helper_target_matches_first_segment() {
        let result = extract_fixture("sample_php_config.php");
        assert!(edge_pairs(&result, "uses_config", None)
            .iter()
            .any(|(_, target)| target.contains("Throttle")));
    }

    #[test]
    fn test_php_finds_container_bind() {
        assert_relation(&extract_fixture("sample_php_container.php"), "bound_to");
    }

    #[test]
    fn test_php_container_bind_links_contract_to_implementation() {
        let result = extract_fixture("sample_php_container.php");
        assert!(edge_pairs(&result, "bound_to", None)
            .contains(&("PaymentGateway".into(), "StripeGateway".into())));
    }

    #[test]
    fn test_php_finds_event_listeners() {
        assert_relation(&extract_fixture("sample_php_listen.php"), "listened_by");
    }

    #[test]
    fn test_php_event_listener_links_event_to_listener() {
        let result = extract_fixture("sample_php_listen.php");
        assert!(edge_pairs(&result, "listened_by", None)
            .contains(&("UserRegistered".into(), "SendWelcomeEmail".into())));
    }

    #[test]
    fn test_php_splits_inherits_implements_mixes_in() {
        let result = extract_fixture("sample.php");
        assert_edge(&result, "inherits", "DataProcessor", "BaseProcessor");
        assert_edge(&result, "implements", "DataProcessor", "Loggable");
        assert_edge(&result, "mixes_in", "DataProcessor", "HasName");
    }

    #[test]
    fn test_php_property_parameter_and_return_contexts() {
        let result = extract_fixture("sample.php");
        assert_context_edge(&result, "references", "field", "DataProcessor", "Result");
        assert_context_edge(
            &result,
            "references",
            "parameter_type",
            "run",
            "DataProcessor",
        );
        assert_context_edge(&result, "references", "return_type", "run", "Result");
    }

    #[test]
    fn test_php_constructor_property_promotion_contexts() {
        let result = extract_fixture("sample.php");
        assert_context_edge(&result, "references", "field", "Service", "Result");
        assert_context_edge(
            &result,
            "references",
            "parameter_type",
            "__construct",
            "Result",
        );
        assert!(!edge_pairs(&result, "references", Some("field"))
            .contains(&("Service".into(), "string".into())));
    }
}

mod swift {
    use super::*;

    #[test]
    fn test_swift_no_error() {
        extract_fixture("sample.swift");
    }

    fixture_label_tests!("sample.swift";
        test_swift_finds_class => ["DataProcessor"],
        test_swift_finds_protocol => ["Processor"],
        test_swift_finds_struct => ["Config"],
        test_swift_finds_methods => ["addItem", "process"],
        test_swift_finds_function => ["createProcessor"],
        test_swift_finds_actor => ["CacheManager"],
        test_swift_finds_enum => ["NetworkError"],
        test_swift_finds_enum_methods => ["describe"],
        test_swift_finds_enum_cases => ["timeout", "connectionFailed"],
        test_swift_finds_deinit => ["deinit"],
        test_swift_finds_subscript => ["subscript"],
    );

    #[test]
    fn test_swift_finds_imports() {
        assert_relation(&extract_fixture("sample.swift"), "imports");
    }

    #[test]
    fn test_swift_import_edges_have_import_context() {
        assert_context(
            &extract_fixture("sample.swift"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_swift_no_dangling_edges() {
        assert_no_dangling_edges(&extract_fixture("sample.swift"));
    }

    #[test]
    fn test_swift_imports_survive_build() {
        let result = extract_fixture("sample.swift");
        let imports: Vec<_> = result
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports")
            .collect();
        assert!(!imports.is_empty());
        let node_ids: HashSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
        assert!(imports
            .iter()
            .all(|edge| node_ids.contains(edge.true_target())));
        let modules: HashSet<_> = result
            .nodes
            .iter()
            .filter(|node| {
                node.extra.get("type").and_then(|value| value.as_str()) == Some("module")
            })
            .map(|node| node.label.as_str())
            .collect();
        assert!(["Foundation", "UIKit"]
            .into_iter()
            .all(|module| modules.contains(module)));
        assert!(result
            .edges
            .iter()
            .all(|edge| !edge.extra.contains_key("_import_label")));
        let graph = graphoxide_graph::build_graph(&[result]).expect("build graph");
        assert!(graph.links.iter().any(|edge| edge.relation == "imports"));
    }

    #[test]
    fn test_swift_enum_cases_have_case_of_edge() {
        let result = extract_fixture("sample.swift");
        assert!(
            result
                .edges
                .iter()
                .filter(|edge| edge.relation == "case_of")
                .count()
                >= 2
        );
    }

    #[test]
    fn test_swift_enum_associated_value_type_emits_references() {
        assert_context_edge(
            &extract_fixture("sample.swift"),
            "references",
            "type",
            "NetworkError",
            "Config",
        );
    }

    #[test]
    fn test_swift_extension_methods_attach_to_type() {
        assert_edge(
            &extract_fixture("sample.swift"),
            "method",
            "Config",
            "isValid",
        );
    }

    #[test]
    fn test_swift_extension_does_not_duplicate_type_node() {
        let result = extract_fixture("sample.swift");
        assert_eq!(
            result
                .nodes
                .iter()
                .filter(|node| node.label == "Config")
                .count(),
            1
        );
    }

    #[test]
    fn test_swift_protocol_conformance_emits_implements() {
        assert_edge(
            &extract_fixture("sample.swift"),
            "implements",
            "DataProcessor",
            "Processor",
        );
    }

    #[test]
    fn test_swift_extension_conformance_emits_implements() {
        assert_edge(
            &extract_fixture("sample.swift"),
            "implements",
            "DataProcessor",
            "Loggable",
        );
    }

    #[test]
    fn test_swift_splits_inherits_and_implements() {
        let result = extract_fixture("sample.swift");
        assert_edge(&result, "inherits", "DataProcessor", "BaseProcessor");
        assert_edge(&result, "implements", "DataProcessor", "Processor");
    }

    #[test]
    fn test_swift_parameter_return_generic_and_field_contexts() {
        let result = extract_fixture("sample.swift");
        assert_context_edge(
            &result,
            "references",
            "parameter_type",
            "run",
            "DataProcessor",
        );
        assert_context_edge(&result, "references", "return_type", "run", "Result");
        assert_context_edge(&result, "references", "generic_arg", "run", "DataProcessor");
        assert_context_edge(&result, "references", "field", "DataProcessor", "Result");
    }

    #[test]
    fn test_swift_emits_calls() {
        assert_edge(
            &extract_fixture("sample.swift"),
            "calls",
            "process",
            "validate",
        );
    }

    #[test]
    fn test_swift_call_edges_have_call_context() {
        assert_context(&extract_fixture("sample.swift"), &["calls"], "call");
    }

    #[test]
    fn test_swift_extension_across_files_merges_into_canonical_type() {
        let temp = TempFixture::new("swift-cross-file-extension");
        temp.write(
            "Foo.swift",
            &fs::read_to_string(fixture("swift_cross_file/Foo.swift")).expect("read Foo fixture"),
        );
        temp.write(
            "Foo+Ext.swift",
            &fs::read_to_string(fixture("swift_cross_file/Foo+Ext.swift"))
                .expect("read Foo extension fixture"),
        );
        let result = temp.extract_project();
        let foo_nodes: Vec<_> = result
            .nodes
            .iter()
            .filter(|node| node.label == "Foo")
            .collect();
        assert_eq!(
            foo_nodes.len(),
            1,
            "duplicate canonical Foo nodes: {foo_nodes:?}"
        );
        let methods = edge_pairs(&result, "method", None);
        assert!(methods.contains(&("Foo".into(), "one".into())));
        assert!(methods.contains(&("Foo".into(), "two".into())));
    }
}

mod elixir {
    use super::*;

    fixture_label_tests!("sample.ex";
        test_elixir_finds_module => ["MyApp.Accounts.User"],
        test_elixir_finds_functions => ["create", "find", "validate"],
    );

    #[test]
    fn test_elixir_finds_imports() {
        let result = extract_fixture("sample.ex");
        assert!(
            result
                .edges
                .iter()
                .filter(|edge| edge.relation == "imports")
                .count()
                >= 2
        );
    }

    #[test]
    fn test_elixir_import_edges_have_import_context() {
        assert_context(
            &extract_fixture("sample.ex"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_elixir_multi_alias_expands() {
        let result = extract_fixture("sample.ex");
        let imported: HashSet<_> = edge_pairs(&result, "imports", None)
            .into_iter()
            .map(|(_, target)| target)
            .collect();
        assert!(
            imported.iter().any(|target| target.ends_with("Account")),
            "expanded Account import missing: {imported:?}"
        );
        assert!(
            imported.iter().any(|target| target.ends_with("Token")),
            "expanded Token import missing: {imported:?}"
        );
    }

    #[test]
    fn test_elixir_finds_calls() {
        assert_edge(&extract_fixture("sample.ex"), "calls", "create", "validate");
    }

    #[test]
    fn test_elixir_call_edges_have_call_context() {
        assert_context(&extract_fixture("sample.ex"), &["calls"], "call");
    }

    #[test]
    fn test_elixir_method_edges() {
        assert!(
            extract_fixture("sample.ex")
                .edges
                .iter()
                .filter(|edge| edge.relation == "method")
                .count()
                >= 3
        );
    }
}

mod objective_c_and_go {
    use super::*;

    fn source(label: &str, filename: &str, contents: &str) -> Extraction {
        let temp = TempFixture::new(label);
        let path = temp.write(filename, contents);
        extract(&path).expect("extract Objective-C fixture")
    }

    fixture_label_tests!("sample.m";
        test_objc_finds_interface => ["Animal"],
        test_objc_finds_subclass => ["Dog"],
        test_objc_finds_methods => ["speak", "fetch", "initWithName"],
    );

    #[test]
    fn test_objc_finds_imports() {
        assert_relation(&extract_fixture("sample.m"), "imports");
    }

    #[test]
    fn test_objc_import_edges_have_import_context() {
        assert_context(
            &extract_fixture("sample.m"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_objc_inherits_edge() {
        assert_relation(&extract_fixture("sample.m"), "inherits");
    }

    #[test]
    fn test_objc_splits_inherits_and_implements() {
        let result = extract_fixture("sample.m");
        assert_edge(&result, "inherits", "Animal", "NSObject");
        assert_edge(&result, "inherits", "Dog", "Animal");
        assert_edge(&result, "implements", "Animal", "SampleDelegate");
    }

    #[test]
    fn test_objc_protocol_adopts_protocol() {
        assert_edge(
            &extract_fixture("sample.m"),
            "implements",
            "<Derived>",
            "<Base>",
        );
    }

    #[test]
    fn test_objc_property_type_context() {
        assert_context_edge(
            &extract_fixture("sample.m"),
            "references",
            "field",
            "Animal",
            "NSString",
        );
    }

    #[test]
    fn test_objc_no_dangling_edges() {
        assert_no_dangling_sources(&extract_fixture("sample.m"));
    }

    #[test]
    fn test_objc_resolves_self_method_calls() {
        let result = extract_fixture("sample.m");
        assert_edge(&result, "calls", "-fetch", "-speak");
    }

    #[test]
    fn test_objc_class_method_labeled_with_plus() {
        let result = source(
            "objc-class-method",
            "S.m",
            "@implementation S\n+ (instancetype)shared { return nil; }\n- (void)go { }\n@end\n",
        );
        let actual = labels(&result);
        assert!(actual.contains(&"+shared"));
        assert!(actual.contains(&"-go"));
    }

    #[test]
    fn test_objc_compound_selector_call_resolves() {
        let result = source(
            "objc-compound-selector",
            "V.m",
            "@implementation V\n- (void)tableView:(id)tv numberOfRowsInSection:(int)s { }\n- (void)go { [self tableView:nil numberOfRowsInSection:0]; }\n@end\n",
        );
        assert_edge(&result, "calls", "-go", "-tableViewnumberOfRowsInSection");
    }

    #[test]
    fn test_objc_generic_property_type_extracted() {
        let result = source(
            "objc-generic-property",
            "M.h",
            "@interface M : NSObject\n@property (strong) NSArray<Product *> *items;\n@end\n",
        );
        assert_context_edge(&result, "references", "field", "M", "Product");
        assert_context_edge(&result, "references", "field", "M", "NSArray");
    }

    #[test]
    fn test_objc_module_import_edge() {
        let result = source(
            "objc-module-import",
            "X.m",
            "@import Foundation;\n@import UIKit.UIView;\n@implementation X\n@end\n",
        );
        let imported: HashSet<_> = edge_pairs(&result, "imports", None)
            .into_iter()
            .map(|(_, target)| target)
            .collect();
        assert!(imported.contains("Foundation"));
        assert!(imported.contains("UIKit"));
    }

    #[test]
    fn test_objc_header_dispatch_routes_objc_not_c() {
        let objc = source(
            "objc-header-dispatch",
            "AppDelegate.h",
            "@interface AppDelegate : NSObject <UIApplicationDelegate>\n@end\n",
        );
        assert_labels_contain(&objc, &["AppDelegate"]);
        let c = source(
            "c-header-dispatch",
            "util.h",
            "#include <stdio.h>\nint add(int a, int b);\nstruct Point { int x; };\n",
        );
        assert!(!labels(&c).contains(&"AppDelegate"));
        assert!(c.nodes.iter().any(|node| node.label == "util.h"));
    }

    #[test]
    fn test_objc_ns_assume_nonnull_macro_does_not_break_parsing() {
        let result = source(
            "objc-nullability-macro",
            "AlertManager.h",
            "#import <Foundation/Foundation.h>\nNS_ASSUME_NONNULL_BEGIN\n@class Other;\n@interface AlertManager : NSObject\n- (void)show;\n@end\nNS_ASSUME_NONNULL_END\n",
        );
        assert_labels_contain(&result, &["AlertManager"]);
        assert_edge(&result, "inherits", "AlertManager", "NSObject");
        assert!(!labels(&result).contains(&"Other"));
    }

    #[test]
    fn test_objc_macro_free_header_unchanged() {
        let result = source(
            "objc-plain-header",
            "Plain.h",
            "@interface Plain : NSObject\n- (void)go;\n@end\n",
        );
        assert_labels_contain(&result, &["Plain"]);
        assert_edge(&result, "inherits", "Plain", "NSObject");
    }

    #[test]
    fn test_objc_quoted_import_edges_resolve_to_real_nodes() {
        let temp = TempFixture::new("objc-quoted-imports");
        for (name, contents) in [
            ("Product.h", "@interface Product : NSObject\n@end\n"),
            (
                "Product.m",
                "#import \"Product.h\"\n@implementation Product\n@end\n",
            ),
            ("Order.h", "@interface Order : NSObject\n@end\n"),
            (
                "Order.m",
                "#import \"Order.h\"\n@implementation Order\n@end\n",
            ),
            (
                "ConsumerA.m",
                "#import \"Product.h\"\n@implementation ConsumerA\n@end\n",
            ),
            (
                "ConsumerB.m",
                "#import \"Order.h\"\n@implementation ConsumerB\n@end\n",
            ),
        ] {
            temp.write(name, contents);
        }
        let result = temp.extract_project();
        let by_id = node_labels(&result);
        let imports: Vec<_> = result
            .edges
            .iter()
            .filter(|edge| matches!(edge.relation.as_str(), "imports" | "imports_from"))
            .collect();
        assert!(!imports.is_empty());
        for edge in &imports {
            assert_ne!(edge.true_source(), edge.true_target());
            assert!(by_id
                .get(edge.true_target())
                .is_some_and(|label| label.ends_with(".h")));
        }
        let product_imports: Vec<_> = imports
            .iter()
            .filter(|edge| by_id.get(edge.true_source()) == Some(&"Product.m"))
            .collect();
        assert!(!product_imports.is_empty());
        assert!(product_imports
            .iter()
            .all(|edge| by_id.get(edge.true_target()) == Some(&"Product.h")));
    }

    #[test]
    fn test_objc_alloc_init_emits_type_reference() {
        let temp = TempFixture::new("objc-alloc-reference");
        temp.write("Foo.h", "@interface Foo : NSObject\n@end\n");
        temp.write("Foo.m", "#import \"Foo.h\"\n@implementation Foo\n@end\n");
        temp.write(
            "User.m",
            "#import \"Foo.h\"\n@implementation User\n- (void)build { Foo *x = [[Foo alloc] init]; }\n@end\n",
        );
        assert_edge(&temp.extract_project(), "references", "-build", "Foo");
    }

    #[test]
    fn test_objc_alloc_init_unknown_class_no_resolved_edge() {
        let result = source(
            "objc-unknown-allocation",
            "Caller.m",
            "@implementation Caller\n- (void)build { id x = [[Unknown alloc] init]; }\n- (void)other { [self build]; [x doStuff]; }\n@end\n",
        );
        let sourced: HashSet<_> = result
            .nodes
            .iter()
            .filter(|node| !node.source_file.is_empty())
            .map(|node| node.id.as_str())
            .collect();
        assert!(result
            .edges
            .iter()
            .filter(|edge| edge.relation == "references")
            .all(|edge| !sourced.contains(edge.true_target())));
    }

    #[test]
    fn test_objc_dot_syntax_property_accesses_edge() {
        let result = source(
            "objc-dot-access",
            "Dog.m",
            "@implementation Dog\n- (NSString *)name { return @\"Rex\"; }\n- (void)greet { NSLog(@\"%@\", self.name); }\n@end\n",
        );
        let accesses = edge_pairs(&result, "accesses", None);
        assert_eq!(
            accesses,
            BTreeSet::from([("-greet".into(), "-name".into())])
        );
    }

    #[test]
    fn test_objc_dot_syntax_no_fanout_two_same_named_properties() {
        let result = source(
            "objc-two-dot-accesses",
            "AB.m",
            "@implementation A\n- (NSString *)name { return @\"A\"; }\n- (void)show { NSLog(@\"%@\", self.name); }\n@end\n@implementation B\n- (NSString *)name { return @\"B\"; }\n- (void)show { NSLog(@\"%@\", self.name); }\n@end\n",
        );
        let accesses: Vec<_> = result
            .edges
            .iter()
            .filter(|edge| edge.relation == "accesses")
            .collect();
        assert_eq!(accesses.len(), 2);
        let by_id = node_labels(&result);
        for edge in accesses {
            assert_eq!(normalize_symbol_label(by_id[edge.true_source()]), "-show");
            assert_eq!(normalize_symbol_label(by_id[edge.true_target()]), "-name");
        }
    }

    #[test]
    fn test_objc_dot_syntax_unresolvable_property_zero_edges() {
        let result = source(
            "objc-missing-dot-access",
            "X.m",
            "@implementation X\n- (void)run { NSLog(@\"%@\", self.missing); }\n@end\n",
        );
        assert!(!result.edges.iter().any(|edge| edge.relation == "accesses"));
    }

    #[test]
    fn test_objc_selector_expression_calls_edge() {
        let result = source(
            "objc-selector",
            "Sched.m",
            "@implementation Sched\n- (void)fetch { }\n- (void)schedule { [self performSelector:@selector(fetch)]; }\n@end\n",
        );
        assert_context_edge(&result, "calls", "call", "-schedule", "-fetch");
    }

    #[test]
    fn test_objc_selector_no_fanout_two_same_named_methods() {
        let result = source(
            "objc-ambiguous-selector",
            "Dual.m",
            "@implementation A\n- (void)doThing { }\n- (void)run { [self performSelector:@selector(doThing)]; }\n@end\n@implementation B\n- (void)doThing { }\n@end\n",
        );
        assert!(!edge_pairs(&result, "calls", None)
            .iter()
            .any(|(_, target)| target.ends_with("doThing")));
    }

    #[test]
    fn test_objc_dot_syntax_substring_sibling_exact_match() {
        let result = source(
            "objc-substring-property",
            "Person.m",
            "@implementation Person\n- (NSString *)name { return @\"n\"; }\n- (NSString *)surname { return @\"s\"; }\n- (void)show { NSLog(@\"%@\", self.name); }\n@end\n",
        );
        assert_edge(&result, "accesses", "-show", "-name");
        assert!(
            !edge_pairs(&result, "accesses", None).contains(&("-show".into(), "-surname".into()))
        );
    }

    #[test]
    fn test_objc_selector_substring_method_exact_match() {
        let result = source(
            "objc-substring-selector",
            "Worker.m",
            "@implementation Worker\n- (void)doThing { }\n- (void)reallyDoThing { }\n- (void)run { [self performSelector:@selector(doThing)]; }\n@end\n",
        );
        assert_context_edge(&result, "calls", "call", "-run", "-doThing");
        assert!(
            !edge_pairs(&result, "calls", None).contains(&("-run".into(), "-reallyDoThing".into()))
        );
    }

    #[test]
    fn test_go_receiver_methods_share_type_node() {
        let result = extract_fixture("sample.go");
        assert_eq!(
            result
                .nodes
                .iter()
                .filter(|node| node.label == "Server")
                .count(),
            1
        );
        assert_edge(&result, "method", "Server", "Start");
        assert_edge(&result, "method", "Server", "Stop");
    }

    #[test]
    fn test_go_receiver_uses_pkg_scope() {
        let result = extract_fixture("sample.go");
        let server = node_by_label(&result, "Server");
        let sample_stem = fixture("sample.go").with_extension("");
        assert_ne!(
            server.id,
            graphoxide_core::make_id(&[&sample_stem.to_string_lossy(), "Server"])
        );
    }
}

mod julia_fortran_powershell {
    use super::*;

    fn source(label: &str, filename: &str, contents: &str) -> Extraction {
        let temp = TempFixture::new(label);
        let path = temp.write(filename, contents);
        extract(&path).expect("extract language fixture")
    }

    fixture_label_tests!("sample.jl";
        test_julia_finds_module => ["Geometry"],
        test_julia_finds_structs => ["Point", "Circle"],
        test_julia_finds_abstract_type => ["Shape"],
        test_julia_finds_functions => ["area", "distance"],
        test_julia_finds_short_function => ["perimeter"],
    );

    #[test]
    fn test_julia_finds_imports() {
        assert_relation(&extract_fixture("sample.jl"), "imports");
    }

    #[test]
    fn test_julia_import_edges_have_import_context() {
        assert_context(
            &extract_fixture("sample.jl"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_julia_qualified_and_relative_imports() {
        let targets: HashSet<_> = edge_pairs(&extract_fixture("sample.jl"), "imports", None)
            .into_iter()
            .map(|(_, target)| target)
            .collect();
        assert!(targets.iter().any(|target| target == "Base.Threads"));
        assert!(targets.iter().any(|target| target == "ParentModule"));
    }

    #[test]
    fn test_julia_finds_inherits() {
        assert_relation(&extract_fixture("sample.jl"), "inherits");
    }

    #[test]
    fn test_julia_abstract_concrete_hierarchy_inherits() {
        let result = extract_fixture("sample.jl");
        assert_edge(&result, "inherits", "Point", "Shape");
        assert_edge(&result, "inherits", "Circle", "Shape");
    }

    #[test]
    fn test_julia_struct_field_type_context() {
        let result = extract_fixture("sample.jl");
        assert_context_edge(&result, "references", "field", "Point", "Float64");
        assert_context_edge(&result, "references", "field", "Circle", "Point");
        assert_context_edge(&result, "references", "field", "Circle", "Float64");
    }

    #[test]
    fn test_julia_finds_calls() {
        assert_relation(&extract_fixture("sample.jl"), "calls");
    }

    #[test]
    fn test_julia_call_edges_have_call_context() {
        assert_context(&extract_fixture("sample.jl"), &["calls"], "call");
    }

    #[test]
    fn test_julia_no_dangling_edges() {
        assert_no_dangling_sources(&extract_fixture("sample.jl"));
    }

    fixture_label_tests!("sample.f90";
        test_fortran_finds_module => ["geometry"],
        test_fortran_finds_subroutines => ["circle_area", "print_area"],
        test_fortran_finds_function => ["distance"],
        test_fortran_finds_program => ["main"],
        test_fortran_finds_derived_type => ["point"],
    );

    #[test]
    fn test_fortran_finds_use_imports() {
        assert!(
            extract_fixture("sample.f90")
                .edges
                .iter()
                .filter(|edge| edge.relation == "imports")
                .count()
                >= 2
        );
    }

    #[test]
    fn test_fortran_use_edges_have_use_context() {
        assert_context(&extract_fixture("sample.f90"), &["imports"], "use");
    }

    #[test]
    fn test_fortran_finds_calls() {
        assert_relation(&extract_fixture("sample.f90"), "calls");
    }

    #[test]
    fn test_fortran_finds_function_call() {
        assert_edge(
            &extract_fixture("sample.f90"),
            "calls",
            "report",
            "double_val",
        );
    }

    #[test]
    fn test_fortran_case_insensitive_names() {
        let result = extract_fixture("sample.f90");
        for expected in ["geometry", "main", "point", "circle_area", "distance"] {
            let node = node_by_label(&result, expected);
            assert_eq!(normalize_symbol_label(&node.label), expected);
        }
    }

    #[test]
    fn test_fortran_parameter_and_return_type_contexts() {
        let result = extract_fixture("sample.f90");
        assert_context_edge(
            &result,
            "references",
            "parameter_type",
            "translate",
            "point",
        );
        assert_context_edge(&result, "references", "return_type", "origin", "point");
    }

    #[test]
    fn test_fortran_no_dangling_edges() {
        assert_no_dangling_sources(&extract_fixture("sample.f90"));
    }

    #[test]
    #[allow(non_snake_case)]
    fn test_fortran_capital_F_parses_preprocessed() {
        let result = extract_fixture("sample_preprocessed.F90");
        assert_labels_contain(&result, &["shapes", "compute_volume"]);
    }

    #[test]
    fn test_powershell_no_error() {
        extract_fixture("sample.ps1");
    }

    #[test]
    fn test_powershell_psm1_dispatched_and_extracted() {
        let result = source(
            "powershell-psm1",
            "Utils.psm1",
            "function Get-Greeting { param([string]$Name) return \"Hi $Name\" }\n",
        );
        assert_labels_contain(&result, &["Get-Greeting"]);
    }

    #[test]
    fn test_powershell_finds_class_and_method() {
        assert_labels_contain(
            &extract_fixture("sample.ps1"),
            &["DataProcessor", "Transform"],
        );
    }

    #[test]
    fn test_powershell_class_base_type_emits_inherits_edge() {
        assert_edge(
            &extract_fixture("sample.ps1"),
            "inherits",
            "Circle",
            "Shape",
        );
    }

    #[test]
    fn test_powershell_property_field_type_context() {
        assert_context_edge(
            &extract_fixture("sample.ps1"),
            "references",
            "field",
            "DataProcessor",
            "string",
        );
    }

    #[test]
    fn test_powershell_method_parameter_and_return_type_contexts() {
        let result = extract_fixture("sample.ps1");
        assert_context_edge(
            &result,
            "references",
            "parameter_type",
            "Transform",
            "string",
        );
        assert_context_edge(&result, "references", "return_type", "Transform", "string");
        assert_context_edge(&result, "references", "return_type", "Save", "void");
    }

    fn import_targets(fixture_name: &str) -> HashSet<String> {
        edge_pairs(&extract_fixture(fixture_name), "imports_from", None)
            .into_iter()
            .map(|(_, target)| target.to_ascii_lowercase())
            .collect()
    }

    #[test]
    fn test_powershell_import_module_emits_edge() {
        assert!(import_targets("sample_import.ps1").contains("foo"));
    }

    #[test]
    fn test_powershell_import_module_with_name_param() {
        assert!(import_targets("sample_import.ps1").contains("bar"));
    }

    #[test]
    fn test_powershell_dot_source_forward_slash_emits_edge() {
        assert!(import_targets("sample_import.ps1").contains("shared"));
    }

    #[test]
    fn test_powershell_dot_source_backslash_emits_edge() {
        assert!(import_targets("sample_import.ps1").contains("utils"));
    }

    #[test]
    fn test_powershell_import_module_inside_function_emits_edge() {
        assert!(import_targets("sample_import.ps1").contains("innermod"));
    }

    #[test]
    fn test_powershell_import_module_not_a_raw_call() {
        let result = extract_fixture("sample_import.ps1");
        assert!(!result.edges.iter().any(|edge| {
            edge.relation == "calls"
                && node_labels(&result)
                    .get(edge.true_target())
                    .is_some_and(|label| label.eq_ignore_ascii_case("Import-Module"))
        }));
    }

    #[test]
    fn test_powershell_dot_source_inside_function_emits_edge() {
        assert!(import_targets("sample_import.ps1").contains("innershared"));
    }

    #[test]
    fn test_powershell_psd1_dispatched() {
        let result = source(
            "powershell-psd1-dispatch",
            "Demo.psd1",
            "@{\n    RootModule = 'X.psm1'\n}\n",
        );
        assert!(edge_pairs(&result, "imports_from", None)
            .iter()
            .any(|(_, target)| target == "X"));
    }

    #[test]
    fn test_powershell_psd1_no_error() {
        extract_fixture("sample.psd1");
    }

    #[test]
    fn test_powershell_psd1_has_file_node() {
        assert_labels_contain(&extract_fixture("sample.psd1"), &["sample.psd1"]);
    }

    #[test]
    fn test_powershell_psd1_root_module() {
        assert!(import_targets("sample.psd1").contains("mymodule"));
    }

    #[test]
    fn test_powershell_psd1_nested_modules() {
        let targets = import_targets("sample.psd1");
        assert!(targets.contains("helpers"));
        assert!(targets.contains("logger"));
    }

    #[test]
    fn test_powershell_psd1_required_modules_string() {
        assert!(import_targets("sample.psd1").contains("psreadline"));
    }

    #[test]
    fn test_powershell_psd1_required_modules_hashtable() {
        assert!(import_targets("sample.psd1").contains("pester"));
    }

    #[test]
    fn test_powershell_psd1_no_moduleversion_as_edge() {
        let targets = import_targets("sample.psd1");
        for version in ["5_0", "1_0_0", "5.0", "1.0.0"] {
            assert!(!targets.contains(version));
        }
    }

    #[test]
    fn test_powershell_psd1_no_dangling_edges() {
        assert_no_dangling_sources(&extract_fixture("sample.psd1"));
    }
}

mod typescript_dynamic_and_injection {
    use super::*;

    fn source(label: &str, filename: &str, contents: &str) -> Extraction {
        let temp = TempFixture::new(label);
        let path = temp.write(filename, contents);
        extract(&path).expect("extract JavaScript/TypeScript fixture")
    }

    fn dynamic_imports() -> Extraction {
        extract_fixture("dynamic_import.ts")
    }

    fn import_edges(result: &Extraction) -> Vec<&Edge> {
        result
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports_from")
            .collect()
    }

    #[test]
    fn test_ts_dynamic_import_no_error() {
        dynamic_imports();
    }

    #[test]
    fn test_ts_dynamic_import_extracts_edges() {
        let result = dynamic_imports();
        let targets: HashSet<_> = import_edges(&result)
            .into_iter()
            .map(|edge| edge.true_target().to_ascii_lowercase())
            .collect();
        for expected in ["logger", "mayaengine", "queue"] {
            assert!(
                targets.iter().any(|target| target.contains(expected)),
                "missing {expected} import: {targets:?}"
            );
        }
    }

    #[test]
    fn test_ts_dynamic_import_confidence() {
        let result = dynamic_imports();
        let edge = import_edges(&result)
            .into_iter()
            .find(|edge| {
                edge.true_target()
                    .to_ascii_lowercase()
                    .contains("mayaengine")
            })
            .expect("mayaEngine dynamic import");
        assert_eq!(edge.confidence, Confidence::Extracted);
    }

    #[test]
    fn test_ts_dynamic_import_source_is_function() {
        let result = dynamic_imports();
        let by_id = node_labels(&result);
        let edge = import_edges(&result)
            .into_iter()
            .find(|edge| {
                edge.true_target()
                    .to_ascii_lowercase()
                    .contains("mayaengine")
            })
            .expect("mayaEngine dynamic import");
        assert!(by_id
            .get(edge.true_source())
            .is_some_and(|label| label.contains("processInbound")));
    }

    #[test]
    fn test_ts_no_dynamic_import_in_sync_fn() {
        let result = dynamic_imports();
        let sync = node_by_label(&result, "syncOnly");
        assert!(!import_edges(&result)
            .iter()
            .any(|edge| edge.true_source() == sync.id));
    }

    #[test]
    fn test_ts_dynamic_template_literal_skipped() {
        let result = dynamic_imports();
        assert!(import_edges(&result).iter().all(|edge| {
            !edge.true_target().contains('$')
                && !edge.true_target().contains('{')
                && !edge.true_target().contains('}')
        }));
    }

    #[test]
    fn test_ts_static_template_literal_resolved() {
        assert!(import_edges(&dynamic_imports()).iter().any(|edge| edge
            .true_target()
            .to_ascii_lowercase()
            .contains("statichelper")));
    }

    #[test]
    fn test_js_local_const_does_not_emit_phantom_node() {
        let result = source(
            "js-scope-guard",
            "scope_guard.js",
            "describe('suite', () => {\n  const inner = new Set([1, 2, 3]);\n  let other = [1, 2];\n});\n\nconst moduleConst = new Set([4, 5]);\nexport const exportedConst = { a: 1 };\n",
        );
        let actual = labels(&result);
        assert!(!actual.contains(&"inner"));
        assert!(!actual.contains(&"other"));
        assert!(actual.contains(&"moduleConst"));
        assert!(actual.contains(&"exportedConst"));
    }

    #[test]
    fn test_js_module_level_arrow_produces_node_and_call_edges() {
        let result = source(
            "js-module-arrow",
            "arrows.js",
            "function helper() { return 1; }\nconst handler = () => {\n  helper();\n};\n",
        );
        assert_labels_contain(&result, &["handler"]);
        assert_edge(&result, "calls", "handler", "helper");
    }

    #[test]
    fn test_ts_local_const_does_not_emit_phantom_node() {
        let result = source(
            "ts-scope-guard",
            "scope_guard.ts",
            "describe('suite', () => {\n  const inner: Set<number> = new Set([1, 2]);\n});\n\nexport const topLevel = { a: 1 };\n",
        );
        let actual = labels(&result);
        assert!(!actual.contains(&"inner"));
        assert!(actual.contains(&"topLevel"));
    }

    #[test]
    fn test_ts_constructor_injection_calls_edge() {
        let temp = TempFixture::new("ts-constructor-injection");
        temp.write(
            "repo.ts",
            "export interface IUserRepository {\n  findById(id: string): Promise<any>;\n  save(user: any): Promise<void>;\n}\n",
        );
        temp.write(
            "service.ts",
            "import { IUserRepository } from './repo';\nexport class UserService {\n  constructor(private repo: IUserRepository) {}\n  getUser(id: string) { return this.repo.findById(id); }\n}\n",
        );
        assert_edge(&temp.extract_project(), "calls", "getUser", "findById");
    }

    #[test]
    fn test_ts_this_field_receiver_not_same_file_collision() {
        let result = source(
            "ts-receiver-collision",
            "collision.ts",
            "function query() { return 'global'; }\nexport class Service {\n  constructor(private db: Database) {}\n  run() { return this.db.query(); }\n}\n",
        );
        assert!(!edge_pairs(&result, "calls", None).contains(&("run".into(), "query".into())));
    }

    #[test]
    fn test_ts_injected_field_resolves_to_typed_class_not_same_named_collision() {
        let temp = TempFixture::new("ts-typed-receiver");
        temp.write(
            "database.ts",
            "export class Database {\n  query(sql: string) { return sql; }\n}\n",
        );
        temp.write(
            "http.ts",
            "export class HttpClient {\n  query(url: string) { return url; }\n}\n",
        );
        temp.write(
            "service.ts",
            "import { Database } from './database';\nexport class Service {\n  constructor(private db: Database) {}\n  run() { return this.db.query('x'); }\n}\n",
        );
        let result = temp.extract_project();
        let by_id = node_labels(&result);
        let method_owner: HashMap<_, _> = result
            .edges
            .iter()
            .filter(|edge| edge.relation == "method")
            .map(|edge| (edge.true_target(), edge.true_source()))
            .collect();
        let targets: Vec<_> = result
            .edges
            .iter()
            .filter(|edge| edge.relation == "calls")
            .filter(|edge| {
                by_id
                    .get(edge.true_source())
                    .is_some_and(|label| label.contains("run"))
                    && by_id
                        .get(edge.true_target())
                        .is_some_and(|label| label.contains("query"))
            })
            .map(|edge| edge.true_target())
            .collect();
        assert!(!targets.is_empty());
        for target in targets {
            let owner = method_owner.get(target).expect("query method owner");
            assert_eq!(by_id.get(*owner), Some(&"Database"));
        }
    }

    #[test]
    fn test_ts_injected_field_ambiguous_type_emits_no_edge() {
        let temp = TempFixture::new("ts-ambiguous-receiver");
        temp.write(
            "a/database.ts",
            "export class Database {\n  query(sql: string) { return sql; }\n}\n",
        );
        temp.write(
            "b/database.ts",
            "export class Database {\n  query(sql: string) { return sql; }\n}\n",
        );
        temp.write(
            "service.ts",
            "export class Service {\n  constructor(private db: Database) {}\n  run() { return this.db.query('x'); }\n}\n",
        );
        let result = temp.extract_project();
        assert!(!edge_pairs(&result, "calls", None).contains(&("run".into(), "query".into())));
    }
}

mod groovy {
    use super::*;

    #[test]
    fn test_groovy_no_error() {
        extract_fixture("sample.groovy");
    }

    fixture_label_tests!("sample.groovy";
        test_groovy_finds_class => ["SampleService"],
        test_groovy_finds_methods => ["process", "reset"],
    );

    #[test]
    fn test_groovy_finds_imports() {
        assert_relation(&extract_fixture("sample.groovy"), "imports");
    }

    #[test]
    fn test_groovy_import_edges_have_import_context() {
        assert_context(
            &extract_fixture("sample.groovy"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_groovy_no_dangling_edges() {
        assert_no_dangling_sources(&extract_fixture("sample.groovy"));
    }

    #[test]
    fn test_groovy_extends_edge() {
        assert_edge(
            &extract_fixture("sample.groovy"),
            "inherits",
            "ExtendedService",
            "SampleService",
        );
    }

    #[test]
    fn test_groovy_implements_edge() {
        assert_edge(
            &extract_fixture("sample.groovy"),
            "implements",
            "ExtendedService",
            "Resettable",
        );
    }

    #[test]
    fn test_groovy_emits_in_class_method_calls_in_the_forward_direction() {
        let temp = TempFixture::new("groovy-method-call-direction");
        let path = temp.write(
            "Worker.groovy",
            "class Worker {\n  String process(String value) { value.trim() }\n}\nclass Runner extends Worker {\n  String execute(String value) { process(value) }\n}\n",
        );
        let result = extract(&path).expect("extract Groovy calls");
        let calls = edge_pairs(&result, "calls", None);
        assert!(calls.contains(&("execute".into(), "process".into())));
        assert!(!calls.contains(&("process".into(), "execute".into())));
    }

    #[test]
    fn test_groovy_statements_are_not_reparsed_as_method_declarations() {
        let temp = TempFixture::new("groovy-statements-not-methods");
        let path = temp.write(
            "Worker.groovy",
            concat!(
                "class Worker {\n",
                "  String process(String value) { return value }\n",
                "  String execute(String value) {\n",
                "    process(value);\n",
                "    if (value) { process(value); }\n",
                "    return process(value);\n",
                "  }\n",
                "}\n",
            ),
        );
        let result = extract(&path).expect("extract Groovy statement bodies");
        let method_labels = labels(&result)
            .into_iter()
            .filter(|label| label.starts_with('.'))
            .collect::<Vec<_>>();
        assert_eq!(
            method_labels
                .iter()
                .filter(|label| normalize_symbol_label(label) == "process")
                .count(),
            1
        );
        assert_eq!(
            method_labels
                .iter()
                .filter(|label| normalize_symbol_label(label) == "execute")
                .count(),
            1
        );
        assert!(!method_labels
            .iter()
            .any(|label| normalize_symbol_label(label) == "if"));
        assert_edge(&result, "calls", "execute", "process");
    }

    #[test]
    fn test_groovy_member_call_without_receiver_type_does_not_bind_by_name() {
        let temp = TempFixture::new("groovy-untyped-member-call");
        let path = temp.write(
            "Worker.groovy",
            concat!(
                "class Foreign {\n",
                "  String ping() { return 'foreign' }\n",
                "}\n",
                "class Runner {\n",
                "  String run(String value) { return value.ping() }\n",
                "}\n",
            ),
        );
        let result = extract(&path).expect("extract Groovy receiver call");
        assert!(!edge_pairs(&result, "calls", None).contains(&("run".into(), "ping".into())));
    }

    #[test]
    fn test_groovy_this_call_selects_the_method_on_the_current_owner() {
        let temp = TempFixture::new("groovy-this-call");
        let path = temp.write(
            "Worker.groovy",
            concat!(
                "class Foreign {\n",
                "  String ping() { return 'foreign' }\n",
                "}\n",
                "class Runner {\n",
                "  String ping() { return 'runner' }\n",
                "  String run() { return this.ping() }\n",
                "}\n",
            ),
        );
        let result = extract(&path).expect("extract Groovy this call");
        let foreign_ping = owned_method_id(&result, "Foreign", "ping");
        let runner_ping = owned_method_id(&result, "Runner", "ping");
        let run = owned_method_id(&result, "Runner", "run");
        assert!(result.edges.iter().any(|edge| {
            edge.relation == "calls"
                && edge.true_source() == run
                && edge.true_target() == runner_ping
        }));
        assert!(!result.edges.iter().any(|edge| {
            edge.relation == "calls"
                && edge.true_source() == run
                && edge.true_target() == foreign_ping
        }));
    }

    #[test]
    fn test_groovy_spock_finds_class() {
        assert_labels_contain(&extract_fixture("sample_spock.groovy"), &["SampleSpec"]);
    }

    #[test]
    fn test_groovy_spock_finds_feature_methods() {
        let result = extract_fixture("sample_spock.groovy");
        assert!(
            labels(&result)
                .iter()
                .filter(|label| label.starts_with('"'))
                .count()
                >= 2
        );
    }

    #[test]
    fn test_groovy_spock_finds_method_with_apostrophe() {
        assert!(labels(&extract_fixture("sample_spock.groovy"))
            .iter()
            .any(|label| label.contains("it's")));
    }

    #[test]
    fn test_groovy_spock_preserves_import_edges() {
        assert_relation(&extract_fixture("sample_spock.groovy"), "imports");
    }

    #[test]
    fn test_groovy_spock_no_dangling_edges() {
        assert_no_dangling_sources(&extract_fixture("sample_spock.groovy"));
    }
}

mod dm_family {
    use super::*;

    fn dm() -> Extraction {
        extract_fixture("sample.dm")
    }

    #[test]
    fn test_dm_no_error() {
        dm();
    }

    #[test]
    fn test_dm_finds_global_proc() {
        let result = dm();
        let actual = labels(&result);
        assert!(actual.contains(&"log_event()"));
        assert!(actual.contains(&"RunTest()"));
    }

    #[test]
    fn test_dm_finds_type_definition() {
        let result = dm();
        let actual = labels(&result);
        assert!(actual.contains(&"/datum/weapon"));
        assert!(actual.contains(&"/datum/weapon/sword"));
    }

    #[test]
    fn test_dm_qualifies_proc_with_type_path() {
        let result = dm();
        let actual = labels(&result);
        assert!(actual.contains(&"/datum/weapon/attack()"));
        assert!(actual.contains(&"/datum/weapon/sword/attack()"));
    }

    #[test]
    fn test_dm_finds_path_form_proc_definition() {
        assert!(labels(&dm()).contains(&"/datum/weapon/sword/sharpen()"));
    }

    #[test]
    fn test_dm_emits_include_edge() {
        assert_context(&dm(), &["imports", "imports_from"], "import");
    }

    #[test]
    fn test_dm_unresolved_include_flagged_external() {
        let result = dm();
        let edge = result
            .edges
            .iter()
            .find(|edge| {
                matches!(edge.relation.as_str(), "imports" | "imports_from")
                    && edge.true_target().contains("helpers")
            })
            .expect("helpers.dm include edge");
        assert_eq!(
            edge.extra.get("external").and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn test_dm_resolves_in_file_calls() {
        let result = dm();
        assert!(edge_pairs(&result, "calls", None)
            .iter()
            .any(|(_, callee)| callee == "log_event"));
        assert_edge(
            &result,
            "calls",
            "/datum/weapon/sword/attack",
            "/datum/weapon/sword/sharpen",
        );
    }

    #[test]
    fn test_dm_ambiguous_member_call_left_unresolved() {
        let result = dm();
        assert_eq!(
            labels(&result)
                .into_iter()
                .filter(|label| label.ends_with("/attack()"))
                .count(),
            2,
            "fixture must retain the ambiguity that guards this assertion"
        );
        assert!(!edge_pairs(&result, "calls", None)
            .iter()
            .any(|(caller, callee)| caller == "RunTest" && callee.ends_with("/attack")));
    }

    #[test]
    fn test_dm_emits_new_as_instantiates() {
        assert_edge(&dm(), "instantiates", "RunTest", "/datum/weapon/sword");
    }

    #[test]
    fn test_dm_call_edges_have_call_context() {
        assert_context(&dm(), &["calls", "instantiates"], "call");
    }

    #[test]
    fn test_dm_no_dangling_edges() {
        assert_no_dangling_edges(&dm());
    }

    #[test]
    fn test_dm_super_call_not_emitted() {
        let result = dm();
        assert!(result
            .edges
            .iter()
            .filter(|edge| edge.relation == "calls")
            .all(|edge| !edge.true_target().contains("..")));
    }

    #[test]
    fn test_dmi_no_error() {
        extract_fixture("sample.dmi");
    }

    #[test]
    fn test_dmi_emits_state_nodes() {
        assert!(labels(&extract_fixture("sample.dmi")).contains(&"\"mob\""));
    }

    #[test]
    fn test_dmi_state_contained_by_file() {
        assert_edge(
            &extract_fixture("sample.dmi"),
            "contains",
            "sample.dmi",
            "\"mob\"",
        );
    }

    #[test]
    fn test_dmm_no_error() {
        extract_fixture("sample.dmm");
    }

    fn dmm_targets() -> HashSet<String> {
        extract_fixture("sample.dmm")
            .edges
            .into_iter()
            .filter(|edge| edge.relation == "uses")
            .map(|edge| edge.true_target().to_owned())
            .collect()
    }

    #[test]
    fn test_dmm_extracts_type_paths_as_uses_edges() {
        let targets = dmm_targets();
        for target in [
            "turf_closed_wall",
            "obj_structure_table",
            "obj_item_weapon_sword",
        ] {
            assert!(targets.contains(target), "missing DMM target {target}");
        }
    }

    #[test]
    fn test_dmm_strips_var_overrides() {
        let targets = dmm_targets();
        assert!(targets.iter().all(|target| !target.contains('{')));
        assert!(targets.contains("obj_item_weapon_sword"));
    }

    #[test]
    fn test_dmm_handles_multiline_tile_definition() {
        assert!(dmm_targets().contains("area_station_maintenance"));
    }

    #[test]
    fn test_dmm_skips_grid_section() {
        assert_eq!(dmm_targets().len(), 5);
    }

    #[test]
    fn test_dmf_no_error() {
        extract_fixture("sample.dmf");
    }

    #[test]
    fn test_dmf_extracts_windows() {
        let result = extract_fixture("sample.dmf");
        let actual = labels(&result);
        assert!(actual.contains(&"window \"mapwindow\""));
        assert!(actual.contains(&"window \"infowindow\""));
    }

    #[test]
    fn test_dmf_elem_labels_carry_control_type() {
        assert!(labels(&extract_fixture("sample.dmf")).contains(&"elem \"map\" [MAP]"));
    }

    #[test]
    fn test_dmf_elem_under_window() {
        assert_edge(
            &extract_fixture("sample.dmf"),
            "contains",
            "window \"mapwindow\"",
            "elem \"map\" [MAP]",
        );
    }

    #[test]
    fn test_dmf_no_dangling_edges() {
        assert_no_dangling_edges(&extract_fixture("sample.dmf"));
    }
}

mod dotnet_apex_systemverilog {
    use super::*;

    #[test]
    fn test_sln_no_error() {
        extract_fixture("sample.sln");
    }

    #[test]
    fn test_sln_finds_projects() {
        assert_labels_contain(&extract_fixture("sample.sln"), &["WebApi", "Domain"]);
    }

    #[test]
    fn test_sln_contains_edges() {
        assert_relation(&extract_fixture("sample.sln"), "contains");
    }

    #[test]
    fn test_sln_project_dependency_edges() {
        assert_relation(&extract_fixture("sample.sln"), "imports");
    }

    #[test]
    fn test_csproj_no_error() {
        extract_fixture("sample.csproj");
    }

    #[test]
    fn test_csproj_finds_packages() {
        assert_labels_contain(
            &extract_fixture("sample.csproj"),
            &["MediatR", "FluentValidation"],
        );
    }

    #[test]
    fn test_csproj_finds_project_references() {
        let fixture = TempFixture::new("csproj-project-references");
        let project = fixture.write(
            "App/sample.csproj",
            &fs::read_to_string(super::fixture("sample.csproj"))
                .expect("read project-reference fixture"),
        );
        fixture.write("Domain/Domain.csproj", "<Project />");
        fixture.write("Infrastructure/Infrastructure.csproj", "<Project />");

        assert_labels_contain(
            &extract(&project).expect("extract project with proven references"),
            &["Domain.csproj"],
        );
    }

    #[test]
    fn test_csproj_finds_target_framework() {
        assert_labels_contain(&extract_fixture("sample.csproj"), &["net8.0"]);
    }

    #[test]
    fn test_csproj_finds_sdk() {
        assert_labels_contain(
            &extract_fixture("sample.csproj"),
            &["Microsoft.NET.Sdk.Web"],
        );
    }

    #[test]
    fn test_xaml_finds_class_and_event_references() {
        let result = extract_fixture("sample.xaml");
        assert!(labels(&result).contains(&"MainWindow"));
        assert!(result
            .edges
            .iter()
            .any(|edge| { edge.relation == "references" && edge_context(edge) == Some("event") }));
    }

    #[test]
    fn test_razor_no_error() {
        extract_fixture("sample.razor");
    }

    #[test]
    fn test_razor_finds_using_directives() {
        assert_relation(&extract_fixture("sample.razor"), "imports");
    }

    #[test]
    fn test_razor_finds_component_references() {
        assert_relation(&extract_fixture("sample.razor"), "calls");
    }

    #[test]
    fn test_razor_finds_inherits() {
        assert_relation(&extract_fixture("sample.razor"), "inherits");
    }

    #[test]
    fn test_razor_finds_code_block_methods() {
        assert_labels_contain(
            &extract_fixture("sample.razor"),
            &["IncrementCount", "LoadData"],
        );
    }

    #[test]
    fn test_razor_no_dangling_edges() {
        assert_no_dangling_sources(&extract_fixture("sample.razor"));
    }

    #[test]
    fn test_apex_class_extraction() {
        assert!(labels(&extract_fixture("sample.cls")).contains(&"AccountService"));
    }

    #[test]
    fn test_apex_enum_extraction() {
        assert!(labels(&extract_fixture("sample.cls")).contains(&"AccountStatus"));
    }

    #[test]
    fn test_apex_interface_extraction() {
        assert!(labels(&extract_fixture("sample.cls")).contains(&"Notifiable"));
    }

    #[test]
    fn test_apex_interface_extends() {
        let temp = TempFixture::new("apex-interface-extends");
        let path = temp.write(
            "PaymentProcessor.cls",
            "public interface PaymentProcessor extends Processor, Auditable { void process(); }\n",
        );
        let result = extract(&path).expect("extract Apex interface");
        assert_edge(&result, "extends", "PaymentProcessor", "Processor");
        assert_edge(&result, "extends", "PaymentProcessor", "Auditable");
    }

    #[test]
    fn test_apex_method_extraction() {
        assert_labels_contain(
            &extract_fixture("sample.cls"),
            &[
                "getAccounts",
                "updateAccountsAsync",
                "createAccounts",
                "deleteOldAccounts",
            ],
        );
    }

    #[test]
    fn test_apex_contains_and_method_relations() {
        let result = extract_fixture("sample.cls");
        assert_relation(&result, "contains");
        assert_relation(&result, "method");
    }

    #[test]
    fn test_apex_soql_uses_edge() {
        let result = extract_fixture("sample.cls");
        assert_relation(&result, "uses");
        assert!(labels(&result).contains(&"Account"));
    }

    #[test]
    fn test_apex_dml_uses_edge() {
        let result = extract_fixture("sample.cls");
        assert!(result.nodes.iter().any(|node| {
            matches!(
                node.label.as_str(),
                "insert" | "update" | "delete" | "upsert"
            )
        }));
    }

    #[test]
    fn test_apex_file_node_present() {
        assert!(labels(&extract_fixture("sample.cls")).contains(&"sample.cls"));
    }

    #[test]
    fn test_apex_trigger_extraction() {
        let result = extract_fixture("sample.trigger");
        let actual = labels(&result);
        assert!(actual.contains(&"sample.trigger"));
        assert!(actual.contains(&"AccountTrigger"));
    }

    #[test]
    fn test_apex_trigger_uses_sobject() {
        let result = extract_fixture("sample.trigger");
        assert_relation(&result, "uses");
        assert!(labels(&result).contains(&"Account"));
    }

    #[test]
    fn test_apex_missing_file_returns_empty() {
        assert!(extract(Path::new("nonexistent.cls")).is_err());
    }

    #[test]
    fn test_apex_no_dangling_edges() {
        for fixture in ["sample.cls", "sample.trigger"] {
            assert_no_dangling_edges(&extract_fixture(fixture));
        }
    }

    #[test]
    fn test_systemverilog_no_error() {
        extract_fixture("sample.sv");
    }

    #[test]
    fn test_systemverilog_splits_inherits_and_implements() {
        let result = extract_fixture("sample.sv");
        assert_edge(&result, "inherits", "DataProcessor", "BaseProcessor");
        assert_edge(&result, "implements", "DataProcessor", "Processor");
    }

    #[test]
    fn test_systemverilog_field_parameter_return_and_generic_contexts() {
        let result = extract_fixture("sample.sv");
        assert_context_edge(&result, "references", "field", "DataProcessor", "Result");
        assert_context_edge(
            &result,
            "references",
            "generic_arg",
            "DataProcessor",
            "Payload",
        );
        assert_context_edge(&result, "references", "parameter_type", "build", "Payload");
        assert_context_edge(&result, "references", "return_type", "build", "Result");
        assert_context_edge(&result, "references", "generic_arg", "build", "Payload");
    }

    #[test]
    fn test_systemverilog_qualified_field_references() {
        let result = extract_fixture("sample.sv");
        assert_context_edge(&result, "references", "field", "DataProcessor", "Config");
        assert_context_edge(
            &result,
            "references",
            "field",
            "DataProcessor",
            "BaseProcessor",
        );
    }

    #[test]
    fn test_systemverilog_does_not_emit_type_parameter_refs() {
        assert!(
            !edge_pairs(&extract_fixture("sample.sv"), "references", Some("field"))
                .contains(&("Result".into(), "T".into()))
        );
    }

    #[test]
    fn test_systemverilog_preserves_existing_module_extraction() {
        let result = extract_fixture("sample.sv");
        let actual: HashSet<_> = labels(&result).into_iter().collect();
        for expected in ["top", "leaf", "add()", "tick"] {
            assert!(
                actual.contains(expected),
                "missing SV node {expected}: {actual:?}"
            );
        }
        assert_relation(&result, "imports_from");
        assert_relation(&result, "instantiates");
    }

    #[test]
    fn test_systemverilog_missing_file_returns_empty() {
        assert!(extract(Path::new("nonexistent.sv")).is_err());
    }

    #[test]
    fn test_systemverilog_no_dangling_edges() {
        assert_no_dangling_edges(&extract_fixture("sample.sv"));
    }
}

mod paired_headers_and_implementations {
    use super::*;

    fn corpus(paths: &[&str]) -> Extraction {
        let temp = TempFixture::new("paired-corpus");
        for relative in paths {
            let contents = fs::read_to_string(fixture(relative))
                .unwrap_or_else(|error| panic!("read paired fixture {relative}: {error}"));
            temp.write(relative, &contents);
        }
        let chunks = extract_project_with_options(&temp.root, true).expect("extract paired corpus");
        let graph = graphoxide_graph::build_graph(&chunks).expect("build paired corpus graph");
        Extraction {
            nodes: graph.nodes,
            edges: graph.links,
            hyperedges: graph.hyperedges,
        }
    }

    fn nodes_with_label<'a>(result: &'a Extraction, label: &str) -> Vec<&'a Node> {
        result
            .nodes
            .iter()
            .filter(|node| node.label == label)
            .collect()
    }

    #[test]
    fn test_cpp_header_routes_to_cpp_extractor() {
        let result = extract_fixture("cpp_paired/Foo.h");
        let foo = node_by_label(&result, "Foo");
        assert_eq!(
            foo.extra.get("type").and_then(|value| value.as_str()),
            Some("class")
        );
    }

    #[test]
    fn test_plain_c_header_stays_on_c_extractor() {
        let result = extract_fixture("cpp_samedir/plain.h");
        assert!(!labels(&result).contains(&"Point"));
        assert!(!result.nodes.iter().any(|node| node.label == "class"));
    }

    fn cpp_pair() -> Extraction {
        corpus(&[
            "cpp_paired/Foo.h",
            "cpp_paired/Foo.cpp",
            "cpp_paired/Main.cpp",
        ])
    }

    #[test]
    fn test_cpp_paired_single_class_node() {
        let result = cpp_pair();
        assert_eq!(nodes_with_label(&result, "Foo").len(), 1);
        assert!(nodes_with_label(&result, "class").is_empty());
        assert!(nodes_with_label(&result, "foo_foo").is_empty());
    }

    #[test]
    fn test_cpp_paired_method_decl_and_def_are_one_node() {
        let result = cpp_pair();
        let foo = nodes_with_label(&result, "Foo")[0];
        let bar_nodes: Vec<_> = result
            .nodes
            .iter()
            .filter(|node| matches!(normalize_symbol_label(&node.label), "bar" | "Foo::bar"))
            .collect();
        assert_eq!(
            bar_nodes.len(),
            1,
            "bar declaration/definition split: {bar_nodes:?}"
        );
        assert!(result.edges.iter().any(|edge| {
            edge.true_source() == foo.id
                && edge.true_target() == bar_nodes[0].id
                && matches!(edge.relation.as_str(), "method" | "defines" | "contains")
        }));
    }

    #[test]
    fn test_cpp_paired_includes_resolve_to_real_header() {
        let result = cpp_pair();
        let foo_header = nodes_with_label(&result, "Foo.h")[0];
        let imports: Vec<_> = result
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports")
            .collect();
        assert!(imports.len() >= 2, "missing C++ include edges: {imports:?}");
        assert!(imports
            .iter()
            .any(|edge| edge.true_target() == foo_header.id));
    }

    #[test]
    fn test_cpp_paired_no_dangling_edges() {
        assert_no_dangling_edges(&cpp_pair());
    }

    #[test]
    fn test_objc_header_with_import_routes_to_objc() {
        let result = extract_fixture("objc_mixed/Bridging-Header.h");
        assert_relation(&result, "imports");
    }

    fn objc_pair(include_bridge: bool, include_swift: bool) -> Extraction {
        let mut paths = vec!["objc_mixed/Widget.h", "objc_mixed/Widget.m"];
        if include_bridge {
            paths.push("objc_mixed/Bridging-Header.h");
        }
        if include_swift {
            paths.push("objc_mixed/WidgetExtras.swift");
        }
        corpus(&paths)
    }

    #[test]
    fn test_objc_paired_single_class_methods_not_duplicated() {
        let result = objc_pair(false, false);
        assert_eq!(nodes_with_label(&result, "Widget").len(), 1);
        assert_eq!(nodes_with_label(&result, "-render").len(), 1);
        assert_eq!(nodes_with_label(&result, "-refresh").len(), 1);
    }

    #[test]
    fn test_objc_bridging_header_not_isolated() {
        let result = objc_pair(true, false);
        let bridge = nodes_with_label(&result, "Bridging-Header.h")[0];
        let widget_header = nodes_with_label(&result, "Widget.h")[0];
        assert!(result.edges.iter().any(|edge| {
            edge.true_source() == bridge.id
                && edge.true_target() == widget_header.id
                && edge.relation == "imports"
        }));
    }

    #[test]
    fn test_objc_paired_no_dangling_edges() {
        assert_no_dangling_edges(&objc_pair(true, false));
    }

    #[test]
    fn test_swift_extension_folds_onto_objc_class() {
        let result = objc_pair(false, true);
        let widgets = nodes_with_label(&result, "Widget");
        assert_eq!(widgets.len(), 1);
        let method_targets: HashSet<_> = result
            .edges
            .iter()
            .filter(|edge| edge.relation == "method" && edge.true_source() == widgets[0].id)
            .map(|edge| edge.true_target())
            .collect();
        assert!(result.nodes.iter().any(|node| {
            method_targets.contains(node.id.as_str()) && node.label.contains("describe")
        }));
        assert_no_dangling_edges(&result);
    }

    #[test]
    fn test_decldef_merge_does_not_merge_across_directories() {
        let result = corpus(&[
            "cpp_logger/a/Logger.h",
            "cpp_logger/a/Logger.cpp",
            "cpp_logger/b/Logger.h",
            "cpp_logger/b/Logger.cpp",
        ]);
        let loggers = nodes_with_label(&result, "Logger");
        assert_eq!(loggers.len(), 2);
        assert_eq!(
            loggers
                .iter()
                .map(|node| &node.id)
                .collect::<HashSet<_>>()
                .len(),
            2
        );
    }

    #[test]
    fn test_decldef_merge_does_not_merge_same_name_same_dir_distinct_files() {
        let result = corpus(&["cpp_samedir/Alpha.h", "cpp_samedir/Beta.h"]);
        let duplicates = nodes_with_label(&result, "Dup");
        assert_eq!(duplicates.len(), 2);
        assert_eq!(
            duplicates
                .iter()
                .map(|node| &node.id)
                .collect::<HashSet<_>>()
                .len(),
            2
        );
    }
}
