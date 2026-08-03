//! Executable ports of Graphify's indirect-dispatch regression suites.

use graphoxide_core::{Confidence, Edge, Extraction, Node};
use graphoxide_extract::extract_project_with_options;
use graphoxide_graph::build_graph;
use graphoxide_query::affected;
use std::{collections::BTreeSet, fs};
use tempfile::TempDir;

struct Project {
    root: TempDir,
}

impl Project {
    fn new() -> Self {
        Self {
            root: TempDir::new().unwrap(),
        }
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    fn extract(&self, force: bool) -> Vec<Extraction> {
        extract_project_with_options(self.root.path(), force).unwrap()
    }
}

fn one(path: &str, source: &str) -> Vec<Extraction> {
    let project = Project::new();
    project.write(path, source);
    project.extract(true)
}

fn many(files: &[(&str, &str)]) -> Vec<Extraction> {
    let project = Project::new();
    for (path, source) in files {
        project.write(path, source);
    }
    project.extract(true)
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|item| &item.nodes)
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|item| &item.edges)
}

fn bare(label: &str) -> &str {
    label.trim_start_matches('.').trim_end_matches("()")
}

fn symbol_id(extractions: &[Extraction], label: &str) -> String {
    let matches = nodes(extractions)
        .filter(|node| bare(&node.label) == label)
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected one symbol {label:?}, got {:?}",
        matches
            .iter()
            .map(|node| (&node.id, &node.label, &node.source_file))
            .collect::<Vec<_>>()
    );
    matches[0].id.clone()
}

fn file_id(extractions: &[Extraction], path: &str) -> String {
    nodes(extractions)
        .find(|node| {
            node.source_file == path
                && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("file")
        })
        .unwrap_or_else(|| panic!("missing file node for {path}"))
        .id
        .clone()
}

fn relation_pairs(extractions: &[Extraction], relation: &str) -> BTreeSet<(String, String)> {
    edges(extractions)
        .filter(|edge| edge.relation == relation)
        .map(|edge| (edge.true_source().to_owned(), edge.true_target().to_owned()))
        .collect()
}

fn indirect(extractions: &[Extraction]) -> BTreeSet<(String, String)> {
    relation_pairs(extractions, "indirect_call")
}

fn assert_indirect(extractions: &[Extraction], source: &str, target: &str) {
    let pair = (
        symbol_id(extractions, source),
        symbol_id(extractions, target),
    );
    assert!(
        indirect(extractions).contains(&pair),
        "missing {source} -[indirect_call]-> {target}; indirect={:?}; all={:?}",
        indirect(extractions),
        edges(extractions)
            .map(|edge| (
                edge.true_source(),
                edge.true_target(),
                edge.relation.as_str(),
                &edge.extra
            ))
            .collect::<Vec<_>>()
    );
}

fn assert_file_indirect(extractions: &[Extraction], path: &str, target: &str) {
    let pair = (file_id(extractions, path), symbol_id(extractions, target));
    assert!(
        indirect(extractions).contains(&pair),
        "missing {path} -[indirect_call]-> {target}; indirect={:?}; all={:?}",
        indirect(extractions),
        edges(extractions)
            .map(|edge| (
                edge.true_source(),
                edge.true_target(),
                edge.relation.as_str(),
                &edge.extra
            ))
            .collect::<Vec<_>>()
    );
}

fn assert_no_indirect_to(extractions: &[Extraction], target: &str) {
    let target = symbol_id(extractions, target);
    assert!(
        indirect(extractions)
            .iter()
            .all(|(_, candidate)| candidate != &target),
        "unexpected indirect_call to {target}; edges={:?}; nodes={:?}",
        indirect(extractions),
        nodes(extractions)
            .map(|node| (&node.id, &node.label, &node.extra))
            .collect::<Vec<_>>()
    );
}

fn affected_text(extractions: &[Extraction], target: &str) -> String {
    let graph = build_graph(extractions).unwrap();
    affected(&graph, &symbol_id(extractions, target), 3, &[])
}

const BASIC: &str = r#"import threading

def handler(x):
    return x * 2

def direct():
    return handler(1)

def via_submit(pool):
    pool.submit(handler, 1)

def via_thread():
    threading.Thread(target=handler).start()

def via_map(xs):
    return map(handler, xs)
"#;

