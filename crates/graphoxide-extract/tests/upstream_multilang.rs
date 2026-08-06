//! One-to-one executable port of pinned upstream `tests/test_multilang.py`.
//!
//! The source-to-test inventory is recorded in
//! `parity/source-maps/test_multilang.mapping.json`.

use graphoxide_core::{make_id, Confidence, Edge, Extraction};
use graphoxide_extract::{extract, extract_files};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/upstream");

fn fixture(name: &str) -> PathBuf {
    Path::new(FIXTURES).join(name)
}

fn extract_fixture(name: &str) -> Extraction {
    let path = fixture(name);
    extract(&path).unwrap_or_else(|error| panic!("extract {name}: {error:#}"))
}

fn combine(extractions: Vec<Extraction>) -> Extraction {
    Extraction {
        nodes: extractions
            .iter()
            .flat_map(|extraction| extraction.nodes.iter().cloned())
            .collect(),
        edges: extractions
            .iter()
            .flat_map(|extraction| extraction.edges.iter().cloned())
            .collect(),
        hyperedges: extractions
            .iter()
            .flat_map(|extraction| extraction.hyperedges.iter().cloned())
            .collect(),
    }
}

fn extract_set(paths: &[PathBuf], cache_root: &Path, force: bool) -> Extraction {
    combine(
        extract_files(paths, Some(cache_root), force)
            .expect("extract explicit fixture set")
            .extractions,
    )
}

fn labels(result: &Extraction) -> Vec<&str> {
    result
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect()
}

fn label_map(result: &Extraction) -> HashMap<&str, &str> {
    result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect()
}

fn normalize_label(label: &str) -> &str {
    label
        .trim_matches(|character| matches!(character, '(' | ')'))
        .trim_start_matches('.')
}

