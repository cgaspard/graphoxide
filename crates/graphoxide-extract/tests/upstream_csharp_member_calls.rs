//! One-to-one executable port of pinned Graphify
//! `tests/test_csharp_member_calls.py`.

use graphoxide_core::{Confidence, Extraction};
use graphoxide_extract::extract_files;
use std::{collections::BTreeSet, fs, path::PathBuf};
use tempfile::TempDir;

fn corpus(files: &[(&str, &str)]) -> Extraction {
    let temp = TempDir::new().expect("temporary C# call corpus");
    let mut paths = Vec::<PathBuf>::new();
    for (name, source) in files {
        let path = temp.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create C# fixture directory");
        }
        fs::write(&path, source).expect("write C# fixture");
        paths.push(path);
    }
    let extractions = extract_files(&paths, Some(temp.path()), true)
        .expect("extract C# call corpus")
        .extractions;
    Extraction {
        nodes: extractions
            .iter()
            .flat_map(|extraction| extraction.nodes.iter().cloned())
            .collect(),
        edges: extractions
            .iter()
            .flat_map(|extraction| extraction.edges.iter().cloned())
            .collect(),
        hyperedges: Vec::new(),
    }
}

fn calls(result: &Extraction) -> BTreeSet<(String, String)> {
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls")
        .map(|edge| (edge.true_source().to_owned(), edge.true_target().to_owned()))
        .collect()
}

fn find(result: &Extraction, label: &str, id_contains: &str) -> String {
    result
        .nodes
        .iter()
        .find(|node| node.label == label && node.id.contains(id_contains))
        .unwrap_or_else(|| panic!("missing {label} node containing {id_contains}"))
        .id
        .clone()
}

const AMBIG: &str = concat!(
    "public class Server { public bool Save() => true; }\n",
    "public class Cache  { public bool Save() => false; }\n",
    "public class Repo {\n",
    "    private Server _server = new Server();\n",
    "    public bool Commit() { return _server.Save(); }\n",
    "}\n"
);

#[test]
fn test_field_receiver_resolves_to_declared_type_not_bare_match() {
    let result = corpus(&[("S.cs", AMBIG)]);
    let edges = calls(&result);
    let commit = find(&result, ".Commit()", "commit");
    let server_save = find(&result, ".Save()", "server");
    let cache_save = find(&result, ".Save()", "cache");
    assert!(edges.contains(&(commit.clone(), server_save)));
    assert!(!edges.contains(&(commit, cache_save)));
}

#[test]
fn test_parameter_receiver_resolves() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Server { public bool Save() => true; }\n",
            "public class Cache  { public bool Save() => false; }\n",
            "public class Svc { public static bool Copy(Server server) { return server.Save(); } }\n"
        ),
    )]);
    let edges = calls(&result);
    assert!(edges
        .iter()
        .any(|(source, target)| source.contains("copy") && target.contains("server_save")));
    assert!(!edges
        .iter()
        .any(|(source, target)| source.contains("copy") && target.contains("cache_save")));
}

#[test]
fn test_local_var_receiver_resolves() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Server { public bool Save() => true; }\n",
            "public class R {\n",
            "    public bool A() { Server s = new Server(); return s.Save(); }\n",
            "    public bool B() { var v = new Server(); return v.Save(); }\n",
            "}\n"
        ),
    )]);
    let edges = calls(&result);
    assert!(edges
        .iter()
        .any(|(source, target)| source.contains("_r_a") && target.contains("server_save")));
    assert!(edges
        .iter()
        .any(|(source, target)| source.contains("_r_b") && target.contains("server_save")));
}

#[test]
fn test_cross_file_receiver_resolves() {
    let result = corpus(&[
        (
            "Server.cs",
            concat!(
                "public class Server { public bool Save() => true; }\n",
                "public class Cache  { public bool Save() => false; }\n"
            ),
        ),
        (
            "Repo.cs",
            "public class Repo { private Server _s = new Server(); public bool Commit() { return _s.Save(); } }\n",
        ),
    ]);
    let edges = calls(&result);
    assert!(edges
        .iter()
        .any(|(source, target)| source.contains("commit") && target.contains("server_save")));
    assert!(!edges
        .iter()
        .any(|(source, target)| source.contains("commit") && target.contains("cache_save")));
}

#[test]
fn test_this_and_static_receivers() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Util { public static int F() => 1; }\n",
            "public class R {\n",
            "    public bool A() { return this.B(); }\n",
            "    public bool B() => true;\n",
            "    public int G() { return Util.F(); }\n",
            "}\n"
        ),
    )]);
    let edges = calls(&result);
    assert!(edges
        .iter()
        .any(|(source, target)| source.contains("_r_a") && target.contains("_r_b")));
    assert!(edges
        .iter()
        .any(|(source, target)| source.contains("_r_g") && target.contains("util_f")));
}

