//! Shared tree-sitter extraction driver for the compiled language set.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::Path,
};
use tree_sitter::{Node as TsNode, Parser};

struct Spec {
    classes: &'static [&'static str],
    functions: &'static [&'static str],
    imports: &'static [&'static str],
    calls: &'static [&'static str],
    function_field: &'static str,
    accessor_types: &'static [&'static str],
    accessor_field: &'static str,
}

fn spec(language: &str) -> Spec {
    match language {
        "python" => Spec {
            classes: &["class_definition"],
            functions: &["function_definition"],
            imports: &["import_statement", "import_from_statement"],
            calls: &["call"],
            function_field: "function",
            accessor_types: &["attribute"],
            accessor_field: "attribute",
        },
        "javascript" => Spec {
            classes: &["class_declaration"],
            functions: &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
            ],
            imports: &["import_statement", "export_statement"],
            calls: &["call_expression", "new_expression"],
            function_field: "function",
            accessor_types: &["member_expression"],
            accessor_field: "property",
        },
        "typescript" | "tsx" => Spec {
            classes: &[
                "class_declaration",
                "abstract_class_declaration",
                "interface_declaration",
                "enum_declaration",
                "type_alias_declaration",
            ],
            functions: &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "method_signature",
            ],
            imports: &["import_statement", "export_statement"],
            calls: &["call_expression", "new_expression"],
            function_field: "function",
            accessor_types: &["member_expression"],
            accessor_field: "property",
        },
        "java" => Spec {
            classes: &[
                "class_declaration",
                "interface_declaration",
                "record_declaration",
                "enum_declaration",
                "annotation_type_declaration",
            ],
            functions: &["method_declaration", "constructor_declaration"],
            imports: &["import_declaration"],
            calls: &["method_invocation", "object_creation_expression"],
            function_field: "name",
            accessor_types: &[],
            accessor_field: "",
        },
        "c" => Spec {
            classes: &[],
            functions: &["function_definition"],
            imports: &["preproc_include"],
            calls: &["call_expression"],
            function_field: "function",
            accessor_types: &["field_expression"],
            accessor_field: "field",
        },
        "cpp" => Spec {
            classes: &["class_specifier", "struct_specifier"],
            functions: &["function_definition"],
            imports: &["preproc_include"],
            calls: &["call_expression"],
            function_field: "function",
            accessor_types: &["field_expression", "qualified_identifier"],
            accessor_field: "field",
        },
        "ruby" => Spec {
            classes: &["class", "module"],
            functions: &["method", "singleton_method"],
            imports: &[],
            calls: &["call"],
            function_field: "method",
            accessor_types: &[],
            accessor_field: "",
        },
        "csharp" => Spec {
            classes: &[
                "class_declaration",
                "interface_declaration",
                "enum_declaration",
                "struct_declaration",
                "record_declaration",
            ],
            functions: &["method_declaration"],
            imports: &["using_directive"],
            calls: &["invocation_expression"],
            function_field: "function",
            accessor_types: &["member_access_expression"],
            accessor_field: "name",
        },
        "go" => Spec {
            classes: &["type_spec"],
            functions: &["function_declaration", "method_declaration"],
            imports: &["import_declaration"],
            calls: &["call_expression"],
            function_field: "function",
            accessor_types: &["selector_expression"],
            accessor_field: "field",
        },
        "rust" => Spec {
            classes: &["struct_item", "enum_item", "trait_item", "type_item"],
            functions: &["function_item"],
            imports: &["use_declaration"],
            calls: &["call_expression"],
            function_field: "function",
            accessor_types: &["field_expression", "scoped_identifier"],
            accessor_field: "field",
        },
        _ => Spec {
            classes: &[],
            functions: &[],
            imports: &[],
            calls: &[],
            function_field: "function",
            accessor_types: &[],
            accessor_field: "",
        },
    }
}

