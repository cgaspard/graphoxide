//! Grammar-backed Swift extraction and precision-gated member-call resolution.
//!
//! Swift's computed properties and receiver chains are structural syntax.  A
//! line-oriented compatibility scanner cannot distinguish a stored property
//! from `var body: some View { ... }`, nor can it safely infer the type behind
//! `self.service.fetch()` or a cached singleton.  This module keeps those facts
//! on the caller node until the corpus-level pass can resolve them against the
//! complete set of source-backed types.

use graphoxide_core::{make_id, normalize_id, Confidence, Edge, Extraction, Node};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use tree_sitter::{Node as TsNode, Parser};

const RAW_CALLS: &str = "_swift_raw_calls";
const SWIFT_MODULE: &str = "swift_module";

const SWIFT_BUILTINS: &[&str] = &[
    "String",
    "Int",
    "Int8",
    "Int16",
    "Int32",
    "Int64",
    "UInt",
    "UInt8",
    "UInt16",
    "UInt32",
    "UInt64",
    "Double",
    "Float",
    "Bool",
    "Character",
    "Array",
    "Dictionary",
    "Set",
    "Optional",
    "Error",
    "Sendable",
    "Codable",
    "Decodable",
    "Encodable",
    "Equatable",
    "Hashable",
    "Identifiable",
    "Comparable",
    "CaseIterable",
    "RawRepresentable",
    "CustomStringConvertible",
    "CustomDebugStringConvertible",
    "Any",
    "AnyObject",
    "Never",
    "LocalizedError",
    "Data",
    "Date",
    "URL",
    "UUID",
    "Decimal",
    "Calendar",
    "Locale",
    "TimeZone",
    "Bundle",
    "IndexPath",
    "IndexSet",
    "NotificationCenter",
    "UserDefaults",
    "FileManager",
    "URLSession",
    "URLRequest",
    "URLComponents",
    "JSONDecoder",
    "JSONEncoder",
    "DateFormatter",
    "NumberFormatter",
    "ISO8601DateFormatter",
    "NSObject",
    "NSString",
    "NSError",
    "NSLock",
    "NSAttributedString",
    "DispatchQueue",
    "DispatchGroup",
    "OperationQueue",
    "RunLoop",
    "View",
    "Color",
    "Font",
];

#[derive(Clone)]
struct Callable<'tree> {
    id: String,
    owner: Option<String>,
    body: TsNode<'tree>,
    parameter_types: HashMap<String, String>,
}

#[derive(Debug)]
struct CallFact {
    callee: String,
    receiver: Option<String>,
    receiver_type: Option<String>,
    type_qualified: bool,
    constructor: bool,
    line: usize,
}

struct SwiftBuilder<'a> {
    source_file: &'a str,
    stem: String,
    file_id: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    seen_nodes: HashSet<String>,
    seen_edges: HashSet<(String, String, String, String)>,
    definitions: HashMap<String, Vec<String>>,
}

impl<'a> SwiftBuilder<'a> {
    fn new(path: &Path, source_file: &'a str) -> Self {
        let stem = Path::new(source_file)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let file_id = make_id(&[&stem]);
        let mut result = Self {
            source_file,
            stem,
            file_id: file_id.clone(),
            nodes: Vec::new(),
            edges: Vec::new(),
            seen_nodes: HashSet::new(),
            seen_edges: HashSet::new(),
            definitions: HashMap::new(),
        };
        result.insert_node(
            file_id,
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(source_file),
            "file",
            1,
            source_file,
        );
        result
    }

    fn finish(self) -> Extraction {
        Extraction {
            nodes: self.nodes,
            edges: self.edges,
            hyperedges: Vec::new(),
        }
    }

    fn insert_node(&mut self, id: String, label: &str, kind: &str, line: usize, source: &str) {
        if !self.seen_nodes.insert(id.clone()) {
            return;
        }
        if source == self.source_file && !matches!(kind, "reference" | "module") {
            self.definitions
                .entry(swift_key(label))
                .or_default()
                .push(id.clone());
        }
        self.nodes.push(Node {
            id,
            label: label.to_owned(),
            file_type: "code".into(),
            source_file: source.into(),
            source_location: Some(format!("L{line}")),
            community: None,
            extra: BTreeMap::from([
                ("_origin".into(), "ast".into()),
                ("type".into(), kind.into()),
            ]),
        });
    }

    fn definition(&mut self, name: &str, owner: Option<&str>, kind: &str, line: usize) -> String {
        let id = owner
            .map(|owner| make_id(&[owner, name]))
            .unwrap_or_else(|| make_id(&[&self.stem, name]));
        let label = if kind == "function" && owner.is_some() {
            format!(".{name}()")
        } else if kind == "function" {
            format!("{name}()")
        } else {
            name.to_owned()
        };
        self.insert_node(id.clone(), &label, kind, line, self.source_file);
        let parent = owner.unwrap_or(&self.file_id).to_owned();
        let relation = if kind == "function" && owner.is_some() {
            "method"
        } else {
            "contains"
        };
        self.edge(
            &parent,
            &id,
            relation,
            None,
            line,
            Confidence::Extracted,
            None,
        );
        id
    }

