//! Namespace-aware C# declaration and type resolution.
//!
//! C# names are not project-global labels: the same short name can denote
//! different declarations in different namespaces, and `using` directives
//! are lexically scoped.  The generic resolver intentionally cannot guess at
//! those rules, so the tree-sitter walker records the necessary facts and this
//! corpus pass arbitrates them after all files have been extracted.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node as TsNode;

pub(crate) const MANAGED_NODE: &str = "_csharp_resolution_managed";
pub(crate) const TYPE_REF_EDGE: &str = "_csharp_type_ref";
pub(crate) const IMPORT_EDGE: &str = "_csharp_import";
pub(crate) const NAMESPACE_NODE: &str = "_csharp_namespace";
pub(crate) const CALL_EDGE: &str = "_csharp_call";

#[derive(Debug, Clone, Default)]
pub(crate) struct NamespaceContext {
    pub namespace: String,
    pub scope_chain: Vec<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct UsingDirective {
    pub kind: &'static str,
    pub target_fqn: String,
    pub alias: Option<String>,
    pub global: bool,
    pub scope_chain: Vec<u64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReceiverFact {
    pub receiver: String,
    pub receiver_type: Option<String>,
}

fn node_text<'a>(node: TsNode<'a>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("").trim()
}

fn namespace_name(node: TsNode<'_>, source: &[u8]) -> String {
    node.child_by_field_name("name")
        .map(|name| node_text(name, source).to_owned())
        .unwrap_or_else(|| {
            let raw = node_text(node, source);
            raw.strip_prefix("namespace")
                .unwrap_or(raw)
                .trim_start()
                .split(['{', ';'])
                .next()
                .unwrap_or("")
                .trim()
                .to_owned()
        })
}

/// Return the fully-qualified namespace and lexical namespace-block identity.
pub(crate) fn namespace_context(node: TsNode<'_>, source: &[u8]) -> NamespaceContext {
    let mut declarations = Vec::new();
    let mut current = Some(node);
    while let Some(value) = current {
        if matches!(
            value.kind(),
            "namespace_declaration" | "file_scoped_namespace_declaration"
        ) {
            declarations.push(value);
        }
        current = value.parent();
    }
    declarations.reverse();

    let mut namespace = String::new();
    let mut scope_chain = Vec::new();
    for declaration in declarations {
        let name = namespace_name(declaration, source);
        if name.is_empty() {
            continue;
        }
        if namespace.is_empty() || name == namespace || name.starts_with(&format!("{namespace}.")) {
            namespace = name;
        } else {
            namespace.push('.');
            namespace.push_str(&name);
        }
        scope_chain.push(declaration.start_byte() as u64);
    }

    // Older tree-sitter-c-sharp releases represented a file-scoped namespace
    // as a sibling rather than an ancestor.  Retain the semantic context in
    // that shape as well, but only for a declaration before this node.
    if namespace.is_empty() {
        let prefix =
            std::str::from_utf8(&source[..node.start_byte().min(source.len())]).unwrap_or("");
        let pattern = Regex::new(r"(?m)^\s*namespace\s+([A-Za-z_][A-Za-z0-9_.]*)\s*;")
            .expect("valid C# file-scoped namespace regex");
        if let Some(capture) = pattern.captures_iter(prefix).last() {
            namespace = capture[1].to_owned();
            scope_chain.push(capture.get(0).map_or(0, |value| value.start()) as u64);
        }
    }

    NamespaceContext {
        namespace,
        scope_chain,
    }
}

pub(crate) fn namespace_id(namespace: &str) -> String {
    format!("csharp_namespace:{namespace}")
}

pub(crate) fn type_parameters(node: TsNode<'_>, source: &[u8], name: &str) -> Vec<String> {
    let raw = node_text(node, source);
    let Some(name_offset) = raw.find(name) else {
        return Vec::new();
    };
    let tail = raw[name_offset + name.len()..].trim_start();
    if !tail.starts_with('<') {
        return Vec::new();
    }
    let Some(close) = matching_angle(tail) else {
        return Vec::new();
    };
    split_top_level(&tail[1..close], ',')
        .into_iter()
        .filter_map(|parameter| {
            Regex::new(r"[A-Za-z_][A-Za-z0-9_]*")
                .expect("valid C# identifier regex")
                .find_iter(parameter)
                .map(|capture| capture.as_str())
                .filter(|word| !matches!(*word, "in" | "out"))
                .last()
                .map(str::to_owned)
        })
        .collect()
}

pub(crate) fn is_partial_declaration(node: TsNode<'_>, source: &[u8]) -> bool {
    let header = node_text(node, source)
        .split(['{', ';'])
        .next()
        .unwrap_or("");
    Regex::new(r"\bpartial\b")
        .expect("valid C# partial modifier regex")
        .is_match(header)
}

fn matching_angle(value: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, character) in value.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level(value: &str, separator: char) -> Vec<&str> {
    let mut values = Vec::new();
    let mut start = 0;
    let mut angle = 0usize;
    let mut square = 0usize;
    let mut round = 0usize;
    for (offset, character) in value.char_indices() {
        match character {
            '<' => angle += 1,
            '>' => angle = angle.saturating_sub(1),
            '[' => square += 1,
            ']' => square = square.saturating_sub(1),
            '(' => round += 1,
            ')' => round = round.saturating_sub(1),
            _ if character == separator && angle == 0 && square == 0 && round == 0 => {
                values.push(value[start..offset].trim());
                start = offset + character.len_utf8();
            }
            _ => {}
        }
    }
    values.push(value[start..].trim());
    values
}

fn clean_type_token(value: &str) -> String {
    let mut token = value
        .split('<')
        .next()
        .unwrap_or(value)
        .trim()
        .trim_start_matches("global::")
        .replace(char::is_whitespace, "");
    while token.ends_with(['?', '*', '&']) {
        token.pop();
    }
    token
        .trim_matches(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '.' && character != ':'
        })
        .to_owned()
}

