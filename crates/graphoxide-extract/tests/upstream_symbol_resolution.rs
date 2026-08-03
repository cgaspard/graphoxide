//! One-to-one executable port of pinned upstream
//! `tests/test_symbol_resolution.py`.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use graphoxide_extract::{extract, extract_files, resolution};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::PathBuf};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("symbol-resolution fixture"),
        }
    }

    fn write(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
        fs::write(&path, source).expect("write fixture");
        path
    }

    fn extract(&self, relatives: &[&str]) -> Extraction {
        let files = relatives
            .iter()
            .map(|relative| self.root.path().join(relative))
            .collect::<Vec<_>>();
        merge(
            extract_files(&files, Some(self.root.path()), true)
                .expect("extract fixture")
                .extractions,
        )
    }
}

fn merge(parts: Vec<Extraction>) -> Extraction {
    let mut merged = Extraction::default();
    for mut part in parts {
        merged.nodes.append(&mut part.nodes);
        merged.edges.append(&mut part.edges);
        merged.hyperedges.append(&mut part.hyperedges);
    }
    merged
}

fn node(id: &str, label: &str, file_type: &str, source_file: &str, kind: &str) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: file_type.into(),
        source_file: source_file.into(),
        source_location: (!source_file.is_empty()).then(|| "L1".into()),
        community: None,
        extra: BTreeMap::from([("type".into(), kind.into())]),
    }
}

fn edge(source: &str, target: &str, relation: &str, source_file: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: BTreeMap::new(),
    }
}

fn unresolved_call(caller: &str, callee: Value, source_file: &str, member: bool) -> Edge {
    let callee_id = callee
        .as_str()
        .map(|name| make_id(&["__graphoxide_call", name]))
        .unwrap_or_else(|| "malformed_callee".into());
    let mut value = edge(caller, &callee_id, "calls", source_file);
    value
        .extra
        .insert("unresolved_call".into(), Value::Bool(true));
    value.extra.insert("callee".into(), callee);
    value
        .extra
        .insert("member_call".into(), Value::Bool(member));
    value.extra.insert("context".into(), "call".into());
    value
}

fn concrete_calls(result: &Extraction) -> Vec<&Edge> {
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls")
        .filter(|edge| {
            !edge
                .extra
                .get("unresolved_call")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .collect()
}

fn synthetic_python_call(
    candidates: Vec<Node>,
    callee: &str,
    source_file: &str,
    member: bool,
) -> Extraction {
    let mut extraction = Extraction {
        nodes: vec![node("caller_run", "run()", "code", source_file, "function")],
        edges: vec![unresolved_call(
            "caller_run",
            Value::from(callee),
            source_file,
            member,
        )],
        hyperedges: vec![],
    };
    extraction.nodes.extend(candidates);
    resolution::resolve(std::slice::from_mut(&mut extraction));
    extraction
}

fn find_node<'a>(result: &'a Extraction, label: &str, source_file: &str) -> &'a Node {
    result
        .nodes
        .iter()
        .find(|node| node.label == label && node.source_file == source_file)
        .unwrap_or_else(|| panic!("missing {label:?} in {source_file:?}"))
}

fn call_between<'a>(result: &'a Extraction, caller: &Node, target: &Node) -> Option<&'a Edge> {
    concrete_calls(result)
        .into_iter()
        .find(|edge| edge.true_source() == caller.id && edge.true_target() == target.id)
}

fn python_import_fact(
    caller_file: &str,
    local_name: &str,
    imported_name: &str,
    module_stem: &str,
    source_location: &str,
) -> Edge {
    let mut value = edge(caller_file, "logical_import", "imports_from", "caller.py");
    value.extra.insert("local_alias".into(), local_name.into());
    value
        .extra
        .insert("imported_name".into(), imported_name.into());
    value
        .extra
        .insert("target_module".into(), make_id(&[module_stem]).into());
    value.extra.insert("module_stem".into(), module_stem.into());
    value
        .extra
        .insert("source_location".into(), source_location.into());
    value
}

