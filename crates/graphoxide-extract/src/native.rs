//! Receiver-aware call facts for C++, Objective-C, and Objective-C++.
//!
//! Native member resolution is deliberately precision-first. A call is only
//! connected when its receiver names (or is locally bound to) one unambiguous
//! in-corpus type and that type owns exactly one matching method.

use graphoxide_core::{make_id, normalize_id, Confidence, Extraction};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node as TsNode;

pub(crate) const CALL_FACT: &str = "native_call_fact";
pub(crate) const INFERRED_RECEIVER: &str = "native_inferred_receiver";

pub(crate) struct ReceiverFact {
    pub receiver: String,
    pub receiver_type: Option<String>,
}

/// Recover the receiver and its type from a C++ call expression.
///
/// `Foo::bar()` and `this->bar()` contain explicit type evidence. For
/// `f.bar()`/`f->bar()`, the declaration preceding the call inside the same
/// function must bind `f` to a concrete type; an unbound lowercase receiver is
/// never guessed from the member name.
pub(crate) fn cpp_receiver_fact(
    call: TsNode<'_>,
    callee: TsNode<'_>,
    source: &[u8],
    enclosing_type: Option<&str>,
) -> Option<ReceiverFact> {
    let text = |node: TsNode<'_>| {
        String::from_utf8_lossy(&source[node.byte_range()])
            .trim()
            .to_owned()
    };
    if callee.kind() == "qualified_identifier" {
        let receiver = callee
            .child_by_field_name("scope")
            .or_else(|| callee.named_child(0))
            .map(text)?;
        let receiver_type = simple_type_name(&receiver);
        return (!receiver_type.is_empty()).then_some(ReceiverFact {
            receiver,
            receiver_type: Some(receiver_type),
        });
    }
    if callee.kind() != "field_expression" {
        return None;
    }
    let receiver = callee
        .child_by_field_name("argument")
        .or_else(|| callee.named_child(0))
        .map(text)?;
    if receiver == "this" {
        let receiver_type = enclosing_type
            .map(simple_type_name)
            .filter(|name| !name.is_empty())
            .or_else(|| cpp_qualified_owner(call, source));
        return Some(ReceiverFact {
            receiver,
            receiver_type,
        });
    }
    if receiver.chars().next().is_some_and(char::is_uppercase) {
        return Some(ReceiverFact {
            receiver: receiver.clone(),
            receiver_type: Some(simple_type_name(&receiver)),
        });
    }
    let receiver_type = cpp_local_receiver_type(call, source, &receiver);
    Some(ReceiverFact {
        receiver,
        receiver_type,
    })
}