#[test]
fn test_emits_indirect_call_edges_and_keeps_calls_precise() {
    let result = one("dispatch.py", BASIC);
    let handler = symbol_id(&result, "handler");
    let calls = relation_pairs(&result, "calls");
    assert!(calls.contains(&(symbol_id(&result, "direct"), handler.clone())));
    for caller in ["via_submit", "via_thread", "via_map"] {
        let pair = (symbol_id(&result, caller), handler.clone());
        assert!(indirect(&result).contains(&pair));
        assert!(!calls.contains(&pair));
    }
    for edge in edges(&result).filter(|edge| edge.relation == "indirect_call") {
        assert_eq!(
            edge.extra
                .get("context")
                .and_then(serde_json::Value::as_str),
            Some("argument")
        );
        assert_eq!(edge.confidence, Confidence::Inferred);
    }
}

#[test]
fn test_affected_includes_indirect_callers() {
    let result = one("dispatch.py", BASIC);
    let text = affected_text(&result, "handler");
    for caller in ["via_submit", "via_thread", "via_map"] {
        assert!(text.contains(caller), "affected output: {text}");
    }
}

#[test]
fn test_param_shadow_emits_no_indirect_call() {
    let result = one(
        "m.py",
        "def handler(): return 1\n\ndef via(pool, handler):\n    pool.submit(handler)\n",
    );
    assert_no_indirect_to(&result, "handler");
}

#[test]
fn test_local_assignment_shadow_emits_no_indirect_call() {
    let result = one(
        "m.py",
        "def handler(): return 1\ndef make(): return lambda: None\ndef via(pool):\n    handler = make()\n    pool.submit(handler)\n",
    );
    assert_no_indirect_to(&result, "handler");
}

#[test]
fn test_data_var_matching_function_name_emits_no_indirect_call() {
    let result = one(
        "m.py",
        "def config(): return {'k': 'v'}\ndef process(x): return x\ndef use():\n    config = {'k': 'v'}\n    process(config)\n",
    );
    assert_no_indirect_to(&result, "config");
}

#[test]
fn test_genuine_module_function_still_emits_indirect_call() {
    let result = one(
        "m.py",
        "def handler(): return 1\ndef via(pool):\n    pool.submit(handler)\n",
    );
    assert_indirect(&result, "via", "handler");
}

fn cross_file_callback() -> Vec<Extraction> {
    many(&[
        ("pkg/handlers.py", "def on_event(x):\n    return x\n"),
        (
            "pkg/scheduler.py",
            "from handlers import on_event\n\ndef schedule(pool):\n    pool.submit(on_event)\n",
        ),
    ])
}

#[test]
fn test_cross_file_indirect_survives_id_relativization() {
    let result = many(&[
        ("handlers/__init__.py", "def on_event(x):\n    return x\n"),
        (
            "scheduler.py",
            "from handlers import on_event\n\ndef schedule(pool):\n    pool.submit(on_event)\n",
        ),
    ]);
    assert_indirect(&result, "schedule", "on_event");
    assert!(nodes(&result).all(|node| !node.extra.contains_key("_callable")));
}

#[test]
fn test_cross_file_imported_callback_emits_indirect_call() {
    let result = cross_file_callback();
    assert_indirect(&result, "schedule", "on_event");
    let pair = (
        symbol_id(&result, "schedule"),
        symbol_id(&result, "on_event"),
    );
    assert!(!relation_pairs(&result, "calls").contains(&pair));
    assert!(edges(&result).any(|edge| {
        edge.relation == "indirect_call"
            && edge.true_target() == pair.1
            && edge.confidence == Confidence::Inferred
    }));
}

#[test]
fn test_cross_file_class_ref_is_not_indirect_call() {
    let result = many(&[
        (
            "pkg/models.py",
            "class Widget:\n    pass\n\ndef on_event(x):\n    return x\n",
        ),
        (
            "pkg/scheduler.py",
            "from models import Widget, on_event\n\ndef schedule(pool, db, i):\n    db.get(Widget, i)\n    pool.submit(on_event)\n",
        ),
    ]);
    assert_no_indirect_to(&result, "Widget");
    assert_indirect(&result, "schedule", "on_event");
}

#[test]
fn test_cross_file_affected_includes_importing_dispatcher() {
    let result = cross_file_callback();
    assert!(affected_text(&result, "on_event").contains("schedule"));
}

#[test]
fn test_cross_file_param_shadow_emits_no_indirect_call() {
    let result = many(&[
        ("pkg/handlers.py", "def on_event(x):\n    return x\n"),
        (
            "pkg/scheduler.py",
            "from handlers import on_event\n\ndef schedule(pool, on_event):\n    pool.submit(on_event)\n",
        ),
    ]);
    assert_no_indirect_to(&result, "on_event");
}