/// Parse a C# type expression, retaining qualification for the primary type.
pub(crate) fn type_tokens(value: &str) -> Vec<String> {
    let identifier =
        Regex::new(r"[A-Za-z_][A-Za-z0-9_]*(?:(?:\s*\.\s*|\s*::\s*)[A-Za-z_][A-Za-z0-9_]*)*")
            .expect("valid C# type-token regex");
    let mut tokens = Vec::new();
    for capture in identifier.find_iter(value) {
        let token = clean_type_token(capture.as_str());
        if token.is_empty()
            || matches!(
                token.as_str(),
                "class"
                    | "struct"
                    | "record"
                    | "interface"
                    | "enum"
                    | "public"
                    | "private"
                    | "protected"
                    | "internal"
                    | "readonly"
                    | "ref"
                    | "out"
                    | "in"
                    | "params"
                    | "where"
                    | "new"
                    | "global"
            )
            || tokens.contains(&token)
        {
            continue;
        }
        tokens.push(token);
    }
    tokens
}

pub(crate) fn base_type_groups(value: &str) -> Vec<Vec<String>> {
    let value = value.trim().trim_start_matches(':').trim();
    split_top_level(value, ',')
        .into_iter()
        .map(type_tokens)
        .filter(|tokens| !tokens.is_empty())
        .collect()
}

pub(crate) fn simple_type_name(value: &str) -> String {
    clean_type_token(value)
        .rsplit(['.', ':'])
        .find(|part| !part.is_empty())
        .unwrap_or("")
        .to_owned()
}