    fn property_callable(&mut self, name: &str, owner: &str, line: usize) -> String {
        let id = make_id(&[owner, name]);
        self.insert_node(
            id.clone(),
            &format!(".{name}"),
            "function",
            line,
            self.source_file,
        );
        self.edge(
            owner,
            &id,
            "method",
            None,
            line,
            Confidence::Extracted,
            None,
        );
        id
    }

    fn unique_definition(&self, name: &str) -> Option<String> {
        self.definitions
            .get(&swift_key(name))
            .filter(|ids| ids.len() == 1)
            .and_then(|ids| ids.first())
            .cloned()
    }

    fn external(&mut self, name: &str, line: usize) -> String {
        if let Some(id) = self.unique_definition(name) {
            return id;
        }
        let id = make_id(&[name]);
        let before = self.nodes.len();
        self.insert_node(id.clone(), name, "reference", line, "");
        if self.nodes.len() > before {
            self.nodes
                .last_mut()
                .expect("new Swift reference node")
                .extra
                .insert("origin_file".into(), self.source_file.into());
        }
        id
    }

    fn module(&mut self, name: &str, line: usize) -> String {
        let id = make_id(&[name]);
        self.insert_node(id.clone(), name, "module", line, self.source_file);
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            node.extra.insert(SWIFT_MODULE.into(), true.into());
        }
        id
    }

    #[allow(clippy::too_many_arguments)]
    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        context: Option<&str>,
        line: usize,
        confidence: Confidence,
        confidence_score: Option<f64>,
    ) {
        if source.is_empty() || target.is_empty() || source == target {
            return;
        }
        let context_key = context.unwrap_or("").to_owned();
        if !self.seen_edges.insert((
            source.to_owned(),
            target.to_owned(),
            relation.to_owned(),
            context_key,
        )) {
            return;
        }
        let mut extra = BTreeMap::from([
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), 1.0.into()),
        ]);
        if let Some(context) = context {
            extra.insert("context".into(), context.into());
        }
        if let Some(score) = confidence_score {
            extra.insert("confidence_score".into(), score.into());
        }
        self.edges.push(Edge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            confidence,
            source_file: self.source_file.into(),
            extra,
        });
    }

    fn raw_call(&mut self, caller: &str, call: &CallFact) {
        let Some(node) = self.nodes.iter_mut().find(|node| node.id == caller) else {
            return;
        };
        let mut fact = Map::from_iter([
            ("callee".into(), call.callee.clone().into()),
            ("line".into(), call.line.into()),
            ("type_qualified".into(), call.type_qualified.into()),
            ("constructor".into(), call.constructor.into()),
        ]);
        if let Some(receiver) = &call.receiver {
            fact.insert("receiver".into(), receiver.clone().into());
        }
        if let Some(receiver_type) = &call.receiver_type {
            fact.insert("receiver_type".into(), receiver_type.clone().into());
        }
        node.extra
            .entry(RAW_CALLS.into())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("Swift raw-call store is always an array")
            .push(Value::Object(fact));
    }

    fn method_index(&self) -> HashMap<(String, String), Vec<String>> {
        let labels: HashMap<_, _> = self
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), swift_key(&node.label)))
            .collect();
        let mut result: HashMap<(String, String), Vec<String>> = HashMap::new();
        for edge in &self.edges {
            if edge.relation != "method" {
                continue;
            }
            let Some(label) = labels.get(edge.true_target()) else {
                continue;
            };
            result
                .entry((edge.true_source().to_owned(), (*label).clone()))
                .or_default()
                .push(edge.true_target().to_owned());
        }
        result
    }

    fn type_index(&self) -> HashMap<String, Vec<String>> {
        let mut result: HashMap<String, Vec<String>> = HashMap::new();
        for node in &self.nodes {
            if node.source_file == self.source_file
                && node.extra.get("type").and_then(Value::as_str) == Some("class")
            {
                result
                    .entry(swift_key(&node.label))
                    .or_default()
                    .push(node.id.clone());
            }
        }
        result
    }
}