#[test]
fn test_module_level_dict_registry_emits_indirect_call() {
    let result = one(
        "m.py",
        "def create(x): return x\ndef delete(x): return x\nROUTES = {'create': create, 'delete': delete}\n",
    );
    assert_file_indirect(&result, "m.py", "create");
    assert_file_indirect(&result, "m.py", "delete");
}

#[test]
fn test_module_level_list_registry_emits_indirect_call() {
    let result = one(
        "m.py",
        "def on_start(): pass\ndef on_stop(): pass\nHOOKS = [on_start, on_stop]\n",
    );
    assert_file_indirect(&result, "m.py", "on_start");
    assert_file_indirect(&result, "m.py", "on_stop");
}

#[test]
fn test_function_scoped_dispatch_table_attributes_to_function() {
    let result = one(
        "m.py",
        "def cb(x): return x\ndef build():\n    return {'k': cb}\n",
    );
    assert_indirect(&result, "build", "cb");
}

#[test]
fn test_dict_keys_are_not_dispatch_targets() {
    let result = one(
        "m.py",
        "def keyfn(): pass\ndef valfn(): pass\nT = {keyfn: valfn}\n",
    );
    assert_no_indirect_to(&result, "keyfn");
    assert_file_indirect(&result, "m.py", "valfn");
}

#[test]
fn test_non_callable_collection_value_emits_no_indirect_call() {
    let result = one(
        "m.py",
        "def use(): pass\nCONF = {'timeout': 30, 'name': use}\n",
    );
    assert_file_indirect(&result, "m.py", "use");
    assert_eq!(indirect(&result).len(), 1);
}

#[test]
fn test_module_level_reassigned_name_shadows_dispatch_value() {
    let result = one(
        "m.py",
        "def handler(): pass\nhandler = object()\nT = {'h': handler}\n",
    );
    assert_no_indirect_to(&result, "handler");
}

#[test]
fn test_cross_file_dict_registry_emits_indirect_call() {
    let result = many(&[
        ("pkg/handlers.py", "def on_event(x):\n    return x\n"),
        (
            "pkg/registry.py",
            "from handlers import on_event\n\nROUTES = {'event': on_event}\n",
        ),
    ]);
    assert_file_indirect(&result, "pkg/registry.py", "on_event");
}

#[test]
fn test_js_function_scoped_call_argument() {
    let result = one(
        "a.js",
        "function handler(x){ return x; }\nfunction via(pool){ pool.submit(handler); }\n",
    );
    assert_indirect(&result, "via", "handler");
}

#[test]
fn test_js_module_object_and_array_registry() {
    let result = one(
        "a.js",
        "function handler(x){ return x; }\nconst cb = () => {};\nconst ROUTES = { create: handler, run: cb };\nconst HOOKS = [handler, cb];\n",
    );
    assert_file_indirect(&result, "a.js", "handler");
    assert_file_indirect(&result, "a.js", "cb");
}

#[test]
fn test_js_module_level_callback_registration() {
    let result = one(
        "r.js",
        "function home(req, res){}\nconst list = () => {};\napp.get('/', home);\nemitter.on('evt', list);\nsetTimeout(home, 100);\n",
    );
    assert_file_indirect(&result, "r.js", "home");
    assert_file_indirect(&result, "r.js", "list");
}

#[test]
fn test_js_inline_arrow_argument_is_not_a_reference() {
    let result = one(
        "i.js",
        "function via(arr){ arr.map(x => x * 2); arr.forEach(function(y){}); }\n",
    );
    assert!(indirect(&result).is_empty());
}

#[test]
fn test_js_parameter_shadow_emits_no_indirect_call() {
    let result = one(
        "s.js",
        "function handler(){}\nfunction via(pool, handler){ pool.submit(handler); }\n",
    );
    assert_no_indirect_to(&result, "handler");
}

#[test]
fn test_js_object_keys_and_data_values_excluded() {
    let result = one(
        "k.js",
        "function keyfn(){}\nfunction valfn(){}\nconst T = { [keyfn]: valfn, timeout: 30 };\n",
    );
    assert_no_indirect_to(&result, "keyfn");
    assert_file_indirect(&result, "k.js", "valfn");
}

#[test]
fn test_js_shorthand_property_reference() {
    let result = one("sh.js", "function handler(){}\nconst obj = { handler };\n");
    assert_file_indirect(&result, "sh.js", "handler");
}