pub fn extract(path: &Path) -> anyhow::Result<Extraction> {
    extract_as(path, &path.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn extract_as(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let mut lang = crate::languages::for_path(path);
    if path.extension().and_then(|value| value.to_str()) == Some("m") {
        let head = fs::read_to_string(path).unwrap_or_default();
        if !["@interface", "@implementation", "@protocol", "#import"]
            .iter()
            .any(|marker| head.contains(marker))
        {
            // Objective-C and MATLAB share `.m`. MATLAB is intentionally out of
            // scope, so do not manufacture Objective-C symbols from it.
            return Ok(Extraction::default());
        }
    }
    if path.extension().and_then(|value| value.to_str()) == Some("h") {
        let head = fs::read_to_string(path).unwrap_or_default();
        if head.contains("@interface") || head.contains("@protocol") {
            return crate::fallback::extract_text(path, source_file);
        }
        if ["namespace ", "class ", "template<", "template <"]
            .iter()
            .any(|marker| head.contains(marker))
        {
            lang = crate::languages::named("cpp");
        }
    }
    let Some(lang) = lang else {
        return crate::fallback::extract_text(path, source_file);
    };
    if matches!(lang.name, "bash" | "json") {
        return crate::fallback::extract_text(path, source_file);
    }
    let source = fs::read(path)?;
    let mut parser = Parser::new();
    parser.set_language(&(lang.language)())?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter returned no tree"))?;
    let stem_owned = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .into_owned();
    let stem = stem_owned.as_str();
    let file_id = make_id(&[stem]);
    let mut state = State {
        source: &source,
        source_file,
        stem,
        file_id: file_id.clone(),
        spec: spec(lang.name),
        nodes: Vec::new(),
        edges: Vec::new(),
        seen: HashSet::new(),
        definitions: HashMap::new(),
        node_labels: HashMap::new(),
        calls: Vec::new(),
        indirect_calls: Vec::new(),
        callable_ids: HashSet::new(),
        // Unresolved calls are facts for the corpus-level second pass, not graph
        // nodes.  Emitting a node for every library/builtin call creates the same
        // phantom hubs that upstream deliberately filters out.
        emit_unresolved_calls: false,
        language_name: lang.name,
        package_scope: path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|v| v.to_str())
            .unwrap_or(stem)
            .to_owned(),
        enum_ids: HashSet::new(),
        pending_local_refs: Vec::new(),
        method_owners: HashMap::new(),
        python_parameter_types: HashMap::new(),
        python_attribute_types: HashMap::new(),
    };
    state.add_node(
        file_id.clone(),
        path.file_name()
            .and_then(|v| v.to_str())
            .unwrap_or(source_file)
            .to_owned(),
        1,
        Some("file"),
    );
    if lang.name == "python" {
        state.emit_python_docstring(tree.root_node(), &file_id);
        state.emit_python_comment_rationales(tree.root_node(), &file_id);
    }
    state.walk(tree.root_node(), None, None);
    state.resolve_local_refs();
    state.resolve_calls();
    Ok(Extraction {
        nodes: state.nodes,
        edges: state.edges,
        hyperedges: Vec::new(),
    })
}

struct CallSite {
    source: String,
    name: String,
    line: usize,
    member_call: bool,
    receiver: Option<String>,
    receiver_type: Option<String>,
}

struct State<'a> {
    source: &'a [u8],
    source_file: &'a str,
    stem: &'a str,
    file_id: String,
    spec: Spec,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    seen: HashSet<String>,
    definitions: HashMap<String, Vec<String>>,
    node_labels: HashMap<String, String>,
    calls: Vec<CallSite>,
    indirect_calls: Vec<(String, String, usize, &'static str)>,
    callable_ids: HashSet<String>,
    emit_unresolved_calls: bool,
    language_name: &'static str,
    package_scope: String,
    enum_ids: HashSet<String>,
    pending_local_refs: Vec<(String, String, String, usize)>,
    method_owners: HashMap<String, String>,
    python_parameter_types: HashMap<(String, String), String>,
    python_attribute_types: HashMap<(String, String), String>,
}

impl State<'_> {
    fn text(&self, node: TsNode<'_>) -> String {
        node.utf8_text(self.source).unwrap_or("").trim().to_owned()
    }
    fn add_node(&mut self, id: String, label: String, line: usize, kind: Option<&str>) {
        if !self.seen.insert(id.clone()) {
            return;
        }
        let mut extra = BTreeMap::new();
        extra.insert("_origin".into(), "ast".into());
        if let Some(kind) = kind {
            extra.insert("type".into(), kind.into());
        }
        let normalized_label = definition_key(&label);
        self.node_labels
            .insert(id.clone(), normalized_label.clone());
        self.definitions
            .entry(normalized_label)
            .or_default()
            .push(id.clone());
        self.nodes.push(Node {
            id,
            label,
            file_type: "code".into(),
            source_file: self.source_file.into(),
            source_location: Some(format!("L{line}")),
            community: None,
            extra,
        });
    }
    fn add_edge(
        &mut self,
        source: String,
        target: String,
        relation: &str,
        line: usize,
        confidence: Confidence,
    ) {
        let mut extra = BTreeMap::new();
        extra.insert("source_location".into(), format!("L{line}").into());
        extra.insert("weight".into(), 1.0.into());
        extra.insert("_src".into(), source.clone().into());
        extra.insert("_tgt".into(), target.clone().into());
        self.edges.push(Edge {
            source,
            target,
            relation: relation.into(),
            confidence,
            source_file: self.source_file.into(),
            extra,
        });
    }
    fn name(&self, node: TsNode<'_>) -> String {
        if let Some(name) = node.child_by_field_name("name") {
            return self.text(name);
        }
        if matches!(node.kind(), "function_definition") {
            if let Some(decl) = node.child_by_field_name("declarator") {
                return deepest_identifier(decl, self.source);
            }
        }
        node.named_children(&mut node.walk())
            .find(|child| matches!(child.kind(), "identifier" | "type_identifier" | "constant"))
            .map(|child| self.text(child))
            .unwrap_or_default()
    }
    fn is_javascript_family(&self) -> bool {
        matches!(self.language_name, "javascript" | "typescript" | "tsx")
    }
    fn variable_bound_callable<'tree>(
        &self,
        node: TsNode<'tree>,
    ) -> Option<(String, TsNode<'tree>)> {
        if !self.is_javascript_family() || node.kind() != "variable_declarator" {
            return None;
        }
        let name = node
            .child_by_field_name("name")
            .filter(|name| name.kind() == "identifier")?;
        let value = node.child_by_field_name("value")?;
        if !matches!(
            value.kind(),
            "arrow_function" | "function_expression" | "generator_function"
        ) {
            return None;
        }
        Some((self.text(name), value))
    }
    fn mark_exported(&mut self, id: &str, node: TsNode<'_>) {
        if !self.is_javascript_family() {
            return;
        }
        let declaration = if node.kind() == "variable_declarator" {
            node.parent()
        } else {
            Some(node)
        };
        let is_exported = declaration
            .and_then(|declaration| declaration.parent())
            .is_some_and(|parent| parent.kind() == "export_statement");
        if !is_exported {
            return;
        }
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            node.extra.insert("exported".into(), true.into());
        }
    }
    fn emit_function(
        &mut self,
        node: TsNode<'_>,
        export_anchor: TsNode<'_>,
        name: String,
        kind: &str,
        line: usize,
        class: Option<String>,
    ) {
        let inferred_owner = if self.language_name == "go" && kind == "method_declaration" {
            node.child_by_field_name("receiver")
                .and_then(|receiver| descendant_text(receiver, self.source, &["type_identifier"]))
                .map(|name| make_id(&[&self.package_scope, name.trim_start_matches('*')]))
        } else {
            None
        };
        let owner = class.or(inferred_owner);
        if owner.as_ref().is_some_and(|id| self.enum_ids.contains(id))
            && kind == "constructor_declaration"
        {
            return;
        }
        let id = if let Some(owner) = &owner {
            make_id(&[owner, &name])
        } else {
            make_id(&[self.stem, &name])
        };
        let label = if owner.is_some() {
            format!(".{name}()")
        } else {
            format!("{name}()")
        };
        if let Some(owner) = &owner {
            self.method_owners.insert(id.clone(), owner.clone());
        }
        self.add_node(id.clone(), label, line, Some("function"));
        self.mark_exported(&id, export_anchor);
        self.callable_ids.insert(id.clone());
        self.add_edge(
            owner.clone().unwrap_or_else(|| self.file_id.clone()),
            id.clone(),
            if owner.is_some() {
                "method"
            } else {
                "contains"
            },
            line,
            Confidence::Extracted,
        );
        if self.language_name == "python" {
            self.collect_python_parameter_types(node, &id);
            self.emit_python_docstring(node, &id);
            self.emit_python_type_refs(node, &id, line);
        }
        self.emit_type_refs(node, &id, line);
        self.collect_go_type_refs(node, &id);
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, owner.clone(), Some(id.clone()))
        }
    }
    fn walk(&mut self, node: TsNode<'_>, class: Option<String>, function: Option<String>) {
        let kind = node.kind();
        let line = node.start_position().row + 1;
        if self.language_name == "rust" && kind == "impl_item" {
            let owner_name = node
                .child_by_field_name("type")
                .map(|n| self.text(n))
                .unwrap_or_default()
                .rsplit("::")
                .next()
                .unwrap_or("")
                .to_owned();
            if !owner_name.is_empty() {
                let owner = make_id(&[self.stem, &owner_name]);
                if !self.seen.contains(&owner) {
                    self.add_node(owner.clone(), owner_name, line, Some("class"));
                    self.add_edge(
                        self.file_id.clone(),
                        owner.clone(),
                        "contains",
                        line,
                        Confidence::Extracted,
                    );
                }
                if let Some(trait_node) = node.child_by_field_name("trait") {
                    let trait_name = self
                        .text(trait_node)
                        .rsplit("::")
                        .next()
                        .unwrap_or("")
                        .to_owned();
                    if !trait_name.is_empty() {
                        let target = self.ensure_reference(&trait_name, line);
                        self.add_edge(
                            owner.clone(),
                            target,
                            "implements",
                            line,
                            Confidence::Extracted,
                        );
                    }
                }
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.walk(child, Some(owner.clone()), None);
                }
                return;
            }
        }
        if self.spec.imports.contains(&kind) {
            if self.is_javascript_family() && kind == "export_statement" {
                // Local exports wrap their definition. Re-exports have no
                // declaration and still belong in the import resolver.
                if let Some(declaration) = node.child_by_field_name("declaration") {
                    self.walk(declaration, class, function);
                } else {
                    self.import(node, line);
                }
                return;
            }
            let edge_start = self.edges.len();
            self.import(node, line);
            if self.language_name == "python" && function.is_some() {
                for edge in &mut self.edges[edge_start..] {
                    edge.extra.insert("resolution_only".into(), true.into());
                }
            }
            return;
        }
        if self.spec.classes.contains(&kind) {
            if self.language_name == "python" && function.is_some() {
                return;
            }
            let name = self.name(node);
            if name.is_empty() {
                return;
            }
            let id = if self.language_name == "go" {
                make_id(&[&self.package_scope, &name])
            } else {
                make_id(&[self.stem, &name])
            };
            self.add_node(id.clone(), name, line, Some("class"));
            self.mark_exported(&id, node);
            if self.language_name == "java" && kind == "enum_declaration" {
                self.enum_ids.insert(id.clone());
            }
            self.add_edge(
                class.clone().unwrap_or_else(|| self.file_id.clone()),
                id.clone(),
                "contains",
                line,
                Confidence::Extracted,
            );
            if self.language_name == "python" {
                self.emit_python_docstring(node, &id);
                self.emit_python_type_refs(node, &id, line);
            }
            self.emit_type_refs(node, &id, line);
            self.collect_go_type_refs(node, &id);
            self.emit_inheritance(node, &id, line);
            if self.language_name == "java" && kind == "enum_declaration" {
                let mut cases = Vec::new();
                collect_descendants(node, &["enum_constant"], &mut cases);
                for child in cases {
                    let case_name = self.name(child);
                    if case_name.is_empty() {
                        continue;
                    }
                    let case_id = make_id(&[&id, &case_name]);
                    self.add_node(
                        case_id.clone(),
                        case_name,
                        child.start_position().row + 1,
                        Some("enum_case"),
                    );
                    self.add_edge(
                        id.clone(),
                        case_id,
                        "case_of",
                        child.start_position().row + 1,
                        Confidence::Extracted,
                    );
                }
                // Java enum constructors share the enum's symbol upstream;
                // the generic walker therefore records the file containment
                // fact twice even though the node itself is de-duplicated.
                self.add_edge(
                    class.clone().unwrap_or_else(|| self.file_id.clone()),
                    id.clone(),
                    "contains",
                    line,
                    Confidence::Extracted,
                );
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                self.walk(child, Some(id.clone()), None)
            }
            return;
        }
        if self.spec.functions.contains(&kind) {
            if self.language_name == "python" && function.is_some() {
                let mut imports = Vec::new();
                collect_descendants(node, self.spec.imports, &mut imports);
                for import in imports {
                    let edge_start = self.edges.len();
                    self.import(import, import.start_position().row + 1);
                    for edge in &mut self.edges[edge_start..] {
                        edge.extra.insert("resolution_only".into(), true.into());
                    }
                }
                return;
            }
            let name = self.name(node);
            if name.is_empty() {
                return;
            }
            self.emit_function(node, node, name, kind, line, class);
            return;
        }
        if let Some((name, callable)) = self.variable_bound_callable(node) {
            self.emit_function(callable, node, name, callable.kind(), line, class);
            return;
        }
        if self.spec.calls.contains(&kind) {
            if let Some(owner) = function.clone() {
                let callee = node
                    .child_by_field_name(self.spec.function_field)
                    .or_else(|| node.child_by_field_name("type"));
                if let Some(callee) = callee {
                    let member_call = self.spec.accessor_types.contains(&callee.kind());
                    let (receiver, receiver_type) = if self.language_name == "python" && member_call
                    {
                        self.python_receiver(callee, &owner)
                    } else {
                        (None, None)
                    };
                    let name = if self.spec.accessor_types.contains(&callee.kind()) {
                        callee
                            .child_by_field_name(self.spec.accessor_field)
                            .map(|n| self.text(n))
                            .unwrap_or_else(|| {
                                self.text(callee)
                                    .rsplit(['.', ':'])
                                    .next()
                                    .unwrap_or("")
                                    .into()
                            })
                    } else {
                        self.text(callee)
                            .rsplit(['.', ':'])
                            .next()
                            .unwrap_or("")
                            .trim()
                            .into()
                    };
                    if !name.is_empty() && !python_builtin(&name) {
                        self.calls.push(CallSite {
                            source: owner.clone(),
                            name,
                            line,
                            member_call,
                            receiver,
                            receiver_type,
                        });
                    }
                }
                if self.language_name == "python" {
                    if let Some(arguments) = node.child_by_field_name("arguments") {
                        let mut cursor = arguments.walk();
                        for argument in arguments.named_children(&mut cursor) {
                            let candidate = if argument.kind() == "identifier" {
                                Some(argument)
                            } else if argument.kind() == "keyword_argument" {
                                argument.child_by_field_name("value")
                            } else {
                                None
                            };
                            if let Some(candidate) =
                                candidate.filter(|value| value.kind() == "identifier")
                            {
                                self.indirect_calls.push((
                                    owner.clone(),
                                    self.text(candidate),
                                    candidate.start_position().row + 1,
                                    "argument",
                                ));
                            }
                        }
                    }
                }
            }
        }
        if self.language_name == "python" {
            let owner = function.clone().unwrap_or_else(|| self.file_id.clone());
            if kind == "assignment" {
                self.record_python_attribute_type(node, class.as_deref(), function.as_deref());
            }
            if matches!(kind, "dictionary" | "list" | "set" | "tuple") {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    let candidate = if kind == "dictionary" && child.kind() == "pair" {
                        child.child_by_field_name("value")
                    } else if child.kind() == "identifier" {
                        Some(child)
                    } else {
                        None
                    };
                    if let Some(candidate) = candidate.filter(|value| value.kind() == "identifier")
                    {
                        self.indirect_calls.push((
                            owner.clone(),
                            self.text(candidate),
                            candidate.start_position().row + 1,
                            "collection",
                        ));
                    }
                }
            } else if matches!(kind, "return_statement" | "assignment") {
                let candidate = if kind == "assignment" {
                    node.child_by_field_name("right")
                } else {
                    node.named_child(0)
                };
                if let Some(candidate) = candidate.filter(|value| value.kind() == "identifier") {
                    self.indirect_calls.push((
                        owner,
                        self.text(candidate),
                        candidate.start_position().row + 1,
                        if kind == "return_statement" {
                            "return"
                        } else {
                            "assignment"
                        },
                    ));
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, class.clone(), function.clone())
        }
    }
    fn import(&mut self, node: TsNode<'_>, line: usize) {
        if self.language_name == "python" {
            let raw = self.text(node);
            if let Some(rest) = raw.strip_prefix("from ") {
                let module = rest.split_whitespace().next().unwrap_or("");
                if !module.is_empty() {
                    self.add_edge(
                        self.file_id.clone(),
                        make_id(&[module]),
                        "imports_from",
                        line,
                        Confidence::Extracted,
                    );
                }
            } else if let Some(rest) = raw.strip_prefix("import ") {
                for item in rest.split(',') {
                    let module = item.split_whitespace().next().unwrap_or("");
                    if !module.is_empty() {
                        self.add_edge(
                            self.file_id.clone(),
                            make_id(&[module]),
                            "imports",
                            line,
                            Confidence::Extracted,
                        );
                    }
                }
            }
            return;
        }
        if matches!(self.language_name, "javascript" | "typescript" | "tsx") {
            let raw = self.text(node);
            if raw.starts_with("export") && !raw.contains(" from ") {
                return;
            }
            let strings = quoted_values(&raw);
            let Some(module) = strings.last() else { return };
            let base = Path::new(self.source_file)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            let module_path = base.join(module.trim_start_matches("./"));
            let module_id = make_id(&[&module_path.to_string_lossy()]);
            self.add_edge(
                self.file_id.clone(),
                module_id.clone(),
                "imports_from",
                line,
                Confidence::Extracted,
            );
            if let (Some(open), Some(close)) = (raw.find('{'), raw.find('}')) {
                for symbol in raw[open + 1..close].split(',') {
                    let symbol = symbol.split_whitespace().next().unwrap_or("");
                    if !symbol.is_empty() {
                        self.add_edge(
                            self.file_id.clone(),
                            make_id(&[&module_id, symbol]),
                            "imports",
                            line,
                            Confidence::Extracted,
                        );
                    }
                }
            }
            return;
        }
        if self.language_name == "go" {
            let raw = self.text(node);
            for module in quoted_values(&raw) {
                self.add_edge(
                    self.file_id.clone(),
                    make_id(&["go_pkg", &module.replace('/', "_")]),
                    "imports_from",
                    line,
                    Confidence::Extracted,
                );
            }
            return;
        }
        if self.language_name == "java" {
            let module = self
                .text(node)
                .trim_start_matches("import ")
                .trim_start_matches("static ")
                .trim_end_matches(';')
                .to_owned();
            if !module.is_empty() {
                self.add_edge(
                    self.file_id.clone(),
                    make_id(&[&module]),
                    "imports",
                    line,
                    Confidence::Extracted,
                );
            }
            return;
        }
        if self.language_name == "rust" {
            let argument = node
                .child_by_field_name("argument")
                .map(|n| self.text(n))
                .unwrap_or_else(|| self.text(node).trim_start_matches("use ").to_owned());
            let name = argument
                .trim_end_matches(';')
                .trim_end_matches("::*")
                .rsplit("::")
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if !name.is_empty() {
                let target = make_id(&[name]);
                if !self.seen.contains(&target) {
                    self.add_reference_node(target.clone(), name.to_owned());
                }
                self.add_edge(
                    self.file_id.clone(),
                    target,
                    "imports_from",
                    line,
                    Confidence::Extracted,
                );
            }
            return;
        }
        let raw = self.text(node);
        let cleaned = raw
            .trim_start_matches("import ")
            .trim_start_matches("from ")
            .trim_start_matches("use ")
            .trim_start_matches("using ")
            .trim_start_matches("#include")
            .trim_matches(|c: char| {
                c == '"' || c == '\'' || c == '<' || c == '>' || c == ';' || c.is_whitespace()
            });
        let module = cleaned
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches(';');
        if module.is_empty() {
            return;
        }
        let target = make_id(&[module]);
        self.add_edge(
            self.file_id.clone(),
            target,
            "imports",
            line,
            Confidence::Extracted,
        );
    }
    fn emit_type_refs(&mut self, node: TsNode<'_>, owner: &str, line: usize) {
        if !matches!(self.language_name, "rust" | "java") {
            return;
        }
        let mut names = Vec::new();
        if (self.language_name == "java" && self.spec.classes.contains(&node.kind()))
            || (self.language_name == "rust" && node.kind() == "trait_item")
        {
            return;
        }
        if self.spec.functions.contains(&node.kind()) {
            let fields: &[&str] = if self.language_name == "java" {
                &["parameters", "type"]
            } else {
                &["parameters", "return_type"]
            };
            for field in fields {
                if let Some(value) = node.child_by_field_name(field) {
                    collect_descendant_text(
                        value,
                        self.source,
                        &["type_identifier", "scoped_type_identifier"],
                        &mut names,
                    );
                }
            }
            if self.language_name == "java" {
                collect_descendant_text(node, self.source, &["marker_annotation"], &mut names);
            }
        } else {
            collect_descendant_text(
                node,
                self.source,
                &["type_identifier", "scoped_type_identifier"],
                &mut names,
            );
        }
        if self.language_name == "java" {
            collect_descendant_text(node, self.source, &["marker_annotation"], &mut names);
        }
        if self.language_name != "rust" {
            names.sort();
            names.dedup();
        }
        let primitives = [
            "int", "long", "short", "byte", "float", "double", "boolean", "char", "void", "bool",
            "str", "usize", "isize", "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "f32",
            "f64",
        ];
        let java_builtins = [
            "String",
            "List",
            "ArrayList",
            "Map",
            "Set",
            "Collection",
            "Object",
            "Integer",
            "Long",
            "Double",
            "Float",
            "Boolean",
            "Character",
            "Byte",
            "Short",
            "Void",
        ];
        for raw in names {
            let name = raw
                .trim_start_matches('@')
                .rsplit("::")
                .next()
                .unwrap_or(&raw)
                .to_owned();
            if name.is_empty()
                || primitives.contains(&name.as_str())
                || (self.language_name == "java"
                    && (java_builtins.contains(&name.as_str()) || name.len() == 1))
                || normalize_name(owner) == normalize_name(&name)
            {
                continue;
            }
            let target = self.ensure_reference(&name, line);
            self.add_edge(
                owner.into(),
                target,
                "references",
                line,
                Confidence::Extracted,
            );
        }
    }
    fn emit_inheritance(&mut self, node: TsNode<'_>, owner: &str, line: usize) {
        let fields: &[(&str, &str)] = match self.language_name {
            "java" => &[("superclass", "inherits"), ("interfaces", "implements")],
            "typescript" | "tsx" | "javascript" => &[
                ("superclass", "inherits"),
                ("extends_type_clause", "inherits"),
            ],
            _ => &[],
        };
        for (field, relation) in fields {
            let Some(value) = node.child_by_field_name(field) else {
                continue;
            };
            let mut names = Vec::new();
            collect_descendant_text(
                value,
                self.source,
                &["identifier", "type_identifier"],
                &mut names,
            );
            for name in names {
                if normalize_name(owner) == normalize_name(&name) {
                    continue;
                }
                let target = self.ensure_reference(&name, line);
                self.add_edge(
                    owner.to_owned(),
                    target,
                    relation,
                    line,
                    Confidence::Extracted,
                );
            }
        }
        if self.language_name == "rust" && node.kind() == "trait_item" {
            let mut cursor = node.walk();
            for bounds in node
                .named_children(&mut cursor)
                .filter(|n| n.kind() == "trait_bounds")
            {
                let mut names = Vec::new();
                collect_descendant_text(
                    bounds,
                    self.source,
                    &["type_identifier", "scoped_type_identifier"],
                    &mut names,
                );
                for name in names {
                    let target = self.ensure_reference(&name, line);
                    self.add_edge(
                        owner.to_owned(),
                        target,
                        "inherits",
                        line,
                        Confidence::Extracted,
                    );
                }
            }
        }
    }
    fn collect_go_type_refs(&mut self, node: TsNode<'_>, owner: &str) {
        if self.language_name != "go" {
            return;
        }
        if node.kind() == "type_spec" {
            let mut fields = Vec::new();
            collect_descendants(node, &["field_declaration", "type_elem"], &mut fields);
            for field in fields {
                let relation =
                    if field.kind() == "type_elem" || field.child_by_field_name("name").is_none() {
                        "embeds"
                    } else {
                        "references"
                    };
                let mut names = Vec::new();
                collect_descendant_text(field, self.source, &["type_identifier"], &mut names);
                for name in names {
                    self.pending_local_refs.push((
                        owner.to_owned(),
                        name,
                        relation.into(),
                        field.start_position().row + 1,
                    ));
                }
            }
        } else {
            let mut names = Vec::new();
            for field in ["parameters", "result"] {
                if let Some(value) = node.child_by_field_name(field) {
                    collect_descendant_text(value, self.source, &["type_identifier"], &mut names);
                }
            }
            for name in names {
                self.pending_local_refs.push((
                    owner.to_owned(),
                    name,
                    "references".into(),
                    node.start_position().row + 1,
                ));
            }
        }
    }
    fn emit_python_docstring(&mut self, node: TsNode<'_>, owner: &str) {
        let container = node.child_by_field_name("body").unwrap_or(node);
        let mut cursor = container.walk();
        let Some(statement) = container.named_children(&mut cursor).next() else {
            return;
        };
        if statement.kind() != "expression_statement" {
            return;
        }
        let mut statement_cursor = statement.walk();
        let Some(string) = statement.named_children(&mut statement_cursor).next() else {
            return;
        };
        if !matches!(string.kind(), "string" | "concatenated_string") {
            return;
        }
        let raw = self.text(string);
        let clean = clean_docstring(&raw);
        if clean.is_empty() || (node.kind() == "module" && clean.split_whitespace().count() < 3) {
            return;
        }
        let line = string.start_position().row + 1;
        let id = make_id(&[self.stem, "rationale", &line.to_string()]);
        if self.seen.insert(id.clone()) {
            let mut label: String = clean.chars().take(80).collect();
            if clean.chars().count() > 80 {
                label.pop();
                label.push('…');
            }
            self.nodes.push(Node {
                id: id.clone(),
                label,
                file_type: "rationale".into(),
                source_file: self.source_file.into(),
                source_location: Some(format!("L{line}")),
                community: None,
                extra: BTreeMap::from([("_origin".into(), "ast".into())]),
            });
            self.add_edge(
                id,
                owner.to_owned(),
                "rationale_for",
                line,
                Confidence::Extracted,
            );
        }
    }
    fn emit_python_type_refs(&mut self, node: TsNode<'_>, owner: &str, line: usize) {
        let mut type_nodes = Vec::new();
        if let Some(parameters) = node.child_by_field_name("parameters") {
            let mut typed = Vec::new();
            collect_descendants(
                parameters,
                &["typed_parameter", "typed_default_parameter"],
                &mut typed,
            );
            type_nodes.extend(
                typed
                    .into_iter()
                    .filter_map(|parameter| parameter.child_by_field_name("type")),
            );
        }
        if let Some(return_type) = node.child_by_field_name("return_type") {
            type_nodes.push(return_type);
        }
        if node.kind() == "class_definition" {
            if let Some(superclasses) = node.child_by_field_name("superclasses") {
                type_nodes.push(superclasses);
            }
        }
        let noise = [
            "str",
            "int",
            "float",
            "bool",
            "bytes",
            "object",
            "list",
            "dict",
            "set",
            "tuple",
            "optional",
            "union",
            "none",
            "callable",
            "type",
            "self",
            "cls",
            "iterable",
            "sequence",
            "mapping",
            "frozenset",
            "filesystemeventhandler",
            "httpconnection",
            "httphandler",
            "httpredirecthandler",
            "httpsconnection",
            "httpshandler",
            "noreturn",
        ];
        let mut names = Vec::new();
        for value in type_nodes {
            collect_python_annotation_names(value, self.source, &mut names);
        }
        names.sort();
        names.dedup();
        for name in names {
            if noise.contains(&name.to_lowercase().as_str())
                && !(node.kind() == "class_definition" && name == "str")
            {
                continue;
            }
            let target = make_id(&[&name]);
            if !self.seen.contains(&target) {
                self.add_reference_node(target.clone(), name);
            }
            self.add_edge(
                owner.to_owned(),
                target,
                if node.kind() == "class_definition" {
                    "inherits"
                } else {
                    "references"
                },
                line,
                Confidence::Extracted,
            );
        }
    }
    fn emit_python_comment_rationales(&mut self, root: TsNode<'_>, owner: &str) {
        let mut comments = Vec::new();
        collect_descendants(root, &["comment"], &mut comments);
        for comment in comments {
            let raw = self.text(comment);
            if !(raw.trim_start().starts_with("# NOTE:")
                || raw.trim_start().starts_with("# IMPORTANT:"))
            {
                continue;
            }
            let line = comment.start_position().row + 1;
            let id = make_id(&[self.stem, "rationale", &line.to_string()]);
            if !self.seen.insert(id.clone()) {
                continue;
            }
            let mut label: String = raw.trim().chars().take(80).collect();
            if raw.trim().chars().count() > 80 {
                label.pop();
                label.push('…');
            }
            self.nodes.push(Node {
                id: id.clone(),
                label,
                file_type: "rationale".into(),
                source_file: self.source_file.into(),
                source_location: Some(format!("L{line}")),
                community: None,
                extra: BTreeMap::from([("_origin".into(), "ast".into())]),
            });
            self.add_edge(
                id,
                owner.to_owned(),
                "rationale_for",
                line,
                Confidence::Extracted,
            );
        }
    }
    fn add_reference_node(&mut self, id: String, label: String) {
        if !self.seen.insert(id.clone()) {
            return;
        }
        self.definitions
            .entry(normalize_name(&label))
            .or_default()
            .push(id.clone());
        self.nodes.push(Node {
            id,
            label,
            file_type: "code".into(),
            source_file: String::new(),
            source_location: None,
            community: None,
            extra: BTreeMap::from([
                ("_origin".into(), "ast".into()),
                ("origin_file".into(), self.source_file.into()),
            ]),
        });
    }
    fn ensure_reference(&mut self, name: &str, _line: usize) -> String {
        let local = self
            .definitions
            .get(&normalize_name(name))
            .filter(|ids| ids.len() == 1)
            .and_then(|ids| ids.first())
            .cloned();
        if let Some(local) = local {
            return local;
        }
        let target = make_id(&[name]);
        if !self.seen.contains(&target) {
            self.add_reference_node(target.clone(), name.to_owned());
        }
        target
    }
    fn collect_python_parameter_types(&mut self, node: TsNode<'_>, function: &str) {
        let Some(parameters) = node.child_by_field_name("parameters") else {
            return;
        };
        let mut typed_parameters = Vec::new();
        collect_descendants(
            parameters,
            &["typed_parameter", "typed_default_parameter"],
            &mut typed_parameters,
        );
        for parameter in typed_parameters {
            let parameter_name = parameter
                .child_by_field_name("name")
                .map(|name| deepest_identifier(name, self.source))
                .or_else(|| {
                    let mut cursor = parameter.walk();
                    let name = parameter
                        .named_children(&mut cursor)
                        .find(|child| child.kind() != "type")
                        .map(|name| deepest_identifier(name, self.source));
                    name
                })
                .unwrap_or_default();
            if parameter_name.is_empty() {
                continue;
            }
            let Some(annotation) = parameter.child_by_field_name("type") else {
                continue;
            };
            let mut names = Vec::new();
            collect_python_annotation_names(annotation, self.source, &mut names);
            let Some(type_name) = names
                .into_iter()
                .rev()
                .find(|name| !matches!(name.as_str(), "None" | "Optional"))
            else {
                continue;
            };
            self.python_parameter_types
                .insert((function.to_owned(), parameter_name), type_name);
        }
    }
    fn record_python_attribute_type(
        &mut self,
        node: TsNode<'_>,
        class: Option<&str>,
        function: Option<&str>,
    ) {
        let (Some(class), Some(function), Some(left), Some(right)) = (
            class,
            function,
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
        ) else {
            return;
        };
        if right.kind() != "identifier" {
            return;
        }
        let left = self.text(left);
        let Some(attribute) = left
            .strip_prefix("self.")
            .filter(|attribute| !attribute.contains('.'))
        else {
            return;
        };
        let parameter = self.text(right);
        if let Some(type_name) = self
            .python_parameter_types
            .get(&(function.to_owned(), parameter))
            .cloned()
        {
            self.python_attribute_types
                .insert((class.to_owned(), attribute.to_owned()), type_name);
        }
    }
    fn python_receiver(
        &self,
        callee: TsNode<'_>,
        function: &str,
    ) -> (Option<String>, Option<String>) {
        let Some(object) = callee.child_by_field_name("object") else {
            return (None, None);
        };
        let receiver = self.text(object);
        let receiver_type = if let Some(attribute) = receiver
            .strip_prefix("self.")
            .filter(|attribute| !attribute.contains('.'))
        {
            self.method_owners.get(function).and_then(|class| {
                self.python_attribute_types
                    .get(&(class.clone(), attribute.to_owned()))
                    .cloned()
            })
        } else if object.kind() == "identifier" {
            self.python_parameter_types
                .get(&(function.to_owned(), receiver.clone()))
                .cloned()
        } else {
            None
        };
        (Some(receiver), receiver_type)
    }
    fn annotate_call_edge(&mut self, call: &CallSite) {
        let Some(edge) = self.edges.last_mut() else {
            return;
        };
        if let Some(receiver) = &call.receiver {
            edge.extra
                .insert("receiver".into(), receiver.clone().into());
        }
        if let Some(receiver_type) = &call.receiver_type {
            edge.extra
                .insert("receiver_type".into(), receiver_type.clone().into());
        }
        let context = match (&call.receiver, &call.receiver_type) {
            (Some(receiver), Some(receiver_type)) => {
                Some(format!("receiver={receiver} type={receiver_type}"))
            }
            (Some(receiver), None) => Some(format!("receiver={receiver}")),
            _ => None,
        };
        if let Some(context) = context {
            edge.extra.insert("context".into(), context.into());
        }
    }
    fn emit_unresolved_call(&mut self, call: &CallSite) {
        let target = make_id(&["__graphoxide_call", &call.name]);
        self.add_edge(
            call.source.clone(),
            target,
            "calls",
            call.line,
            Confidence::Inferred,
        );
        if let Some(edge) = self.edges.last_mut() {
            edge.extra.insert("unresolved_call".into(), true.into());
            edge.extra.insert("callee".into(), call.name.clone().into());
        }
        self.annotate_call_edge(call);
    }
    fn resolve_calls(&mut self) {
        self.definitions.values_mut().for_each(|ids| ids.sort());
        let calls = std::mem::take(&mut self.calls);
        let mut seen_pairs = BTreeSet::new();
        for call in calls {
            let targets = self
                .definitions
                .get(&definition_key(&call.name))
                .cloned()
                .unwrap_or_default();
            let compatible: Vec<_> = targets
                .into_iter()
                .filter(|target| target != &call.source)
                .filter(|target| {
                    if self.language_name != "python" {
                        return true;
                    }
                    if !call.member_call {
                        return !self.method_owners.contains_key(target);
                    }
                    if call.receiver.as_deref() == Some("self") {
                        return self.method_owners.get(&call.source)
                            == self.method_owners.get(target);
                    }
                    let Some(receiver_type) = call.receiver_type.as_deref() else {
                        return false;
                    };
                    self.method_owners
                        .get(target)
                        .and_then(|owner| self.node_labels.get(owner))
                        .is_some_and(|owner| owner == &definition_key(receiver_type))
                })
                .collect();
            if compatible.len() == 1 {
                let target = compatible[0].clone();
                if !seen_pairs.insert((call.source.clone(), target.clone())) {
                    continue;
                }
                self.add_edge(
                    call.source.clone(),
                    target,
                    "calls",
                    call.line,
                    if self.language_name == "python"
                        && call.member_call
                        && call.receiver_type.is_some()
                    {
                        Confidence::Inferred
                    } else {
                        Confidence::Extracted
                    },
                );
                self.annotate_call_edge(&call);
            } else if self.language_name == "python" {
                let target = make_id(&["__graphoxide_call", &call.name]);
                if !seen_pairs.insert((call.source.clone(), target)) {
                    continue;
                }
                self.emit_unresolved_call(&call);
            } else if self.emit_unresolved_calls {
                let target = make_id(&[&call.name]);
                if !self.seen.contains(&target) {
                    let mut extra = BTreeMap::from([("_origin".into(), "ast".into())]);
                    extra.insert("origin_file".into(), self.source_file.into());
                    self.seen.insert(target.clone());
                    self.nodes.push(Node {
                        id: target.clone(),
                        label: format!("{}()", call.name),
                        file_type: "code".into(),
                        source_file: String::new(),
                        source_location: None,
                        community: None,
                        extra,
                    });
                }
                self.add_edge(
                    call.source,
                    target,
                    "calls",
                    call.line,
                    Confidence::Inferred,
                );
            }
        }
        let indirect = std::mem::take(&mut self.indirect_calls);
        for (source, name, line, context) in indirect {
            let callable_targets: Vec<_> = self
                .definitions
                .get(&normalize_name(&name))
                .into_iter()
                .flatten()
                .filter(|target| self.callable_ids.contains(*target))
                .cloned()
                .collect();
            let target = (callable_targets.len() == 1).then(|| callable_targets[0].clone());
            if let Some(target) = target {
                if target == source || seen_pairs.contains(&(source.clone(), target.clone())) {
                    continue;
                }
                self.add_edge(
                    source.clone(),
                    target.clone(),
                    "indirect_call",
                    line,
                    Confidence::Inferred,
                );
                if let Some(edge) = self.edges.last_mut() {
                    edge.extra.insert("context".into(), context.into());
                }
            } else if !name.is_empty() && !python_builtin(&name) {
                let target = make_id(&["__graphoxide_call", &name]);
                self.add_edge(source, target, "indirect_call", line, Confidence::Inferred);
                if let Some(edge) = self.edges.last_mut() {
                    edge.extra.insert("unresolved_call".into(), true.into());
                    edge.extra.insert("callee".into(), name.into());
                    edge.extra.insert("context".into(), context.into());
                }
            }
        }
    }
    fn resolve_local_refs(&mut self) {
        let refs = std::mem::take(&mut self.pending_local_refs);
        for (source, name, relation, line) in refs {
            let key = normalize_name(&name);
            if let Some(target) = self
                .definitions
                .get(&key)
                .filter(|ids| ids.len() == 1)
                .and_then(|ids| ids.first())
                .cloned()
            {
                if target != source {
                    self.add_edge(source, target, &relation, line, Confidence::Extracted);
                }
            }
        }
    }
}

