//! Java-specific receiver facts and corpus-wide type resolution.
//!
//! Java member lookup is only sound when the receiver's declared type is
//! known.  This module deliberately leaves inherited fields, chained
//! expressions, conflicting lexical bindings, and ambiguous imported types
//! unresolved instead of falling back to a bare method-name match.

use graphoxide_core::{normalize_id, Extraction};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

pub(crate) const QUALIFIED_TYPE: &str = "java_qualified_type";
pub(crate) const PACKAGE: &str = "java_package";
pub(crate) const IMPORT_PATH: &str = "java_import_path";
pub(crate) const IMPORT_ALIAS: &str = "java_import_alias";
pub(crate) const STATIC_IMPORT: &str = "java_static_import";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReceiverFact {
    pub receiver: String,
    pub receiver_type: Option<String>,
}

fn text(node: Node<'_>, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").trim().to_owned()
}

fn simple_type(raw: &str) -> Option<String> {
    let mut token = String::new();
    for character in raw.chars() {
        if character.is_alphanumeric() || matches!(character, '_' | '.' | '$') {
            token.push(character);
        } else if !token.is_empty() {
            break;
        }
    }
    let name = token
        .rsplit('.')
        .next()
        .unwrap_or("")
        .trim_matches(|character: char| !character.is_alphanumeric() && character != '_');
    (!name.is_empty()).then(|| name.to_owned())
}