#[test]
fn test_js_cross_file_imported_callback_in_object() {
    let result = many(&[
        ("src/h.js", "export function onEvent(x){ return x; }\n"),
        (
            "src/reg.js",
            "import { onEvent } from './h.js';\nconst ROUTES = { e: onEvent };\n",
        ),
    ]);
    assert_file_indirect(&result, "src/reg.js", "onEvent");
}

#[test]
fn test_typescript_typed_params_and_arrow_consts() {
    let result = one(
        "t.ts",
        "function handler(x: number): number { return x; }\nconst cb = (): void => {};\nfunction via(pool: Pool): void { pool.submit(handler); }\nconst ROUTES: Record<string, unknown> = { create: handler, run: cb };\n",
    );
    assert_indirect(&result, "via", "handler");
    assert_file_indirect(&result, "t.ts", "handler");
    assert_file_indirect(&result, "t.ts", "cb");
}

#[test]
fn test_class_ref_is_not_indirect_call() {
    let result = one(
        "orm.py",
        "class ErrorA(Exception): pass\nclass ErrorB(Exception): pass\nclass KbArticle: pass\ndef handler(x): return x\ndef use_except():\n    try: pass\n    except (ErrorA, ErrorB) as e: return e\ndef use_getattr(run): return getattr(run, 'KbArticle', 0)\ndef use_orm(db, i):\n    db.get(KbArticle, i)\n    return select(KbArticle)\ndef register(pool): pool.submit(handler)\n",
    );
    for class in ["ErrorA", "ErrorB", "KbArticle"] {
        assert_no_indirect_to(&result, class);
    }
    assert_indirect(&result, "register", "handler");
}

const ASSIGN_RETURN: &str = "def handler(): ...\ndef other(): ...\ndef bind():\n    cb = handler\n    return cb\ndef make():\n    return other\n";

#[test]
fn test_assignment_and_return_emit_indirect_call() {
    let result = one("m.py", ASSIGN_RETURN);
    assert_indirect(&result, "bind", "handler");
    assert_indirect(&result, "make", "other");
    let calls = relation_pairs(&result, "calls");
    assert!(!calls.contains(&(symbol_id(&result, "bind"), symbol_id(&result, "handler"))));
    for edge in edges(&result).filter(|edge| edge.relation == "indirect_call") {
        assert!(matches!(
            edge.extra
                .get("context")
                .and_then(serde_json::Value::as_str),
            Some("assignment" | "return")
        ));
        assert_eq!(edge.confidence, Confidence::Inferred);
    }
}

#[test]
fn test_multiple_assignment_emits_for_each() {
    let result = one(
        "m.py",
        "def f(): ...\ndef g(): ...\ndef via():\n    a, b = f, g\n    return a\n",
    );
    assert_indirect(&result, "via", "f");
    assert_indirect(&result, "via", "g");
}

#[test]
fn test_module_level_assignment_emits_indirect_call() {
    let result = one("m.py", "def handler(): ...\nCALLBACK = handler\n");
    assert_file_indirect(&result, "m.py", "handler");
}

#[test]
fn test_assignment_feeds_affected() {
    let result = one("m.py", ASSIGN_RETURN);
    assert!(affected_text(&result, "handler").contains("bind"));
}

#[test]
fn test_param_shadow_emits_nothing() {
    let result = one(
        "m.py",
        "def handler(): ...\ndef via(handler):\n    cb = handler\n    return handler\n",
    );
    assert_no_indirect_to(&result, "handler");
}

#[test]
fn test_local_shadow_emits_nothing() {
    let result = one(
        "m.py",
        "def handler(): ...\ndef via():\n    handler = object()\n    cb = handler\n    return handler\n",
    );
    assert_no_indirect_to(&result, "handler");
}

#[test]
fn test_non_callable_value_emits_nothing() {
    let result = one(
        "m.py",
        "def handler(): ...\ndef via():\n    cb = TIMEOUT\n    return cb\n",
    );
    assert!(indirect(&result).is_empty());
}

const GETATTR: &str = "def handler(): ...\ndef other(): ...\ndef dispatch(obj):\n    fn = getattr(obj, 'handler')\n    return fn()\n";

#[test]
fn test_getattr_string_literal_emits_indirect_call() {
    let result = one("m.py", GETATTR);
    assert_indirect(&result, "dispatch", "handler");
    let pair = (
        symbol_id(&result, "dispatch"),
        symbol_id(&result, "handler"),
    );
    assert!(!relation_pairs(&result, "calls").contains(&pair));
    assert!(edges(&result).any(|edge| {
        edge.relation == "indirect_call"
            && edge.true_source() == pair.0
            && edge.true_target() == pair.1
            && edge
                .extra
                .get("context")
                .and_then(serde_json::Value::as_str)
                == Some("getattr")
            && edge.confidence == Confidence::Inferred
    }));
}