pub(crate) fn parse_using(node: TsNode<'_>, source: &[u8]) -> Option<UsingDirective> {
    let mut raw = node_text(node, source).trim().trim_end_matches(';').trim();
    let global = raw.starts_with("global ");
    raw = raw.strip_prefix("global ").unwrap_or(raw).trim_start();
    raw = raw.strip_prefix("using ")?.trim_start();
    let static_using = raw.starts_with("static ");
    raw = raw.strip_prefix("static ").unwrap_or(raw).trim_start();

    let (kind, alias, target) = if static_using {
        ("static", None, raw)
    } else if let Some((alias, target)) = raw.split_once('=') {
        ("alias", Some(alias.trim().to_owned()), target.trim())
    } else {
        ("namespace", None, raw)
    };
    let target_fqn = clean_type_token(target);
    if target_fqn.is_empty() || alias.as_deref() == Some("") {
        return None;
    }
    Some(UsingDirective {
        kind,
        target_fqn,
        alias,
        global,
        scope_chain: namespace_context(node, source).scope_chain,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    Typed(String),
    Unknown,
    Poisoned,
}

fn bind(table: &mut BTreeMap<String, Binding>, name: String, type_name: Option<String>) {
    if name.is_empty() {
        return;
    }
    let incoming = type_name.map_or(Binding::Unknown, Binding::Typed);
    match table.get(&name) {
        None => {
            table.insert(name, incoming);
        }
        Some(existing) if existing == &incoming => {}
        Some(_) => {
            table.insert(name, Binding::Poisoned);
        }
    }
}

fn semantic_type(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    let token = type_tokens(node_text(node, source)).into_iter().next()?;
    (!matches!(token.as_str(), "var" | "dynamic")).then_some(token)
}

fn variable_bindings(
    declaration: TsNode<'_>,
    source: &[u8],
    table: &mut BTreeMap<String, Binding>,
) {
    let declared = declaration
        .child_by_field_name("type")
        .and_then(|node| semantic_type(node, source));
    let declared_is_var = declaration
        .child_by_field_name("type")
        .is_some_and(|node| node_text(node, source) == "var");
    let mut cursor = declaration.walk();
    for declarator in declaration
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "variable_declarator")
    {
        let name = declarator
            .child_by_field_name("name")
            .map(|node| node_text(node, source).to_owned())
            .unwrap_or_default();
        let inferred = if declared_is_var {
            let mut values = Vec::new();
            collect_nodes(declarator, "object_creation_expression", &mut values);
            values
                .first()
                .and_then(|creation| creation.child_by_field_name("type"))
                .and_then(|node| semantic_type(node, source))
        } else {
            declared.clone()
        };
        bind(table, name, inferred);
    }
}

fn collect_nodes<'tree>(node: TsNode<'tree>, kind: &str, output: &mut Vec<TsNode<'tree>>) {
    if node.kind() == kind {
        output.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_nodes(child, kind, output);
    }
}

fn collect_class_fields(declaration: TsNode<'_>, source: &[u8]) -> BTreeMap<String, Binding> {
    let mut table = BTreeMap::new();
    let Some(body) = declaration.child_by_field_name("body") else {
        return table;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        match member.kind() {
            "field_declaration" => {
                let mut declarations = Vec::new();
                collect_nodes(member, "variable_declaration", &mut declarations);
                for declaration in declarations {
                    variable_bindings(declaration, source, &mut table);
                }
            }
            "property_declaration" => {
                let name = member
                    .child_by_field_name("name")
                    .map(|node| node_text(node, source).to_owned())
                    .unwrap_or_default();
                let type_name = member
                    .child_by_field_name("type")
                    .and_then(|node| semantic_type(node, source));
                bind(&mut table, name, type_name);
            }
            _ => {}
        }
    }
    table
}

fn enclosing<'tree>(node: TsNode<'tree>, kinds: &[&str]) -> Option<TsNode<'tree>> {
    let mut current = node.parent();
    while let Some(value) = current {
        if kinds.contains(&value.kind()) {
            return Some(value);
        }
        current = value.parent();
    }
    None
}

fn method_bindings(
    method: TsNode<'_>,
    source: &[u8],
    mut table: BTreeMap<String, Binding>,
) -> BTreeMap<String, Binding> {
    if let Some(parameters) = method.child_by_field_name("parameters") {
        let mut values = Vec::new();
        collect_nodes(parameters, "parameter", &mut values);
        for parameter in values {
            let name = parameter
                .child_by_field_name("name")
                .map(|node| node_text(node, source).to_owned())
                .unwrap_or_default();
            let type_name = parameter
                .child_by_field_name("type")
                .and_then(|node| semantic_type(node, source));
            bind(&mut table, name, type_name);
        }
    }
    if let Some(body) = method.child_by_field_name("body") {
        let mut declarations = Vec::new();
        collect_nodes(body, "variable_declaration", &mut declarations);
        for declaration in declarations {
            variable_bindings(declaration, source, &mut table);
        }
        for kind in [
            "declaration_expression",
            "declaration_pattern",
            "recursive_pattern",
        ] {
            let mut patterns = Vec::new();
            collect_nodes(body, kind, &mut patterns);
            for pattern in patterns {
                let name = pattern
                    .child_by_field_name("name")
                    .map(|node| node_text(node, source).to_owned())
                    .unwrap_or_default();
                let type_name = pattern
                    .child_by_field_name("type")
                    .and_then(|node| semantic_type(node, source));
                bind(&mut table, name, type_name);
            }
        }
    }
    table
}