pub(crate) fn extract_swift(
    path: &Path,
    text: &str,
    source_file: &str,
) -> anyhow::Result<Extraction> {
    let source = text.as_bytes();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_swift::LANGUAGE.into())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter returned no Swift tree"))?;
    let mut builder = SwiftBuilder::new(path, source_file);

    let mut protocols = HashSet::new();
    let mut classes = HashSet::new();
    collect_type_kinds(tree.root_node(), source, &mut protocols, &mut classes);

    let mut callables = Vec::new();
    let mut fields = HashMap::<String, HashMap<String, String>>::new();
    walk_declarations(
        tree.root_node(),
        source,
        None,
        &protocols,
        &classes,
        &mut builder,
        &mut callables,
        &mut fields,
    );

    let method_index = builder.method_index();
    let type_index = builder.type_index();
    for callable in callables {
        let mut receiver_types = callable
            .owner
            .as_ref()
            .and_then(|owner| fields.get(owner))
            .cloned()
            .unwrap_or_default();
        for (name, type_name) in callable.parameter_types {
            receiver_types.entry(name).or_insert(type_name);
        }
        collect_local_types(callable.body, source, &mut receiver_types);
        walk_calls(
            callable.body,
            source,
            &callable.id,
            callable.owner.as_deref(),
            &receiver_types,
            &method_index,
            &type_index,
            &mut builder,
            true,
        );
    }

    if tree.root_node().has_error()
        && let Some(file) = builder
            .nodes
            .iter_mut()
            .find(|node| node.id == builder.file_id)
    {
        file.extra.insert("parser_has_error".into(), true.into());
    }
    Ok(builder.finish())
}

fn collect_type_kinds(
    node: TsNode<'_>,
    source: &[u8],
    protocols: &mut HashSet<String>,
    classes: &mut HashSet<String>,
) {
    if matches!(node.kind(), "class_declaration" | "protocol_declaration")
        && let Some(name) = declaration_name(node, source)
    {
        if node.kind() == "protocol_declaration" {
            protocols.insert(swift_key(&name));
        } else if declaration_kind(node, source) == "class" {
            classes.insert(swift_key(&name));
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_kinds(child, source, protocols, classes);
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_declarations<'tree>(
    node: TsNode<'tree>,
    source: &[u8],
    owner: Option<&str>,
    protocols: &HashSet<String>,
    classes: &HashSet<String>,
    builder: &mut SwiftBuilder<'_>,
    callables: &mut Vec<Callable<'tree>>,
    fields: &mut HashMap<String, HashMap<String, String>>,
) {
    let line = node.start_position().row + 1;
    match node.kind() {
        "import_declaration" => {
            if let Some(module) = first_descendant_text(node, source, &["identifier"]) {
                let target = builder.module(&module, line);
                builder.edge(
                    &builder.file_id.clone(),
                    &target,
                    "imports",
                    Some("import"),
                    line,
                    Confidence::Extracted,
                    Some(1.0),
                );
            }
            return;
        }
        "class_declaration" | "protocol_declaration" => {
            let Some(name) = declaration_name(node, source) else {
                return;
            };
            let kind = declaration_kind(node, source);
            let id = if kind == "extension" {
                builder
                    .unique_definition(&name)
                    .unwrap_or_else(|| builder.external(&name, line))
            } else {
                builder.definition(&name, owner, "class", line)
            };
            emit_inheritance(node, source, &id, &kind, protocols, classes, builder);
            if kind == "enum" {
                emit_enum_cases(node, source, &id, builder);
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    if child.kind() == "enum_entry" {
                        continue;
                    }
                    walk_declarations(
                        child,
                        source,
                        Some(&id),
                        protocols,
                        classes,
                        builder,
                        callables,
                        fields,
                    );
                }
            }
            return;
        }
        "function_declaration"
        | "protocol_function_declaration"
        | "init_declaration"
        | "deinit_declaration"
        | "subscript_declaration" => {
            let name = match node.kind() {
                "init_declaration" => "init".to_owned(),
                "deinit_declaration" => "deinit".to_owned(),
                "subscript_declaration" => "subscript".to_owned(),
                _ => declaration_name(node, source).unwrap_or_default(),
            };
            if name.is_empty() {
                return;
            }
            let id = builder.definition(&name, owner, "function", line);
            let parameter_types = emit_parameters(node, source, &id, builder, line);
            for return_type in return_type_nodes(node) {
                emit_type_references(return_type, source, &id, "return_type", line, builder);
            }
            if let Some(body) = node.child_by_field_name("body") {
                callables.push(Callable {
                    id,
                    owner: owner.map(str::to_owned),
                    body,
                    parameter_types,
                });
            }
            return;
        }
        "property_declaration" => {
            let Some(owner) = owner else {
                return;
            };
            let Some(name) = property_name(node, source) else {
                return;
            };
            let annotation = node
                .named_children(&mut node.walk())
                .find(|child| child.kind() == "type_annotation");
            let mut receiver_type = annotation.and_then(|annotation| {
                emit_type_references(annotation, source, owner, "field", line, builder);
                type_head(annotation, source)
            });
            let value = node.child_by_field_name("value").or_else(|| {
                direct_named_child(node, &["call_expression", "navigation_expression"])
            });
            if receiver_type.is_none() {
                receiver_type = value.and_then(|value| inferred_binding_type(value, source));
            }
            if let Some(receiver_type) = receiver_type {
                fields
                    .entry(owner.to_owned())
                    .or_default()
                    .entry(name.clone())
                    .or_insert(receiver_type);
            }
            if let Some(value) = value.filter(|value| value.kind() == "call_expression")
                && let Some(call) = call_fact(value, source, &HashMap::new())
            {
                emit_or_defer_call(owner, None, call, &HashMap::new(), &HashMap::new(), builder);
            }
            let bodies: Vec<_> = node
                .named_children(&mut node.walk())
                .filter(|child| {
                    matches!(child.kind(), "computed_property" | "willset_didset_block")
                })
                .collect();
            if !bodies.is_empty() {
                let id = builder.property_callable(&name, owner, line);
                for body in bodies {
                    callables.push(Callable {
                        id: id.clone(),
                        owner: Some(owner.to_owned()),
                        body,
                        parameter_types: HashMap::new(),
                    });
                }
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_declarations(
            child, source, owner, protocols, classes, builder, callables, fields,
        );
    }
}

fn emit_inheritance(
    node: TsNode<'_>,
    source: &[u8],
    id: &str,
    kind: &str,
    protocols: &HashSet<String>,
    classes: &HashSet<String>,
    builder: &mut SwiftBuilder<'_>,
) {
    let line = node.start_position().row + 1;
    let mut index = 0_usize;
    let mut cursor = node.walk();
    for parent in node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "inheritance_specifier")
    {
        let Some(name) = first_descendant_text(parent, source, &["type_identifier"]) else {
            continue;
        };
        let key = swift_key(&name);
        let relation = if protocols.contains(&key)
            || matches!(kind, "struct" | "enum" | "extension" | "actor" | "protocol")
            || (kind == "class" && index > 0 && !classes.contains(&key))
        {
            "implements"
        } else {
            "inherits"
        };
        let target = builder.external(&name, line);
        builder.edge(
            id,
            &target,
            relation,
            None,
            line,
            Confidence::Extracted,
            Some(1.0),
        );
        index += 1;
    }
}

fn emit_enum_cases(node: TsNode<'_>, source: &[u8], owner: &str, builder: &mut SwiftBuilder<'_>) {
    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for entry in body
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "enum_entry")
    {
        let Some(name) = entry
            .child_by_field_name("name")
            .map(|name| node_text(name, source))
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let line = entry.start_position().row + 1;
        let id = builder.definition(&name, Some(owner), "enum_case", line);
        builder.edge(
            owner,
            &id,
            "case_of",
            None,
            line,
            Confidence::Extracted,
            Some(1.0),
        );
        for associated in children_by_field_name(entry, "data_contents") {
            emit_type_references(associated, source, owner, "type", line, builder);
        }
    }
}

fn emit_parameters(
    node: TsNode<'_>,
    source: &[u8],
    owner: &str,
    builder: &mut SwiftBuilder<'_>,
    line: usize,
) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let mut cursor = node.walk();
    for parameter in node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "parameter")
    {
        let Some(type_node) = parameter_type_node(parameter) else {
            continue;
        };
        emit_type_references(type_node, source, owner, "parameter_type", line, builder);
        if let (Some(name), Some(type_name)) = (
            parameter_name(parameter, source),
            type_head(type_node, source),
        ) {
            result.entry(name).or_insert(type_name);
        }
    }
    result
}