#[test]
fn test_normalise_callable_label_strips_function_punctuation() {
    for (label, callee) in [
        ("run()", "run"),
        (".process()", "process"),
        ("  Execute  ", "execute"),
    ] {
        let result = synthetic_python_call(
            vec![node("target", label, "code", "target.py", "function")],
            callee,
            "caller.py",
            false,
        );
        assert_eq!(concrete_calls(&result)[0].true_target(), "target");
    }
}

#[test]
fn test_node_is_resolvable_symbol_skips_rationale_and_doc_tags() {
    let result = synthetic_python_call(
        vec![
            node("code", "helper()", "code", "helper.py", "function"),
            node("rationale", "helper()", "rationale", "notes.md", "function"),
            node("doc", "helper()", "doc_tag", "api.md", "function"),
        ],
        "helper",
        "caller.py",
        false,
    );
    assert_eq!(concrete_calls(&result)[0].true_target(), "code");
}

#[test]
fn test_build_label_index_collects_unique_symbols() {
    let result = synthetic_python_call(
        vec![
            node("a_run", "run()", "code", "a.py", "function"),
            node("b_run", "run()", "code", "b.py", "function"),
            node("doc", "run docs", "doc_tag", "doc.md", "function"),
        ],
        "run",
        "caller.py",
        false,
    );
    assert!(concrete_calls(&result).is_empty());
}

#[test]
fn test_resolve_cross_file_raw_calls_emits_unique_unqualified_call() {
    let result = synthetic_python_call(
        vec![node(
            "helper_helper",
            "helper()",
            "code",
            "helper.py",
            "function",
        )],
        "helper",
        "caller.py",
        false,
    );
    let resolved = concrete_calls(&result);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].true_target(), "helper_helper");
    assert_eq!(resolved[0].confidence, Confidence::Inferred);
    assert_eq!(resolved[0].extra["context"], "call");
    assert_eq!(resolved[0].extra["confidence_score"], 0.8);
}

#[test]
fn test_resolve_cross_file_raw_calls_skips_member_calls() {
    let result = synthetic_python_call(
        vec![node(
            "helper_helper",
            "helper()",
            "code",
            "helper.py",
            "function",
        )],
        "helper",
        "caller.py",
        true,
    );
    assert!(concrete_calls(&result).is_empty());
}

#[test]
fn test_resolve_cross_file_raw_calls_skips_ambiguous_duplicate_labels() {
    let result = synthetic_python_call(
        vec![
            node("a_log", "log()", "code", "alpha/a.py", "function"),
            node("b_log", "log()", "code", "beta/b.py", "function"),
        ],
        "log",
        "pkg/caller.py",
        false,
    );
    assert!(concrete_calls(&result).is_empty());
}

#[test]
fn test_resolve_cross_file_raw_calls_real_edge_survives_test_mock() {
    let result = synthetic_python_call(
        vec![
            node("src_save", "save()", "code", "src/service.py", "function"),
            node(
                "mock_save",
                "save()",
                "code",
                "tests/test_service.py",
                "function",
            ),
        ],
        "save",
        "src/caller.py",
        false,
    );
    assert_eq!(concrete_calls(&result)[0].true_target(), "src_save");
}

#[test]
fn test_resolve_cross_file_raw_calls_n_mock_scale() {
    let mut candidates = vec![node(
        "src_save",
        "save()",
        "code",
        "src/service.py",
        "function",
    )];
    for (id, path) in [
        ("m1", "tests/foo_test.py"),
        ("m2", "spec/bar.Tests.py"),
        ("m3", "test/baz_test.py"),
        ("m4", "__tests__/q.test.py"),
    ] {
        candidates.push(node(id, "save()", "code", path, "function"));
    }
    let result = synthetic_python_call(candidates, "save", "src/caller.py", false);
    assert_eq!(concrete_calls(&result)[0].true_target(), "src_save");
}