fn cpp_qualified_owner(call: TsNode<'_>, source: &[u8]) -> Option<String> {
    let mut scope = call;
    while scope.kind() != "function_definition" {
        scope = scope.parent()?;
    }
    let declaration_end = scope
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or(scope.end_byte());
    let declaration = String::from_utf8_lossy(&source[scope.start_byte()..declaration_end]);
    Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*::\s*[A-Za-z_][A-Za-z0-9_]*\s*\(")
        .ok()?
        .captures(&declaration)
        .and_then(|capture| capture.get(1))
        .map(|capture| capture.as_str().to_owned())
}

fn cpp_local_receiver_type(call: TsNode<'_>, source: &[u8], receiver: &str) -> Option<String> {
    let mut function = call;
    while function.kind() != "function_definition" {
        function = function.parent()?;
    }
    let mut declarations = Vec::new();
    collect_nodes(
        function,
        &[
            "declaration",
            "parameter_declaration",
            "optional_parameter_declaration",
        ],
        &mut declarations,
    );
    let binding = declarations
        .into_iter()
        .filter(|declaration| declaration.start_byte() < call.start_byte())
        .filter(|declaration| declaration_visible_at(*declaration, call))
        .filter(|declaration| declaration_binds(*declaration, receiver, source))
        .max_by_key(TsNode::start_byte)?;
    let type_node = binding.child_by_field_name("type")?;
    let type_name = simple_type_name(type_node.utf8_text(source).unwrap_or(""));
    (!type_name.is_empty() && !cpp_type_noise(&type_name)).then_some(type_name)
}

fn collect_nodes<'tree>(node: TsNode<'tree>, kinds: &[&str], found: &mut Vec<TsNode<'tree>>) {
    if kinds.contains(&node.kind()) {
        found.push(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_nodes(child, kinds, found);
    }
}

fn declaration_visible_at(declaration: TsNode<'_>, call: TsNode<'_>) -> bool {
    let mut ancestor = declaration.parent();
    while let Some(node) = ancestor {
        if node.kind() == "compound_statement" {
            return node.start_byte() <= call.start_byte() && call.end_byte() <= node.end_byte();
        }
        if node.kind() == "function_definition" {
            return true;
        }
        ancestor = node.parent();
    }
    false
}

fn declaration_binds(declaration: TsNode<'_>, receiver: &str, source: &[u8]) -> bool {
    let mut cursor = declaration.walk();
    let declarators = declaration
        .children_by_field_name("declarator", &mut cursor)
        .collect::<Vec<_>>();
    declarators
        .into_iter()
        .any(|declarator| declarator_binds(declarator, receiver, source))
}

fn declarator_binds(node: TsNode<'_>, receiver: &str, source: &[u8]) -> bool {
    if matches!(node.kind(), "identifier" | "field_identifier") {
        return node.utf8_text(source).unwrap_or("") == receiver;
    }
    if let Some(declarator) = node.child_by_field_name("declarator") {
        return declarator_binds(declarator, receiver, source);
    }
    let mut cursor = node.walk();
    let binds = node
        .named_children(&mut cursor)
        .any(|child| declarator_binds(child, receiver, source));
    binds
}

pub(crate) fn objc_receiver_type(body_prefix: &str, receiver: &str) -> Option<String> {
    let receiver = regex::escape(receiver);
    let pattern = format!(r"\b([A-Z][A-Za-z0-9_]*)\s*(?:<[^;{{}}]+>\s*)?\*+\s*{receiver}\b");
    Regex::new(&pattern)
        .ok()?
        .captures_iter(body_prefix)
        .filter_map(|capture| capture.get(1))
        .map(|capture| capture.as_str().to_owned())
        .last()
}

fn simple_type_name(raw: &str) -> String {
    let raw = raw.trim().trim_start_matches("::");
    let without_generics = raw.split('<').next().unwrap_or(raw).trim();
    without_generics
        .rsplit("::")
        .next()
        .unwrap_or(without_generics)
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .to_owned()
}

fn cpp_type_noise(name: &str) -> bool {
    matches!(
        name,
        "auto"
            | "bool"
            | "char"
            | "double"
            | "float"
            | "int"
            | "long"
            | "short"
            | "signed"
            | "size_t"
            | "unsigned"
            | "void"
            | "wchar_t"
    )
}

/// Resolve only edges carrying [`CALL_FACT`]. The generic resolver never sees
/// unresolved native member facts, which makes the zero-edge ambiguity rule an
/// invariant instead of an incidental consequence of graph-build pruning.
pub(crate) fn resolve_calls(extractions: &mut [Extraction]) {
    let mut labels = BTreeMap::<String, String>::new();
    let mut sources = BTreeMap::<String, BTreeSet<String>>::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            labels.insert(node.id.clone(), node.label.clone());
            if !node.source_file.is_empty() {
                sources
                    .entry(node.id.clone())
                    .or_default()
                    .insert(node.source_file.clone());
            }
        }
    }

    let mut owner_by_method = BTreeMap::<String, BTreeSet<String>>::new();
    let mut parent_types = BTreeMap::<String, BTreeSet<String>>::new();
    for extraction in extractions.iter() {
        for edge in &extraction.edges {
            if edge.relation == "inherits" {
                if let (Some(child), Some(parent)) = (
                    labels.get(edge.true_source()),
                    labels.get(edge.true_target()),
                ) {
                    parent_types
                        .entry(normalize_id(child))
                        .or_default()
                        .insert(normalize_id(parent));
                }
            }
            if edge.relation == "method" {
                let Some(owner) = labels.get(edge.true_source()) else {
                    continue;
                };
                owner_by_method
                    .entry(edge.true_target().to_owned())
                    .or_default()
                    .insert(normalize_id(owner));
            }
        }
    }

    let mut methods = BTreeMap::<String, BTreeSet<String>>::new();
    for (id, owners) in &owner_by_method {
        let Some(label) = labels.get(id) else {
            continue;
        };
        if owners.len() != 1 || sources.get(id).is_none_or(BTreeSet::is_empty) {
            continue;
        }
        methods
            .entry(normalize_id(
                label
                    .trim_start_matches(['.', '-', '+'])
                    .trim_end_matches("()"),
            ))
            .or_default()
            .insert(id.clone());
    }

    for extraction in extractions.iter_mut() {
        let mut resolved_native = BTreeSet::new();
        extraction.edges.retain_mut(|edge| {
            if edge.extra.get(CALL_FACT).and_then(|value| value.as_bool()) != Some(true) {
                return true;
            }
            let callee = edge
                .extra
                .get("callee")
                .and_then(|value| value.as_str())
                .map(normalize_id)
                .unwrap_or_default();
            let receiver_type = edge
                .extra
                .get("receiver_type")
                .and_then(|value| value.as_str())
                .map(|name| normalize_id(&simple_type_name(name)))
                .unwrap_or_default();
            if callee.is_empty() || receiver_type.is_empty() {
                return false;
            }
            let mut receiver_types = BTreeSet::from([receiver_type]);
            loop {
                let inherited = receiver_types
                    .iter()
                    .filter_map(|owner| parent_types.get(owner))
                    .flatten()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let old_len = receiver_types.len();
                receiver_types.extend(inherited);
                if receiver_types.len() == old_len {
                    break;
                }
            }
            let candidates = methods
                .get(&callee)
                .into_iter()
                .flatten()
                .filter(|id| id.as_str() != edge.true_source())
                .filter(|id| {
                    owner_by_method.get(*id).is_some_and(|owners| {
                        owners.len() == 1
                            && owners.iter().any(|owner| receiver_types.contains(owner))
                    })
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            let Some(target) = (candidates.len() == 1).then(|| {
                candidates
                    .into_iter()
                    .next()
                    .expect("one native call target")
            }) else {
                return false;
            };
            if !resolved_native.insert((edge.true_source().to_owned(), target.clone())) {
                return false;
            }
            edge.target = target.clone();
            edge.extra.insert("_tgt".into(), target.into());
            edge.confidence = if edge
                .extra
                .get(INFERRED_RECEIVER)
                .and_then(|value| value.as_bool())
                == Some(true)
            {
                Confidence::Inferred
            } else {
                Confidence::Extracted
            };
            for key in [
                CALL_FACT,
                INFERRED_RECEIVER,
                "unresolved_call",
                "member_call",
                "callee",
            ] {
                edge.extra.remove(key);
            }
            true
        });
    }
}

pub(crate) fn unresolved_target(name: &str) -> String {
    make_id(&["__graphoxide_native_call", name])
}