fn emit_type_references(
    node: TsNode<'_>,
    source: &[u8],
    owner: &str,
    primary_context: &str,
    line: usize,
    builder: &mut SwiftBuilder<'_>,
) {
    let mut names = Vec::new();
    collect_type_names(node, source, false, &mut names);
    let mut seen = HashSet::new();
    for (name, generic) in names {
        if name.is_empty() || !seen.insert((name.clone(), generic)) {
            continue;
        }
        let target = builder.external(&name, line);
        builder.edge(
            owner,
            &target,
            "references",
            Some(if generic {
                "generic_arg"
            } else {
                primary_context
            }),
            line,
            Confidence::Extracted,
            Some(1.0),
        );
    }
}

fn collect_type_names(
    node: TsNode<'_>,
    source: &[u8],
    generic: bool,
    output: &mut Vec<(String, bool)>,
) {
    match node.kind() {
        "user_type" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "type_identifier" => output.push((node_text(child, source), generic)),
                    "type_arguments" => {
                        let mut arguments = child.walk();
                        for argument in child.named_children(&mut arguments) {
                            collect_type_names(argument, source, true, output);
                        }
                    }
                    _ => {}
                }
            }
            return;
        }
        "type_identifier" => {
            output.push((node_text(node, source), generic));
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_names(child, source, generic, output);
    }
}

fn type_head(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    let mut names = Vec::new();
    collect_type_names(node, source, false, &mut names);
    names
        .into_iter()
        .find_map(|(name, generic)| (!generic && !name.is_empty()).then_some(name))
}