fn deepest_identifier(node: TsNode<'_>, source: &[u8]) -> String {
    if matches!(node.kind(), "identifier" | "field_identifier") {
        return node.utf8_text(source).unwrap_or("").into();
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let value = deepest_identifier(child, source);
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

fn descendant_text<'a>(node: TsNode<'a>, source: &'a [u8], kinds: &[&str]) -> Option<String> {
    if kinds.contains(&node.kind()) {
        return node.utf8_text(source).ok().map(str::to_owned);
    }
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find_map(|child| descendant_text(child, source, kinds));
    found
}

fn collect_descendant_text(node: TsNode<'_>, source: &[u8], kinds: &[&str], out: &mut Vec<String>) {
    if kinds.contains(&node.kind()) {
        if let Ok(text) = node.utf8_text(source) {
            out.push(text.to_owned());
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_descendant_text(child, source, kinds, out);
    }
}

fn collect_descendants<'a>(node: TsNode<'a>, kinds: &[&str], out: &mut Vec<TsNode<'a>>) {
    if kinds.contains(&node.kind()) {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_descendants(child, kinds, out);
    }
}

fn collect_python_annotation_names(node: TsNode<'_>, source: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "attribute" => {
            if let Some(name) = node.child_by_field_name("attribute") {
                out.push(name.utf8_text(source).unwrap_or("").to_owned());
            }
            return;
        }
        "identifier" => {
            out.push(node.utf8_text(source).unwrap_or("").to_owned());
            return;
        }
        "string" => {
            let value = clean_docstring(node.utf8_text(source).unwrap_or(""));
            if value == "str" {
                out.push(value);
            }
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_python_annotation_names(child, source, out);
    }
}