/// Recover the receiver spelling and conservative static type for a C# member
/// invocation. Bindings are scoped to the enclosing method; any conflicting or
/// untyped redeclaration poisons that name rather than selecting a false edge.
pub(crate) fn receiver_fact(callee: TsNode<'_>, source: &[u8]) -> ReceiverFact {
    let Some(expression) = callee.child_by_field_name("expression") else {
        return ReceiverFact::default();
    };
    let receiver = node_text(expression, source).to_owned();
    let class = enclosing(
        callee,
        &[
            "class_declaration",
            "struct_declaration",
            "record_declaration",
            "interface_declaration",
        ],
    );
    let method = enclosing(callee, &["method_declaration"]);
    let fields = class
        .map(|declaration| collect_class_fields(declaration, source))
        .unwrap_or_default();
    let bindings = method
        .map(|method| method_bindings(method, source, fields.clone()))
        .unwrap_or_else(|| fields.clone());

    let receiver_type = if receiver == "this" {
        Some("__csharp_this".into())
    } else if receiver == "base" {
        Some("__csharp_base".into())
    } else if let Some(field) = receiver.strip_prefix("this.") {
        match fields.get(field) {
            Some(Binding::Typed(type_name)) => Some(type_name.clone()),
            _ => None,
        }
    } else if expression.kind() == "object_creation_expression" {
        expression
            .child_by_field_name("type")
            .and_then(|node| semantic_type(node, source))
    } else if expression.kind() == "identifier" {
        match bindings.get(&receiver) {
            Some(Binding::Typed(type_name)) => Some(type_name.clone()),
            Some(Binding::Unknown | Binding::Poisoned) => None,
            None if receiver.chars().next().is_some_and(char::is_uppercase) => {
                Some(receiver.clone())
            }
            None => None,
        }
    } else if matches!(expression.kind(), "qualified_name" | "alias_qualified_name") {
        Some(receiver.clone())
    } else {
        None
    };
    ReceiverFact {
        receiver,
        receiver_type,
    }
}

fn metadata_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn metadata_string<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    metadata_object(node.extra.get("metadata"))?
        .get(key)?
        .as_str()
}