fn collect_local_types(node: TsNode<'_>, source: &[u8], table: &mut HashMap<String, String>) {
    if node.kind() == "function_declaration" {
        return;
    }
    if node.kind() == "property_declaration"
        && let Some(name) = property_name(node, source)
    {
        let annotation = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "type_annotation");
        let value = node
            .child_by_field_name("value")
            .or_else(|| direct_named_child(node, &["call_expression", "navigation_expression"]));
        let type_name = annotation
            .and_then(|annotation| type_head(annotation, source))
            .or_else(|| value.and_then(|value| inferred_binding_type(value, source)));
        if let Some(type_name) = type_name {
            table.entry(name).or_insert(type_name);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_local_types(child, source, table);
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_calls(
    node: TsNode<'_>,
    source: &[u8],
    caller: &str,
    owner: Option<&str>,
    receiver_types: &HashMap<String, String>,
    method_index: &HashMap<(String, String), Vec<String>>,
    type_index: &HashMap<String, Vec<String>>,
    builder: &mut SwiftBuilder<'_>,
    is_root: bool,
) {
    if !is_root
        && matches!(
            node.kind(),
            "function_declaration"
                | "protocol_function_declaration"
                | "init_declaration"
                | "deinit_declaration"
                | "subscript_declaration"
                | "computed_property"
                | "willset_didset_block"
        )
    {
        return;
    }
    if node.kind() == "call_expression"
        && let Some(call) = call_fact(node, source, receiver_types)
    {
        emit_or_defer_call(caller, owner, call, method_index, type_index, builder);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_calls(
            child,
            source,
            caller,
            owner,
            receiver_types,
            method_index,
            type_index,
            builder,
            false,
        );
    }
}

fn call_fact(
    call: TsNode<'_>,
    source: &[u8],
    receiver_types: &HashMap<String, String>,
) -> Option<CallFact> {
    let callee = call.named_child(0)?;
    let line = call.start_position().row + 1;
    if callee.kind() == "simple_identifier" {
        let name = node_text(callee, source);
        if name.is_empty() {
            return None;
        }
        return Some(CallFact {
            constructor: name.chars().next().is_some_and(char::is_uppercase),
            callee: name,
            receiver: None,
            receiver_type: None,
            type_qualified: false,
            line,
        });
    }
    if callee.kind() != "navigation_expression" {
        return None;
    }
    let method = navigation_suffix_name(callee, source)?;
    let receiver = navigation_receiver(callee, source)?;
    let type_qualified = receiver.chars().next().is_some_and(char::is_uppercase);
    let receiver_type = if type_qualified {
        Some(receiver.clone())
    } else {
        receiver_types.get(&receiver).cloned()
    };
    Some(CallFact {
        callee: method,
        receiver: Some(receiver),
        receiver_type,
        type_qualified,
        constructor: false,
        line,
    })
}

fn emit_or_defer_call(
    caller: &str,
    owner: Option<&str>,
    call: CallFact,
    method_index: &HashMap<(String, String), Vec<String>>,
    type_index: &HashMap<String, Vec<String>>,
    builder: &mut SwiftBuilder<'_>,
) {
    if call.constructor {
        if swift_builtin(&call.callee) {
            return;
        }
        if let Some(target) = exactly_one_vec(type_index.get(&swift_key(&call.callee))) {
            builder.edge(
                caller,
                target,
                "calls",
                Some("call"),
                call.line,
                Confidence::Extracted,
                Some(1.0),
            );
        } else {
            builder.raw_call(caller, &call);
        }
        return;
    }

    if call.receiver.is_none() {
        if let Some(owner) = owner
            && let Some(target) =
                exactly_one_vec(method_index.get(&(owner.to_owned(), swift_key(&call.callee))))
        {
            builder.edge(
                caller,
                target,
                "calls",
                Some("call"),
                call.line,
                Confidence::Extracted,
                Some(1.0),
            );
            return;
        }
        builder.raw_call(caller, &call);
        return;
    }

    let Some(receiver_type) = call.receiver_type.as_deref() else {
        builder.raw_call(caller, &call);
        return;
    };
    if swift_builtin(receiver_type) {
        return;
    }
    let Some(type_id) = exactly_one_vec(type_index.get(&swift_key(receiver_type))) else {
        builder.raw_call(caller, &call);
        return;
    };
    if let Some(target) =
        exactly_one_vec(method_index.get(&(type_id.to_owned(), swift_key(&call.callee))))
    {
        let confidence = if call.type_qualified {
            Confidence::Extracted
        } else {
            Confidence::Inferred
        };
        builder.edge(
            caller,
            target,
            "calls",
            Some("call"),
            call.line,
            confidence,
            Some(if confidence == Confidence::Extracted {
                1.0
            } else {
                0.8
            }),
        );
    } else {
        builder.raw_call(caller, &call);
    }
}

pub(crate) fn resolve(extractions: &mut [Extraction]) {
    namespace_colliding_module_anchors(extractions);

    let mut type_definitions = HashMap::<String, BTreeSet<String>>::new();
    let mut method_definitions = HashMap::<(String, String), BTreeSet<String>>::new();
    let mut method_owners = HashMap::<String, String>::new();
    let mut node_labels = HashMap::<String, String>::new();
    let mut free_functions = HashMap::<String, BTreeSet<String>>::new();

    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            node_labels.insert(node.id.clone(), node.label.clone());
            if is_swift_source(&node.source_file)
                && node.extra.get("type").and_then(Value::as_str) == Some("class")
            {
                type_definitions
                    .entry(swift_key(&node.label))
                    .or_default()
                    .insert(node.id.clone());
            }
        }
    }
    for extraction in extractions.iter() {
        for edge in &extraction.edges {
            if edge.relation == "method" {
                if let Some(label) = node_labels.get(edge.true_target()) {
                    method_definitions
                        .entry((edge.true_source().to_owned(), swift_key(label)))
                        .or_default()
                        .insert(edge.true_target().to_owned());
                    method_owners
                        .insert(edge.true_target().to_owned(), edge.true_source().to_owned());
                }
            } else if edge.relation == "contains"
                && let Some(label) = node_labels.get(edge.true_target())
                && label.ends_with("()")
                && is_swift_source(&edge.source_file)
            {
                free_functions
                    .entry(swift_key(label))
                    .or_default()
                    .insert(edge.true_target().to_owned());
            }
        }
    }

    let mut existing: BTreeSet<(String, String, String)> = extractions
        .iter()
        .flat_map(|extraction| extraction.edges.iter())
        .filter(|edge| matches!(edge.relation.as_str(), "calls" | "references"))
        .map(|edge| {
            (
                edge.true_source().to_owned(),
                edge.true_target().to_owned(),
                edge.relation.clone(),
            )
        })
        .collect();

    for extraction in extractions {
        let mut pending = Vec::<(String, Map<String, Value>, String)>::new();
        for node in &mut extraction.nodes {
            let Some(raw) = node
                .extra
                .remove(RAW_CALLS)
                .and_then(|value| value.as_array().cloned())
            else {
                continue;
            };
            for fact in raw {
                if let Some(fact) = fact.as_object() {
                    pending.push((node.id.clone(), fact.clone(), node.source_file.clone()));
                }
            }
        }

        for (caller, fact, source_file) in pending {
            let Some(callee) = fact.get("callee").and_then(Value::as_str) else {
                continue;
            };
            let line = fact
                .get("line")
                .and_then(Value::as_u64)
                .and_then(|line| usize::try_from(line).ok())
                .unwrap_or(1);
            let constructor = fact
                .get("constructor")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let type_qualified = fact
                .get("type_qualified")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let receiver_type = fact.get("receiver_type").and_then(Value::as_str);

            let (target, relation, confidence, score) = if constructor {
                if swift_builtin(callee) {
                    continue;
                }
                let Some(target) = exactly_one(type_definitions.get(&swift_key(callee))) else {
                    continue;
                };
                (target.to_owned(), "calls", Confidence::Extracted, 1.0)
            } else if let Some(receiver_type) = receiver_type {
                if swift_builtin(receiver_type) {
                    continue;
                }
                let Some(type_id) = exactly_one(type_definitions.get(&swift_key(receiver_type)))
                else {
                    continue;
                };
                if let Some(method) =
                    exactly_one(method_definitions.get(&(type_id.to_owned(), swift_key(callee))))
                {
                    (
                        method.to_owned(),
                        "calls",
                        if type_qualified {
                            Confidence::Extracted
                        } else {
                            Confidence::Inferred
                        },
                        if type_qualified { 1.0 } else { 0.8 },
                    )
                } else {
                    (
                        type_id.to_owned(),
                        "references",
                        if type_qualified {
                            Confidence::Extracted
                        } else {
                            Confidence::Inferred
                        },
                        if type_qualified { 1.0 } else { 0.8 },
                    )
                }
            } else {
                let owner = method_owners.get(&caller);
                let method = owner.and_then(|owner| {
                    exactly_one(method_definitions.get(&(owner.clone(), swift_key(callee))))
                });
                let free = exactly_one(free_functions.get(&swift_key(callee)));
                let Some(target) = method.or(free) else {
                    continue;
                };
                (target.to_owned(), "calls", Confidence::Extracted, 1.0)
            };
            if target == caller
                || !existing.insert((caller.clone(), target.clone(), relation.to_owned()))
            {
                continue;
            }
            crate::resolution::push_resolved_edge(
                &mut extraction.edges,
                resolved_edge(
                    &caller,
                    &target,
                    relation,
                    confidence,
                    score,
                    &source_file,
                    line,
                ),
            );
        }
    }
}