fn quoted_values(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut quote = None;
    let mut current = String::new();
    for ch in value.chars() {
        match quote {
            Some(active) if ch == active => {
                values.push(std::mem::take(&mut current));
                quote = None;
            }
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' || ch == '`' => quote = Some(ch),
            None => {}
        }
    }
    values
}

fn clean_docstring(raw: &str) -> String {
    let mut value = raw.trim();
    while value
        .chars()
        .next()
        .is_some_and(|c| matches!(c, 'r' | 'R' | 'u' | 'U' | 'f' | 'F' | 'b' | 'B'))
    {
        value = &value[1..];
    }
    for quote in ["\"\"\"", "'''", "\"", "'"] {
        if value.starts_with(quote) && value.ends_with(quote) && value.len() >= quote.len() * 2 {
            value = &value[quote.len()..value.len() - quote.len()];
            break;
        }
    }
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_name(value: &str) -> String {
    value
        .rsplit('_')
        .next()
        .unwrap_or(value)
        .trim_matches('.')
        .trim_end_matches("()")
        .to_lowercase()
}

fn definition_key(value: &str) -> String {
    value
        .trim_start_matches('.')
        .trim_end_matches("()")
        .to_lowercase()
}

fn python_builtin(value: &str) -> bool {
    matches!(
        value,
        "abs"
            | "all"
            | "any"
            | "bool"
            | "bytes"
            | "callable"
            | "chr"
            | "dict"
            | "dir"
            | "enumerate"
            | "filter"
            | "float"
            | "format"
            | "getattr"
            | "hasattr"
            | "hash"
            | "help"
            | "hex"
            | "id"
            | "int"
            | "isinstance"
            | "issubclass"
            | "iter"
            | "len"
            | "list"
            | "map"
            | "max"
            | "min"
            | "next"
            | "object"
            | "open"
            | "ord"
            | "print"
            | "property"
            | "range"
            | "repr"
            | "reversed"
            | "round"
            | "set"
            | "setattr"
            | "slice"
            | "sorted"
            | "str"
            | "sum"
            | "super"
            | "tuple"
            | "type"
            | "vars"
            | "zip"
    )
}