#[test]
fn test_untyped_receiver_emits_no_edge() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Server { public bool Save() => true; }\n",
            "public class R { public bool C(dynamic x) { return x.Save(); } }\n"
        ),
    )]);
    assert!(!calls(&result)
        .iter()
        .any(|(_, target)| target.to_ascii_lowercase().contains("save")));
}

#[test]
fn test_method_absent_on_type_emits_no_edge() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Server { public bool Save() => true; }\n",
            "public class R { private Server _s = new Server(); public bool C() { return _s.Missing(); } }\n"
        ),
    )]);
    assert!(!calls(&result).iter().any(|(source, target)| {
        source.contains("_r_c") && target.to_ascii_lowercase().contains("save")
    }));
}

#[test]
fn test_unqualified_call_still_resolves() {
    let result = corpus(&[(
        "S.cs",
        "public class R { public bool A() { Helper(); return true; } private void Helper() {} }\n",
    )]);
    assert!(calls(&result)
        .iter()
        .any(|(source, target)| source.contains("_r_a") && target.contains("helper")));
}

const A_SVC: &str = "namespace A { public class Svc { public bool Do() => true; } }\n";
const B_SVC: &str = "namespace B { public class Svc { public bool Do() => false; } }\n";

#[test]
fn test_namespace_using_directive_disambiguates_receiver_type() {
    let result = corpus(&[
        ("A.cs", A_SVC),
        ("B.cs", B_SVC),
        (
            "Caller.cs",
            "using A;\nnamespace App { public class Runner { public bool Go(Svc s) { return s.Do(); } } }\n",
        ),
    ]);
    let edges = calls(&result);
    let a_do = find(&result, ".Do()", "a_a_svc");
    let b_do = find(&result, ".Do()", "b_b_svc");
    let go = find(&result, ".Go()", "runner");
    assert!(edges.contains(&(go.clone(), a_do)));
    assert!(!edges.contains(&(go, b_do)));
}

#[test]
fn test_namespace_using_directive_resolves_to_other_namespace() {
    let result = corpus(&[
        ("A.cs", A_SVC),
        ("B.cs", B_SVC),
        (
            "Caller.cs",
            "using B;\nnamespace App { public class Runner { public bool Go(Svc s) { return s.Do(); } } }\n",
        ),
    ]);
    let edges = calls(&result);
    let a_do = find(&result, ".Do()", "a_a_svc");
    let b_do = find(&result, ".Do()", "b_b_svc");
    let go = find(&result, ".Go()", "runner");
    assert!(edges.contains(&(go.clone(), b_do)));
    assert!(!edges.contains(&(go, a_do)));
}

#[test]
fn test_namespace_ambiguous_without_using_bails() {
    let result = corpus(&[
        ("A.cs", A_SVC),
        ("B.cs", B_SVC),
        (
            "Caller.cs",
            "namespace App { public class Runner { public bool Go(Svc s) { return s.Do(); } } }\n",
        ),
    ]);
    assert!(!calls(&result)
        .iter()
        .any(|(source, target)| source.contains("runner") && target.contains("svc_do")));
}

#[test]
fn test_same_namespace_receiver_resolves_without_using() {
    let result = corpus(&[
        ("A.cs", A_SVC),
        ("B.cs", B_SVC),
        (
            "A2.cs",
            "namespace A { public class Client { public bool Go(Svc s) { return s.Do(); } } }\n",
        ),
    ]);
    let edges = calls(&result);
    let a_do = find(&result, ".Do()", "a_a_svc");
    let b_do = find(&result, ".Do()", "b_b_svc");
    let go = find(&result, ".Go()", "client");
    assert!(edges.contains(&(go.clone(), a_do)));
    assert!(!edges.contains(&(go, b_do)));
}

#[test]
fn test_local_shadowing_field_of_different_type_poisons_name() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Server { public bool Run() => true; }\n",
            "public class Other  { public bool Run() => false; }\n",
            "public class Holder {\n",
            "    private Server x = new Server();\n",
            "    public bool A() { Other x = new Other(); return x.Run(); }\n",
            "}\n"
        ),
    )]);
    let edges = calls(&result);
    let source = find(&result, ".A()", "holder");
    assert!(!edges.contains(&(source.clone(), find(&result, ".Run()", "server"))));
    assert!(!edges.contains(&(source, find(&result, ".Run()", "other"))));
}