fn named_binding(declaration: Node<'_>, source: &[u8], name: &str) -> bool {
    if declaration
        .child_by_field_name("name")
        .is_some_and(|candidate| text(candidate, source) == name)
    {
        return true;
    }
    let mut pending = vec![declaration];
    while let Some(node) = pending.pop() {
        if node.kind() == "variable_declarator"
            && node
                .child_by_field_name("name")
                .is_some_and(|candidate| text(candidate, source) == name)
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

fn declared_type(declaration: Node<'_>, source: &[u8]) -> Option<String> {
    declaration
        .child_by_field_name("type")
        .and_then(|node| simple_type(&text(node, source)))
}

fn nearest_ancestor<'tree>(mut node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    while let Some(parent) = node.parent() {
        if kinds.contains(&parent.kind()) {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn class_field_type(call: Node<'_>, source: &[u8], name: &str) -> Option<String> {
    let declaration = nearest_ancestor(
        call,
        &[
            "class_declaration",
            "record_declaration",
            "enum_declaration",
        ],
    )?;
    let body = declaration.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let matches = body
        .named_children(&mut cursor)
        .filter(|member| member.kind() == "field_declaration")
        .filter(|member| named_binding(*member, source, name))
        .filter_map(|member| declared_type(member, source))
        .collect::<BTreeSet<_>>();
    (matches.len() == 1).then(|| matches.into_iter().next().unwrap())
}

fn direct_parameter_type(method: Node<'_>, source: &[u8], name: &str) -> Option<Option<String>> {
    let parameters = method.child_by_field_name("parameters")?;
    let mut pending = vec![parameters];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "formal_parameter" | "spread_parameter")
            && named_binding(node, source, name)
        {
            return Some(declared_type(node, source));
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    None
}

fn nested_within(declaration: Node<'_>, body: Node<'_>) -> bool {
    let mut current = declaration.parent();
    while let Some(parent) = current {
        if parent.id() == body.id() {
            return false;
        }
        if matches!(
            parent.kind(),
            "block"
                | "lambda_expression"
                | "switch_block"
                | "switch_expression"
                | "catch_clause"
                | "enhanced_for_statement"
                | "for_statement"
        ) {
            return true;
        }
        current = parent.parent();
    }
    true
}

fn lambda_binding(lambda: Node<'_>, source: &[u8], name: &str) -> Option<Option<String>> {
    let parameters = lambda.child_by_field_name("parameters")?;
    if parameters.kind() == "identifier" {
        return (text(parameters, source) == name).then_some(None);
    }
    let mut pending = vec![parameters];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "formal_parameter" | "spread_parameter")
            && named_binding(node, source, name)
        {
            return Some(declared_type(node, source));
        }
        if node.kind() == "identifier" && text(node, source) == name {
            return Some(None);
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    None
}

fn lexical_receiver_type(call: Node<'_>, source: &[u8], name: &str) -> Option<String> {
    let method = nearest_ancestor(call, &["method_declaration", "constructor_declaration"])?;
    if let Some(parameter_type) = direct_parameter_type(method, source, name) {
        return parameter_type;
    }

    let body = method.child_by_field_name("body")?;
    let field = class_field_type(call, source, name);
    let mut local_types = BTreeSet::new();
    let mut nested_local = false;
    let mut lambda_types = Vec::<Option<String>>::new();
    let mut pending = vec![body];
    while let Some(node) = pending.pop() {
        if node.id() != body.id()
            && matches!(
                node.kind(),
                "method_declaration"
                    | "constructor_declaration"
                    | "class_declaration"
                    | "record_declaration"
                    | "enum_declaration"
                    | "class_body"
            )
        {
            continue;
        }
        if node.kind() == "local_variable_declaration" && named_binding(node, source, name) {
            if let Some(kind) = declared_type(node, source) {
                local_types.insert(kind);
            }
            nested_local |= nested_within(node, body);
        }
        if node.kind() == "lambda_expression" {
            if let Some(binding) = lambda_binding(node, source, name) {
                lambda_types.push(binding);
            }
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }

    let mut candidates = if local_types.is_empty() {
        field.into_iter().collect::<BTreeSet<_>>()
    } else {
        if nested_local {
            if let Some(field) = field {
                local_types.insert(field);
            }
        }
        local_types
    };
    for binding in lambda_types {
        let binding = binding?;
        candidates.insert(binding);
    }
    (candidates.len() == 1).then(|| candidates.into_iter().next().unwrap())
}

/// Return a receiver fact only for direct Java receivers whose type can be
/// justified by syntax in the current declaration.
pub(crate) fn receiver_fact(
    call: Node<'_>,
    source: &[u8],
    current_type: Option<&str>,
) -> Option<ReceiverFact> {
    let object = call.child_by_field_name("object")?;
    let receiver = text(object, source);
    let receiver_type = match object.kind() {
        "this" => current_type.map(str::to_owned),
        "identifier" | "type_identifier" => {
            if receiver.chars().next().is_some_and(char::is_uppercase) {
                simple_type(&receiver)
            } else {
                lexical_receiver_type(call, source, &receiver)
            }
        }
        "field_access" => {
            let base = object.child_by_field_name("object")?;
            if base.kind() != "this" {
                return Some(ReceiverFact {
                    receiver,
                    receiver_type: None,
                });
            }
            let field = object
                .child_by_field_name("field")
                .or_else(|| object.child_by_field_name("name"))
                .map(|node| text(node, source))?;
            class_field_type(call, source, &field)
        }
        // Method chains, array access, parenthesized expressions, and other
        // computed receivers require return/data-flow typing that this pass
        // intentionally does not guess.
        _ => None,
    };
    Some(ReceiverFact {
        receiver,
        receiver_type,
    })
}

fn java_source(source: &str) -> bool {
    std::path::Path::new(source)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
}

fn simple_label(label: &str) -> String {
    normalize_id(label.trim_start_matches('.').trim_end_matches("()"))
}

fn matching_types<'a>(
    all: &'a [(String, String, String)],
    imported: Option<&BTreeSet<String>>,
    package: &str,
    label: &str,
) -> Vec<&'a (String, String, String)> {
    if let Some(paths) = imported {
        return all
            .iter()
            .filter(|(_, _, qualified)| paths.contains(qualified))
            .collect();
    }
    if !package.is_empty() {
        let qualified_name = format!("{package}.{label}");
        let same_package = all
            .iter()
            .filter(|(_, _, candidate)| candidate.as_str() == qualified_name.as_str())
            .collect::<Vec<_>>();
        if !same_package.is_empty() {
            return same_package;
        }
    }
    all.iter().collect()
}

/// Resolve Java type-reference/heritage phantoms using exact package imports.
/// A unique project definition is sufficient; collisions require an exact
/// non-static import or same-package match.  Anything else stays unresolved.
pub(crate) fn resolve_types(extractions: &mut [Extraction]) {
    let mut definitions = BTreeMap::<String, Vec<(String, String, String)>>::new();
    let mut method_names = BTreeMap::<String, String>::new();
    let mut java_node_ids = BTreeSet::<String>::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if !java_source(&node.source_file) {
                continue;
            }
            java_node_ids.insert(node.id.clone());
            if node.extra.get("type").and_then(Value::as_str) == Some("function") {
                method_names.insert(node.id.clone(), simple_label(&node.label));
                continue;
            }
            if node.extra.get("type").and_then(Value::as_str) != Some("class") {
                continue;
            }
            definitions
                .entry(simple_label(&node.label))
                .or_default()
                .push((
                    node.id.clone(),
                    node.source_file.clone(),
                    node.extra
                        .get(QUALIFIED_TYPE)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                ));
        }
    }
    for candidates in definitions.values_mut() {
        candidates.sort();
        candidates.dedup();
    }
    let mut method_owners = BTreeMap::<String, String>::new();
    for extraction in extractions.iter() {
        for edge in &extraction.edges {
            if edge.relation == "method"
                && java_node_ids.contains(edge.true_source())
                && java_node_ids.contains(edge.true_target())
            {
                method_owners.insert(edge.true_target().to_owned(), edge.true_source().to_owned());
            }
        }
    }

    for extraction in extractions.iter_mut() {
        let mut imports = BTreeMap::<String, BTreeSet<String>>::new();
        for edge in &extraction.edges {
            if edge.extra.get(STATIC_IMPORT).and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let Some(path) = edge.extra.get(IMPORT_PATH).and_then(Value::as_str) else {
                continue;
            };
            if path.ends_with(".*") {
                continue;
            }
            let alias = edge
                .extra
                .get(IMPORT_ALIAS)
                .and_then(Value::as_str)
                .unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path));
            imports
                .entry(normalize_id(alias))
                .or_default()
                .insert(path.to_owned());
        }
        let package = extraction
            .nodes
            .iter()
            .filter(|node| java_source(&node.source_file))
            .filter_map(|node| node.extra.get(PACKAGE).and_then(Value::as_str))
            .find(|package| !package.is_empty())
            .unwrap_or("")
            .to_owned();

        let mut remap = BTreeMap::<String, String>::new();
        for node in &extraction.nodes {
            if !node.source_file.is_empty() {
                continue;
            }
            let key = simple_label(&node.label);
            let Some(all) = definitions.get(&key) else {
                continue;
            };
            let candidates = matching_types(all, imports.get(&key), &package, &node.label);
            if candidates.len() == 1 {
                remap.insert(node.id.clone(), candidates[0].0.clone());
            }
        }
        if !remap.is_empty() {
            for edge in &mut extraction.edges {
                if let Some(target) = remap.get(edge.true_target()) {
                    edge.target = target.clone();
                    edge.extra.insert("_tgt".into(), target.clone().into());
                }
            }
            extraction
                .nodes
                .retain(|node| !remap.contains_key(&node.id));
        }

        for edge in &mut extraction.edges {
            if !java_source(&edge.source_file)
                || edge.extra.get("unresolved_call").and_then(Value::as_bool) != Some(true)
                || edge.extra.get("member_call").and_then(Value::as_bool) != Some(true)
            {
                continue;
            }
            let receiver_name = edge
                .extra
                .get("receiver_type")
                .and_then(Value::as_str)
                .unwrap_or("");
            let receiver_label = simple_type(receiver_name).unwrap_or_default();
            let receiver = simple_label(&receiver_label);
            let callee = edge
                .extra
                .get("callee")
                .and_then(Value::as_str)
                .map(normalize_id)
                .unwrap_or_default();
            let Some(types) = definitions.get(&receiver) else {
                continue;
            };
            let owners = matching_types(types, imports.get(&receiver), &package, &receiver_label)
                .into_iter()
                .map(|(id, _, _)| id.as_str())
                .collect::<BTreeSet<_>>();
            if owners.is_empty() {
                continue;
            }
            let candidates = method_names
                .iter()
                .filter(|(_, name)| name.as_str() == callee)
                .filter(|(id, _)| {
                    method_owners
                        .get(*id)
                        .is_some_and(|owner| owners.contains(owner.as_str()))
                })
                .map(|(id, _)| id)
                .collect::<BTreeSet<_>>();
            if candidates.len() != 1 {
                continue;
            }
            let target = (*candidates
                .into_iter()
                .next()
                .expect("one Java method target"))
            .clone();
            edge.target = target.clone();
            edge.extra.insert("_tgt".into(), target.into());
            for key in ["unresolved_call", "member_call", "callee"] {
                edge.extra.remove(key);
            }
        }
    }
}