fn namespace_colliding_module_anchors(extractions: &mut [Extraction]) {
    let source_backed_ids = extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .filter(|node| {
            !node.source_file.is_empty()
                && node.extra.get("type").and_then(Value::as_str) != Some("module")
        })
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    let remap = extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .filter(|node| {
            source_backed_ids.contains(&node.id)
                && node.extra.get("type").and_then(Value::as_str) == Some("module")
                && node.extra.get(SWIFT_MODULE).and_then(Value::as_bool) == Some(true)
        })
        .map(|node| (node.id.clone(), make_id(&["__swift_module", &node.label])))
        .collect::<BTreeMap<_, _>>();
    if remap.is_empty() {
        return;
    }

    for extraction in extractions {
        for node in &mut extraction.nodes {
            if node.extra.get("type").and_then(Value::as_str) == Some("module")
                && node.extra.get(SWIFT_MODULE).and_then(Value::as_bool) == Some(true)
                && let Some(target) = remap.get(&node.id)
            {
                node.id = target.clone();
            }
        }
        for edge in &mut extraction.edges {
            if let Some(target) = remap.get(edge.true_target()) {
                edge.target = target.clone();
                edge.extra.insert("_tgt".into(), target.clone().into());
            }
        }
    }
}

fn resolved_edge(
    source: &str,
    target: &str,
    relation: &str,
    confidence: Confidence,
    confidence_score: f64,
    source_file: &str,
    line: usize,
) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence,
        source_file: source_file.into(),
        extra: BTreeMap::from([
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
            ("context".into(), "call".into()),
            ("confidence_score".into(), confidence_score.into()),
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), 1.0.into()),
        ]),
    }
}