#[test]
fn test_untyped_redeclaration_poisons_typed_field() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Server { public bool Run() => true; }\n",
            "public class Holder {\n",
            "    private Server x = new Server();\n",
            "    public object Compute() => new object();\n",
            "    public bool A() { var x = Compute(); return x.Run(); }\n",
            "}\n"
        ),
    )]);
    assert!(!calls(&result)
        .iter()
        .any(|(source, target)| source.contains("holder_a") && target.contains("run")));
}

#[test]
fn test_this_field_receiver_resolves() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Server { public bool Save() => true; }\n",
            "public class Cache  { public bool Save() => false; }\n",
            "public class Repo { private Server _s = new Server(); public bool Commit() { return this._s.Save(); } }\n"
        ),
    )]);
    let edges = calls(&result);
    let source = find(&result, ".Commit()", "commit");
    assert!(edges.contains(&(source.clone(), find(&result, ".Save()", "server"))));
    assert!(!edges.contains(&(source, find(&result, ".Save()", "cache"))));
}

#[test]
fn test_base_receiver_resolves_to_base_class_method() {
    let result = corpus(&[
        (
            "Base.cs",
            "public class BaseSvc { public bool Ping() => true; }\n",
        ),
        (
            "Sub.cs",
            "public class Sub : BaseSvc { public bool Go() { return base.Ping(); } }\n",
        ),
    ]);
    assert!(calls(&result).contains(&(
        find(&result, ".Go()", "sub"),
        find(&result, ".Ping()", "basesvc")
    )));
}

#[test]
fn test_inherited_method_resolves_through_base_chain() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class BaseSvc { public bool Ping() => true; }\n",
            "public class Derived : BaseSvc { }\n",
            "public class User { public bool Use(Derived d) { return d.Ping(); } }\n"
        ),
    )]);
    assert!(calls(&result).contains(&(
        find(&result, ".Use()", "user"),
        find(&result, ".Ping()", "basesvc")
    )));
}

#[test]
fn test_unresolved_base_poisons_inherited_member_lookup() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Server { public bool Save() => true; }\n",
            "public class Ext : NotInCorpus { }\n",
            "public class User { public bool U(Ext e) { return e.Save(); } }\n"
        ),
    )]);
    assert!(!calls(&result)
        .iter()
        .any(|(source, target)| source.contains("user_u") && target.contains("save")));
}

#[test]
fn test_cross_method_name_reuse_does_not_poison() {
    let result = corpus(&[
        (
            "Item.cs",
            "namespace Demo { public class Item { public void Handle() {} } }\n",
        ),
        (
            "Runner.cs",
            concat!(
                "using System.Collections.Generic;\n",
                "namespace Demo { public class Runner {\n",
                "public void RunOne(Item item) { item.Handle(); }\n",
                "public void RunIndexed(List<Item> items, int i) { var item = items[i]; item.Handle(); }\n",
                "} }\n"
            ),
        ),
    ]);
    let run_one = find(&result, ".RunOne()", "runner");
    let run_indexed = find(&result, ".RunIndexed()", "runner");
    let handle = find(&result, ".Handle()", "item");
    let edges = calls(&result);
    assert!(edges.contains(&(run_one.clone(), handle.clone())));
    let edge = result.edges.iter().find(|edge| {
        edge.relation == "calls" && edge.true_source() == run_one && edge.true_target() == handle
    });
    assert_eq!(edge.map(|edge| edge.confidence), Some(Confidence::Inferred));
    assert!(!edges.contains(&(run_indexed, handle)));
}

#[test]
fn test_per_method_locals_resolve_independently() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class HtmlWriter { public void Render() {} }\n",
            "public class TextWriter { public void Render() {} }\n",
            "public class Doc {\n",
            "public void AsHtml() { var w = new HtmlWriter(); w.Render(); }\n",
            "public void AsText() { var w = new TextWriter(); w.Render(); }\n",
            "}\n"
        ),
    )]);
    let html = find(&result, ".AsHtml()", "doc");
    let text = find(&result, ".AsText()", "doc");
    let html_render = find(&result, ".Render()", "htmlwriter");
    let text_render = find(&result, ".Render()", "textwriter");
    let edges = calls(&result);
    assert!(edges.contains(&(html.clone(), html_render.clone())));
    assert!(edges.contains(&(text.clone(), text_render.clone())));
    assert!(!edges.contains(&(html, text_render)));
    assert!(!edges.contains(&(text, html_render)));
}