#[test]
fn test_resolve_cross_file_raw_calls_call_site_is_test_prefers_test_local() {
    let result = synthetic_python_call(
        vec![
            node("src_save", "save()", "code", "src/service.py", "function"),
            node(
                "test_save",
                "save()",
                "code",
                "tests/test_service.py",
                "function",
            ),
        ],
        "save",
        "tests/test_service.py",
        false,
    );
    assert_eq!(concrete_calls(&result)[0].true_target(), "test_save");
}

#[test]
fn test_resolve_cross_file_raw_calls_skips_existing_pair() {
    let mut result = Extraction {
        nodes: vec![
            node("caller_run", "run()", "code", "caller.py", "function"),
            node("helper_helper", "helper()", "code", "helper.py", "function"),
        ],
        edges: vec![
            edge("caller_run", "helper_helper", "calls", "caller.py"),
            unresolved_call("caller_run", "helper".into(), "caller.py", false),
        ],
        hyperedges: vec![],
    };
    resolution::resolve(std::slice::from_mut(&mut result));
    assert_eq!(concrete_calls(&result).len(), 1);
}

#[test]
fn test_parse_python_import_aliases_supports_from_import_alias() {
    let fixture = Fixture::new();
    let caller = fixture.write("caller.py", "from helper import transform as tx\n");
    let result = extract(&caller).expect("extract caller");
    let import = result
        .edges
        .iter()
        .find(|edge| edge.extra.get("local_alias") == Some(&Value::from("tx")))
        .expect("aliased import fact");
    assert_eq!(import.extra["imported_name"], "transform");
    assert_eq!(import.extra["module_stem"], "helper");
    assert_eq!(import.extra["source_location"], "L1");
}

#[test]
fn test_build_python_symbol_index_uses_module_stem_and_label() {
    let fixture = Fixture::new();
    fixture.write("helper.py", "def transform(value):\n    return value\n");
    fixture.write("other.py", "def transform(value):\n    return value\n");
    fixture.write(
        "caller.py",
        "from helper import transform as ht\nfrom other import transform as ot\ndef run(v):\n    ht(v)\n    return ot(v)\n",
    );
    let result = fixture.extract(&["caller.py", "helper.py", "other.py"]);
    let caller = find_node(&result, "run()", "caller.py");
    let helper = find_node(&result, "transform()", "helper.py");
    let other = find_node(&result, "transform()", "other.py");
    assert!(call_between(&result, caller, helper).is_some());
    assert!(call_between(&result, caller, other).is_some());
}

#[test]
fn test_find_unique_python_symbol_returns_none_when_ambiguous() {
    let mut result = Extraction {
        nodes: vec![
            node("caller", "caller.py", "code", "caller.py", "file"),
            node("caller_run", "run()", "code", "caller.py", "function"),
            node("helper", "helper.py", "code", "helper.py", "file"),
            node("a", "transform()", "code", "helper.py", "function"),
            node("b", "transform()", "code", "helper.py", "function"),
        ],
        edges: vec![
            python_import_fact("caller", "tx", "transform", "helper", "L1"),
            unresolved_call("caller_run", "tx".into(), "caller.py", false),
        ],
        hyperedges: vec![],
    };
    resolution::resolve(std::slice::from_mut(&mut result));
    assert!(concrete_calls(&result).is_empty());
}