fn edge_pairs(
    result: &Extraction,
    relation: &str,
    context: Option<&str>,
) -> BTreeSet<(String, String)> {
    let node_labels = label_map(result);
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == relation)
        .filter(|edge| context.is_none_or(|expected| edge_context(edge) == Some(expected)))
        .map(|edge| {
            (
                normalize_label(
                    node_labels
                        .get(edge.true_source())
                        .copied()
                        .unwrap_or(edge.true_source()),
                )
                .to_owned(),
                normalize_label(
                    node_labels
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

fn assert_labels(result: &Extraction, expected: &[&str]) {
    let actual = labels(result);
    for expected in expected {
        assert!(
            actual.iter().any(|label| label.contains(expected)),
            "missing {expected:?}; labels={actual:?}"
        );
    }
}

fn assert_edge(result: &Extraction, source: &str, target: &str, relation: &str) {
    let pairs = edge_pairs(result, relation, None);
    assert!(
        pairs.contains(&(source.to_owned(), target.to_owned())),
        "missing {source:?} -[{relation}]-> {target:?}; pairs={pairs:?}"
    );
}

fn assert_context_edge(
    result: &Extraction,
    source: &str,
    target: &str,
    relation: &str,
    context: &str,
) {
    let pairs = edge_pairs(result, relation, Some(context));
    assert!(
        pairs.contains(&(source.to_owned(), target.to_owned())),
        "missing {source:?} -[{relation}; {context}]-> {target:?}; pairs={pairs:?}"
    );
}

fn assert_edges_have_context(result: &Extraction, relations: &[&str], context: &str) {
    let edges: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| relations.contains(&edge.relation.as_str()))
        .collect();
    assert!(!edges.is_empty(), "missing {relations:?} edges");
    assert!(
        edges.iter().all(|edge| edge_context(edge) == Some(context)),
        "not all {relations:?} edges have context={context:?}: {edges:?}"
    );
}

fn assert_no_dangling_sources(result: &Extraction, relations: Option<&[&str]>) {
    let ids: HashSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    for edge in &result.edges {
        if relations.is_none_or(|relations| relations.contains(&edge.relation.as_str())) {
            assert!(
                ids.contains(edge.true_source()),
                "dangling source: {edge:?}"
            );
        }
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

mod typescript {
    use super::*;

    #[test]
    fn test_ts_finds_class() {
        assert_labels(&extract_fixture("sample.ts"), &["HttpClient"]);
    }

    #[test]
    fn test_ts_finds_methods() {
        assert_labels(&extract_fixture("sample.ts"), &["get", "post"]);
    }

    #[test]
    fn test_ts_finds_function() {
        assert_labels(&extract_fixture("sample.ts"), &["buildHeaders"]);
    }

    #[test]
    fn test_ts_emits_calls() {
        let result = extract_fixture("sample.ts");
        assert!(edge_pairs(&result, "calls", None)
            .iter()
            .any(|(source, target)| source.contains("post") && target.contains("get")));
    }

    #[test]
    fn test_ts_calls_are_extracted() {
        let result = extract_fixture("sample.ts");
        assert!(result
            .edges
            .iter()
            .filter(|edge| edge.relation == "calls")
            .all(|edge| edge.confidence == Confidence::Extracted));
    }

    #[test]
    fn test_ts_import_edges_have_import_context() {
        assert_edges_have_context(
            &extract_fixture("sample.ts"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_ts_call_edges_have_call_context() {
        assert_edges_have_context(&extract_fixture("sample.ts"), &["calls"], "call");
    }

    #[test]
    fn test_ts_no_dangling_edges() {
        assert_no_dangling_sources(
            &extract_fixture("sample.ts"),
            Some(&["contains", "method", "calls"]),
        );
    }
}

mod go {
    use super::*;

    #[test]
    fn test_go_finds_struct() {
        assert_labels(&extract_fixture("sample.go"), &["Server"]);
    }

    #[test]
    fn test_go_finds_methods() {
        assert_labels(&extract_fixture("sample.go"), &["Start", "Stop"]);
    }

    #[test]
    fn test_go_finds_constructor() {
        assert_labels(&extract_fixture("sample.go"), &["NewServer"]);
    }

    #[test]
    fn test_go_emits_calls() {
        assert!(!edge_pairs(&extract_fixture("sample.go"), "calls", None).is_empty());
    }

    #[test]
    fn test_go_has_extracted_calls() {
        assert!(extract_fixture("sample.go")
            .edges
            .iter()
            .any(|edge| edge.relation == "calls" && edge.confidence == Confidence::Extracted));
    }

    #[test]
    fn test_go_import_edges_have_import_context() {
        assert_edges_have_context(
            &extract_fixture("sample.go"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_go_call_edges_have_call_context() {
        assert_edges_have_context(&extract_fixture("sample.go"), &["calls"], "call");
    }

    #[test]
    fn test_go_no_dangling_edges() {
        assert_no_dangling_sources(
            &extract_fixture("sample.go"),
            Some(&["contains", "method", "calls"]),
        );
    }

    #[test]
    fn test_go_embeds_struct_field() {
        assert_edge(
            &extract_fixture("sample.go"),
            "DataProcessor",
            "BaseProcessor",
            "embeds",
        );
    }

    #[test]
    fn test_go_interface_embedding_emits_embeds() {
        assert_edge(
            &extract_fixture("sample.go"),
            "ReaderLogger",
            "Logger",
            "embeds",
        );
    }

    #[test]
    fn test_go_struct_named_field_emits_field_context() {
        assert_context_edge(
            &extract_fixture("sample.go"),
            "DataProcessor",
            "Result",
            "references",
            "field",
        );
    }

    #[test]
    fn test_go_method_parameter_return_contexts() {
        let result = extract_fixture("sample.go");
        assert_context_edge(
            &result,
            "Build",
            "DataProcessor",
            "references",
            "parameter_type",
        );
        assert_context_edge(&result, "Build", "Result", "references", "return_type");
    }

    #[test]
    fn test_go_method_declaration_emits_refs_only_when_name_present() {
        // Rust cannot introspect the old Python local-variable guard, so port
        // the invariant at its public boundary: missing declaration names are
        // recoverable syntax errors and must not panic or manufacture refs.
        let fixture = TempDir::new().expect("Go malformed fixture");
        let path = fixture.path().join("malformed.go");
        fs::write(
            &path,
            "package demo\ntype Server struct{}\nfunc (s *Server) (x DataProcessor) Result { return Result{} }\nfunc () {}\n",
        )
        .expect("write malformed Go");
        let result = extract(&path).expect("malformed declaration remains extractable");
        assert_no_dangling_sources(&result, Some(&["references"]));
    }
}

mod rust {
    use super::*;

    #[test]
    fn test_rust_finds_struct() {
        assert_labels(&extract_fixture("sample.rs"), &["Graph"]);
    }

    #[test]
    fn test_rust_finds_impl_methods() {
        assert_labels(&extract_fixture("sample.rs"), &["add_node", "add_edge"]);
    }

    #[test]
    fn test_rust_finds_function() {
        assert_labels(&extract_fixture("sample.rs"), &["build_graph"]);
    }

    #[test]
    fn test_rust_emits_calls() {
        assert!(edge_pairs(&extract_fixture("sample.rs"), "calls", None)
            .iter()
            .any(|(source, _)| source.contains("build_graph")));
    }

    #[test]
    fn test_rust_calls_are_extracted() {
        assert!(extract_fixture("sample.rs")
            .edges
            .iter()
            .filter(|edge| edge.relation == "calls")
            .all(|edge| edge.confidence == Confidence::Extracted));
    }

    #[test]
    fn test_rust_import_edges_have_import_context() {
        assert_edges_have_context(
            &extract_fixture("sample.rs"),
            &["imports", "imports_from"],
            "import",
        );
    }

    #[test]
    fn test_rust_anchored_imports_resolve_without_root_name_collisions() {
        let project = TempDir::new().expect("Rust import fixture");
        for (relative, source) in [
            ("foo.rs", "pub struct Unrelated;\n"),
            ("src/foo.rs", "pub struct Item;\n"),
            ("src/sub/foo.rs", "pub struct Local;\n"),
            (
                "src/sub/mod.rs",
                "use self::foo;\nuse super::foo;\nuse crate::foo;\nuse vendor::foo;\nuse crate::foo::Item;\n",
            ),
        ] {
            let path = project.path().join(relative);
            fs::create_dir_all(path.parent().expect("Rust fixture parent"))
                .expect("create Rust fixture parent");
            fs::write(path, source).expect("write Rust fixture");
        }
        let paths = [
            project.path().join("foo.rs"),
            project.path().join("src/foo.rs"),
            project.path().join("src/sub/foo.rs"),
            project.path().join("src/sub/mod.rs"),
        ];
        let result = extract_set(&paths, project.path(), true);
        let file_id = |source_file: &str| {
            result
                .nodes
                .iter()
                .find(|node| {
                    node.source_file == source_file
                        && node.extra.get("type").and_then(|value| value.as_str()) == Some("file")
                })
                .unwrap_or_else(|| panic!("missing Rust file node {source_file}"))
                .id
                .as_str()
        };
        let import_at = |line: &str| {
            result
                .edges
                .iter()
                .find(|edge| {
                    edge.source_file == "src/sub/mod.rs"
                        && edge.relation == "imports_from"
                        && edge
                            .extra
                            .get("source_location")
                            .and_then(|value| value.as_str())
                            == Some(line)
                })
                .unwrap_or_else(|| panic!("missing Rust import at {line}"))
        };

        assert_eq!(import_at("L1").true_target(), file_id("src/sub/foo.rs"));
        assert_eq!(import_at("L2").true_target(), file_id("src/foo.rs"));
        assert_eq!(import_at("L3").true_target(), file_id("src/foo.rs"));
        assert_ne!(import_at("L4").true_target(), file_id("foo.rs"));
        let item = result
            .nodes
            .iter()
            .find(|node| node.source_file == "src/foo.rs" && node.label == "Item")
            .expect("Rust imported item");
        assert_eq!(import_at("L5").true_target(), item.id);
    }

    #[test]
    fn test_rust_call_edges_have_call_context() {
        assert_edges_have_context(&extract_fixture("sample.rs"), &["calls"], "call");
    }

    #[test]
    fn test_rust_no_dangling_edges() {
        assert_no_dangling_sources(
            &extract_fixture("sample.rs"),
            Some(&["contains", "method", "calls"]),
        );
    }

    #[test]
    fn test_rust_trait_impl_emits_implements() {
        assert_edge(
            &extract_fixture("sample.rs"),
            "DataProcessor",
            "Processor",
            "implements",
        );
    }

    #[test]
    fn test_rust_supertrait_emits_inherits() {
        assert_edge(
            &extract_fixture("sample.rs"),
            "Logger",
            "Processor",
            "inherits",
        );
    }

    #[test]
    fn test_rust_enum_variant_references() {
        let result = extract_fixture("sample.rs");
        assert_edge(&result, "GraphEvent", "Graph", "references");
        assert_edge(&result, "GraphEvent", "DataProcessor", "references");
    }

    #[test]
    fn test_rust_struct_field_emits_field_context() {
        let result = extract_fixture("sample.rs");
        assert_context_edge(&result, "DataProcessor", "Result", "references", "field");
        assert!(!edge_pairs(&result, "references", Some("field"))
            .contains(&("DataProcessor".into(), "DataProcessor".into())));
    }

    #[test]
    fn test_rust_tuple_struct_field_references() {
        let result = extract_fixture("sample.rs");
        assert_context_edge(&result, "GraphPair", "Graph", "references", "field");
        assert_context_edge(&result, "GraphPair", "Result", "references", "field");
        assert_context_edge(
            &result,
            "GraphPair",
            "DataProcessor",
            "references",
            "generic_arg",
        );
    }

    #[test]
    fn test_rust_method_parameter_return_and_generic_contexts() {
        let result = extract_fixture("sample.rs");
        assert_context_edge(
            &result,
            "build",
            "DataProcessor",
            "references",
            "parameter_type",
        );
        assert_context_edge(&result, "build", "Result", "references", "return_type");
        assert_context_edge(
            &result,
            "build",
            "DataProcessor",
            "references",
            "generic_arg",
        );
    }

    #[test]
    fn test_rust_no_cross_crate_spurious_edges() {
        let cache = TempDir::new().expect("Rust cache root");
        let result = extract_set(
            &[fixture("crate_a/src/lib.rs"), fixture("crate_b/src/lib.rs")],
            cache.path(),
            true,
        );
        let from_a: HashSet<_> = result
            .nodes
            .iter()
            .filter(|node| node.source_file.contains("crate_a"))
            .map(|node| node.id.as_str())
            .collect();
        let from_b: HashSet<_> = result
            .nodes
            .iter()
            .filter(|node| node.source_file.contains("crate_b"))
            .map(|node| node.id.as_str())
            .collect();
        let cross: Vec<_> = result
            .edges
            .iter()
            .filter(|edge| {
                edge.relation == "calls"
                    && from_b.contains(edge.true_source())
                    && from_a.contains(edge.true_target())
            })
            .collect();
        assert!(cross.is_empty(), "spurious cross-crate calls: {cross:?}");
    }
}

mod dispatch_cache {
    use super::*;

    #[test]
    fn test_extract_dispatches_all_languages() {
        let cache = TempDir::new().expect("dispatch cache");
        let result = extract_set(
            &[
                fixture("sample.py"),
                fixture("sample.ts"),
                fixture("sample.go"),
                fixture("sample.rs"),
            ],
            cache.path(),
            true,
        );
        let sources: HashSet<_> = result
            .nodes
            .iter()
            .filter(|node| !node.source_file.is_empty())
            .map(|node| node.source_file.as_str())
            .collect();
        for expected in ["sample.py", "sample.ts", "sample.go", "sample.rs"] {
            assert!(sources.iter().any(|source| source.ends_with(expected)));
        }
    }

    #[test]
    fn test_cache_hit_returns_same_result() {
        let fixture = TempDir::new().expect("cache fixture");
        let path = fixture.path().join("sample.py");
        fs::copy(super::fixture("sample.py"), &path).expect("copy Python fixture");
        let first = extract_set(std::slice::from_ref(&path), fixture.path(), false);
        let second = extract_set(std::slice::from_ref(&path), fixture.path(), false);
        assert_eq!(first.nodes.len(), second.nodes.len());
        assert_eq!(first.edges.len(), second.edges.len());
    }

    #[test]
    fn test_cache_miss_after_file_change() {
        let fixture = TempDir::new().expect("cache mutation fixture");
        let path = fixture.path().join("a.py");
        fs::write(&path, "def foo(): pass\n").expect("write first Python source");
        let _ = extract_set(std::slice::from_ref(&path), fixture.path(), false);
        fs::write(&path, "def foo(): pass\ndef bar(): pass\n")
            .expect("write changed Python source");
        let second = extract_set(std::slice::from_ref(&path), fixture.path(), false);
        assert_labels(&second, &["bar"]);
    }
}

mod sql {
    use super::*;

    #[test]
    fn test_sql_finds_tables() {
        assert_labels(&extract_fixture("sample.sql"), &["users", "organizations"]);
    }

    #[test]
    fn test_sql_finds_view() {
        assert_labels(&extract_fixture("sample.sql"), &["active_users"]);
    }

    #[test]
    fn test_sql_finds_function() {
        assert_labels(&extract_fixture("sample.sql"), &["get_user"]);
    }

    #[test]
    fn test_sql_emits_foreign_key_edge() {
        assert!(extract_fixture("sample.sql")
            .edges
            .iter()
            .any(|edge| edge.relation == "references"));
    }

    #[test]
    fn test_sql_emits_reads_from_edge() {
        assert!(extract_fixture("sample.sql")
            .edges
            .iter()
            .any(|edge| edge.relation == "reads_from"));
    }

    #[test]
    fn test_sql_no_dangling_edges() {
        assert_no_dangling_edges(&extract_fixture("sample.sql"));
    }

    #[test]
    fn test_sql_cross_file_fk_resolves_and_never_leaks_scan_path() {
        let fixture = TempDir::new().expect("SQL migration fixture");
        let m1 = fixture.path().join("prisma/migrations/m1/migration.sql");
        let m2 = fixture.path().join("prisma/migrations/m2/migration.sql");
        fs::create_dir_all(m1.parent().expect("m1 parent")).expect("create m1");
        fs::create_dir_all(m2.parent().expect("m2 parent")).expect("create m2");
        fs::write(
            &m1,
            "CREATE TABLE \"Tenant\" (\n  \"id\" TEXT NOT NULL,\n  CONSTRAINT \"Tenant_pkey\" PRIMARY KEY (\"id\")\n);\n",
        )
        .expect("write m1");
        fs::write(
            &m2,
            "CREATE TABLE \"StockGapEvent\" (\n  \"id\" TEXT NOT NULL,\n  \"tenantId\" TEXT NOT NULL,\n  CONSTRAINT \"StockGapEvent_pkey\" PRIMARY KEY (\"id\")\n);\nALTER TABLE \"StockGapEvent\" ADD CONSTRAINT \"StockGapEvent_tenantId_fkey\" FOREIGN KEY (\"tenantId\") REFERENCES \"Tenant\"(\"id\");\n",
        )
        .expect("write m2");
        let result = extract_set(&[m1, m2], fixture.path(), true);
        let ids: HashSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
        let tenants: Vec<_> = ids
            .iter()
            .copied()
            .filter(|id| id.ends_with("m1_migration_tenant"))
            .collect();
        assert_eq!(tenants.len(), 1, "real Tenant ids: {tenants:?}");
        assert!(result
            .edges
            .iter()
            .any(|edge| { edge.relation == "references" && edge.true_target() == tenants[0] }));
        assert_no_dangling_edges(&result);
        let absolute = fixture.path().canonicalize().expect("canonical fixture");
        let absolute_slug = make_id(&[absolute.to_string_lossy().as_ref()]);
        assert!(result
            .nodes
            .iter()
            .all(|node| !node.id.contains(&absolute_slug)));
        assert!(result.edges.iter().all(|edge| {
            !edge.true_source().contains(&absolute_slug)
                && !edge.true_target().contains(&absolute_slug)
        }));
    }

    #[test]
    fn test_sql_alter_table_fk_edge() {
        let result = extract_fixture("sample_alter_fk.sql");
        assert!(result
            .edges
            .iter()
            .any(|edge| edge.relation == "references"));
        assert_no_dangling_edges(&result);
    }

    #[test]
    fn test_sql_schema_qualified_names() {
        assert_labels(
            &extract_fixture("sample_schema_qualified.sql"),
            &["Sales.Customer", "Sales.SalesOrder"],
        );
    }

    #[test]
    fn test_sql_schema_qualified_alter_fk() {
        let result = extract_fixture("sample_schema_qualified.sql");
        assert!(result
            .edges
            .iter()
            .any(|edge| edge.relation == "references"));
        assert_no_dangling_edges(&result);
    }

    #[test]
    fn test_sql_plpgsql_functions_survive_parse_errors() {
        let result = extract_fixture("sample_plpgsql.sql");
        let actual = labels(&result);
        assert!(actual.contains(&"exposed.important_function()"));
        assert!(actual.contains(&"tagged_quote_fn()"));
        assert_labels(&result, &["accounts", "audit_log"]);
        assert!(actual
            .iter()
            .all(|label| !label.is_empty() && *label != "ERROR"));
        let contains_targets: HashSet<_> = result
            .edges
            .iter()
            .filter(|edge| edge.relation == "contains")
            .map(|edge| edge.true_target())
            .collect();
        assert!(result
            .nodes
            .iter()
            .filter(|node| node.label.ends_with("()"))
            .all(|node| contains_targets.contains(node.id.as_str())));
    }

    #[test]
    fn test_sql_plpgsql_clean_function_not_double_emitted() {
        let result = extract_fixture("sample_plpgsql.sql");
        assert_eq!(
            result
                .nodes
                .iter()
                .filter(|node| node.label == "plain_sql_fn()")
                .count(),
            1
        );
        assert_eq!(
            result
                .nodes
                .iter()
                .map(|node| &node.id)
                .collect::<HashSet<_>>()
                .len(),
            result.nodes.len()
        );
    }

    #[test]
    fn test_sql_quoted_plpgsql_routines_are_recovered() {
        let result = extract_fixture("sample_plpgsql_quoted.sql");
        let actual = labels(&result);
        for name in [
            "raise_exception_fn",
            "raise_notice_fn",
            "perform_fn",
            "assign_fn",
            "if_then_fn",
            "null_body_fn",
            "quoted_proc",
        ] {
            let expected = format!(r#""public"."{name}"()"#);
            assert!(actual.contains(&expected.as_str()), "missing {expected:?}");
        }
    }

    #[test]
    fn test_sql_quoted_plpgsql_file_stays_clean() {
        let result = extract_fixture("sample_plpgsql_quoted.sql");
        assert_labels(&result, &["accounts", "audit_log"]);
        let actual = labels(&result);
        assert!(actual
            .iter()
            .all(|label| !label.is_empty() && *label != "ERROR"));
        assert_eq!(
            result
                .nodes
                .iter()
                .map(|node| &node.id)
                .collect::<HashSet<_>>()
                .len(),
            result.nodes.len()
        );
        assert_eq!(
            actual.iter().copied().collect::<HashSet<_>>().len(),
            actual.len()
        );
        let contains_targets: HashSet<_> = result
            .edges
            .iter()
            .filter(|edge| edge.relation == "contains")
            .map(|edge| edge.true_target())
            .collect();
        assert!(result
            .nodes
            .iter()
            .filter(|node| node.label.ends_with("()"))
            .all(|node| contains_targets.contains(node.id.as_str())));
    }
}