#[test]
fn test_same_method_shadow_still_poisons() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Server { public bool Run() => true; }\n",
            "public class Other  { public bool Run() => false; }\n",
            "public class Holder { public bool A(Server x) { Other x = new Other(); return x.Run(); } }\n"
        ),
    )]);
    let source = find(&result, ".A()", "holder");
    let edges = calls(&result);
    assert!(!edges.contains(&(source.clone(), find(&result, ".Run()", "server"))));
    assert!(!edges.contains(&(source, find(&result, ".Run()", "other"))));
}

#[test]
fn test_file_scoped_namespace_receiver_resolves() {
    let result = corpus(&[
        (
            "Item.cs",
            "namespace Demo;\npublic class Item { public void Handle() {} }\n",
        ),
        (
            "Runner.cs",
            "namespace Demo;\npublic class Runner { public void RunOne(Item item) { item.Handle(); } }\n",
        ),
    ]);
    assert!(calls(&result).contains(&(
        find(&result, ".RunOne()", "runner"),
        find(&result, ".Handle()", "item")
    )));
}

#[test]
fn test_method_chained_off_new_expression_resolves() {
    let result = corpus(&[(
        "S.cs",
        concat!(
            "public class Merger { public Merger(int x) {} public int Combine(int a, bool b) { return a; } }\n",
            "public class Svc { public int Run(int ctx) { return new Merger(ctx).Combine(ctx, true); } }\n"
        ),
    )]);
    let labels = result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert!(calls(&result)
        .iter()
        .any(|(source, target)| source.contains("run")
            && labels.get(target.as_str()) == Some(&".Combine()")));
}

const TWO_GO: &str = concat!(
    "public class Sect { public bool Go() => true; }\n",
    "public class Twig { public bool Go() => false; }\n"
);

fn assert_sect_go(source_body: &str) {
    let source = format!("{TWO_GO}{source_body}");
    let result = corpus(&[("S.cs", &source)]);
    let method = find(&result, ".A()", "_r_a");
    let edges = calls(&result);
    assert!(edges.contains(&(method.clone(), find(&result, ".Go()", "sect"))));
    assert!(!edges.contains(&(method, find(&result, ".Go()", "twig"))));
}

#[test]
fn test_out_declared_receiver_resolves() {
    assert_sect_go(concat!(
        "public class Box { public bool TryGet(out Sect s) { s = new Sect(); return true; } }\n",
        "public class R { public bool A(Box b) { if (b.TryGet(out Sect s)) { return s.Go(); } return false; } }\n"
    ));
}

#[test]
fn test_out_var_receiver_stays_unbound() {
    let source = format!(
        "{TWO_GO}{}{}",
        "public class Box { public bool TryGet(out Sect s) { s = new Sect(); return true; } }\n",
        "public class R { public bool B(Box b) { b.TryGet(out var v); return v.Go(); } }\n"
    );
    let result = corpus(&[("S.cs", &source)]);
    assert!(!calls(&result)
        .iter()
        .any(|(source, target)| source.contains("_r_b") && target.contains("go")));
}

#[test]
fn test_is_pattern_receiver_resolves() {
    assert_sect_go(
        "public class R { public bool A(object o) { if (o is Sect s) { return s.Go(); } return false; } }\n",
    );
}

#[test]
fn test_is_not_pattern_receiver_resolves() {
    assert_sect_go(
        "public class R { public bool A(object o) { if (o is not Sect s) { return false; } return s.Go(); } }\n",
    );
}

#[test]
fn test_case_pattern_receiver_resolves() {
    assert_sect_go(concat!(
        "public class R { public bool A(object o) {\n",
        "switch (o) { case Sect s: return s.Go(); } return false; } }\n"
    ));
}

#[test]
fn test_switch_arm_pattern_receiver_resolves() {
    assert_sect_go(
        "public class R { public bool A(object o) { return o switch { Sect s => s.Go(), _ => false }; } }\n",
    );
}

#[test]
fn test_sibling_pattern_rebind_conflict_poisons() {
    let source = format!(
        "{TWO_GO}{}",
        concat!(
            "public class R { public bool A(object o) {\n",
            "if (o is Sect x) { return x.Go(); }\n",
            "if (o is Twig x) { return x.Go(); }\n",
            "return false; } }\n"
        )
    );
    let result = corpus(&[("S.cs", &source)]);
    let method = find(&result, ".A()", "_r_a");
    let edges = calls(&result);
    assert!(!edges.contains(&(method.clone(), find(&result, ".Go()", "sect"))));
    assert!(!edges.contains(&(method, find(&result, ".Go()", "twig"))));
}