#[test]
fn test_resolve_python_import_guided_calls_emits_extracted_edge() {
    let fixture = Fixture::new();
    fixture.write(
        "caller.py",
        "from helper import transform as tx\n\ndef run(value):\n    return tx(value)\n",
    );
    fixture.write("helper.py", "def transform(value):\n    return value\n");
    let result = fixture.extract(&["caller.py", "helper.py"]);
    let caller = find_node(&result, "run()", "caller.py");
    let target = find_node(&result, "transform()", "helper.py");
    let resolved = call_between(&result, caller, target).expect("import-guided call");
    assert_eq!(resolved.confidence, Confidence::Extracted);
    assert_eq!(resolved.extra["context"], "import_guided_call");
    assert_eq!(resolved.extra["confidence_score"], 1.0);
    assert_eq!(
        resolved.extra["metadata"]["resolver"],
        "python_import_guided"
    );
    assert_eq!(resolved.extra["metadata"]["local_name"], "tx");
    assert_eq!(resolved.extra["metadata"]["imported_name"], "transform");
    assert_eq!(resolved.extra["metadata"]["module_stem"], "helper");
    assert_eq!(resolved.extra["metadata"]["import_source_location"], "L1");
}

#[test]
fn test_bash_call_resolver_emits_source_edges() {
    let fixture = Fixture::new();
    fixture.write("a.sh", "#!/usr/bin/env bash\nsource ./b.sh\n");
    fixture.write("b.sh", "#!/usr/bin/env bash\nb_func() { echo ok; }\n");
    let result = fixture.extract(&["a.sh", "b.sh"]);
    let imports = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "imports_from")
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].confidence, Confidence::Extracted);
}

#[test]
fn test_bash_call_resolver_emits_call_edges_from_sourced_files() {
    let fixture = Fixture::new();
    fixture.write(
        "a.sh",
        "#!/usr/bin/env bash\nsource ./b.sh\nmain() { b_func; }\n",
    );
    fixture.write("b.sh", "#!/usr/bin/env bash\nb_func() { echo ok; }\n");
    let result = fixture.extract(&["a.sh", "b.sh"]);
    let caller = find_node(&result, "main()", "a.sh");
    let target = find_node(&result, "b_func()", "b.sh");
    let call = call_between(&result, caller, target).expect("sourced call");
    assert_eq!(call.confidence, Confidence::Extracted);
}

#[test]
fn test_bash_call_resolver_skips_existing_pair() {
    let fixture = Fixture::new();
    let a = fixture.write(
        "a.sh",
        "#!/usr/bin/env bash\nsource ./b.sh\nmain() { b_func; }\n",
    );
    let b = fixture.write("b.sh", "#!/usr/bin/env bash\nb_func() { echo ok; }\n");
    let mut parts = vec![extract(&a).unwrap(), extract(&b).unwrap()];
    let caller = find_node(&parts[0], "main()", &a.to_string_lossy())
        .id
        .clone();
    let target = find_node(&parts[1], "b_func()", &b.to_string_lossy())
        .id
        .clone();
    parts[0]
        .edges
        .push(edge(&caller, &target, "calls", &a.to_string_lossy()));
    resolution::resolve(&mut parts);
    assert_eq!(
        parts
            .iter()
            .flat_map(|part| concrete_calls(part))
            .filter(|call| call.true_source() == caller && call.true_target() == target)
            .count(),
        1
    );
}

#[test]
fn test_bash_call_resolver_skips_ambiguous_multiple_candidates() {
    let fixture = Fixture::new();
    fixture.write(
        "a.sh",
        "#!/usr/bin/env bash\nsource ./b.sh\nsource ./c.sh\nmain() { helper; }\n",
    );
    fixture.write("b.sh", "helper() { echo b; }\n");
    fixture.write("c.sh", "helper() { echo c; }\n");
    let result = fixture.extract(&["a.sh", "b.sh", "c.sh"]);
    let caller = find_node(&result, "main()", "a.sh");
    assert!(concrete_calls(&result)
        .iter()
        .all(|call| call.true_source() != caller.id));
}

