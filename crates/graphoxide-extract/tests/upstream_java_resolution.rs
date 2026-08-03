//! One-to-one executable port of pinned Graphify Java member-call and
//! type-resolution tests.

use graphoxide_core::{Edge, Extraction, Node};
use graphoxide_extract::extract_files;
use graphoxide_graph::build_graph;
use std::{collections::BTreeSet, fs, path::PathBuf};
use tempfile::TempDir;

fn corpus(files: &[(&str, &str)]) -> Extraction {
    let temp = TempDir::new().expect("temporary Java corpus");
    let mut paths = Vec::<PathBuf>::new();
    for (name, source) in files {
        let path = temp.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create Java fixture directory");
        }
        fs::write(&path, source).expect("write Java fixture");
        paths.push(path);
    }
    let extractions = extract_files(&paths, Some(temp.path()), true)
        .expect("extract Java corpus")
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

fn nodes(result: &Extraction) -> impl Iterator<Item = &Node> {
    result.nodes.iter()
}

fn edges(result: &Extraction) -> impl Iterator<Item = &Edge> {
    result.edges.iter()
}

fn node_by_id<'a>(result: &'a Extraction, id: &str) -> Option<&'a Node> {
    nodes(result).find(|node| node.id == id)
}

fn find(result: &Extraction, label: &str, id_contains: &str) -> String {
    let needle = id_contains.to_ascii_lowercase();
    nodes(result)
        .find(|node| node.label == label && node.id.to_ascii_lowercase().contains(&needle))
        .unwrap_or_else(|| panic!("missing {label:?} with id containing {id_contains:?}"))
        .id
        .clone()
}

fn calls(result: &Extraction) -> BTreeSet<(String, String)> {
    edges(result)
        .filter(|edge| edge.relation == "calls")
        .map(|edge| (edge.true_source().to_owned(), edge.true_target().to_owned()))
        .collect()
}

fn described_calls(result: &Extraction, source: &str) -> Vec<(String, String)> {
    edges(result)
        .filter(|edge| edge.relation == "calls" && edge.true_source() == source)
        .map(|edge| {
            (
                node_by_id(result, edge.true_target())
                    .map(|node| node.label.clone())
                    .unwrap_or_else(|| edge.true_target().to_owned()),
                edge.extra
                    .get("receiver_context")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            )
        })
        .collect()
}