fn node_text(node: TsNode<'_>, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").trim().to_owned()
}

fn declaration_name(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .map(|name| node_text(name, source))
        .filter(|name| !name.is_empty())
        .or_else(|| {
            let mut cursor = node.walk();

            node.named_children(&mut cursor)
                .find(|child| {
                    matches!(
                        child.kind(),
                        "simple_identifier" | "type_identifier" | "user_type"
                    )
                })
                .map(|name| node_text(name, source))
                .filter(|name| !name.is_empty())
        })
}

fn declaration_kind(node: TsNode<'_>, source: &[u8]) -> String {
    node.child_by_field_name("declaration_kind")
        .map(|kind| node_text(kind, source))
        .filter(|kind| !kind.is_empty())
        .unwrap_or_else(|| {
            if node.kind() == "protocol_declaration" {
                "protocol".into()
            } else {
                let mut cursor = node.walk();

                node.children(&mut cursor)
                    .find(|child| {
                        matches!(
                            child.kind(),
                            "class" | "struct" | "enum" | "actor" | "extension"
                        )
                    })
                    .map(|kind| kind.kind().to_owned())
                    .unwrap_or_else(|| "class".into())
            }
        })
}

fn property_name(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    let pattern = node.child_by_field_name("name").or_else(|| {
        let mut cursor = node.walk();

        node.named_children(&mut cursor)
            .find(|child| child.kind() == "pattern")
    })?;
    pattern
        .child_by_field_name("bound_identifier")
        .map(|name| node_text(name, source))
        .filter(|name| !name.is_empty())
        .or_else(|| first_descendant_text(pattern, source, &["simple_identifier"]))
}

fn parameter_name(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    let type_node = parameter_type_node(node)?;
    let mut names = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.id() == type_node.id() || child.kind() != "simple_identifier" {
            continue;
        }
        let name = node_text(child, source);
        if name != "_" {
            names.push(name);
        }
    }
    names.pop()
}