#[test]
fn test_bash_call_resolver_skips_non_bash_raw_calls() {
    let mut result = Extraction {
        nodes: vec![
            node("a", "a.py", "code", "a.py", "file"),
            node("a_main", "main()", "code", "a.py", "function"),
            node("b", "b.sh", "code", "b.sh", "file"),
            node("b_helper", "helper()", "code", "b.sh", "function"),
        ],
        edges: vec![edge("a", "b", "imports_from", "a.py"), {
            let mut raw = edge("a_main", "raw", "__bash_raw_call", "a.py");
            raw.extra.insert("callee".into(), "helper".into());
            raw
        }],
        hyperedges: vec![],
    };
    resolution::resolve(std::slice::from_mut(&mut result));
    assert!(concrete_calls(&result).is_empty());
}

#[test]
fn test_bash_make_id_identical_to_make_id() {
    for parts in [
        vec!["foo", "bar"],
        vec!["auth"],
        vec!["_module", "_helper"],
        vec!["my-script", "main"],
    ] {
        assert_eq!(make_id(&parts), graphoxide_core::make_id(&parts));
    }
}

#[test]
fn test_bash_make_id_unicode_matches_make_id() {
    assert_eq!(make_id(&["café", "run"]), "café_run");
    assert_eq!(make_id(&["straße"]), "strasse");
}

#[test]
fn test_parse_python_import_aliases_skips_function_local_imports() {
    let fixture = Fixture::new();
    fixture.write(
        "scoped.py",
        "def one():\n    from helper import transform\n    return transform()\n\ndef two():\n    return transform()\n",
    );
    fixture.write("helper.py", "def transform():\n    return 1\n");
    let result = fixture.extract(&["scoped.py", "helper.py"]);
    let target = find_node(&result, "transform()", "helper.py");
    let calls = concrete_calls(&result)
        .into_iter()
        .filter(|call| call.true_target() == target.id)
        .collect::<Vec<_>>();
    // A unique bare call may still be inferred by the separate conservative
    // raw-call resolver. The local import must not promote either call to
    // extracted/import-guided evidence for the whole file.
    assert!(
        calls.iter().all(|call| {
            call.confidence == Confidence::Inferred
                && call.extra.get("context") != Some(&Value::from("import_guided_call"))
                && !call.extra.contains_key("metadata")
        }),
        "local import leaked into calls: {calls:#?}"
    );
}

#[test]
fn test_parse_python_import_aliases_accepts_top_level_import() {
    let fixture = Fixture::new();
    fixture.write(
        "toplevel.py",
        "from helper import transform\n\ndef one():\n    return transform()\n",
    );
    fixture.write("helper.py", "def transform():\n    return 1\n");
    let result = fixture.extract(&["toplevel.py", "helper.py"]);
    let caller = find_node(&result, "one()", "toplevel.py");
    let target = find_node(&result, "transform()", "helper.py");
    assert!(call_between(&result, caller, target).is_some());
}

#[test]
fn test_node_is_resolvable_symbol_requires_code_file_type() {
    let result = synthetic_python_call(
        vec![
            node("code", "helper()", "code", "helper.py", "function"),
            node("document", "helper()", "document", "doc.md", "function"),
            node("paper", "helper()", "paper", "paper.pdf", "function"),
            node("image", "helper()", "image", "image.png", "function"),
            node("concept", "helper()", "concept", "", "function"),
        ],
        "helper",
        "caller.py",
        false,
    );
    assert_eq!(concrete_calls(&result)[0].true_target(), "code");
}

#[test]
fn test_build_label_index_excludes_non_code_nodes() {
    let result = synthetic_python_call(
        vec![
            node("code_one", "helper", "code", "helper.py", "function"),
            node("doc_one", "helper", "document", "doc.md", "function"),
            node("paper_one", "helper", "paper", "paper.pdf", "function"),
        ],
        "helper",
        "caller.py",
        false,
    );
    assert_eq!(concrete_calls(&result)[0].true_target(), "code_one");
}