fn metadata_bool(node: &Node, key: &str) -> bool {
    metadata_object(node.extra.get("metadata"))
        .and_then(|metadata| metadata.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn metadata_scope(value: Option<&Value>) -> Vec<u64> {
    metadata_object(value)
        .and_then(|metadata| metadata.get("scope_chain"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .collect()
}

fn metadata_parameters(node: &Node) -> BTreeSet<String> {
    metadata_object(node.extra.get("metadata"))
        .and_then(|metadata| metadata.get("type_parameters"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

#[derive(Debug, Clone)]
struct TypeDef {
    id: String,
    fqn: String,
    namespace: String,
    declaration_kind: String,
    nested: bool,
    scope_chain: Vec<u64>,
    type_parameters: BTreeSet<String>,
}

fn merge_partial_classes(extractions: &mut [Extraction]) {
    let mut groups = BTreeMap::<String, Vec<(String, bool, bool)>>::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            let Some(fqn) = metadata_string(node, "fqn") else {
                continue;
            };
            if metadata_string(node, "declaration_kind") != Some("class") {
                continue;
            }
            groups.entry(fqn.to_owned()).or_default().push((
                node.id.clone(),
                metadata_bool(node, "partial"),
                metadata_bool(node, "is_nested_type"),
            ));
        }
    }
    let mut remap = BTreeMap::<String, String>::new();
    for members in groups.into_values() {
        if members.len() < 2
            || members
                .iter()
                .any(|(_, partial, nested)| !partial || *nested)
        {
            continue;
        }
        let canonical = members
            .iter()
            .map(|(id, _, _)| id)
            .min()
            .expect("partial group is non-empty")
            .clone();
        for (id, _, _) in members {
            if id != canonical {
                remap.insert(id, canonical.clone());
            }
        }
    }
    if remap.is_empty() {
        return;
    }
    for extraction in extractions {
        extraction
            .nodes
            .retain(|node| !remap.contains_key(&node.id));
        for edge in &mut extraction.edges {
            if let Some(target) = remap.get(edge.true_source()) {
                edge.source = target.clone();
                edge.extra.insert("_src".into(), target.clone().into());
            }
            if let Some(target) = remap.get(edge.true_target()) {
                edge.target = target.clone();
                edge.extra.insert("_tgt".into(), target.clone().into());
            }
        }
    }
}

#[derive(Debug, Clone)]
struct UsingFact {
    kind: String,
    target_fqn: String,
    alias: Option<String>,
    scope_chain: Vec<u64>,
}

fn using_facts(extraction: &Extraction) -> Vec<UsingFact> {
    extraction
        .edges
        .iter()
        .filter(|edge| edge.extra.get(IMPORT_EDGE).and_then(Value::as_bool) == Some(true))
        .filter_map(|edge| {
            let metadata = edge_metadata(edge)?;
            Some(UsingFact {
                kind: metadata.get("using_kind")?.as_str()?.to_owned(),
                target_fqn: metadata.get("target_fqn")?.as_str()?.to_owned(),
                alias: metadata
                    .get("alias")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                scope_chain: metadata_scope(edge.extra.get("metadata")),
            })
        })
        .collect()
}

fn scope_visible(binding: &[u64], owner: &[u64]) -> bool {
    binding.len() <= owner.len() && binding.iter().zip(owner).all(|(left, right)| left == right)
}

fn unique(values: impl IntoIterator<Item = String>) -> Option<String> {
    let values: BTreeSet<_> = values.into_iter().collect();
    (values.len() == 1).then(|| values.into_iter().next().expect("one candidate"))
}

fn exact_type(types_by_fqn: &BTreeMap<String, Vec<TypeDef>>, fqn: &str) -> Option<String> {
    unique(
        types_by_fqn
            .get(fqn)
            .into_iter()
            .flatten()
            .map(|definition| definition.id.clone()),
    )
}

fn visible_aliases<'a>(usings: &'a [UsingFact], scope: &[u64], alias: &str) -> Vec<&'a UsingFact> {
    let visible: Vec<_> = usings
        .iter()
        .filter(|using| using.kind == "alias")
        .filter(|using| using.alias.as_deref() == Some(alias))
        .filter(|using| scope_visible(&using.scope_chain, scope))
        .collect();
    let strongest = visible
        .iter()
        .map(|using| using.scope_chain.len())
        .max()
        .unwrap_or(0);
    visible
        .into_iter()
        .filter(|using| using.scope_chain.len() == strongest)
        .collect()
}

fn resolve_token(
    token: &str,
    owner: &TypeDef,
    usings: &[UsingFact],
    types_by_fqn: &BTreeMap<String, Vec<TypeDef>>,
) -> Option<String> {
    let token = clean_type_token(token);
    if token.is_empty() {
        return None;
    }
    let qualified = token.contains('.') || token.contains("::");
    if qualified {
        let token = token.replace("::", ".");
        let mut parts = token.split('.');
        let qualifier = parts.next().unwrap_or("");
        let suffix = parts.collect::<Vec<_>>().join(".");
        let aliases = visible_aliases(usings, &owner.scope_chain, qualifier);
        if !aliases.is_empty() {
            return unique(aliases.into_iter().filter_map(|alias| {
                let fqn = if suffix.is_empty() {
                    alias.target_fqn.clone()
                } else {
                    format!("{}.{}", alias.target_fqn, suffix)
                };
                exact_type(types_by_fqn, &fqn)
            }));
        }

        let mut candidates = Vec::new();
        if !owner.namespace.is_empty()
            && let Some(candidate) =
                exact_type(types_by_fqn, &format!("{}.{}", owner.namespace, token))
        {
            candidates.push(candidate);
        }
        if let Some(candidate) = exact_type(types_by_fqn, &token) {
            candidates.push(candidate);
        }
        return unique(candidates);
    }

    let aliases = visible_aliases(usings, &owner.scope_chain, &token);
    if !aliases.is_empty() {
        return unique(
            aliases
                .into_iter()
                .filter_map(|alias| exact_type(types_by_fqn, &alias.target_fqn)),
        );
    }

    let same_namespace = if owner.namespace.is_empty() {
        token.clone()
    } else {
        format!("{}.{}", owner.namespace, token)
    };
    if let Some(candidate) = exact_type(types_by_fqn, &same_namespace) {
        return Some(candidate);
    }

    let candidates = usings
        .iter()
        .filter(|using| using.kind == "namespace")
        .filter(|using| scope_visible(&using.scope_chain, &owner.scope_chain))
        .flat_map(|using| {
            types_by_fqn
                .get(&format!("{}.{}", using.target_fqn, token))
                .into_iter()
                .flatten()
        })
        .filter(|definition| !definition.nested)
        .map(|definition| definition.id.clone());
    unique(candidates)
}

fn edge_metadata(edge: &Edge) -> Option<&Map<String, Value>> {
    edge.extra.get("metadata").and_then(Value::as_object)
}

fn set_edge_target(edge: &mut Edge, target: String) {
    edge.target = target.clone();
    edge.extra.insert("_tgt".into(), target.into());
}

fn unresolved_stub(extraction: &mut Extraction, token: &str, line: Option<String>) -> String {
    let source = extraction
        .nodes
        .iter()
        .find(|node| node.extra.get("type").and_then(Value::as_str) == Some("file"))
        .map(|node| node.source_file.as_str())
        .unwrap_or("");
    let id = make_id(&["__csharp_ref", source, token]);
    if !extraction.nodes.iter().any(|node| node.id == id) {
        crate::resolution::push_resolved_node(
            &mut extraction.nodes,
            Node {
                id: id.clone(),
                label: simple_type_name(token),
                file_type: "code".into(),
                source_file: String::new(),
                source_location: line,
                community: None,
                extra: BTreeMap::from([
                    ("_origin".into(), "ast".into()),
                    (MANAGED_NODE.into(), true.into()),
                ]),
            },
        );
    }
    id
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MethodLookup {
    Found(String),
    Missing,
    Poisoned,
}

fn lookup_method(
    type_id: &str,
    name: &str,
    methods: &BTreeMap<(String, String), Vec<String>>,
    bases: &BTreeMap<String, Vec<String>>,
    definitions: &BTreeMap<String, TypeDef>,
    visited: &mut BTreeSet<String>,
) -> MethodLookup {
    if !visited.insert(type_id.to_owned()) {
        return MethodLookup::Poisoned;
    }
    let direct = methods
        .get(&(type_id.to_owned(), name.to_owned()))
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if direct.len() == 1 {
        return MethodLookup::Found(direct[0].clone());
    }
    if direct.len() > 1 {
        return MethodLookup::Poisoned;
    }
    let parents = bases.get(type_id).map(Vec::as_slice).unwrap_or(&[]);
    if parents.is_empty() {
        return MethodLookup::Missing;
    }
    let mut found = BTreeSet::new();
    for parent in parents {
        if !definitions.contains_key(parent) {
            return MethodLookup::Poisoned;
        }
        match lookup_method(parent, name, methods, bases, definitions, visited) {
            MethodLookup::Found(method) => {
                found.insert(method);
            }
            MethodLookup::Missing => {}
            MethodLookup::Poisoned => return MethodLookup::Poisoned,
        }
    }
    if found.len() == 1 {
        MethodLookup::Found(found.into_iter().next().expect("one inherited method"))
    } else if found.is_empty() {
        MethodLookup::Missing
    } else {
        MethodLookup::Poisoned
    }
}

fn resolve_calls(
    extractions: &mut [Extraction],
    definitions: &BTreeMap<String, TypeDef>,
    types_by_fqn: &BTreeMap<String, Vec<TypeDef>>,
) {
    let labels: BTreeMap<String, String> = extractions
        .iter()
        .flat_map(|extraction| extraction.nodes.iter())
        .map(|node| (node.id.clone(), node.label.clone()))
        .collect();
    let mut method_owners = BTreeMap::<String, String>::new();
    let mut methods = BTreeMap::<(String, String), Vec<String>>::new();
    for edge in extractions
        .iter()
        .flat_map(|extraction| extraction.edges.iter())
        .filter(|edge| edge.relation == "method")
    {
        let method = edge.true_target().to_owned();
        let owner = edge.true_source().to_owned();
        let Some(label) = labels.get(&method) else {
            continue;
        };
        let name = label
            .trim_start_matches('.')
            .trim_end_matches("()")
            .to_owned();
        method_owners.insert(method.clone(), owner.clone());
        methods.entry((owner, name)).or_default().push(method);
    }
    for candidates in methods.values_mut() {
        candidates.sort();
        candidates.dedup();
    }
    let mut bases = BTreeMap::<String, Vec<String>>::new();
    for edge in extractions
        .iter()
        .flat_map(|extraction| extraction.edges.iter())
        .filter(|edge| matches!(edge.relation.as_str(), "inherits" | "implements"))
        .filter(|edge| definitions.contains_key(edge.true_source()))
    {
        bases
            .entry(edge.true_source().to_owned())
            .or_default()
            .push(edge.true_target().to_owned());
    }
    for parents in bases.values_mut() {
        parents.sort();
        parents.dedup();
    }

    for extraction in extractions.iter_mut() {
        let usings = using_facts(extraction);
        let mut remove = BTreeSet::new();
        for (index, edge) in extraction.edges.iter_mut().enumerate() {
            if edge.extra.get(CALL_EDGE).and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let member_call = edge
                .extra
                .get("csharp_member_call")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let Some(callee) = edge
                .extra
                .get("csharp_callee")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let Some(owner_id) = method_owners.get(edge.true_source()) else {
                if member_call {
                    remove.insert(index);
                }
                continue;
            };
            let Some(owner) = definitions.get(owner_id) else {
                if member_call {
                    remove.insert(index);
                }
                continue;
            };

            let lookup = if member_call {
                let receiver_type = edge.extra.get("receiver_type").and_then(Value::as_str);
                let Some(receiver_type) = receiver_type else {
                    remove.insert(index);
                    continue;
                };
                if receiver_type == "__csharp_base" {
                    let parents = bases.get(owner_id).map(Vec::as_slice).unwrap_or(&[]);
                    let mut found = BTreeSet::new();
                    let mut poisoned = parents.is_empty();
                    for parent in parents {
                        if !definitions.contains_key(parent) {
                            poisoned = true;
                            break;
                        }
                        match lookup_method(
                            parent,
                            &callee,
                            &methods,
                            &bases,
                            definitions,
                            &mut BTreeSet::new(),
                        ) {
                            MethodLookup::Found(method) => {
                                found.insert(method);
                            }
                            MethodLookup::Missing => {}
                            MethodLookup::Poisoned => {
                                poisoned = true;
                                break;
                            }
                        }
                    }
                    if poisoned || found.len() > 1 {
                        MethodLookup::Poisoned
                    } else if let Some(method) = found.into_iter().next() {
                        MethodLookup::Found(method)
                    } else {
                        MethodLookup::Missing
                    }
                } else {
                    let receiver_id = if receiver_type == "__csharp_this" {
                        Some(owner_id.clone())
                    } else {
                        resolve_token(receiver_type, owner, &usings, types_by_fqn)
                    };
                    receiver_id.map_or(MethodLookup::Poisoned, |receiver_id| {
                        lookup_method(
                            &receiver_id,
                            &callee,
                            &methods,
                            &bases,
                            definitions,
                            &mut BTreeSet::new(),
                        )
                    })
                }
            } else {
                lookup_method(
                    owner_id,
                    &callee,
                    &methods,
                    &bases,
                    definitions,
                    &mut BTreeSet::new(),
                )
            };

            if let MethodLookup::Found(target) = lookup {
                set_edge_target(edge, target);
                edge.confidence = if member_call {
                    Confidence::Inferred
                } else {
                    Confidence::Extracted
                };
                edge.extra.insert(
                    "confidence_score".into(),
                    edge.confidence.default_score().into(),
                );
                edge.extra.remove("unresolved_call");
                edge.extra.remove("callee");
                edge.extra.remove("member_call");
                edge.extra.insert(
                    "metadata".into(),
                    serde_json::json!({
                        "resolver": if member_call {
                            "csharp_receiver_type"
                        } else {
                            "csharp_owner_type"
                        }
                    }),
                );
            } else if member_call {
                remove.insert(index);
            }
        }
        if !remove.is_empty() {
            extraction.edges = extraction
                .edges
                .drain(..)
                .enumerate()
                .filter_map(|(index, edge)| (!remove.contains(&index)).then_some(edge))
                .collect();
        }
    }
}

/// Resolve marked C# type/import facts using namespace and lexical-using rules.
pub(crate) fn resolve_types(extractions: &mut [Extraction]) {
    merge_partial_classes(extractions);
    let mut definitions = Vec::<TypeDef>::new();
    let mut owner_by_member = BTreeMap::<String, String>::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if !node.source_file.to_ascii_lowercase().ends_with(".cs") {
                continue;
            }
            let Some(fqn) = metadata_string(node, "fqn") else {
                continue;
            };
            definitions.push(TypeDef {
                id: node.id.clone(),
                fqn: fqn.to_owned(),
                namespace: metadata_string(node, "namespace").unwrap_or("").to_owned(),
                declaration_kind: metadata_string(node, "declaration_kind")
                    .unwrap_or("class")
                    .to_owned(),
                nested: metadata_bool(node, "is_nested_type"),
                scope_chain: metadata_scope(node.extra.get("metadata")),
                type_parameters: metadata_parameters(node),
            });
        }
        for edge in &extraction.edges {
            if matches!(edge.relation.as_str(), "method" | "contains") {
                owner_by_member
                    .insert(edge.true_target().to_owned(), edge.true_source().to_owned());
            }
        }
    }
    let definitions_by_id: BTreeMap<_, _> = definitions
        .iter()
        .map(|definition| (definition.id.clone(), definition.clone()))
        .collect();
    let mut types_by_fqn = BTreeMap::<String, Vec<TypeDef>>::new();
    for definition in definitions {
        types_by_fqn
            .entry(definition.fqn.clone())
            .or_default()
            .push(definition);
    }

    let internal_namespaces: BTreeSet<String> = types_by_fqn
        .values()
        .flatten()
        .filter(|definition| !definition.namespace.is_empty())
        .map(|definition| definition.namespace.clone())
        .collect();

    for extraction in extractions.iter_mut() {
        let usings = using_facts(extraction);

        for edge in extraction
            .edges
            .iter_mut()
            .filter(|edge| edge.extra.get(IMPORT_EDGE).and_then(Value::as_bool) == Some(true))
        {
            let Some(metadata) = edge_metadata(edge) else {
                continue;
            };
            let kind = metadata
                .get("using_kind")
                .and_then(Value::as_str)
                .unwrap_or("");
            let target_fqn = metadata
                .get("target_fqn")
                .and_then(Value::as_str)
                .unwrap_or("");
            let resolved = match kind {
                "namespace" if internal_namespaces.contains(target_fqn) => {
                    Some(namespace_id(target_fqn))
                }
                "alias" => exact_type(&types_by_fqn, target_fqn),
                _ => None,
            };
            set_edge_target(
                edge,
                resolved.unwrap_or_else(|| {
                    make_id(&[
                        "__csharp_import",
                        kind,
                        target_fqn,
                        metadata.get("alias").and_then(Value::as_str).unwrap_or(""),
                    ])
                }),
            );
        }

        let mut pending = Vec::new();
        for (index, edge) in extraction.edges.iter().enumerate() {
            if edge.extra.get(TYPE_REF_EDGE).and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let Some(token) = edge_metadata(edge)
                .and_then(|metadata| metadata.get("ref_token"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let mut owner_id = edge.true_source();
            while !definitions_by_id.contains_key(owner_id) {
                let Some(parent) = owner_by_member.get(owner_id) else {
                    break;
                };
                owner_id = parent;
            }
            let owner = definitions_by_id.get(owner_id);
            let type_parameter = owner.is_some_and(|definition| {
                definition
                    .type_parameters
                    .contains(&simple_type_name(token))
            });
            let target =
                owner.and_then(|owner| resolve_token(token, owner, &usings, &types_by_fqn));
            pending.push((
                index,
                token.to_owned(),
                owner.cloned(),
                type_parameter,
                target,
            ));
        }

        let mut remove_edges = BTreeSet::new();
        for (index, token, owner, type_parameter, target) in pending {
            if type_parameter {
                remove_edges.insert(index);
                continue;
            }
            let line = extraction.edges[index]
                .extra
                .get("source_location")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let target = target.unwrap_or_else(|| unresolved_stub(extraction, &token, line));
            let target_kind = definitions_by_id
                .get(&target)
                .map(|definition| definition.declaration_kind.as_str());
            let edge = &mut extraction.edges[index];
            set_edge_target(edge, target);
            if matches!(edge.relation.as_str(), "inherits" | "implements") {
                edge.relation = if target_kind == Some("interface")
                    && owner
                        .as_ref()
                        .is_some_and(|source| source.declaration_kind != "interface")
                {
                    "implements".into()
                } else {
                    "inherits".into()
                };
            }
        }
        if !remove_edges.is_empty() {
            extraction.edges = extraction
                .edges
                .drain(..)
                .enumerate()
                .filter_map(|(index, edge)| (!remove_edges.contains(&index)).then_some(edge))
                .collect();
        }
    }

    resolve_calls(extractions, &definitions_by_id, &types_by_fqn);

    // Namespace identities are corpus-global. Keep one material node while
    // allowing containment/import edges from every contributing file to point
    // to that canonical identity.
    let mut seen_namespaces = BTreeSet::new();
    for extraction in extractions.iter_mut() {
        extraction.nodes.retain(|node| {
            node.extra.get(NAMESPACE_NODE).and_then(Value::as_bool) != Some(true)
                || seen_namespaces.insert(node.id.clone())
        });
    }

    let referenced: BTreeSet<String> = extractions
        .iter()
        .flat_map(|extraction| extraction.edges.iter())
        .map(|edge| edge.true_target().to_owned())
        .collect();
    for extraction in extractions.iter_mut() {
        extraction.nodes.retain(|node| {
            node.extra.get(MANAGED_NODE).and_then(Value::as_bool) != Some(true)
                || node.extra.get(NAMESPACE_NODE).and_then(Value::as_bool) == Some(true)
                || referenced.contains(&node.id)
        });
    }
}