fn parameter_type_node(node: TsNode<'_>) -> Option<TsNode<'_>> {
    node.child_by_field_name("type").or_else(|| {
        // tree-sitter-swift 0.7 currently labels both a parameter's identifier
        // and its type as `name`.  The grammar shape is still unambiguous: the
        // type is the first named child following `:`.
        let mut after_colon = false;
        for index in 0..node.child_count() {
            let child = node.child(index)?;
            if child.kind() == ":" {
                after_colon = true;
                continue;
            }
            if after_colon && child.is_named() {
                return Some(child);
            }
        }
        None
    })
}

fn return_type_nodes(node: TsNode<'_>) -> Vec<TsNode<'_>> {
    let declared = children_by_field_name(node, "return_type");
    if !declared.is_empty() {
        return declared;
    }

    // The same grammar release exposes a function return type under the
    // `name` field in the runtime tree even though node-types.json calls the
    // field `return_type`.  Fall back to the first named child after `->`.
    let mut after_arrow = false;
    for index in 0..node.child_count() {
        let Some(child) = node.child(index) else {
            continue;
        };
        if child.kind() == "->" {
            after_arrow = true;
            continue;
        }
        if after_arrow && child.is_named() {
            return vec![child];
        }
    }
    Vec::new()
}

fn inferred_binding_type(value: TsNode<'_>, source: &[u8]) -> Option<String> {
    match value.kind() {
        "call_expression" => {
            let callee = value.named_child(0)?;
            (callee.kind() == "simple_identifier")
                .then(|| node_text(callee, source))
                .filter(|name| name.chars().next().is_some_and(char::is_uppercase))
        }
        "navigation_expression" => navigation_head(value, source)
            .filter(|name| name.chars().next().is_some_and(char::is_uppercase)),
        _ => None,
    }
}

fn navigation_suffix_name(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() == "navigation_suffix")
        .last()
        .and_then(|suffix| {
            suffix
                .child_by_field_name("suffix")
                .or_else(|| suffix.named_child(0))
        })
        .map(|name| node_text(name, source))
        .filter(|name| !name.is_empty())
}

fn navigation_head(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    let target = node
        .child_by_field_name("target")
        .or_else(|| node.named_child(0))?;
    match target.kind() {
        "simple_identifier" => Some(node_text(target, source)),
        "navigation_expression" => navigation_head(target, source),
        _ => None,
    }
}

fn navigation_receiver(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    let target = node
        .child_by_field_name("target")
        .or_else(|| node.named_child(0))?;
    match target.kind() {
        "simple_identifier" => Some(node_text(target, source)),
        "navigation_expression" => {
            let inner_target = target
                .child_by_field_name("target")
                .or_else(|| target.named_child(0))?;
            if inner_target.kind() == "self_expression" {
                navigation_suffix_name(target, source)
            } else {
                navigation_head(target, source)
            }
        }
        "self_expression" => Some("self".into()),
        _ => None,
    }
}

fn first_descendant_text(node: TsNode<'_>, source: &[u8], kinds: &[&str]) -> Option<String> {
    if kinds.contains(&node.kind()) {
        let text = node_text(node, source);
        if !text.is_empty() {
            return Some(text);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(text) = first_descendant_text(child, source, kinds) {
            return Some(text);
        }
    }
    None
}

fn direct_named_child<'tree>(node: TsNode<'tree>, kinds: &[&str]) -> Option<TsNode<'tree>> {
    let mut cursor = node.walk();

    node.named_children(&mut cursor)
        .find(|child| kinds.contains(&child.kind()))
}

fn children_by_field_name<'tree>(node: TsNode<'tree>, field: &str) -> Vec<TsNode<'tree>> {
    let Some(field_id) = node.language().field_id_for_name(field) else {
        return Vec::new();
    };
    let mut cursor = node.walk();
    node.children_by_field_id(field_id, &mut cursor).collect()
}

fn swift_key(value: &str) -> String {
    normalize_id(value.trim_start_matches('.').trim_end_matches("()"))
}

fn exactly_one<T>(values: Option<&BTreeSet<T>>) -> Option<&T> {
    values.filter(|values| values.len() == 1)?.iter().next()
}

fn exactly_one_vec<T>(values: Option<&Vec<T>>) -> Option<&T> {
    values.filter(|values| values.len() == 1)?.first()
}

fn swift_builtin(name: &str) -> bool {
    SWIFT_BUILTINS.contains(&name)
}

fn is_swift_source(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("swift"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imported_module(extraction: &Extraction) -> &str {
        extraction
            .edges
            .iter()
            .find(|edge| edge.relation == "imports")
            .expect("Swift import edge")
            .true_target()
    }

    #[test]
    fn module_anchors_are_corpus_wide_and_do_not_collide_with_files() {
        let first = extract_swift(
            Path::new("Sources/App.swift"),
            "import Helpers\nstruct App {}",
            "Sources/App.swift",
        )
        .expect("extract first Swift importer");
        let second = extract_swift(
            Path::new("Sources/Other.swift"),
            "import Helpers\nstruct Other {}",
            "Sources/Other.swift",
        )
        .expect("extract second Swift importer");
        let colliding_file = extract_swift(
            Path::new("Helpers.swift"),
            "struct Helpers {}",
            "Helpers.swift",
        )
        .expect("extract same-named Swift file");

        assert_eq!(imported_module(&first), make_id(&["Helpers"]));
        assert_eq!(imported_module(&second), make_id(&["Helpers"]));
        let mut extractions = vec![first, second, colliding_file];
        resolve(&mut extractions);

        let expected = make_id(&["__swift_module", "Helpers"]);
        assert_eq!(imported_module(&extractions[0]), expected);
        assert_eq!(imported_module(&extractions[1]), expected);
        assert_ne!(expected, make_id(&["Helpers"]));
        assert!(extractions[2].nodes.iter().any(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("file")
                && node.id == make_id(&["Helpers"])
        }));
        let node_ids = extractions
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .map(|node| node.id.as_str())
            .collect::<BTreeSet<_>>();
        assert!(extractions
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .all(|edge| node_ids.contains(edge.true_source())
                && node_ids.contains(edge.true_target())));
    }

    #[test]
    fn uncollided_module_anchor_preserves_the_legacy_id() {
        let importer = extract_swift(
            Path::new("Sources/App.swift"),
            "import Helpers\nstruct App {}",
            "Sources/App.swift",
        )
        .expect("extract Swift importer");
        let mut extractions = vec![importer];
        resolve(&mut extractions);

        assert_eq!(imported_module(&extractions[0]), make_id(&["Helpers"]));
    }
}