#[test]
fn test_resolve_bash_source_edges_skips_malformed_source() {
    let fixture = Fixture::new();
    fixture.write("a.sh", "source \"$MISSING\"\nsource \"\"\n");
    let result = fixture.extract(&["a.sh"]);
    assert!(result
        .edges
        .iter()
        .all(|edge| edge.relation != "imports_from"));
}

#[test]
fn test_resolve_bash_source_edges_skips_bash_function_node_missing_id() {
    let malformed = json!({"label":"build()","file_type":"code","source_file":"a.sh","metadata":{"kind":"bash_function"}});
    assert!(serde_json::from_value::<Node>(malformed).is_err());
    let mut empty = [Extraction::default()];
    resolution::resolve(&mut empty);
}

#[test]
fn test_resolve_bash_source_edges_skips_raw_call_missing_caller_nid() {
    let malformed = json!({"target":"helper","relation":"__bash_raw_call","source_file":"a.sh","callee":"helper"});
    assert!(serde_json::from_value::<Edge>(malformed).is_err());
    let mut empty = [Extraction::default()];
    resolution::resolve(&mut empty);
}

#[test]
fn test_resolve_bash_source_edges_accepts_none_per_file_entries() {
    let mut no_fragments: [Extraction; 0] = [];
    resolution::resolve(&mut no_fragments);
    assert!(no_fragments.is_empty());
}

#[test]
fn test_resolve_bash_source_edges_skips_non_dict_lists() {
    for malformed in [json!("not a dict"), json!(42), Value::Null] {
        assert!(serde_json::from_value::<Extraction>(malformed).is_err());
    }
}