fn label_edges(result: &Extraction, relations: &[&str]) -> BTreeSet<(String, String, String)> {
    edges(result)
        .filter(|edge| relations.contains(&edge.relation.as_str()))
        .map(|edge| {
            (
                node_by_id(result, edge.true_source())
                    .map(|node| node.label.clone())
                    .unwrap_or_default(),
                edge.relation.clone(),
                node_by_id(result, edge.true_target())
                    .map(|node| node.label.clone())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

fn ambiguous_services<'a>() -> (&'a str, &'a str) {
    (
        "Services.java",
        "class PaymentGateway { static void ping() {} void charge() {} }\n\
         class AuditLog { static void ping() {} void charge() {} }\n",
    )
}

#[test]
fn test_explicit_type_receiver_resolves_to_owned_method() {
    let result = corpus(&[
        ambiguous_services(),
        (
            "Checkout.java",
            "class Checkout { void run() { PaymentGateway.ping(); } }\n",
        ),
    ]);
    let calls = calls(&result);
    let run = find(&result, ".run()", "checkout");
    let gateway = find(&result, ".ping()", "paymentgateway");
    let audit = find(&result, ".ping()", "auditlog");
    assert!(calls.contains(&(run.clone(), gateway)));
    assert!(!calls.contains(&(run, audit)));
}

#[test]
fn test_field_receiver_resolves_to_declared_type() {
    let result = corpus(&[
        ambiguous_services(),
        (
            "Checkout.java",
            "class Checkout {\n\
                 void run() { gateway.charge(); }\n\
                 PaymentGateway gateway;\n\
             }\n",
        ),
    ]);
    let calls = calls(&result);
    let run = find(&result, ".run()", "checkout");
    let gateway = find(&result, ".charge()", "paymentgateway");
    let audit = find(&result, ".charge()", "auditlog");
    assert!(calls.contains(&(run.clone(), gateway)));
    assert!(!calls.contains(&(run, audit)));
}

#[test]
fn test_this_field_receiver_resolves_to_declared_type() {
    let result = corpus(&[
        ambiguous_services(),
        (
            "Checkout.java",
            "class Checkout {\n\
                 PaymentGateway gateway;\n\
                 void run() { this.gateway.charge(); }\n\
             }\n",
        ),
    ]);
    let calls = calls(&result);
    assert!(calls.contains(&(
        find(&result, ".run()", "checkout"),
        find(&result, ".charge()", "paymentgateway"),
    )));
}

#[test]
fn test_this_field_uses_field_type_when_parameter_shadows_name() {
    let result = corpus(&[
        ambiguous_services(),
        (
            "Checkout.java",
            "class Checkout {\n\
                 PaymentGateway service;\n\
                 void run(AuditLog service) {\n\
                     service.charge();\n\
                     this.service.charge();\n\
                 }\n\
             }\n",
        ),
    ]);
    let calls = calls(&result);
    let run = find(&result, ".run()", "checkout");
    assert!(
        calls.contains(&(run.clone(), find(&result, ".charge()", "paymentgateway"),)),
        "outgoing: {:?}",
        described_calls(&result, &run)
    );
    assert!(calls.contains(&(run, find(&result, ".charge()", "auditlog"))));
}

#[test]
fn test_parameter_and_local_receivers_resolve_per_method() {
    let result = corpus(&[
        ambiguous_services(),
        (
            "Checkout.java",
            "class Checkout {\n\
                 void fromParameter(PaymentGateway service) { service.charge(); }\n\
                 void fromLocal() { AuditLog service = new AuditLog(); service.charge(); }\n\
             }\n",
        ),
    ]);
    let calls = calls(&result);
    let parameter = find(&result, ".fromParameter()", "checkout");
    let local = find(&result, ".fromLocal()", "checkout");
    let gateway = find(&result, ".charge()", "paymentgateway");
    let audit = find(&result, ".charge()", "auditlog");
    assert!(calls.contains(&(parameter.clone(), gateway.clone())));
    assert!(!calls.contains(&(parameter, audit.clone())));
    assert!(calls.contains(&(local.clone(), audit)));
    assert!(!calls.contains(&(local, gateway)));
}

#[test]
fn test_nested_receiver_bindings_do_not_escape_their_scope() {
    let result = corpus(&[
        ambiguous_services(),
        (
            "Checkout.java",
            "class Checkout {\n\
                 PaymentGateway service;\n\
                 void blockLocal() {\n\
                     service.charge();\n\
                     { AuditLog service = null; service.charge(); }\n\
                 }\n\
                 void anonymousClass() {\n\
                     new Object() { void nested() { AuditLog service = null; } };\n\
                     service.charge();\n\
                 }\n\
             }\n",
        ),
    ]);
    let calls = calls(&result);
    let block_local = find(&result, ".blockLocal()", "checkout");
    let anonymous = find(&result, ".anonymousClass()", "checkout");
    let gateway = find(&result, ".charge()", "paymentgateway");
    let audit = find(&result, ".charge()", "auditlog");
    assert!(!calls.iter().any(|(source, target)| {
        source == &block_local && (target == &gateway || target == &audit)
    }));
    assert!(calls.contains(&(anonymous.clone(), gateway)));
    assert!(!calls.contains(&(anonymous, audit)));
}

#[test]
fn test_lambda_shadowing_does_not_reuse_enclosing_receiver_type() {
    let result = corpus(&[
        ambiguous_services(),
        (
            "Checkout.java",
            "class Checkout {\n\
                 PaymentGateway service;\n\
                 void captured() { Runnable task = () -> service.charge(); }\n\
                 void shadowed() { java.util.function.Consumer<AuditLog> task =\n\
                     service -> service.charge(); }\n\
                 void parenthesized() { java.util.function.Consumer<AuditLog> task =\n\
                     (service) -> service.charge(); }\n\
                 void typed() { java.util.function.Consumer<AuditLog> task =\n\
                     (AuditLog service) -> service.charge(); }\n\
                 void sameType() { java.util.function.Consumer<PaymentGateway> task =\n\
                     (PaymentGateway service) -> service.charge(); }\n\
             }\n",
        ),
    ]);
    let calls = calls(&result);
    let gateway = find(&result, ".charge()", "paymentgateway");
    assert!(calls.contains(&(find(&result, ".captured()", "checkout"), gateway.clone(),)));
    assert!(calls.contains(&(find(&result, ".sameType()", "checkout"), gateway,)));
    for name in ["shadowed", "parenthesized", "typed"] {
        let caller = find(&result, &format!(".{name}()"), "checkout");
        let outgoing = calls
            .iter()
            .filter(|(source, _)| source == &caller)
            .map(|(_, target)| {
                node_by_id(&result, target)
                    .map(|node| node.label.clone())
                    .unwrap_or_else(|| target.clone())
            })
            .collect::<Vec<_>>();
        assert!(
            !calls
                .iter()
                .any(|(source, target)| source == &caller && target.contains("charge")),
            "{name} unexpectedly resolved: {outgoing:?}"
        );
    }
}

#[test]
fn test_overloaded_callers_keep_body_scoped_receiver_types() {
    let result = corpus(&[
        ambiguous_services(),
        (
            "Checkout.java",
            "class Checkout {\n\
                 void run(int value) { PaymentGateway service = null; service.charge(); }\n\
                 void run(String value) { AuditLog service = null; service.charge(); }\n\
             }\n",
        ),
    ]);
    let calls = calls(&result);
    let run = find(&result, ".run()", "checkout");
    assert!(
        calls.contains(&(run.clone(), find(&result, ".charge()", "paymentgateway"),)),
        "outgoing: {:?}",
        described_calls(&result, &run)
    );
    assert!(
        calls.contains(&(run.clone(), find(&result, ".charge()", "auditlog"))),
        "outgoing: {:?}",
        described_calls(&result, &run)
    );
}

#[test]
fn test_ambiguous_receiver_type_emits_no_edge() {
    let result = corpus(&[
        (
            "a/Gateway.java",
            "package a; public class Gateway { public void send() {} }\n",
        ),
        (
            "b/Gateway.java",
            "package b; public class Gateway { public void send() {} }\n",
        ),
        (
            "Caller.java",
            "class Caller { void run(Gateway gateway) { gateway.send(); } }\n",
        ),
    ]);
    let run = find(&result, ".run()", "caller");
    assert!(!calls(&result)
        .iter()
        .any(|(source, target)| source == &run && target.contains("send")));
}

#[test]
fn test_inherited_field_and_chained_receiver_are_deferred() {
    let result = corpus(&[(
        "Services.java",
        "class Gateway { void charge() {} Gateway create() { return this; } }\n\
         class Base { Gateway gateway; }\n\
         class Checkout extends Base {\n\
             Gateway factory;\n\
             void inherited() { this.gateway.charge(); }\n\
             void chained() { factory.create().charge(); }\n\
         }\n",
    )]);
    let callers = [
        find(&result, ".inherited()", "checkout"),
        find(&result, ".chained()", "checkout"),
    ];
    assert!(!calls(&result).iter().any(|(source, target)| {
        callers.iter().any(|caller| caller == source) && target.contains("charge")
    }));
}

#[test]
fn test_unqualified_call_still_resolves() {
    let result = corpus(&[(
        "Checkout.java",
        "class Checkout {\n\
             void run() { helper(); this.other(); }\n\
             void helper() {}\n\
             void other() {}\n\
         }\n",
    )]);
    let calls = calls(&result);
    let run = find(&result, ".run()", "checkout");
    assert!(calls.contains(&(run.clone(), find(&result, ".helper()", "checkout"))));
    assert!(calls.contains(&(run, find(&result, ".other()", "checkout"))));
}

#[test]
fn test_java_cross_file_implements_resolves_to_real_def() {
    let result = corpus(&[
        (
            "src/com/x/handler/AIResponseHandler.java",
            "package com.x.handler;\npublic interface AIResponseHandler {}\n",
        ),
        (
            "src/com/x/service/DifyAiServiceImpl.java",
            "package com.x.service;\n\
             import com.x.handler.AIResponseHandler;\n\
             public class DifyAiServiceImpl implements AIResponseHandler {}\n",
        ),
    ]);
    let implements = edges(&result)
        .filter(|edge| edge.relation == "implements")
        .collect::<Vec<_>>();
    assert!(!implements.is_empty());
    for edge in implements {
        let target = node_by_id(&result, edge.true_target()).expect("implements target node");
        assert!(!target.source_file.is_empty());
        assert!(target.source_file.contains("handler"));
    }
}

#[test]
fn test_java_ambiguous_implements_disambiguated_by_import() {
    let result = corpus(&[
        (
            "src/com/a/handler/AIResponseHandler.java",
            "package com.a.handler;\npublic interface AIResponseHandler {}\n",
        ),
        (
            "src/com/b/handler/AIResponseHandler.java",
            "package com.b.handler;\npublic interface AIResponseHandler {}\n",
        ),
        (
            "src/com/x/service/Impl.java",
            "package com.x.service;\n\
             import com.a.handler.AIResponseHandler;\n\
             public class Impl implements AIResponseHandler {}\n",
        ),
    ]);
    assert!(!nodes(&result)
        .any(|node| node.label == "AIResponseHandler" && node.source_file.is_empty()));
    let implements = edges(&result)
        .filter(|edge| edge.relation == "implements")
        .collect::<Vec<_>>();
    assert_eq!(implements.len(), 1);
    let target = node_by_id(&result, implements[0].true_target()).expect("implements target node");
    assert!(target.source_file.contains("com/a/handler"));
    assert!(!target.source_file.contains("com/b/handler"));
}

#[test]
fn test_java_ambiguous_reference_disambiguated_by_import() {
    let result = corpus(&[
        (
            "payment/src/com/example/payment/FinancialEntryValidator.java",
            "package com.example.payment;\n\
             public class FinancialEntryValidator {\n\
                 public boolean validateCurrency(String c) { return c.length() == 3; }\n\
             }\n",
        ),
        (
            "core/src/com/example/core/FinancialEntryValidator.java",
            "package com.example.core;\n\
             public class FinancialEntryValidator {\n\
                 public void auditEntry(String id) {}\n\
             }\n",
        ),
        (
            "app/src/com/example/app/PaymentService.java",
            "package com.example.app;\n\
             import com.example.payment.FinancialEntryValidator;\n\
             public class PaymentService {\n\
                 private FinancialEntryValidator validator = new FinancialEntryValidator();\n\
             }\n",
        ),
    ]);
    let definitions = nodes(&result)
        .filter(|node| node.label == "FinancialEntryValidator")
        .collect::<Vec<_>>();
    assert_eq!(
        definitions
            .iter()
            .filter(|node| !node.source_file.is_empty())
            .count(),
        2
    );
    assert!(!definitions.iter().any(|node| node.source_file.is_empty()));
    let targets = edges(&result)
        .filter(|edge| edge.relation == "references")
        .filter_map(|edge| node_by_id(&result, edge.true_target()))
        .filter(|node| node.label == "FinancialEntryValidator")
        .collect::<Vec<_>>();
    assert!(!targets.is_empty());
    assert!(targets
        .iter()
        .all(|node| node.source_file.contains("payment/")));
    assert!(targets
        .iter()
        .all(|node| !node.source_file.contains("core/")));
}

#[test]
fn test_java_implements_edge_survives_build() {
    let result = corpus(&[
        (
            "src/com/x/handler/Handler.java",
            "package com.x.handler;\npublic interface Handler {}\n",
        ),
        (
            "src/com/x/service/Svc.java",
            "package com.x.service;\n\
             import com.x.handler.Handler;\n\
             public class Svc implements Handler {}\n",
        ),
    ]);
    let graph = build_graph(std::slice::from_ref(&result)).expect("build Java graph");
    assert!(graph.links.iter().any(|edge| edge.relation == "implements"));
}

#[test]
fn test_java_record_becomes_type_node() {
    let result = corpus(&[(
        "Foo.java",
        "package com.app;\npublic record Foo(int x, String y) {}\n",
    )]);
    assert!(nodes(&result).any(|node| node.label == "Foo" && !node.source_file.is_empty()));
    assert!(label_edges(&result, &["contains"]).contains(&(
        "Foo.java".into(),
        "contains".into(),
        "Foo".into(),
    )));
}

#[test]
fn test_java_record_implements_interface() {
    let result = corpus(&[
        ("I.java", "package com.app;\npublic interface I {}\n"),
        (
            "Foo.java",
            "package com.app;\npublic record Foo(int x) implements I {}\n",
        ),
    ]);
    assert!(edges(&result).any(|edge| edge.relation == "implements"));
}

#[test]
fn test_java_type_parameters_do_not_resolve_to_real_class() {
    let result = corpus(&[
        ("T.java", "public class T {}\n"),
        (
            "Generic.java",
            "public class Generic<T> { java.util.List<T> values; }\n",
        ),
    ]);
    assert!(!label_edges(&result, &["references"]).contains(&(
        "Generic".into(),
        "references".into(),
        "T".into(),
    )));
}

#[test]
fn test_java_builtin_library_types_not_emitted_as_references() {
    let result = corpus(&[(
        "Svc.java",
        "package com.app;\n\
         import java.util.List;\n\
         import java.util.Map;\n\
         public class Svc {\n\
             private String name;\n\
             private List<Integer> ids;\n\
             public Map<String, Object> lookup(Long id) { return null; }\n\
             public java.util.Optional<Boolean> flag() { return null; }\n\
         }\n",
    )]);
    let targets = label_edges(&result, &["references"])
        .into_iter()
        .map(|(_, _, target)| target)
        .collect::<BTreeSet<_>>();
    for builtin in [
        "String", "Integer", "Map", "Object", "Long", "List", "Optional", "Boolean",
    ] {
        assert!(
            !targets.contains(builtin),
            "builtin/library type {builtin:?} should not be a references target"
        );
    }
}

#[test]
fn test_java_user_types_still_emit_references() {
    let result = corpus(&[
        (
            "OrderDto.java",
            "package com.app;\npublic class OrderDto {}\n",
        ),
        (
            "OrderSvc.java",
            "package com.app;\n\
             public class OrderSvc {\n\
                 private java.util.List<OrderDto> orders;\n\
                 public OrderDto first() { return null; }\n\
             }\n",
        ),
    ]);
    assert!(label_edges(&result, &["references"])
        .iter()
        .any(|(_, _, target)| target == "OrderDto"));
}

#[test]
fn test_java_cross_file_constructor_call_resolves() {
    let result = corpus(&[
        (
            "Foo.java",
            "package com.app;\npublic record Foo(int x, String y) {}\n",
        ),
        (
            "Helper.java",
            "package com.app;\n\
             public class Helper {\n\
                 public void build() {\n\
                     Object o = new Foo(1, \"a\");\n\
                     System.out.println(o);\n\
                 }\n\
             }\n",
        ),
    ]);
    let foo = nodes(&result)
        .find(|node| node.label == "Foo" && !node.source_file.is_empty())
        .expect("real Foo node")
        .id
        .clone();
    assert!(edges(&result).any(|edge| {
        matches!(edge.relation.as_str(), "calls" | "references") && edge.true_target() == foo
    }));
    let graph = build_graph(std::slice::from_ref(&result)).expect("build Java graph");
    assert!(graph.nodes.iter().any(|node| node.id == foo));
}

#[test]
fn java_receiver_resolution_never_guesses_under_type_ambiguity() {
    let result = corpus(&[
        ("a/Dupe.java", "package a; class Dupe { void act() {} }\n"),
        ("b/Dupe.java", "package b; class Dupe { void act() {} }\n"),
        (
            "Caller.java",
            "class Caller { void run(Dupe d) { d.act(); } }\n",
        ),
    ]);
    let run = find(&result, ".run()", "caller");
    assert!(!edges(&result).any(|edge| {
        edge.relation == "calls"
            && edge.true_source() == run
            && node_by_id(&result, edge.true_target()).is_some_and(|node| node.label == ".act()")
    }));
}

#[test]
fn java_untyped_member_call_never_binds_to_unique_unrelated_method() {
    let result = corpus(&[
        ("Target.java", "class Target { void unique() {} }\n"),
        (
            "Caller.java",
            "class Caller { void run(Object unknown) { unknown.unique(); } }\n",
        ),
    ]);
    let run = find(&result, ".run()", "caller");
    let unique = find(&result, ".unique()", "target");
    assert!(!calls(&result).contains(&(run, unique)));
}

#[test]
fn java_import_disambiguation_does_not_retarget_unrelated_phantoms() {
    let result = corpus(&[
        ("a/Chosen.java", "package a; public class Chosen {}\n"),
        ("b/Other.java", "package b; public class Other {}\n"),
        (
            "use/Holder.java",
            "package use; import a.Chosen; class Holder { Missing value; Chosen chosen; }\n",
        ),
    ]);
    assert!(nodes(&result).any(|node| node.label == "Missing" && node.source_file.is_empty()));
    assert!(edges(&result).any(|edge| {
        edge.relation == "references"
            && node_by_id(&result, edge.true_target())
                .is_some_and(|node| node.label == "Chosen" && node.source_file.contains("a/"))
    }));
}

#[test]
fn java_import_disambiguates_member_receiver_type() {
    let result = corpus(&[
        (
            "a/Gateway.java",
            "package a; public class Gateway { public void send() {} }\n",
        ),
        (
            "b/Gateway.java",
            "package b; public class Gateway { public void send() {} }\n",
        ),
        (
            "use/Caller.java",
            "package use; import a.Gateway; class Caller { void run(Gateway gateway) { gateway.send(); } }\n",
        ),
    ]);
    let run = find(&result, ".run()", "caller");
    let selected = find(&result, ".send()", "a_gateway");
    let rejected = find(&result, ".send()", "b_gateway");
    let calls = calls(&result);
    assert!(calls.contains(&(run.clone(), selected)));
    assert!(!calls.contains(&(run, rejected)));
}

#[test]
fn java_same_package_disambiguates_member_receiver_type() {
    let result = corpus(&[
        (
            "src/a/Gateway.java",
            "package a; public class Gateway { public void send() {} }\n",
        ),
        (
            "src/b/Gateway.java",
            "package b; public class Gateway { public void send() {} }\n",
        ),
        (
            "src/a/Caller.java",
            "package a; class Caller { void run(Gateway gateway) { gateway.send(); } }\n",
        ),
    ]);
    let run = find(&result, ".run()", "caller");
    let selected = find(&result, ".send()", "a_gateway");
    let rejected = find(&result, ".send()", "b_gateway");
    let calls = calls(&result);
    assert!(calls.contains(&(run.clone(), selected)));
    assert!(!calls.contains(&(run, rejected)));
}

#[test]
fn java_builtin_filter_does_not_hide_project_type_with_other_name() {
    let result = corpus(&[
        ("DomainOptional.java", "class DomainOptional {}\n"),
        (
            "Holder.java",
            "class Holder { DomainOptional value; java.util.Optional<String> wrapper; }\n",
        ),
    ]);
    assert!(label_edges(&result, &["references"])
        .iter()
        .any(|(_, _, target)| target == "DomainOptional"));
}

#[test]
fn java_chained_receiver_does_not_create_phantom_member_edge() {
    let result = corpus(&[(
        "Chain.java",
        "class Leaf { void finish() {} }\n\
         class Factory { Leaf create() { return null; } }\n\
         class Caller { Factory factory; void run() { factory.create().finish(); } }\n",
    )]);
    let run = find(&result, ".run()", "caller");
    let finish = find(&result, ".finish()", "leaf");
    assert!(!calls(&result).contains(&(run, finish)));
}

#[test]
fn java_receiver_shadowing_is_lexically_conservative() {
    let result = corpus(&[
        ambiguous_services(),
        (
            "Checkout.java",
            "class Checkout { PaymentGateway service; void run() {\n\
                 service.charge();\n\
                 { AuditLog service = null; service.charge(); }\n\
                 service.charge();\n\
             } }\n",
        ),
    ]);
    let run = find(&result, ".run()", "checkout");
    let targets = calls(&result)
        .into_iter()
        .filter_map(|(source, target)| {
            (source == run
                && node_by_id(&result, &target).is_some_and(|node| node.label == ".charge()"))
            .then_some(target)
        })
        .collect::<BTreeSet<_>>();
    assert!(
        targets.is_empty(),
        "a single caller node cannot represent the conflicting lexical receiver bindings safely"
    );
}

#[test]
fn java_type_reference_targets_are_real_after_build_when_resolved() {
    let result = corpus(&[
        ("model/Dto.java", "package model; public class Dto {}\n"),
        (
            "service/Svc.java",
            "package service; import model.Dto; class Svc { Dto value; }\n",
        ),
    ]);
    let graph = build_graph(std::slice::from_ref(&result)).expect("build Java graph");
    let dto = nodes(&result)
        .find(|node| node.label == "Dto" && !node.source_file.is_empty())
        .expect("Dto definition");
    assert!(graph.nodes.iter().any(|node| node.id == dto.id));
    assert!(graph
        .links
        .iter()
        .any(|edge| { edge.relation == "references" && edge.true_target() == dto.id }));
}

#[test]
fn java_unresolved_external_type_remains_an_explicit_phantom() {
    let result = corpus(&[(
        "ExternalUse.java",
        "class ExternalUse { ThirdPartyOnly value; }\n",
    )]);
    assert!(
        nodes(&result).any(|node| node.label == "ThirdPartyOnly" && node.source_file.is_empty())
    );
}