#[test]
fn test_getattr_with_default_emits() {
    let result = one(
        "m.py",
        "def handler(): ...\ndef dispatch(obj):\n    return getattr(obj, 'handler', None)()\n",
    );
    assert_indirect(&result, "dispatch", "handler");
}

#[test]
fn test_module_level_getattr_emits() {
    let result = one(
        "m.py",
        "import sys\ndef handler(): ...\nHANDLER = getattr(sys.modules[__name__], 'handler')\n",
    );
    assert_file_indirect(&result, "m.py", "handler");
}

#[test]
fn test_getattr_feeds_affected() {
    let result = one("m.py", GETATTR);
    assert!(affected_text(&result, "handler").contains("dispatch"));
}

#[test]
fn test_getattr_string_not_shadowed_by_param() {
    let result = one(
        "m.py",
        "def handler(): ...\ndef via(handler):\n    return getattr(handler, 'handler')\n",
    );
    assert_indirect(&result, "via", "handler");
}

#[test]
fn test_dynamic_getattr_names_emit_nothing() {
    let result = one(
        "m.py",
        "def handler(): ...\ndef via(obj, name, evt):\n    a = getattr(obj, name)\n    b = getattr(obj, f'on_{evt}')\n    c = getattr(obj, 'on_' + evt)\n    return a, b, c\n",
    );
    let via = symbol_id(&result, "via");
    assert!(indirect(&result).iter().all(|(source, _)| source != &via));
}

#[test]
fn test_getattr_non_callable_name_emits_nothing() {
    let result = one(
        "m.py",
        "TIMEOUT = 30\ndef via(obj):\n    return getattr(obj, 'TIMEOUT')\n",
    );
    assert!(indirect(&result).is_empty());
}

#[test]
fn test_method_named_getattr_is_not_the_builtin() {
    let result = one(
        "m.py",
        "def handler(): ...\nclass Registry:\n    def getattr(self, name): ...\ndef via(reg):\n    return reg.getattr('handler')\n",
    );
    assert_no_indirect_to(&result, "handler");
}

const NESTED_SHADOW_FILES: &[(&str, &str)] = &[
    ("src/round.test.ts", "const r = (v) => Math.round(v);\n"),
    (
        "src/report.ts",
        "export function buildSheet(rows, cols) {\n  return rows.map((r) => {\n    const o = {};\n    for (const c of cols) o[c.label] = c.get(r);\n    return o;\n  });\n}\n",
    ),
];

#[test]
fn test_untracked_arrow_param_shadow_emits_no_indirect_call() {
    let result = many(NESTED_SHADOW_FILES);
    assert_no_indirect_to(&result, "r");
}

#[test]
fn test_untracked_arrow_genuine_reference_still_emits_indirect_call() {
    let result = one(
        "a.js",
        "function handler(x){ return x; }\nfunction via(rows, pool){\n  return rows.map((row) => { pool.submit(handler); return row; });\n}\n",
    );
    assert_indirect(&result, "via", "handler");
}

#[test]
fn test_double_nested_untracked_closures_shadow_compounds() {
    let result = one(
        "a.js",
        "function outer(){}\nfunction inner(){}\nfunction via(rows){\n  return rows.map((outer) => [1].map((inner) => { pool.submit(outer); pool.submit(inner); return 0; }));\n}\n",
    );
    assert_no_indirect_to(&result, "outer");
    assert_no_indirect_to(&result, "inner");
}

#[test]
fn test_tracked_const_arrow_param_shadow_still_emits_no_indirect_call() {
    let result = one(
        "a.js",
        "function handler(){}\nconst via = (pool, handler) => { pool.submit(handler); };\n",
    );
    assert_no_indirect_to(&result, "handler");
}

#[test]
fn test_affected_excludes_shadowed_untracked_closure_caller() {
    let result = many(NESTED_SHADOW_FILES);
    assert!(!affected_text(&result, "r").contains("buildSheet"));
}

#[test]
fn test_untracked_arrow_param_shadow_stable_on_warm_cache() {
    let project = Project::new();
    for (path, source) in NESTED_SHADOW_FILES {
        project.write(path, source);
    }
    let cold = project.extract(false);
    assert_no_indirect_to(&cold, "r");
    let warm = project.extract(false);
    assert_no_indirect_to(&warm, "r");
}