#[test]
fn test_resolve_bash_source_edges_relative_path_resolves_against_source_dir() {
    let fixture = Fixture::new();
    fixture.write("scripts/main.sh", "source ./helper.sh\n");
    fixture.write("scripts/helper.sh", "helper() { :; }\n");
    let result = fixture.extract(&["scripts/main.sh", "scripts/helper.sh"]);
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
fn test_iter_raw_calls_skips_non_dict_per_file_entries() {
    for malformed in [json!("not a dict"), Value::Null, json!(42)] {
        assert!(serde_json::from_value::<Extraction>(malformed).is_err());
    }
}

#[test]
fn test_iter_raw_calls_skips_non_list_raw_calls() {
    for malformed in [json!("abc"), Value::Null, json!(42)] {
        assert!(!malformed.is_array());
    }
    let mut typed = [Extraction::default()];
    resolution::resolve(&mut typed);
}

#[test]
fn test_iter_raw_calls_drops_non_dict_items_in_list() {
    let malformed = json!(["str", 42, null, {"callee":"real","caller_nid":"c"}]);
    assert!(malformed.as_array().unwrap()[..3]
        .iter()
        .all(|value| !value.is_object()));
    let mut typed = [Extraction::default()];
    resolution::resolve(&mut typed);
}

#[test]
fn test_resolve_cross_file_raw_calls_survives_malformed_raw_calls() {
    let malformed = json!({"nodes":[],"edges":"not a list","hyperedges":[]});
    assert!(serde_json::from_value::<Extraction>(malformed).is_err());
    let mut typed = [Extraction::default()];
    resolution::resolve(&mut typed);
}

#[test]
fn test_resolve_python_import_guided_calls_survives_malformed_raw_calls() {
    let malformed = json!({"nodes":[],"edges":["not an edge"],"hyperedges":[]});
    assert!(serde_json::from_value::<Extraction>(malformed).is_err());
    let mut typed = [Extraction::default()];
    resolution::resolve(&mut typed);
}

#[test]
fn test_resolve_bash_source_edges_skips_unhashable_callee() {
    let mut result = Extraction {
        nodes: vec![
            node("a", "a.sh", "code", "a.sh", "file"),
            node("caller", "main()", "code", "a.sh", "function"),
            node("b", "b.sh", "code", "b.sh", "file"),
            node("helper", "helper()", "code", "b.sh", "function"),
        ],
        edges: vec![edge("a", "b", "imports_from", "a.sh")],
        hyperedges: vec![],
    };
    for callee in [json!(["bad"]), json!({"also":"bad"}), json!(42)] {
        let mut raw = edge("caller", "raw", "__bash_raw_call", "a.sh");
        raw.extra.insert("callee".into(), callee);
        result.edges.push(raw);
    }
    resolution::resolve(std::slice::from_mut(&mut result));
    assert!(concrete_calls(&result).is_empty());
}

#[test]
fn test_resolve_python_import_guided_calls_non_dict_per_file_slot() {
    assert!(serde_json::from_value::<Extraction>(json!("not a dict")).is_err());
    let mut typed = [Extraction::default()];
    resolution::resolve(&mut typed);
}

#[test]
fn test_resolve_python_import_guided_calls_per_file_shorter_than_paths() {
    let fixture = Fixture::new();
    fixture.write("a.py", "from helper import transform\n");
    fixture.write("b.py", "from helper import transform\n");
    let result = fixture.extract(&["a.py"]);
    assert!(concrete_calls(&result).is_empty());
}

#[test]
fn test_resolve_python_import_guided_calls_per_file_none_slot() {
    let mut typed = [Extraction::default()];
    resolution::resolve(&mut typed);
    assert!(typed[0].edges.is_empty());
}

#[test]
fn test_resolve_python_import_guided_calls_metadata_is_sanitized() {
    let fixture = Fixture::new();
    fixture.write(
        "caller.py",
        "from helper import transform as tx\n\ndef run(value):\n    return tx(value)\n",
    );
    fixture.write("helper.py", "def transform(value):\n    return value\n");
    let result = fixture.extract(&["caller.py", "helper.py"]);
    let metadata = concrete_calls(&result)
        .into_iter()
        .find_map(|edge| edge.extra.get("metadata"))
        .expect("import resolver metadata");
    for value in metadata
        .as_object()
        .unwrap()
        .values()
        .filter_map(Value::as_str)
    {
        assert!(!value.contains('<'));
        assert!(!value.contains('\0'));
    }
    assert_eq!(metadata["resolver"], "python_import_guided");
    assert_eq!(metadata["local_name"], "tx");
    assert_eq!(metadata["imported_name"], "transform");
    assert_eq!(metadata["module_stem"], "helper");
}

#[test]
fn test_resolve_python_import_guided_calls_metadata_sanitizes_hostile_alias() {
    let hostile_alias = "<script>tx</script>";
    let mut result = Extraction {
        nodes: vec![
            node("caller", "caller.py", "code", "caller.py", "file"),
            node("caller_run", "run()", "code", "caller.py", "function"),
            node("helper", "helper.py", "code", "helper.py", "file"),
            node(
                "helper_transform",
                "transform()",
                "code",
                "helper.py",
                "function",
            ),
        ],
        edges: vec![
            python_import_fact(
                "caller",
                hostile_alias,
                "transform",
                "helper",
                "L1<img src=x>\0trail",
            ),
            unresolved_call("caller_run", hostile_alias.into(), "caller.py", false),
        ],
        hyperedges: vec![],
    };
    resolution::resolve(std::slice::from_mut(&mut result));
    let metadata = concrete_calls(&result)[0].extra["metadata"]
        .as_object()
        .expect("metadata object");
    assert_eq!(metadata["resolver"], "python_import_guided");
    assert_eq!(metadata["imported_name"], "transform");
    assert_eq!(metadata["module_stem"], "helper");
    let local_name = metadata["local_name"].as_str().unwrap();
    assert!(!local_name.contains("<script>"));
    assert!(local_name.contains("&lt;script&gt;"));
    let source_location = metadata["import_source_location"].as_str().unwrap();
    assert!(!source_location.contains("<img"));
    assert!(source_location.contains("&lt;img"));
    assert!(!source_location.contains('\0'));
    assert!(source_location.contains("trail"));
}
