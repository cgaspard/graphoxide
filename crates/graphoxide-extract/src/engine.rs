//! Shared tree-sitter extraction driver for the compiled language set.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
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
            functions: &["function_definition", "function_declarator"],
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

pub(crate) fn has_ast_extractor(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "r" {
        return false;
    }
    if extension == "m" {
        let head = fs::read_to_string(path).unwrap_or_default();
        return ["@interface", "@implementation", "@protocol", "#import"]
            .iter()
            .any(|marker| head.contains(marker));
    }
    true
}

pub(crate) fn extract_as(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let mut lang = crate::languages::for_path(path);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if let Some(extractor) = crate::extractor_registry::extractor_for_path(path) {
        return (extractor.extract)(path, source_file);
    }
    if crate::dotnet::supports_extension(&extension) {
        return crate::dotnet::extract_dotnet(path, source_file, &extension);
    }
    let shebang = extension
        .is_empty()
        .then(|| crate::detect::shebang_interpreter(path))
        .flatten();
    if matches!(extension.as_str(), "sh" | "bash" | "zsh")
        || shebang
            .as_deref()
            .is_some_and(|name| matches!(name, "bash" | "sh" | "dash" | "zsh" | "ksh"))
    {
        return crate::bash::extract_bash(path, source_file);
    }
    if extension == "dart" {
        return crate::dart::extract_dart(path, source_file);
    }
    if crate::pascal::supports_extension(&extension) {
        return crate::pascal::extract_pascal_family(path, source_file, &extension);
    }
    if extension == "sql" {
        return crate::sql::extract_sql(path, source_file);
    }
    if extension == "r" {
        // R is classified as code, but no sound AST extractor is compiled in
        // yet. Returning an empty extraction lets the batch API surface the
        // unsupported-language warning instead of manufacturing regex nodes.
        return Ok(Extraction::default());
    }
    if extension.is_empty()
        && shebang
            .as_deref()
            .is_some_and(|name| matches!(name, "python" | "python2" | "python3"))
    {
        lang = crate::languages::named("python");
    }
    if extension.is_empty() && lang.is_none() {
        return Ok(Extraction::default());
    }
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "sv" | "svh"))
    {
        return crate::fallback::extract_text(path, source_file);
    }
    if extension == "m" {
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
    if extension == "h" {
        let head = fs::read_to_string(path).unwrap_or_default();
        if head.contains("@interface") || head.contains("@protocol") || head.contains("#import") {
            return crate::fallback::extract_text(path, source_file);
        }
        if ["namespace ", "class ", "template<", "template <"]
            .iter()
            .any(|marker| head.contains(marker))
        {
            lang = crate::languages::named("cpp");
        }
    }
    let prepared_sfc = crate::sfc::prepare(path)?;
    if let Some(prepared) = &prepared_sfc {
        lang = crate::languages::named(prepared.language);
    }
    let Some(lang) = lang else {
        return crate::fallback::extract_text(path, source_file);
    };
    if matches!(lang.name, "bash" | "json") {
        return crate::fallback::extract_text(path, source_file);
    }
    let source = prepared_sfc
        .as_ref()
        .map(|prepared| prepared.original.clone())
        .map_or_else(|| fs::read(path), Ok)?;
    let parser_source = prepared_sfc
        .as_ref()
        .map(|prepared| prepared.parser.clone())
        .unwrap_or_else(|| parser_compatible_source(&source, lang.name, path));
    let mut parser = Parser::new();
    parser.set_language(&(lang.language)())?;
    let tree = parser
        .parse(&parser_source, None)
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
        physical_path: path.to_path_buf(),
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
        javascript_injected_fields: HashMap::new(),
        javascript_receiver_types: collect_typescript_receiver_types(tree.root_node(), &source),
        go_receiver_types: HashMap::new(),
        java_type_parameters: collect_java_type_parameters(tree.root_node(), &source),
        java_package: collect_java_package(tree.root_node(), &source),
        declared_interfaces: collect_declared_interfaces(tree.root_node(), &source),
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
    if lang.name == "java" && !state.java_package.is_empty() {
        if let Some(file) = state.nodes.first_mut() {
            file.extra.insert(
                crate::java::PACKAGE.into(),
                state.java_package.clone().into(),
            );
        }
    }
    let diagnostics = syntax_diagnostics(tree.root_node(), &source, lang.name);
    if tree.root_node().has_error()
        || diagnostics.error_nodes > 0
        || diagnostics.missing_nodes > 0
        || !diagnostics.compatibility_spans.is_empty()
    {
        let file_node = state
            .nodes
            .first_mut()
            .expect("the AST extractor always emits a file anchor");
        file_node
            .extra
            .insert("parser_has_error".into(), true.into());
        file_node
            .extra
            .insert("parse_error_count".into(), diagnostics.error_nodes.into());
        file_node.extra.insert(
            "missing_node_count".into(),
            diagnostics.missing_nodes.into(),
        );
        file_node
            .extra
            .insert("parse_error_spans".into(), diagnostics.error_spans.into());
        file_node.extra.insert(
            "parser_compatibility_count".into(),
            diagnostics.compatibility_spans.len().into(),
        );
        file_node.extra.insert(
            "parser_compatibility_spans".into(),
            diagnostics.compatibility_spans.into(),
        );
    }
    if lang.name == "python" {
        state.emit_python_docstring(tree.root_node(), &file_id);
        state.emit_python_comment_rationales(tree.root_node(), &file_id);
    } else if matches!(lang.name, "javascript" | "typescript" | "tsx") {
        state.emit_javascript_comment_metadata(tree.root_node(), &file_id);
    }
    state.walk(tree.root_node(), None, None);
    state.resolve_local_refs();
    state.resolve_calls();
    let mut extraction = Extraction {
        nodes: state.nodes,
        edges: state.edges,
        hyperedges: Vec::new(),
    };
    if prepared_sfc.is_some() {
        crate::sfc::augment_imports(&mut extraction, path, source_file, &source, &parser_source);
    }
    Ok(extraction)
}

fn parser_compatible_source(source: &[u8], language_name: &str, path: &Path) -> Vec<u8> {
    if language_name != "cpp"
        || !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "cu" | "cuh" | "metal"
                )
            })
    {
        return source.to_vec();
    }
    let mut compatible = source.to_vec();
    for qualifier in [
        b"__global__".as_slice(),
        b"__device__".as_slice(),
        b"__host__".as_slice(),
        b"constant".as_slice(),
        b"threadgroup".as_slice(),
        b"device".as_slice(),
        b"kernel".as_slice(),
        b"thread".as_slice(),
    ] {
        let mut start = 0;
        while let Some(relative) = compatible[start..]
            .windows(qualifier.len())
            .position(|window| window == qualifier)
        {
            let offset = start + relative;
            let before_is_word = offset > 0
                && (compatible[offset - 1].is_ascii_alphanumeric()
                    || compatible[offset - 1] == b'_');
            let after = offset + qualifier.len();
            let after_is_word = after < compatible.len()
                && (compatible[after].is_ascii_alphanumeric() || compatible[after] == b'_');
            if !before_is_word && !after_is_word {
                compatible[offset..after].fill(b' ');
            }
            start = after;
        }
    }
    compatible
}

struct CallSite {
    source: String,
    name: String,
    line: usize,
    member_call: bool,
    receiver: Option<String>,
    receiver_type: Option<String>,
    ruby_constant_receiver: bool,
}

#[derive(Default)]
struct SyntaxDiagnostics {
    error_nodes: usize,
    missing_nodes: usize,
    error_spans: Vec<String>,
    compatibility_spans: Vec<String>,
}

fn syntax_diagnostics(root: TsNode<'_>, source: &[u8], language_name: &str) -> SyntaxDiagnostics {
    let mut diagnostics = SyntaxDiagnostics::default();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.is_error() {
            if rust_raw_reference_compatibility(node, source, language_name) {
                if diagnostics.compatibility_spans.len() < 20 {
                    diagnostics
                        .compatibility_spans
                        .push(format_syntax_span(node, "Rust 2021 `&raw` identifier"));
                }
            } else {
                diagnostics.error_nodes += 1;
                if diagnostics.error_spans.len() < 20 {
                    diagnostics
                        .error_spans
                        .push(format_syntax_span(node, node.kind()));
                }
            }
        }
        if node.is_missing() {
            diagnostics.missing_nodes += 1;
            if diagnostics.error_spans.len() < 20 {
                diagnostics.error_spans.push(format_syntax_span(
                    node,
                    &format!("missing {}", node.kind()),
                ));
            }
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    diagnostics.error_spans.sort();
    diagnostics.compatibility_spans.sort();
    diagnostics
}

fn rust_raw_reference_compatibility(node: TsNode<'_>, source: &[u8], language_name: &str) -> bool {
    if language_name != "rust" {
        return false;
    }
    let text = node.utf8_text(source).unwrap_or("").trim();
    let raw_reference = text == "&raw"
        || (text == "raw"
            && source[..node.start_byte()]
                .iter()
                .rev()
                .find(|byte| !byte.is_ascii_whitespace())
                == Some(&b'&'));
    let suffix = std::str::from_utf8(&source[node.end_byte()..])
        .unwrap_or("")
        .trim_start();
    raw_reference && !suffix.starts_with("const") && !suffix.starts_with("mut")
}

fn format_syntax_span(node: TsNode<'_>, kind: &str) -> String {
    let start = node.start_position();
    let end = node.end_position();
    format!(
        "{kind} L{}:{}-L{}:{}",
        start.row + 1,
        start.column + 1,
        end.row + 1,
        end.column + 1
    )
}

struct State<'a> {
    source: &'a [u8],
    source_file: &'a str,
    physical_path: PathBuf,
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
    language_name: &'static str,
    package_scope: String,
    enum_ids: HashSet<String>,
    pending_local_refs: Vec<(String, String, String, Option<&'static str>, usize)>,
    method_owners: HashMap<String, String>,
    python_parameter_types: HashMap<(String, String), String>,
    python_attribute_types: HashMap<(String, String), String>,
    javascript_injected_fields: HashMap<(String, String), String>,
    javascript_receiver_types: HashMap<String, String>,
    go_receiver_types: HashMap<(String, String), String>,
    java_type_parameters: HashSet<String>,
    java_package: String,
    declared_interfaces: HashSet<String>,
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
    fn annotate_edges_since(&mut self, start: usize, key: &str, value: &str) {
        for edge in &mut self.edges[start..] {
            edge.extra.entry(key.into()).or_insert_with(|| value.into());
        }
    }
    fn add_csharp_namespace_node(
        &mut self,
        context: &crate::csharp::NamespaceContext,
        line: usize,
    ) -> String {
        if context.namespace.is_empty() {
            return self.file_id.clone();
        }
        let id = crate::csharp::namespace_id(&context.namespace);
        if self.seen.insert(id.clone()) {
            let metadata = serde_json::json!({
                "namespace": context.namespace,
                "scope_chain": context.scope_chain,
            });
            self.nodes.push(Node {
                id: id.clone(),
                label: context.namespace.clone(),
                file_type: "code".into(),
                source_file: self.source_file.into(),
                source_location: Some(format!("L{line}")),
                community: None,
                extra: BTreeMap::from([
                    ("_origin".into(), "ast".into()),
                    ("type".into(), "namespace".into()),
                    (crate::csharp::MANAGED_NODE.into(), true.into()),
                    (crate::csharp::NAMESPACE_NODE.into(), true.into()),
                    ("metadata".into(), metadata),
                ]),
            });
        }
        id
    }
    fn stamp_csharp_declaration(
        &mut self,
        id: &str,
        node: TsNode<'_>,
        parent: Option<&str>,
        name: &str,
        context: &crate::csharp::NamespaceContext,
    ) {
        let parent_fqn = parent.and_then(|parent| {
            self.nodes
                .iter()
                .find(|candidate| candidate.id == parent)
                .and_then(|candidate| candidate.extra.get("metadata"))
                .and_then(|metadata| metadata.get("fqn"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
        let fqn = parent_fqn.map_or_else(
            || {
                if context.namespace.is_empty() {
                    name.to_owned()
                } else {
                    format!("{}.{}", context.namespace, name)
                }
            },
            |parent| format!("{parent}.{name}"),
        );
        let declaration_kind = node.kind().trim_end_matches("_declaration");
        let parameters = crate::csharp::type_parameters(node, self.source, name);
        let mut metadata = serde_json::Map::new();
        metadata.insert("fqn".into(), fqn.into());
        metadata.insert("declaration_kind".into(), declaration_kind.into());
        metadata.insert("scope_chain".into(), context.scope_chain.clone().into());
        if !context.namespace.is_empty() {
            metadata.insert("namespace".into(), context.namespace.clone().into());
        }
        if parent.is_some() {
            metadata.insert("is_nested_type".into(), true.into());
        }
        if crate::csharp::is_partial_declaration(node, self.source) {
            metadata.insert("partial".into(), true.into());
        }
        if !parameters.is_empty() {
            metadata.insert("type_parameters".into(), parameters.into());
        }
        if let Some(declaration) = self.nodes.iter_mut().find(|candidate| candidate.id == id) {
            declaration
                .extra
                .insert("metadata".into(), serde_json::Value::Object(metadata));
        }
    }
    fn emit_csharp_import(&mut self, node: TsNode<'_>, line: usize) -> bool {
        if self.language_name != "csharp" {
            return false;
        }
        let Some(using) = crate::csharp::parse_using(node, self.source) else {
            return true;
        };
        let target = if using.kind == "namespace" {
            crate::csharp::namespace_id(&using.target_fqn)
        } else {
            make_id(&[
                "__csharp_import",
                using.kind,
                &using.target_fqn,
                using.alias.as_deref().unwrap_or(""),
            ])
        };
        self.add_edge(
            self.file_id.clone(),
            target,
            "imports",
            line,
            Confidence::Extracted,
        );
        let mut metadata = serde_json::Map::new();
        metadata.insert("using_kind".into(), using.kind.into());
        metadata.insert("target_fqn".into(), using.target_fqn.into());
        metadata.insert("scope_chain".into(), using.scope_chain.into());
        if let Some(alias) = using.alias {
            metadata.insert("alias".into(), alias.into());
        }
        if using.global {
            metadata.insert("global".into(), true.into());
        }
        if let Some(edge) = self.edges.last_mut() {
            edge.extra
                .insert(crate::csharp::IMPORT_EDGE.into(), true.into());
            edge.extra
                .insert("metadata".into(), serde_json::Value::Object(metadata));
        }
        true
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
        if self.language_name == "cpp" && node.kind() == "function_declarator" {
            return deepest_identifier(node, self.source);
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
    fn javascript_module_scope(&self, node: TsNode<'_>) -> bool {
        if !self.is_javascript_family() {
            return false;
        }
        let mut ancestor = node.parent();
        while let Some(parent) = ancestor {
            if matches!(
                parent.kind(),
                "arrow_function"
                    | "function_declaration"
                    | "function_expression"
                    | "generator_function"
                    | "generator_function_declaration"
                    | "method_definition"
            ) {
                return false;
            }
            if parent.kind() == "program" {
                return true;
            }
            ancestor = parent.parent();
        }
        false
    }
    fn indirect_name_shadowed(&self, site: TsNode<'_>, name: &str) -> bool {
        let mut ancestor = site.parent();
        let mut inside_callable = false;
        let mut root = site;
        while let Some(node) = ancestor {
            root = node;
            if indirect_scope_boundary(self.language_name, node.kind()) {
                inside_callable = true;
                if indirect_scope_binds(node, name, self.source, self.language_name) {
                    return true;
                }
            }
            ancestor = node.parent();
        }
        !inside_callable && indirect_module_rebinds(root, name, self.source, self.language_name)
    }
    fn record_indirect_identifier(&mut self, node: TsNode<'_>, owner: &str, context: &'static str) {
        if !matches!(node.kind(), "identifier" | "shorthand_property_identifier") {
            return;
        }
        let name = self.text(node);
        if name.is_empty()
            || matches!(name.as_str(), "self" | "cls")
            || self.indirect_name_shadowed(node, &name)
        {
            return;
        }
        self.indirect_calls.push((
            owner.to_owned(),
            name,
            node.start_position().row + 1,
            context,
        ));
    }
    fn emit_javascript_module_variable(&mut self, node: TsNode<'_>, line: usize) {
        if node.kind() != "variable_declarator" || !self.javascript_module_scope(node) {
            return;
        }
        let Some(declaration) = node
            .parent()
            .filter(|parent| parent.kind() == "lexical_declaration")
        else {
            return;
        };
        let Some(name) = node
            .child_by_field_name("name")
            .filter(|name| name.kind() == "identifier")
        else {
            return;
        };
        let name = self.text(name);
        if graphoxide_core::normalize_id(&name).is_empty() {
            return;
        }
        let Some(value) = node.child_by_field_name("value") else {
            return;
        };
        let exported = declaration
            .parent()
            .is_some_and(|parent| parent.kind() == "export_statement");
        if !exported
            && !matches!(
                value.kind(),
                "object" | "array" | "as_expression" | "call_expression" | "new_expression"
            )
        {
            return;
        }
        let id = make_id(&[self.stem, &name]);
        self.add_node(id.clone(), name, line, Some("variable"));
        self.mark_exported(&id, node);
        self.add_edge(
            self.file_id.clone(),
            id,
            "contains",
            line,
            Confidence::Extracted,
        );
    }
    fn javascript_module_id(&self, module: &str) -> String {
        let base = Path::new(self.source_file)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let mut module_path = if module.starts_with('.') {
            base.join(module)
        } else {
            PathBuf::from(module.replace('.', "/"))
        };
        if module_path.extension().is_some_and(|extension| {
            matches!(
                extension.to_string_lossy().to_ascii_lowercase().as_str(),
                "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts"
            )
        }) {
            module_path.set_extension("");
        }
        make_id(&[&module_path.to_string_lossy().replace('\\', "/")])
    }
    fn emit_javascript_require(&mut self, node: TsNode<'_>, line: usize) {
        if !self.is_javascript_family() || node.kind() != "variable_declarator" {
            return;
        }
        let raw = self.text(node);
        let require = Regex::new(r#"require\s*\(\s*['\"]([^'\"]+)['\"]\s*\)"#)
            .expect("valid CommonJS require regex");
        let Some(capture) = require.captures(&raw) else {
            return;
        };
        let module_id = self.javascript_module_id(&capture[1]);
        if module_id.is_empty() {
            return;
        }
        let start = self.edges.len();
        self.add_edge(
            self.file_id.clone(),
            module_id.clone(),
            "imports_from",
            line,
            Confidence::Extracted,
        );
        if let Some(binding) = node.child_by_field_name("name") {
            let binding = self.text(binding);
            if binding.trim_start().starts_with('{') {
                for item in binding
                    .trim()
                    .trim_start_matches('{')
                    .trim_end_matches('}')
                    .split(',')
                {
                    let imported = item.trim().split([':', '=']).next().unwrap_or("").trim();
                    if !imported.is_empty() {
                        self.add_edge(
                            self.file_id.clone(),
                            make_id(&[&module_id, imported]),
                            "imports",
                            line,
                            Confidence::Extracted,
                        );
                    }
                }
            }
        }
        let tail = &raw[capture.get(0).expect("whole require call").end()..];
        if let Some(property) = tail.trim_start().strip_prefix('.') {
            let property = property
                .split(|character: char| !character.is_alphanumeric() && character != '_')
                .next()
                .unwrap_or("");
            if !property.is_empty() {
                self.add_edge(
                    self.file_id.clone(),
                    make_id(&[&module_id, property]),
                    "imports",
                    line,
                    Confidence::Extracted,
                );
            }
        }
        self.annotate_edges_since(start, "context", "import");
    }
    fn emit_javascript_assigned_callable(
        &mut self,
        node: TsNode<'_>,
        class: Option<&str>,
        function: Option<&str>,
        line: usize,
    ) -> bool {
        if !self.is_javascript_family() {
            return false;
        }
        let (left, value) = if node.kind() == "assignment_expression" {
            (
                node.child_by_field_name("left"),
                node.child_by_field_name("right"),
            )
        } else if matches!(node.kind(), "public_field_definition" | "field_definition") {
            (
                node.child_by_field_name("name"),
                node.child_by_field_name("value"),
            )
        } else {
            return false;
        };
        let (Some(left), Some(value)) = (left, value) else {
            return false;
        };
        if !matches!(
            value.kind(),
            "arrow_function" | "function_expression" | "generator_function"
        ) {
            return false;
        }
        let left_text = self.text(left);
        let (name, owner, exported) = if let Some(name) = left_text.strip_prefix("this.") {
            let Some(owner) = function.or(class) else {
                return false;
            };
            (name, Some(owner.to_owned()), false)
        } else if let Some(name) = left_text
            .strip_prefix("module.exports.")
            .or_else(|| left_text.strip_prefix("exports."))
        {
            (name, None, true)
        } else if let Some((type_name, name)) = left_text.split_once(".prototype.") {
            let owner = self
                .definitions
                .get(&normalize_name(type_name))
                .and_then(|ids| (ids.len() == 1).then(|| ids[0].clone()))
                .unwrap_or_else(|| make_id(&[self.stem, type_name]));
            (name, Some(owner), false)
        } else if matches!(node.kind(), "public_field_definition" | "field_definition") {
            let Some(owner) = class else { return false };
            (left_text.as_str(), Some(owner.to_owned()), false)
        } else {
            return false;
        };
        let name = name.trim();
        if name.is_empty() || graphoxide_core::normalize_id(name).is_empty() {
            return true;
        }
        let id = owner
            .as_ref()
            .map(|owner| make_id(&[owner, name]))
            .unwrap_or_else(|| make_id(&[self.stem, name]));
        let label = if owner.is_some() {
            format!(".{name}()")
        } else {
            format!("{name}()")
        };
        if let Some(owner) = &owner {
            self.method_owners.insert(id.clone(), owner.clone());
        }
        self.add_node(id.clone(), label, line, Some("function"));
        self.callable_ids.insert(id.clone());
        if exported {
            if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
                node.extra.insert("exported".into(), true.into());
            }
        }
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
        let mut cursor = value.walk();
        for child in value.named_children(&mut cursor) {
            self.walk(child, owner.clone(), Some(id.clone()));
        }
        true
    }
    fn emit_javascript_dynamic_import(
        &mut self,
        node: TsNode<'_>,
        owner: Option<&str>,
        line: usize,
    ) -> bool {
        if !self.is_javascript_family() || node.kind() != "call_expression" {
            return false;
        }
        let Some(callee) = node.child_by_field_name("function") else {
            return false;
        };
        if self.text(callee).trim() != "import" {
            return false;
        }
        let Some(arguments) = node.child_by_field_name("arguments") else {
            return true;
        };
        let Some(argument) = arguments.named_child(0) else {
            return true;
        };
        if !matches!(argument.kind(), "string" | "template_string") {
            return true;
        }
        let raw = self.text(argument);
        if raw.contains("${") {
            return true;
        }
        let module = raw.trim().trim_matches(['\'', '"', '`']);
        if module.is_empty() {
            return true;
        }
        let base = Path::new(self.source_file)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let module_path = base.join(module.trim_start_matches("./"));
        let target = make_id(&[&module_path.to_string_lossy()]);
        let edge_start = self.edges.len();
        self.add_edge(
            owner.unwrap_or(&self.file_id).to_owned(),
            target,
            "imports_from",
            line,
            Confidence::Extracted,
        );
        self.annotate_edges_since(edge_start, "context", "import");
        true
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
    fn ruby_owner_label(&self, owner: Option<&str>) -> Option<&str> {
        owner.and_then(|owner| {
            self.nodes
                .iter()
                .find(|node| node.id == owner)
                .map(|node| node.label.as_str())
        })
    }
    fn stamp_ruby_declaration(&mut self, id: &str, kind: &str) {
        if self.language_name != "ruby" {
            return;
        }
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            node.extra
                .insert(crate::ruby::DECLARATION_KIND.into(), kind.into());
        }
    }
    fn emit_ruby_dynamic_type(
        &mut self,
        node: TsNode<'_>,
        parent: Option<&str>,
        line: usize,
    ) -> bool {
        if self.language_name != "ruby" {
            return false;
        }
        let Some(fact) = crate::ruby::dynamic_type(node, self.source) else {
            return false;
        };
        let name = crate::ruby::qualify_declaration(&fact.name, self.ruby_owner_label(parent));
        let id = make_id(&[self.stem, &name]);
        self.add_node(id.clone(), name, line, Some("class"));
        self.stamp_ruby_declaration(&id, fact.kind);
        self.add_edge(
            parent.unwrap_or(&self.file_id).to_owned(),
            id.clone(),
            "contains",
            line,
            Confidence::Extracted,
        );
        if let Some(superclass) = fact.superclass {
            let target = self.ensure_reference(&superclass, line);
            self.add_edge(id.clone(), target, "inherits", line, Confidence::Extracted);
        }
        if let Some(block) = fact.block {
            self.walk(block, Some(id), None);
        }
        true
    }
    fn emit_ruby_require_relative(&mut self, node: TsNode<'_>, line: usize) -> bool {
        if self.language_name != "ruby" {
            return false;
        }
        let Some(module) = crate::ruby::require_relative(node, self.source) else {
            return false;
        };
        let target = crate::ruby::require_target(self.source_file, &module);
        self.add_edge(
            self.file_id.clone(),
            target,
            "imports_from",
            line,
            Confidence::Extracted,
        );
        if let Some(edge) = self.edges.last_mut() {
            edge.extra.insert("context".into(), "import".into());
        }
        true
    }
    fn emit_ruby_mixins(&mut self, node: TsNode<'_>, owner: Option<&str>, line: usize) -> bool {
        if self.language_name != "ruby" {
            return false;
        }
        let Some(names) = crate::ruby::mixin_names(node, self.source) else {
            return false;
        };
        let Some(kind) = crate::ruby::mixin_kind(node, self.source) else {
            return false;
        };
        let Some(owner) = owner else {
            return true;
        };
        for name in names {
            let target = make_id(&["__ruby_mixin", &name]);
            self.add_edge(
                owner.to_owned(),
                target,
                crate::ruby::RAW_MIXIN_RELATION,
                line,
                Confidence::Extracted,
            );
            if let Some(edge) = self.edges.last_mut() {
                edge.extra
                    .insert(crate::ruby::MIXIN_NAME.into(), name.into());
                edge.extra
                    .insert(crate::ruby::MIXIN_KIND.into(), kind.into());
            }
        }
        true
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
                .map(|name| {
                    let name = name.trim_start_matches('*');
                    self.definitions
                        .get(&normalize_name(name))
                        .and_then(|ids| (ids.len() == 1).then(|| ids[0].clone()))
                        .unwrap_or_else(|| make_id(&[&self.package_scope, name]))
                })
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
        let label = if self.language_name == "cpp" && owner.is_some() {
            name.clone()
        } else if owner.is_some() {
            format!(".{name}()")
        } else {
            format!("{name}()")
        };
        if let Some(owner) = &owner {
            self.method_owners.insert(id.clone(), owner.clone());
        }
        self.add_node(id.clone(), label, line, Some("function"));
        self.record_go_receiver_types(node, &id);
        if self.language_name == "ruby" && crate::ruby::method_is_singleton(node, self.source) {
            if let Some(method) = self.nodes.iter_mut().find(|method| method.id == id) {
                method
                    .extra
                    .insert(crate::ruby::SINGLETON_METHOD.into(), true.into());
            }
        }
        self.emit_typescript_decorators(node, &id, true);
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
    fn python_decorator_name(&self, decorator: TsNode<'_>) -> Option<String> {
        let mut cursor = decorator.walk();
        let mut target = decorator.named_children(&mut cursor).next()?;
        if target.kind() == "call" {
            target = target.child_by_field_name("function")?;
        }
        if target.kind() == "attribute" {
            return target
                .child_by_field_name("attribute")
                .map(|attribute| self.text(attribute))
                .filter(|name| !name.is_empty());
        }
        (target.kind() == "identifier")
            .then(|| self.text(target))
            .filter(|name| !name.is_empty())
    }
    fn typescript_decorator_name(&self, decorator: TsNode<'_>) -> Option<String> {
        let raw = self.text(decorator);
        let target = raw
            .trim()
            .trim_start_matches('@')
            .split(['(', '<'])
            .next()
            .unwrap_or("")
            .trim()
            .rsplit('.')
            .next()
            .unwrap_or("")
            .trim();
        (!target.is_empty()).then(|| target.to_owned())
    }
    fn emit_typescript_decorators(&mut self, node: TsNode<'_>, owner: &str, recursive: bool) {
        if !matches!(self.language_name, "typescript" | "tsx") {
            return;
        }
        let mut decorators = Vec::new();
        // The TypeScript grammar represents decorators in two different ways:
        // class/field decorators can be children of the declaration, while
        // decorators on exported declarations and methods are contiguous named
        // siblings immediately before the declaration they decorate.
        let mut sibling = node.prev_named_sibling();
        while let Some(candidate) = sibling.filter(|candidate| candidate.kind() == "decorator") {
            decorators.push(candidate);
            sibling = candidate.prev_named_sibling();
        }
        if recursive {
            collect_descendants(node, &["decorator"], &mut decorators);
        } else {
            let mut cursor = node.walk();
            decorators.extend(
                node.named_children(&mut cursor)
                    .filter(|child| child.kind() == "decorator"),
            );
        }
        for decorator in decorators {
            let Some(name) = self.typescript_decorator_name(decorator) else {
                continue;
            };
            let line = decorator.start_position().row + 1;
            let target = self.ensure_reference(&name, line);
            if target == owner {
                continue;
            }
            let edge_start = self.edges.len();
            self.add_edge(
                owner.to_owned(),
                target,
                "references",
                line,
                Confidence::Extracted,
            );
            self.annotate_edges_since(edge_start, "context", "decorator");
        }
    }
    fn typescript_namespace_name(&self, node: TsNode<'_>) -> String {
        let name = node
            .child_by_field_name("name")
            .map(|name| self.text(name))
            .unwrap_or_else(|| self.name(node));
        name.trim().trim_matches(['\'', '"']).trim().to_owned()
    }
    fn walk(&mut self, node: TsNode<'_>, class: Option<String>, function: Option<String>) {
        let kind = node.kind();
        let line = node.start_position().row + 1;
        if self.language_name == "python" && kind == "decorated_definition" {
            // Python decorators wrap the actual class/function definition.  Walk
            // that definition with the current class owner intact, then attach
            // explicit decorator references to the node that was just emitted.
            // This mirrors the TypeScript decorator edge shape and makes reverse
            // impact queries such as `affected my_decorator` work.
            let definition = node.child_by_field_name("definition").or_else(|| {
                let mut cursor = node.walk();
                let definition = node.named_children(&mut cursor).find(|child| {
                    self.spec.classes.contains(&child.kind())
                        || self.spec.functions.contains(&child.kind())
                });
                definition
            });
            let Some(definition) = definition else {
                return;
            };
            let name = self.name(definition);
            self.walk(definition, class.clone(), function);
            if name.is_empty() || graphoxide_core::normalize_id(&name).is_empty() {
                return;
            }
            let owner = if self.spec.classes.contains(&definition.kind()) {
                make_id(&[self.stem, &name])
            } else if let Some(class) = class {
                make_id(&[&class, &name])
            } else {
                make_id(&[self.stem, &name])
            };
            // Nested Python definitions are intentionally not emitted; do not
            // manufacture decorator edges whose owner node would be absent.
            if !self.seen.contains(&owner) {
                return;
            }
            let mut cursor = node.walk();
            for decorator in node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "decorator")
            {
                let Some(name) = self.python_decorator_name(decorator) else {
                    continue;
                };
                if python_decorator_noise(&name) {
                    continue;
                }
                let decorator_line = decorator.start_position().row + 1;
                let target = self.ensure_reference(&name, decorator_line);
                if target == owner {
                    continue;
                }
                let edge_start = self.edges.len();
                self.add_edge(
                    owner.clone(),
                    target,
                    "references",
                    decorator_line,
                    Confidence::Extracted,
                );
                self.annotate_edges_since(edge_start, "context", "decorator");
            }
            return;
        }
        if matches!(self.language_name, "typescript" | "tsx")
            && matches!(
                kind,
                "public_field_definition" | "field_definition" | "property_signature"
            )
        {
            if let Some(owner) = class.as_deref().or(function.as_deref()) {
                self.emit_typescript_decorators(node, owner, false);
            }
        }
        if self.language_name == "rust" && matches!(kind, "const_item" | "static_item") {
            if let Some(name) = node.child_by_field_name("name") {
                let name = self.text(name);
                if !name.is_empty() {
                    let owner = class.as_deref().or(function.as_deref());
                    // Graphify-compatible IDs case-fold their components. Rust
                    // does not: a value named `PATH` and a type named `Path`
                    // are distinct symbols in distinct namespaces. Keep value
                    // declarations in an explicit namespace and append a short
                    // digest of the exact spelling so neither symbol silently
                    // wins insertion order.
                    let exact_name = hex::encode(Sha256::digest(name.as_bytes()));
                    let value_kind = if kind == "const_item" {
                        "const"
                    } else {
                        "static"
                    };
                    let id = owner
                        .map(|owner| make_id(&[owner, value_kind, &name, &exact_name[..12]]))
                        .unwrap_or_else(|| {
                            make_id(&[self.stem, value_kind, &name, &exact_name[..12]])
                        });
                    self.add_node(id.clone(), name, line, Some("variable"));
                    self.add_edge(
                        owner.unwrap_or(&self.file_id).to_owned(),
                        id,
                        "contains",
                        line,
                        Confidence::Extracted,
                    );
                }
            }
        }
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
                    let edge_start = self.edges.len();
                    self.import(node, line);
                    self.annotate_edges_since(edge_start, "context", "import");
                }
                return;
            }
            let edge_start = self.edges.len();
            self.import(node, line);
            self.annotate_edges_since(edge_start, "context", "import");
            if self.language_name == "python" && function.is_some() {
                for edge in &mut self.edges[edge_start..] {
                    edge.extra.insert("resolution_only".into(), true.into());
                }
            }
            return;
        }
        if matches!(self.language_name, "typescript" | "tsx")
            && matches!(kind, "internal_module" | "module")
        {
            let name = self.typescript_namespace_name(node);
            if name.is_empty() {
                return;
            }
            let id = make_id(&[self.stem, &name]);
            self.add_node(id.clone(), name, line, Some("module"));
            self.mark_exported(&id, node);
            self.add_edge(
                self.file_id.clone(),
                id,
                "contains",
                line,
                Confidence::Extracted,
            );
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                self.walk(child, class.clone(), function.clone());
            }
            return;
        }
        if self.language_name == "ruby"
            && kind == "assignment"
            && self.emit_ruby_dynamic_type(node, class.as_deref(), line)
        {
            return;
        }
        if self.spec.classes.contains(&kind) {
            if self.language_name == "python" && function.is_some() {
                return;
            }
            let declared_name = self.name(node);
            if declared_name.is_empty() {
                return;
            }
            let name = if self.language_name == "ruby" {
                crate::ruby::qualify_declaration(
                    &declared_name,
                    self.ruby_owner_label(class.as_deref()),
                )
            } else {
                declared_name
            };
            let csharp_context = (self.language_name == "csharp")
                .then(|| crate::csharp::namespace_context(node, self.source));
            let id = if self.language_name == "go" {
                let package_id = make_id(&[&self.package_scope, &name]);
                if package_id == self.file_id {
                    make_id(&[self.stem, &name])
                } else {
                    package_id
                }
            } else if let Some(context) = &csharp_context {
                if let Some(parent) = class.as_deref() {
                    make_id(&[parent, &name])
                } else if context.namespace.is_empty() {
                    make_id(&[self.stem, &name])
                } else {
                    make_id(&[self.stem, &context.namespace, &name])
                }
            } else {
                make_id(&[self.stem, &name])
            };
            self.add_node(id.clone(), name.clone(), line, Some("class"));
            if self.language_name == "java" {
                if let Some(declaration) = self.nodes.iter_mut().find(|node| node.id == id) {
                    declaration.extra.insert(
                        crate::java::PACKAGE.into(),
                        self.java_package.clone().into(),
                    );
                    let qualified = if self.java_package.is_empty() {
                        name.clone()
                    } else {
                        format!("{}.{}", self.java_package, name)
                    };
                    declaration
                        .extra
                        .insert(crate::java::QUALIFIED_TYPE.into(), qualified.into());
                }
            }
            if self.language_name == "ruby" {
                self.stamp_ruby_declaration(&id, if kind == "module" { "module" } else { "class" });
            }
            if let Some(context) = &csharp_context {
                self.stamp_csharp_declaration(&id, node, class.as_deref(), &name, context);
            }
            self.emit_typescript_decorators(node, &id, false);
            self.record_javascript_injected_fields(node, &id);
            self.mark_exported(&id, node);
            if self.language_name == "java" && kind == "enum_declaration" {
                self.enum_ids.insert(id.clone());
            }
            let containment_parent = if let Some(parent) = class.clone() {
                parent
            } else if let Some(context) = &csharp_context {
                self.add_csharp_namespace_node(context, line)
            } else {
                self.file_id.clone()
            };
            self.add_edge(
                containment_parent,
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
                    self.annotate_edges_since(edge_start, "context", "import");
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
            let qualified_owner = if self.language_name == "cpp"
                && class.is_none()
                && kind == "function_definition"
            {
                // Only the function declarator can establish an out-of-class
                // owner. Searching the full body misclassifies a free function
                // containing `Foo::bar()` as if it were a method of `Foo`.
                let declaration = node
                    .child_by_field_name("declarator")
                    .map(|declarator| self.text(declarator))
                    .unwrap_or_default();
                Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)::[A-Za-z_][A-Za-z0-9_]*\s*\(")
                    .expect("C++ qualified method")
                    .captures(&declaration)
                    .map(|capture| make_id(&[self.stem, &capture[1]]))
            } else {
                None
            };
            self.emit_function(node, node, name, kind, line, class.or(qualified_owner));
            return;
        }
        self.emit_javascript_require(node, line);
        if let Some((name, callable)) = self.variable_bound_callable(node) {
            self.emit_function(callable, node, name, callable.kind(), line, class);
            return;
        }
        if self.emit_javascript_assigned_callable(node, class.as_deref(), function.as_deref(), line)
        {
            return;
        }
        self.emit_javascript_module_variable(node, line);
        if self.spec.calls.contains(&kind) {
            if self.language_name == "ruby" {
                if self.emit_ruby_require_relative(node, line) {
                    return;
                }
                if function.is_none() && self.emit_ruby_mixins(node, class.as_deref(), line) {
                    return;
                }
            }
            if self.emit_javascript_dynamic_import(node, function.as_deref(), line) {
                return;
            }
            if self.language_name == "java" && kind == "object_creation_expression" {
                if let (Some(owner), Some(type_node)) =
                    (function.as_deref(), node.child_by_field_name("type"))
                {
                    self.emit_type_node(owner, type_node, "constructor_call", line);
                }
            }
            if let Some(owner) = function.clone() {
                let callee = node
                    .child_by_field_name(self.spec.function_field)
                    .or_else(|| node.child_by_field_name("type"));
                if let Some(callee) = callee {
                    // Most grammars put a receiver inside the callee accessor, but
                    // Java and Ruby expose it directly on the call node.  Treat
                    // either shape as a member call so a unique same-named method
                    // cannot manufacture an edge without receiver evidence.
                    let member_call = self.spec.accessor_types.contains(&callee.kind())
                        || node.child_by_field_name("object").is_some()
                        || node.child_by_field_name("receiver").is_some();
                    let mut ruby_constant_receiver = false;
                    let (receiver, receiver_type) = if self.language_name == "python" && member_call
                    {
                        self.python_receiver(callee, &owner)
                    } else if self.is_javascript_family() && member_call {
                        self.javascript_receiver(callee, &owner)
                    } else if self.language_name == "cpp" && member_call {
                        let current_type = class.as_ref().and_then(|class| {
                            self.nodes
                                .iter()
                                .find(|node| &node.id == class)
                                .map(|node| node.label.as_str())
                        });
                        crate::native::cpp_receiver_fact(node, callee, self.source, current_type)
                            .map(|fact| (Some(fact.receiver), fact.receiver_type))
                            .unwrap_or((None, None))
                    } else if self.language_name == "java" && member_call {
                        let current_type = class.as_ref().and_then(|class| {
                            self.nodes
                                .iter()
                                .find(|node| &node.id == class)
                                .map(|node| node.label.as_str())
                        });
                        crate::java::receiver_fact(node, self.source, current_type)
                            .map(|fact| (Some(fact.receiver), fact.receiver_type))
                            .unwrap_or((None, None))
                    } else if self.language_name == "csharp" && member_call {
                        let fact = crate::csharp::receiver_fact(callee, self.source);
                        (Some(fact.receiver), fact.receiver_type)
                    } else if self.language_name == "go" && member_call {
                        let receiver = callee
                            .child_by_field_name("operand")
                            .map(|operand| self.text(operand));
                        let receiver_type = receiver.as_ref().and_then(|receiver| {
                            self.go_receiver_types
                                .get(&(owner.clone(), receiver.clone()))
                                .cloned()
                        });
                        (receiver, receiver_type)
                    } else if self.language_name == "rust" && member_call {
                        let receiver = callee
                            .child_by_field_name("path")
                            .or_else(|| callee.child_by_field_name("scope"))
                            .map(|path| self.text(path));
                        let receiver_type = receiver.as_ref().and_then(|receiver| {
                            receiver
                                .rsplit("::")
                                .next()
                                .filter(|name| !name.is_empty())
                                .map(str::to_owned)
                        });
                        (receiver, receiver_type)
                    } else if self.language_name == "ruby" && member_call {
                        if let Some(fact) = crate::ruby::receiver_fact(node, self.source) {
                            ruby_constant_receiver = fact.constant;
                            (Some(fact.receiver), fact.receiver_type)
                        } else {
                            (None, None)
                        }
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
                    if !name.is_empty()
                        && (self.language_name != "python" || !python_builtin(&name))
                        && (self.language_name != "go"
                            || member_call
                            || !go_predeclared_function(&name))
                    {
                        self.calls.push(CallSite {
                            source: owner.clone(),
                            name,
                            line,
                            member_call,
                            receiver,
                            receiver_type,
                            ruby_constant_receiver,
                        });
                    }
                }
            }
            if self.language_name == "python" || self.is_javascript_family() {
                let indirect_owner = function
                    .as_deref()
                    .unwrap_or(self.file_id.as_str())
                    .to_owned();
                if let Some(arguments) = node.child_by_field_name("arguments") {
                    let mut cursor = arguments.walk();
                    for argument in arguments.named_children(&mut cursor) {
                        let candidate = if argument.kind() == "identifier" {
                            Some(argument)
                        } else if self.language_name == "python"
                            && argument.kind() == "keyword_argument"
                        {
                            argument.child_by_field_name("value")
                        } else {
                            None
                        };
                        if let Some(candidate) = candidate {
                            self.record_indirect_identifier(candidate, &indirect_owner, "argument");
                        }
                    }
                }
                if self.language_name == "python" {
                    if let Some((name, line)) = python_getattr_reference(node, self.source) {
                        self.indirect_calls
                            .push((indirect_owner, name, line, "getattr"));
                    }
                }
            }
        }
        if self.language_name == "python" || self.is_javascript_family() {
            let owner = function.clone().unwrap_or_else(|| self.file_id.clone());
            if self.language_name == "python" && kind == "assignment" {
                self.record_python_attribute_type(node, class.as_deref(), function.as_deref());
            }
            if (self.language_name == "python"
                && matches!(kind, "dictionary" | "list" | "set" | "tuple"))
                || (self.is_javascript_family() && matches!(kind, "object" | "array"))
            {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    let candidate = if matches!(kind, "dictionary" | "object")
                        && child.kind() == "pair"
                    {
                        child.child_by_field_name("value")
                    } else if matches!(child.kind(), "identifier" | "shorthand_property_identifier")
                    {
                        Some(child)
                    } else {
                        None
                    };
                    if let Some(candidate) = candidate {
                        self.record_indirect_identifier(candidate, &owner, "collection");
                    }
                }
            } else if self.language_name == "python"
                && matches!(kind, "return_statement" | "assignment")
            {
                let candidate = if kind == "assignment" {
                    node.child_by_field_name("right")
                } else {
                    node.named_child(0)
                };
                if let Some(candidate) = candidate {
                    let mut references = Vec::new();
                    collect_indirect_value_identifiers(candidate, &mut references);
                    for reference in references {
                        self.record_indirect_identifier(
                            reference,
                            &owner,
                            if kind == "return_statement" {
                                "return"
                            } else {
                                "assignment"
                            },
                        );
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, class.clone(), function.clone())
        }
    }
    fn import(&mut self, node: TsNode<'_>, line: usize) {
        if self.emit_csharp_import(node, line) {
            return;
        }
        if self.language_name == "python" {
            let raw = self.text(node);
            let raw = raw.trim();
            if let Some(rest) = raw.strip_prefix("from ") {
                let mut halves = rest.splitn(2, " import ");
                let module = halves.next().unwrap_or("").trim();
                let imported = halves.next().unwrap_or("").trim();
                if !module.is_empty() {
                    let module_target = self.python_module_id(module, None);
                    if !module_target.is_empty() {
                        let start = self.edges.len();
                        self.add_edge(
                            self.file_id.clone(),
                            module_target.clone(),
                            "imports_from",
                            line,
                            Confidence::Extracted,
                        );
                        if self.edges.len() > start {
                            self.edges[start]
                                .extra
                                .insert("target_module".into(), module_target.clone().into());
                        }
                    }
                    for item in imported.trim_matches(['(', ')']).split(',') {
                        let mut words = item.split_whitespace();
                        let name = words.next().unwrap_or("").trim();
                        if name.is_empty() || name == "*" {
                            continue;
                        }
                        let local_alias = if words.next() == Some("as") {
                            words.next().unwrap_or(name)
                        } else {
                            name
                        };
                        let target = self.python_module_id(module, Some(name));
                        if target.is_empty() {
                            continue;
                        }
                        let start = self.edges.len();
                        self.add_edge(
                            self.file_id.clone(),
                            target,
                            "imports_from",
                            line,
                            Confidence::Extracted,
                        );
                        if self.edges.len() > start {
                            self.edges[start]
                                .extra
                                .insert("local_alias".into(), local_alias.into());
                            self.edges[start]
                                .extra
                                .insert("imported_name".into(), name.into());
                            self.edges[start]
                                .extra
                                .insert("target_module".into(), module_target.clone().into());
                            if let Some(module_stem) = module
                                .trim_matches('.')
                                .rsplit('.')
                                .find(|part| !part.is_empty())
                            {
                                self.edges[start]
                                    .extra
                                    .insert("module_stem".into(), module_stem.into());
                            }
                        }
                    }
                }
            } else if let Some(rest) = raw.strip_prefix("import ") {
                for item in rest.split(',') {
                    let mut words = item.split_whitespace();
                    let module = words.next().unwrap_or("");
                    let local_alias = if words.next() == Some("as") {
                        words.next()
                    } else {
                        module.rsplit('.').next()
                    };
                    if !module.is_empty() {
                        let target = self.python_module_id(module, None);
                        let start = self.edges.len();
                        self.add_edge(
                            self.file_id.clone(),
                            target,
                            "imports",
                            line,
                            Confidence::Extracted,
                        );
                        if self.edges.len() > start {
                            if let Some(local_alias) = local_alias {
                                self.edges[start]
                                    .extra
                                    .insert("local_alias".into(), local_alias.into());
                            }
                        }
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
            let re_export = raw.trim_start().starts_with("export") && raw.contains(" from ");
            let strings = quoted_values(&raw);
            let Some(module) = strings.last() else { return };
            let module_id = self.javascript_module_id(module);
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
                        if re_export {
                            let start = self.edges.len();
                            self.add_edge(
                                self.file_id.clone(),
                                make_id(&[&module_id, symbol]),
                                "re_exports",
                                line,
                                Confidence::Extracted,
                            );
                            self.annotate_edges_since(start, "context", "re-export");
                        }
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
            let raw = self.text(node);
            let static_import = raw.trim_start_matches("import ").starts_with("static ");
            let module = raw
                .trim_start_matches("import ")
                .trim_start_matches("static ")
                .trim_end_matches(';')
                .trim()
                .to_owned();
            if !module.is_empty() {
                self.add_edge(
                    self.file_id.clone(),
                    make_id(&[&module]),
                    "imports",
                    line,
                    Confidence::Extracted,
                );
                if let Some(edge) = self.edges.last_mut() {
                    edge.extra
                        .insert(crate::java::IMPORT_PATH.into(), module.clone().into());
                    edge.extra.insert(
                        crate::java::IMPORT_ALIAS.into(),
                        module.rsplit('.').next().unwrap_or(&module).into(),
                    );
                    if static_import {
                        edge.extra
                            .insert(crate::java::STATIC_IMPORT.into(), true.into());
                    }
                }
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
        if matches!(self.language_name, "c" | "cpp") {
            let quoted =
                Regex::new(r#"^\s*#\s*include\s*\"([^\"]+)\""#).expect("quoted C/C++ include");
            if let Some(capture) = quoted.captures(&raw) {
                let module = &capture[1];
                let physical = self
                    .physical_path
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .join(module);
                let logical = Path::new(self.source_file)
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .join(module);
                let logical_text = logical.to_string_lossy().replace('\\', "/");
                let logical_stem = logical
                    .with_extension("")
                    .to_string_lossy()
                    .replace('\\', "/");
                let base = make_id(&[&logical_stem]);
                let paired = matches!(
                    physical.extension().and_then(|value| value.to_str()),
                    Some("h" | "hpp" | "hh")
                ) && ["c", "cc", "cpp", "cxx", "m", "mm"]
                    .iter()
                    .any(|extension| physical.with_extension(extension).is_file());
                let resolved_physical = fs::canonicalize(&physical).unwrap_or(physical.clone());
                let target = inferred_scan_root(&self.physical_path, self.source_file)
                    .and_then(|root| {
                        let resolved_root = fs::canonicalize(&root).unwrap_or(root);
                        if let Ok(relative) = resolved_physical.strip_prefix(&resolved_root) {
                            let relative_stem = make_id(&[&relative
                                .with_extension("")
                                .to_string_lossy()
                                .replace('\\', "/")]);
                            Some(if paired {
                                make_id(&[
                                    &relative.to_string_lossy().replace('\\', "/"),
                                    &relative_stem,
                                ])
                            } else {
                                relative_stem
                            })
                        } else if resolved_physical.is_file() {
                            Some(portable_external_target_id(
                                &resolved_root,
                                &resolved_physical,
                            ))
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| {
                        if paired {
                            make_id(&[&logical_text, &base])
                        } else {
                            base
                        }
                    });
                self.add_edge(
                    self.file_id.clone(),
                    target,
                    "imports",
                    line,
                    Confidence::Extracted,
                );
                return;
            }
        }
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
    fn python_module_id(&self, module: &str, imported: Option<&str>) -> String {
        let level = module
            .chars()
            .take_while(|character| *character == '.')
            .count();
        let module_tail = module.trim_start_matches('.');
        if level == 0 {
            let mut logical = PathBuf::from(module_tail.replace('.', "/"));
            if let Some(imported) = imported {
                logical.push(imported);
            }
            return make_id(&[&logical.to_string_lossy().replace('\\', "/")]);
        }

        let mut physical = self
            .physical_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        let mut logical = Path::new(self.source_file)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        for _ in 1..level {
            physical.pop();
            logical.pop();
        }
        for part in module_tail.split('.').filter(|part| !part.is_empty()) {
            physical.push(part);
            logical.push(part);
        }
        if let Some(imported) = imported {
            physical.push(imported);
            logical.push(imported);
        }
        physical.set_extension("py");
        logical.set_extension("py");

        let root = inferred_scan_root(&self.physical_path, self.source_file);
        if physical.is_file() {
            if let Some(root) = root.as_deref() {
                if let Ok(relative) = physical.strip_prefix(root) {
                    return make_id(&[&relative
                        .with_extension("")
                        .to_string_lossy()
                        .replace('\\', "/")]);
                }
                return portable_external_target_id(root, &physical);
            }
        }
        make_id(&[&logical
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/")])
    }
    fn emit_type_refs(&mut self, node: TsNode<'_>, owner: &str, line: usize) {
        if matches!(self.language_name, "java" | "c" | "cpp" | "csharp") {
            self.emit_structured_type_refs(node, owner, line);
            return;
        }
        if self.language_name != "rust" {
            return;
        }
        if self.spec.functions.contains(&node.kind()) {
            if let Some(parameters) = node.child_by_field_name("parameters") {
                let mut candidates = Vec::new();
                collect_descendants(parameters, &["parameter"], &mut candidates);
                for parameter in candidates {
                    if let Some(type_node) = parameter.child_by_field_name("type") {
                        self.emit_type_node(owner, type_node, "parameter_type", line);
                    }
                }
            }
            if let Some(return_type) = node.child_by_field_name("return_type") {
                self.emit_type_node(owner, return_type, "return_type", line);
            }
            return;
        }
        if node.kind() == "struct_item" {
            if let Some(body) = node.child_by_field_name("body") {
                if body.kind() == "ordered_field_declaration_list" {
                    let mut cursor = body.walk();
                    for type_node in body.named_children(&mut cursor) {
                        self.emit_type_node(owner, type_node, "field", line);
                    }
                } else {
                    let mut fields = Vec::new();
                    collect_descendants(body, &["field_declaration"], &mut fields);
                    for field in fields {
                        if let Some(type_node) = field.child_by_field_name("type") {
                            self.emit_type_node(owner, type_node, "field", line);
                        }
                    }
                }
            }
            return;
        }
        let mut names = Vec::new();
        if node.kind() == "trait_item" {
            return;
        }
        collect_descendant_text(
            node,
            self.source,
            &["type_identifier", "scoped_type_identifier"],
            &mut names,
        );
        names.sort();
        names.dedup();
        let primitives = [
            "int", "long", "short", "byte", "float", "double", "boolean", "char", "void", "bool",
            "str", "usize", "isize", "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "f32",
            "f64",
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
                || self
                    .node_labels
                    .get(owner)
                    .is_some_and(|label| label == &definition_key(&name))
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
    fn emit_structured_type_refs(&mut self, node: TsNode<'_>, owner: &str, line: usize) {
        if self.spec.functions.contains(&node.kind()) {
            if let Some(parameters) = node.child_by_field_name("parameters") {
                let mut candidates = Vec::new();
                collect_descendants(
                    parameters,
                    &[
                        "formal_parameter",
                        "spread_parameter",
                        "parameter_declaration",
                        "parameter",
                    ],
                    &mut candidates,
                );
                for parameter in candidates {
                    if let Some(type_node) = declaration_type_node(parameter) {
                        self.emit_type_node(owner, type_node, "parameter_type", line);
                    }
                }
            } else if let Some(declarator) = node.child_by_field_name("declarator") {
                let mut candidates = Vec::new();
                collect_descendants(
                    declarator,
                    &["parameter_declaration", "parameter"],
                    &mut candidates,
                );
                for parameter in candidates {
                    if let Some(type_node) = declaration_type_node(parameter) {
                        self.emit_type_node(owner, type_node, "parameter_type", line);
                    }
                }
            }
            if let Some(return_type) = node
                .child_by_field_name("type")
                .or_else(|| node.child_by_field_name("returns"))
            {
                self.emit_type_node(owner, return_type, "return_type", line);
            }
            if self.language_name == "java" {
                self.emit_java_annotations(node, owner, line);
            }
            return;
        }

        if !self.spec.classes.contains(&node.kind()) {
            return;
        }
        if self.language_name == "java" {
            self.emit_java_annotations(node, owner, line);
        }
        if node.kind() == "record_declaration" {
            if let Some(parameters) = node.child_by_field_name("parameters") {
                let mut candidates = Vec::new();
                collect_descendants(
                    parameters,
                    &["formal_parameter", "spread_parameter"],
                    &mut candidates,
                );
                for parameter in candidates {
                    if let Some(type_node) = declaration_type_node(parameter) {
                        self.emit_type_node(owner, type_node, "field", line);
                    }
                }
            }
        }
        let body = node.child_by_field_name("body").or_else(|| {
            node.named_children(&mut node.walk()).find(|child| {
                matches!(
                    child.kind(),
                    "class_body" | "field_declaration_list" | "declaration_list"
                )
            })
        });
        let Some(body) = body else { return };
        let mut cursor = body.walk();
        let members: Vec<_> = body.named_children(&mut cursor).collect();
        for member in members {
            if !matches!(member.kind(), "field_declaration" | "property_declaration") {
                continue;
            }
            if let Some(type_node) = find_descendant_field(member, "type") {
                self.emit_type_node(owner, type_node, "field", line);
            }
        }
    }
    fn emit_java_annotations(&mut self, node: TsNode<'_>, owner: &str, line: usize) {
        let mut cursor = node.walk();
        let mut modifiers = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "modifiers");
        for modifiers in &mut modifiers {
            let mut annotations = Vec::new();
            collect_descendants(
                modifiers,
                &["marker_annotation", "annotation"],
                &mut annotations,
            );
            for annotation in annotations {
                let name = annotation
                    .child_by_field_name("name")
                    .map(|name| self.text(name))
                    .unwrap_or_else(|| {
                        self.text(annotation)
                            .trim_start_matches('@')
                            .split(['(', '.'])
                            .next_back()
                            .unwrap_or("")
                            .to_owned()
                    });
                self.emit_named_type_ref(owner, &name, "attribute", line);
            }
        }
    }
    fn emit_type_node(
        &mut self,
        owner: &str,
        type_node: TsNode<'_>,
        primary_context: &str,
        line: usize,
    ) {
        if self.language_name == "csharp" {
            let names = crate::csharp::type_tokens(&self.text(type_node));
            let Some((primary, generic_arguments)) = names.split_first() else {
                return;
            };
            self.emit_csharp_named_type_ref(owner, primary, primary_context, line);
            for generic in generic_arguments {
                self.emit_csharp_named_type_ref(owner, generic, "generic_arg", line);
            }
            return;
        }
        let names = parse_type_names(&self.text(type_node));
        let Some((primary, generic_arguments)) = names.split_first() else {
            return;
        };
        self.emit_named_type_ref(owner, primary, primary_context, line);
        for generic in generic_arguments {
            self.emit_named_type_ref(owner, generic, "generic_arg", line);
        }
    }
    fn emit_named_type_ref(&mut self, owner: &str, name: &str, context: &str, line: usize) {
        let name =
            name.trim_matches(|character: char| !character.is_alphanumeric() && character != '_');
        if name.is_empty()
            || self
                .node_labels
                .get(owner)
                .is_some_and(|label| label == &definition_key(name))
            || type_reference_noise(self.language_name, name)
            || (self.language_name == "java"
                && self.java_type_parameters.contains(&normalize_name(name)))
        {
            return;
        }
        let target = self.ensure_reference(name, line);
        self.add_edge(
            owner.to_owned(),
            target,
            "references",
            line,
            Confidence::Extracted,
        );
        if let Some(edge) = self.edges.last_mut() {
            edge.extra.insert("context".into(), context.into());
        }
    }
    fn emit_csharp_named_type_ref(&mut self, owner: &str, token: &str, context: &str, line: usize) {
        self.emit_csharp_type_edge(owner, token, "references", Some(context), line);
    }
    fn emit_csharp_type_edge(
        &mut self,
        owner: &str,
        token: &str,
        relation: &str,
        context: Option<&str>,
        line: usize,
    ) {
        let name = crate::csharp::simple_type_name(token);
        if name.is_empty()
            || type_reference_noise("csharp", &name)
            || self
                .node_labels
                .get(owner)
                .is_some_and(|label| label == &definition_key(&name))
            || self
                .nodes
                .iter()
                .find(|node| node.id == owner)
                .and_then(|node| node.extra.get("metadata"))
                .and_then(|metadata| metadata.get("type_parameters"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|parameters| {
                    parameters
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .any(|parameter| parameter == name)
                })
        {
            return;
        }
        let target = self.ensure_reference(&name, line);
        if let Some(stub) = self
            .nodes
            .iter_mut()
            .find(|node| node.id == target && node.source_file.is_empty())
        {
            stub.extra
                .insert(crate::csharp::MANAGED_NODE.into(), true.into());
        }
        self.add_edge(
            owner.to_owned(),
            target,
            relation,
            line,
            Confidence::Extracted,
        );
        if let Some(edge) = self.edges.last_mut() {
            if let Some(context) = context {
                edge.extra.insert("context".into(), context.into());
            }
            edge.extra
                .insert(crate::csharp::TYPE_REF_EDGE.into(), true.into());
            edge.extra.insert(
                "metadata".into(),
                serde_json::json!({
                    "ref_token": token,
                    "qualified": token.contains('.') || token.contains("::"),
                }),
            );
        }
    }
    fn emit_inheritance(&mut self, node: TsNode<'_>, owner: &str, line: usize) {
        let fields: &[(&str, &str)] = match self.language_name {
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
                if self
                    .node_labels
                    .get(owner)
                    .is_some_and(|label| label == &definition_key(&name))
                {
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
        if matches!(self.language_name, "typescript" | "tsx") {
            let mut clauses = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "extends_type_clause" | "extends_clause" => {
                        clauses.push((child, "inherits"));
                    }
                    "implements_clause" => clauses.push((child, "implements")),
                    "class_heritage" => {
                        let mut heritage_cursor = child.walk();
                        for clause in child.named_children(&mut heritage_cursor) {
                            match clause.kind() {
                                "extends_clause" | "extends_type_clause" => {
                                    clauses.push((clause, "inherits"));
                                }
                                "implements_clause" => {
                                    clauses.push((clause, "implements"));
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            for (clause, relation) in clauses {
                for (name, generic_arguments) in collect_parent_type_names(clause, self.source) {
                    if name.is_empty()
                        || self
                            .node_labels
                            .get(owner)
                            .is_some_and(|label| label == &definition_key(&name))
                    {
                        continue;
                    }
                    let target = self.ensure_reference(&name, line);
                    let duplicate = self.edges.iter().any(|edge| {
                        edge.true_source() == owner
                            && edge.true_target() == target
                            && edge.relation == relation
                    });
                    if !duplicate {
                        self.add_edge(
                            owner.to_owned(),
                            target,
                            relation,
                            line,
                            Confidence::Extracted,
                        );
                    }
                    for generic in generic_arguments {
                        self.emit_named_type_ref(owner, &generic, "generic_arg", line);
                    }
                }
            }
        }
        if matches!(self.language_name, "java" | "cpp" | "csharp" | "ruby") {
            self.emit_structured_inheritance(node, owner, line);
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
    fn emit_structured_inheritance(&mut self, node: TsNode<'_>, owner: &str, line: usize) {
        if self.language_name == "csharp" {
            let mut cursor = node.walk();
            let clauses: Vec<_> = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "base_list")
                .collect();
            for clause in clauses {
                for names in crate::csharp::base_type_groups(&self.text(clause)) {
                    let Some((parent, generic_arguments)) = names.split_first() else {
                        continue;
                    };
                    let relation = if node.kind() == "interface_declaration" {
                        "inherits"
                    } else if self
                        .declared_interfaces
                        .contains(&normalize_name(&crate::csharp::simple_type_name(parent)))
                    {
                        "implements"
                    } else {
                        "inherits"
                    };
                    self.emit_csharp_type_edge(owner, parent, relation, None, line);
                    for generic in generic_arguments {
                        self.emit_csharp_named_type_ref(owner, generic, "generic_arg", line);
                    }
                }
            }
            return;
        }
        let mut clauses = Vec::new();
        match self.language_name {
            "java" => {
                for field in ["superclass", "interfaces"] {
                    if let Some(clause) = node.child_by_field_name(field) {
                        clauses.push((
                            clause,
                            if field == "interfaces" {
                                "implements"
                            } else {
                                "inherits"
                            },
                        ));
                    }
                }
                let mut cursor = node.walk();
                for child in node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "extends_interfaces")
                {
                    clauses.push((child, "inherits"));
                }
            }
            "cpp" => {
                let mut cursor = node.walk();
                for child in node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "base_class_clause")
                {
                    clauses.push((child, "inherits"));
                }
            }
            "ruby" => {
                if let Some(superclass) = node.child_by_field_name("superclass") {
                    clauses.push((superclass, "inherits"));
                }
            }
            _ => return,
        }

        for (clause, default_relation) in clauses {
            let parents = collect_parent_type_names(clause, self.source);
            for (name, generic_arguments) in parents {
                if name.is_empty()
                    || self
                        .node_labels
                        .get(owner)
                        .is_some_and(|label| label == &definition_key(&name))
                {
                    continue;
                }
                let relation = if default_relation == "csharp_parent" {
                    if node.kind() == "interface_declaration"
                        || self.declared_interfaces.contains(&normalize_name(&name))
                    {
                        if node.kind() == "interface_declaration" {
                            "inherits"
                        } else {
                            "implements"
                        }
                    } else {
                        "inherits"
                    }
                } else {
                    default_relation
                };
                let target = self.ensure_reference(&name, line);
                self.add_edge(
                    owner.to_owned(),
                    target,
                    relation,
                    line,
                    Confidence::Extracted,
                );
                for generic in generic_arguments {
                    self.emit_named_type_ref(owner, &generic, "generic_arg", line);
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
                let context = (relation == "references").then_some("field");
                let mut names = Vec::new();
                collect_descendant_text(field, self.source, &["type_identifier"], &mut names);
                for name in names {
                    if type_reference_noise("go", &name) {
                        continue;
                    }
                    self.pending_local_refs.push((
                        owner.to_owned(),
                        name,
                        relation.into(),
                        context,
                        field.start_position().row + 1,
                    ));
                }
            }
        } else {
            for (field, context) in [("parameters", "parameter_type"), ("result", "return_type")] {
                if let Some(value) = node.child_by_field_name(field) {
                    let mut names = Vec::new();
                    collect_descendant_text(value, self.source, &["type_identifier"], &mut names);
                    for name in names {
                        if type_reference_noise("go", &name) {
                            continue;
                        }
                        self.pending_local_refs.push((
                            owner.to_owned(),
                            name,
                            "references".into(),
                            Some(context),
                            node.start_position().row + 1,
                        ));
                    }
                }
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
        if clean.chars().count() < 20
            || (node.kind() == "module" && self.suppress_python_module_docstring(&clean))
        {
            return;
        }
        let line = string.start_position().row + 1;
        let id = make_id(&[self.stem, "rationale", &line.to_string()]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(Node {
                id: id.clone(),
                label: rationale_label(&clean),
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
    fn suppress_python_module_docstring(&self, docstring: &str) -> bool {
        let source = String::from_utf8_lossy(self.source).to_ascii_lowercase();
        let docstring = docstring.to_ascii_lowercase();
        let alembic = (docstring.contains("revision id:")
            && (docstring.contains("revises:") || source.contains("down_revision")))
            || (source.contains("down_revision") && source.contains("def upgrade"));
        let django = source.contains("from django.db import migrations")
            || source.contains("migrations.migration");
        let generated = docstring.contains("generated by")
            || docstring.contains("do not edit")
            || source.lines().take(8).any(|line| {
                line.contains("generated file") || line.contains("automatically generated")
            });
        alembic || django || generated
    }
    fn emit_python_type_refs(&mut self, node: TsNode<'_>, owner: &str, line: usize) {
        if node.kind() != "class_definition" {
            let mut references = Vec::new();
            if let Some(parameters) = node.child_by_field_name("parameters") {
                let mut typed = Vec::new();
                collect_descendants(
                    parameters,
                    &["typed_parameter", "typed_default_parameter"],
                    &mut typed,
                );
                for parameter in typed {
                    if let Some(annotation) = parameter.child_by_field_name("type") {
                        let mut names = Vec::new();
                        collect_python_type_refs(annotation, self.source, false, &mut names);
                        references.extend(names.into_iter().map(|(name, generic)| {
                            (
                                name,
                                if generic {
                                    "generic_arg"
                                } else {
                                    "parameter_type"
                                },
                            )
                        }));
                    }
                }
            }
            if let Some(annotation) = node.child_by_field_name("return_type") {
                let mut names = Vec::new();
                collect_python_type_refs(annotation, self.source, false, &mut names);
                references.extend(names.into_iter().map(|(name, generic)| {
                    (
                        name,
                        if generic {
                            "generic_arg"
                        } else {
                            "return_type"
                        },
                    )
                }));
            }
            references.sort();
            references.dedup();
            for (name, context) in references {
                self.emit_named_type_ref(owner, &name, context, line);
            }
            return;
        }
        let mut type_nodes = Vec::new();
        if let Some(superclasses) = node.child_by_field_name("superclasses") {
            type_nodes.push(superclasses);
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
                "inherits",
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
            let clean = clean_comment(&raw);
            self.nodes.push(Node {
                id: id.clone(),
                label: rationale_label(&clean),
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
    fn emit_javascript_comment_metadata(&mut self, root: TsNode<'_>, owner: &str) {
        let mut comments = Vec::new();
        collect_descendants(root, &["comment"], &mut comments);
        let adr = Regex::new(r"(?i)\bADR(?:-|\s)?(\d{1,6})\b").expect("valid ADR regex");
        for comment in comments {
            let raw = self.text(comment);
            let clean = clean_comment(&raw);
            let line = comment.start_position().row + 1;
            let upper = clean.to_ascii_uppercase();
            if ["NOTE:", "IMPORTANT:", "WHY:"]
                .iter()
                .any(|prefix| upper.starts_with(prefix))
            {
                let id = make_id(&[self.stem, "rationale", &line.to_string()]);
                if self.seen.insert(id.clone()) {
                    self.nodes.push(Node {
                        id: id.clone(),
                        label: rationale_label(&clean),
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
            for capture in adr.captures_iter(&clean) {
                let Some(number) = capture.get(1) else {
                    continue;
                };
                let Ok(number) = number.as_str().parse::<u64>() else {
                    continue;
                };
                let label = format!("ADR-{number:04}");
                let id = make_id(&[self.stem, "doc_ref", &label]);
                if self.seen.insert(id.clone()) {
                    self.nodes.push(Node {
                        id: id.clone(),
                        label,
                        file_type: "doc_ref".into(),
                        source_file: self.source_file.into(),
                        source_location: Some(format!("L{line}")),
                        community: None,
                        extra: BTreeMap::from([("_origin".into(), "ast".into())]),
                    });
                    self.add_edge(owner.to_owned(), id, "cites", line, Confidence::Extracted);
                }
            }
        }
    }
    fn add_reference_node(&mut self, id: String, label: String) {
        if !self.seen.insert(id.clone()) {
            return;
        }
        self.definitions
            .entry(definition_key(&label))
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
            .get(&definition_key(name))
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
    fn record_javascript_injected_fields(&mut self, node: TsNode<'_>, class: &str) {
        if !self.is_javascript_family() {
            return;
        }
        let Ok(constructor) = Regex::new(r"(?s)\bconstructor\s*\((.*?)\)") else {
            return;
        };
        let Ok(parameter) = Regex::new(
            r"(?i)(?:(?:public|private|protected|readonly)\s+)+([A-Za-z_][A-Za-z0-9_]*)\s*:\s*([A-Za-z_][A-Za-z0-9_.]*)",
        ) else {
            return;
        };
        let source = self.text(node);
        for constructor in constructor.captures_iter(&source) {
            for parameter in parameter.captures_iter(&constructor[1]) {
                self.javascript_injected_fields.insert(
                    (class.to_owned(), parameter[1].to_owned()),
                    parameter[2].to_owned(),
                );
            }
        }
    }
    fn javascript_receiver(
        &self,
        callee: TsNode<'_>,
        function: &str,
    ) -> (Option<String>, Option<String>) {
        let Some(object) = callee.child_by_field_name("object") else {
            return (None, None);
        };
        let receiver = self.text(object);
        let receiver_type = receiver
            .strip_prefix("this.")
            .filter(|field| !field.contains('.'))
            .and_then(|field| {
                self.method_owners.get(function).and_then(|class| {
                    self.javascript_injected_fields
                        .get(&(class.clone(), field.to_owned()))
                        .cloned()
                })
            })
            .or_else(|| {
                (object.kind() == "identifier")
                    .then(|| self.javascript_receiver_types.get(&receiver).cloned())
                    .flatten()
            });
        (Some(receiver), receiver_type)
    }
    fn record_go_receiver_types(&mut self, node: TsNode<'_>, function: &str) {
        if self.language_name != "go" {
            return;
        }
        for field in ["receiver", "parameters"] {
            let Some(container) = node.child_by_field_name(field) else {
                continue;
            };
            let mut declarations = Vec::new();
            collect_descendants(
                container,
                &["parameter_declaration", "variadic_parameter_declaration"],
                &mut declarations,
            );
            for declaration in declarations {
                let Some(type_node) = declaration.child_by_field_name("type") else {
                    continue;
                };
                let type_name = self
                    .text(type_node)
                    .trim_start_matches("...")
                    .trim_start_matches('*')
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                if type_name.is_empty() {
                    continue;
                }
                let mut cursor = declaration.walk();
                for name in declaration
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "identifier")
                    .filter(|child| child.end_byte() <= type_node.start_byte())
                {
                    let name = self.text(name);
                    if !name.is_empty() {
                        self.go_receiver_types
                            .insert((function.to_owned(), name), type_name.clone());
                    }
                }
            }
        }
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
        if self.language_name == "csharp" {
            edge.extra
                .insert(crate::csharp::CALL_EDGE.into(), true.into());
            edge.extra
                .insert("csharp_callee".into(), call.name.clone().into());
            edge.extra
                .insert("csharp_member_call".into(), call.member_call.into());
        }
        if self.language_name == "cpp" && call.member_call {
            edge.extra
                .insert(crate::native::CALL_FACT.into(), true.into());
            let inferred = call.receiver_type.is_some()
                && call.receiver.as_deref() != Some("this")
                && call
                    .receiver
                    .as_deref()
                    .and_then(|receiver| receiver.chars().next())
                    .is_some_and(char::is_lowercase);
            edge.extra
                .insert(crate::native::INFERRED_RECEIVER.into(), inferred.into());
        }
        if self.language_name == "ruby" {
            edge.extra
                .insert(crate::ruby::CALL_FACT.into(), true.into());
            if call.ruby_constant_receiver {
                edge.extra
                    .insert(crate::ruby::CONSTANT_RECEIVER.into(), true.into());
            }
        }
        edge.extra.insert("context".into(), "call".into());
        let receiver_context = match (&call.receiver, &call.receiver_type) {
            (Some(receiver), Some(receiver_type)) => {
                Some(format!("receiver={receiver} type={receiver_type}"))
            }
            (Some(receiver), None) => Some(format!("receiver={receiver}")),
            _ => None,
        };
        if let Some(receiver_context) = receiver_context {
            edge.extra
                .insert("receiver_context".into(), receiver_context.into());
        }
    }
    fn emit_unresolved_call(&mut self, call: &CallSite) {
        let target = make_id(&["__graphoxide_call", &call.name]);
        self.add_edge(
            call.source.clone(),
            target,
            "calls",
            call.line,
            Confidence::Extracted,
        );
        if let Some(edge) = self.edges.last_mut() {
            edge.extra.insert("unresolved_call".into(), true.into());
            edge.extra.insert("callee".into(), call.name.clone().into());
            edge.extra
                .insert("member_call".into(), call.member_call.into());
        }
        self.annotate_call_edge(call);
    }
    fn resolve_calls(&mut self) {
        self.definitions.values_mut().for_each(|ids| ids.sort());
        let calls = std::mem::take(&mut self.calls);
        let mut seen_pairs = BTreeSet::new();
        let mut seen_unresolved = BTreeSet::new();
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
                        if call.member_call {
                            if matches!(self.language_name, "go" | "rust" | "java") {
                                let Some(receiver_type) = call.receiver_type.as_deref() else {
                                    return false;
                                };
                                return self
                                    .method_owners
                                    .get(target)
                                    .and_then(|owner| self.node_labels.get(owner))
                                    .is_some_and(|owner| owner == &definition_key(receiver_type));
                            }
                            return false;
                        }
                        // An unqualified call may refer to a free function, or to
                        // an implicit receiver method on the current owner.  It
                        // must not bind to a method on an unrelated type merely
                        // because that method name is unique in the file.
                        return self.method_owners.get(target).is_none_or(|owner| {
                            self.method_owners.get(&call.source) == Some(owner)
                        });
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
            } else {
                let target = make_id(&["__graphoxide_call", &call.name]);
                if !seen_unresolved.insert((
                    call.source.clone(),
                    target,
                    call.member_call,
                    call.receiver.clone(),
                    call.receiver_type.clone(),
                )) {
                    continue;
                }
                self.emit_unresolved_call(&call);
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
        for (source, name, relation, context, line) in refs {
            let key = normalize_name(&name);
            if let Some(target) = self
                .definitions
                .get(&key)
                .filter(|ids| ids.len() == 1)
                .and_then(|ids| ids.first())
                .cloned()
            {
                if target != source {
                    let edge_start = self.edges.len();
                    self.add_edge(source, target, &relation, line, Confidence::Extracted);
                    if self.edges.len() > edge_start {
                        if let Some(context) = context {
                            self.edges[edge_start]
                                .extra
                                .insert("context".into(), context.into());
                        }
                    }
                }
            } else if !name.is_empty() {
                let target = make_id(&[&name]);
                if !self.seen.contains(&target) {
                    self.add_reference_node(target.clone(), name);
                }
                if target != source {
                    let edge_start = self.edges.len();
                    self.add_edge(source, target, &relation, line, Confidence::Extracted);
                    if self.edges.len() > edge_start {
                        if let Some(context) = context {
                            self.edges[edge_start]
                                .extra
                                .insert("context".into(), context.into());
                        }
                    }
                }
            }
        }
    }
}

fn indirect_scope_boundary(language: &str, kind: &str) -> bool {
    if language == "python" {
        matches!(kind, "function_definition" | "lambda")
    } else if matches!(language, "javascript" | "typescript" | "tsx") {
        matches!(
            kind,
            "arrow_function"
                | "function_declaration"
                | "function_expression"
                | "generator_function"
                | "generator_function_declaration"
                | "method_definition"
        )
    } else {
        false
    }
}

fn indirect_nested_container(language: &str, kind: &str) -> bool {
    indirect_scope_boundary(language, kind)
        || (language == "python" && kind == "class_definition")
        || (matches!(language, "javascript" | "typescript" | "tsx")
            && matches!(kind, "class" | "class_declaration" | "class_expression"))
}

fn binding_pattern_contains(node: TsNode<'_>, name: &str, source: &[u8]) -> bool {
    if node.kind() == "identifier" {
        return node.utf8_text(source).unwrap_or("").trim() == name;
    }
    if matches!(
        node.kind(),
        "type" | "type_annotation" | "predefined_type" | "type_identifier"
    ) {
        return false;
    }
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .any(|child| binding_pattern_contains(child, name, source));
    found
}

fn parameters_bind(scope: TsNode<'_>, name: &str, source: &[u8]) -> bool {
    scope
        .child_by_field_name("parameters")
        .is_some_and(|parameters| binding_pattern_contains(parameters, name, source))
}

fn indirect_scope_binds(scope: TsNode<'_>, name: &str, source: &[u8], language: &str) -> bool {
    if parameters_bind(scope, name, source) {
        return true;
    }
    fn visit(node: TsNode<'_>, name: &str, source: &[u8], language: &str) -> bool {
        if indirect_nested_container(language, node.kind()) {
            return false;
        }
        let target = match node.kind() {
            "assignment" | "assignment_expression" | "augmented_assignment" => {
                node.child_by_field_name("left")
            }
            "variable_declarator" => node.child_by_field_name("name"),
            "for_statement" | "for_in_statement" => node.child_by_field_name("left"),
            _ => None,
        };
        if target.is_some_and(|target| binding_pattern_contains(target, name, source)) {
            return true;
        }
        let mut cursor = node.walk();
        let found = node
            .named_children(&mut cursor)
            .any(|child| visit(child, name, source, language));
        found
    }

    let mut cursor = scope.walk();
    let found = scope
        .named_children(&mut cursor)
        .any(|child| visit(child, name, source, language));
    found
}

fn indirect_module_rebinds(root: TsNode<'_>, name: &str, source: &[u8], language: &str) -> bool {
    fn visit(node: TsNode<'_>, name: &str, source: &[u8], language: &str) -> bool {
        if indirect_nested_container(language, node.kind()) {
            return false;
        }
        if node.kind() == "variable_declarator" {
            let target = node.child_by_field_name("name");
            let callable = node.child_by_field_name("value").is_some_and(|value| {
                matches!(
                    value.kind(),
                    "arrow_function" | "function_expression" | "function" | "generator_function"
                )
            });
            if !callable
                && target.is_some_and(|target| binding_pattern_contains(target, name, source))
            {
                return true;
            }
        } else if matches!(
            node.kind(),
            "assignment" | "assignment_expression" | "augmented_assignment"
        ) && node
            .child_by_field_name("left")
            .is_some_and(|target| binding_pattern_contains(target, name, source))
        {
            return true;
        }
        let mut cursor = node.walk();
        let found = node
            .named_children(&mut cursor)
            .any(|child| visit(child, name, source, language));
        found
    }

    let mut cursor = root.walk();
    let found = root
        .named_children(&mut cursor)
        .any(|child| visit(child, name, source, language));
    found
}

fn python_getattr_reference(node: TsNode<'_>, source: &[u8]) -> Option<(String, usize)> {
    let function = node.child_by_field_name("function")?;
    if function.kind() != "identifier" || function.utf8_text(source).ok()?.trim() != "getattr" {
        return None;
    }
    let arguments = node.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let positional = arguments
        .named_children(&mut cursor)
        .filter(|argument| argument.kind() != "keyword_argument")
        .collect::<Vec<_>>();
    let name = *positional.get(1)?;
    if name.kind() != "string" {
        return None;
    }
    let mut cursor = name.walk();
    if name
        .named_children(&mut cursor)
        .any(|child| child.kind() == "interpolation")
    {
        return None;
    }
    let raw = name.utf8_text(source).ok()?.trim();
    let value = raw.trim_matches(['\'', '"']);
    (!value.is_empty()).then(|| (value.to_owned(), name.start_position().row + 1))
}

fn collect_indirect_value_identifiers<'a>(node: TsNode<'a>, out: &mut Vec<TsNode<'a>>) {
    if node.kind() == "identifier" {
        out.push(node);
    } else if node.kind() == "expression_list" {
        let mut cursor = node.walk();
        out.extend(
            node.named_children(&mut cursor)
                .filter(|child| child.kind() == "identifier"),
        );
    }
}

fn collect_typescript_receiver_types(root: TsNode<'_>, source: &[u8]) -> HashMap<String, String> {
    fn visit(node: TsNode<'_>, source: &[u8], table: &mut HashMap<String, String>) {
        if node.kind() == "variable_declarator" {
            let binding = node
                .child_by_field_name("name")
                .filter(|name| name.kind() == "identifier");
            let constructor = node
                .child_by_field_name("value")
                .filter(|value| value.kind() == "new_expression")
                .and_then(|value| value.child_by_field_name("constructor"))
                .filter(|constructor| {
                    matches!(constructor.kind(), "identifier" | "type_identifier")
                });
            if let (Some(binding), Some(constructor)) = (binding, constructor) {
                let binding = binding.utf8_text(source).unwrap_or("").trim();
                let constructor = constructor.utf8_text(source).unwrap_or("").trim();
                if !binding.is_empty() && !constructor.is_empty() {
                    table
                        .entry(binding.to_owned())
                        .or_insert_with(|| constructor.to_owned());
                }
            }
        } else if matches!(node.kind(), "required_parameter" | "optional_parameter") {
            let binding = node
                .child_by_field_name("pattern")
                .filter(|pattern| pattern.kind() == "identifier");
            let annotation = node.child_by_field_name("type");
            if let (Some(binding), Some(annotation)) = (binding, annotation) {
                let mut cursor = annotation.walk();
                let named = annotation.named_children(&mut cursor).collect::<Vec<_>>();
                if named.len() == 1 && named[0].kind() == "type_identifier" {
                    let binding = binding.utf8_text(source).unwrap_or("").trim();
                    let type_name = named[0].utf8_text(source).unwrap_or("").trim();
                    if !binding.is_empty() && !type_name.is_empty() {
                        table
                            .entry(binding.to_owned())
                            .or_insert_with(|| type_name.to_owned());
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            visit(child, source, table);
        }
    }

    let mut table = HashMap::new();
    visit(root, source, &mut table);
    table
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

fn find_descendant_field<'tree>(node: TsNode<'tree>, field: &str) -> Option<TsNode<'tree>> {
    if let Some(value) = node.child_by_field_name(field) {
        return Some(value);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(value) = find_descendant_field(child, field) {
            return Some(value);
        }
    }
    None
}

fn declaration_type_node(node: TsNode<'_>) -> Option<TsNode<'_>> {
    find_descendant_field(node, "type").or_else(|| {
        let mut cursor = node.walk();
        let found = node.named_children(&mut cursor).find(|child| {
            matches!(
                child.kind(),
                "type_identifier"
                    | "scoped_type_identifier"
                    | "generic_type"
                    | "qualified_identifier"
                    | "template_type"
                    | "generic_name"
                    | "predefined_type"
                    | "integral_type"
                    | "floating_point_type"
                    | "primitive_type"
            )
        });
        found
    })
}

fn collect_java_type_parameters(root: TsNode<'_>, source: &[u8]) -> HashSet<String> {
    let mut parameters = Vec::new();
    collect_descendants(root, &["type_parameter"], &mut parameters);
    parameters
        .into_iter()
        .filter_map(|parameter| {
            descendant_text(parameter, source, &["type_identifier", "identifier"])
        })
        .map(|name| normalize_name(&name))
        .filter(|name| !name.is_empty())
        .collect()
}

fn collect_java_package(root: TsNode<'_>, source: &[u8]) -> String {
    let mut declarations = Vec::new();
    collect_descendants(root, &["package_declaration"], &mut declarations);
    declarations
        .first()
        .and_then(|declaration| declaration.utf8_text(source).ok())
        .map(|declaration| {
            declaration
                .trim()
                .trim_start_matches("package")
                .trim()
                .trim_end_matches(';')
                .trim()
                .to_owned()
        })
        .unwrap_or_default()
}

fn collect_declared_interfaces(root: TsNode<'_>, source: &[u8]) -> HashSet<String> {
    let mut declarations = Vec::new();
    collect_descendants(
        root,
        &["interface_declaration", "protocol_declaration"],
        &mut declarations,
    );
    declarations
        .into_iter()
        .filter_map(|declaration| declaration.child_by_field_name("name"))
        .filter_map(|name| name.utf8_text(source).ok())
        .map(normalize_name)
        .filter(|name| !name.is_empty())
        .collect()
}

fn parse_type_names(text: &str) -> Vec<String> {
    fn flush(token: &mut String, tokens: &mut Vec<String>) {
        if token.is_empty() {
            return;
        }
        let raw = std::mem::take(token);
        let lower = raw.to_ascii_lowercase();
        if [
            "const",
            "volatile",
            "mutable",
            "signed",
            "unsigned",
            "struct",
            "class",
            "enum",
            "typename",
            "extends",
            "implements",
            "public",
            "private",
            "protected",
            "internal",
            "readonly",
            "ref",
            "out",
            "in",
            "params",
        ]
        .contains(&lower.as_str())
        {
            return;
        }
        let tail = raw
            .rsplit("::")
            .next()
            .unwrap_or(&raw)
            .rsplit('.')
            .next()
            .unwrap_or(&raw)
            .to_owned();
        if !tail.is_empty() && !tokens.contains(&tail) {
            tokens.push(tail);
        }
    }

    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in text.chars() {
        if character.is_alphanumeric() || character == '_' || character == ':' || character == '.' {
            token.push(character);
        } else {
            flush(&mut token, &mut tokens);
        }
    }
    flush(&mut token, &mut tokens);
    tokens
}

fn type_reference_noise(language: &str, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if [
        "void", "bool", "boolean", "byte", "char", "short", "int", "long", "float", "double",
        "decimal", "usize", "isize", "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "f32",
        "f64", "size_t", "auto", "var", "null", "nullptr",
    ]
    .contains(&lower.as_str())
    {
        return true;
    }
    match language {
        "java" => [
            "string",
            "list",
            "arraylist",
            "map",
            "set",
            "collection",
            "optional",
            "object",
            "integer",
            "character",
        ]
        .contains(&lower.as_str()),
        "csharp" => ["string", "object", "dynamic"].contains(&lower.as_str()),
        _ => false,
    }
}

fn collect_parent_type_names(node: TsNode<'_>, source: &[u8]) -> Vec<(String, Vec<String>)> {
    fn visit(node: TsNode<'_>, source: &[u8], found: &mut Vec<(String, Vec<String>)>) {
        if matches!(
            node.kind(),
            "generic_type" | "template_type" | "generic_name"
        ) {
            let names = parse_type_names(node.utf8_text(source).unwrap_or(""));
            if let Some((primary, arguments)) = names.split_first() {
                found.push((primary.clone(), arguments.to_vec()));
            }
            return;
        }
        if matches!(
            node.kind(),
            "type_identifier" | "identifier" | "constant" | "scope_resolution"
        ) {
            let names = parse_type_names(node.utf8_text(source).unwrap_or(""));
            if let Some((primary, arguments)) = names.split_first() {
                found.push((primary.clone(), arguments.to_vec()));
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            visit(child, source, found);
        }
    }

    let mut found = Vec::new();
    visit(node, source, &mut found);
    found.sort();
    found.dedup();
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

fn collect_python_type_refs(
    node: TsNode<'_>,
    source: &[u8],
    generic: bool,
    out: &mut Vec<(String, bool)>,
) {
    let keep = |name: &str| {
        !python_type_container(name) && !python_annotation_noise(name) && !name.is_empty()
    };
    match node.kind() {
        "type" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_python_type_refs(child, source, generic, out);
            }
        }
        "identifier" => {
            let name = node.utf8_text(source).unwrap_or("");
            if keep(name) {
                out.push((name.to_owned(), generic));
            }
        }
        "attribute" => {
            let name = node
                .child_by_field_name("attribute")
                .and_then(|attribute| attribute.utf8_text(source).ok())
                .unwrap_or_else(|| {
                    node.utf8_text(source)
                        .unwrap_or("")
                        .rsplit('.')
                        .next()
                        .unwrap_or("")
                });
            if keep(name) {
                out.push((name.to_owned(), generic));
            }
        }
        "generic_type" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "identifier" {
                    let name = child.utf8_text(source).unwrap_or("");
                    if keep(name) {
                        out.push((name.to_owned(), generic));
                    }
                } else if child.kind() == "type_parameter" {
                    let mut parameter_cursor = child.walk();
                    for parameter in child.named_children(&mut parameter_cursor) {
                        collect_python_type_refs(parameter, source, true, out);
                    }
                }
            }
        }
        "subscript" => {
            let value = node.child_by_field_name("value");
            if let Some(value) = value {
                collect_python_type_refs(value, source, generic, out);
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if value.is_some_and(|value| value.id() == child.id()) {
                    continue;
                }
                collect_python_type_refs(child, source, true, out);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_python_type_refs(child, source, generic, out);
            }
        }
    }
}

fn python_type_container(name: &str) -> bool {
    matches!(
        name,
        "list"
            | "dict"
            | "set"
            | "tuple"
            | "frozenset"
            | "type"
            | "List"
            | "Dict"
            | "Set"
            | "Tuple"
            | "FrozenSet"
            | "Type"
            | "Optional"
            | "Union"
            | "Sequence"
            | "Iterable"
            | "Mapping"
            | "MutableMapping"
            | "Iterator"
            | "Callable"
            | "Awaitable"
            | "AsyncIterable"
            | "AsyncIterator"
            | "Coroutine"
            | "Generator"
            | "AsyncGenerator"
            | "ContextManager"
            | "AsyncContextManager"
            | "Annotated"
            | "ClassVar"
            | "Final"
            | "Literal"
            | "Concatenate"
            | "ParamSpec"
            | "TypeVar"
            | "None"
            | "Ellipsis"
    )
}

fn python_annotation_noise(name: &str) -> bool {
    matches!(
        name,
        "str"
            | "int"
            | "float"
            | "bool"
            | "bytes"
            | "bytearray"
            | "complex"
            | "object"
            | "True"
            | "False"
            | "MagicMock"
            | "Mock"
            | "AsyncMock"
            | "NonCallableMock"
            | "NonCallableMagicMock"
            | "PropertyMock"
            | "patch"
            | "sentinel"
    )
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

/// Recover the logical scan root from the physical file and its already
/// relativized `source_file`.  Extraction deliberately does not pass cache or
/// checkout roots into the language walkers, so this inverse keeps import
/// target IDs anchored to the corpus instead of the host path.
fn inferred_scan_root(physical_path: &Path, source_file: &str) -> Option<PathBuf> {
    let source = Path::new(source_file);
    if source.as_os_str().is_empty() || source.is_absolute() {
        return None;
    }
    let component_count = source
        .components()
        .filter(|component| matches!(component, std::path::Component::Normal(_)))
        .count();
    if component_count == 0 {
        return None;
    }
    let mut root = physical_path.to_path_buf();
    for _ in 0..component_count {
        if !root.pop() {
            return None;
        }
    }
    Some(root)
}

/// Mint a checkout-independent ID for a real import target outside the scan
/// root.  Only the target-side suffix below the closest common ancestor is
/// retained (bounded to three components), which preserves useful sibling
/// context such as `lib/foo.h` without embedding `/Users/...` or `/tmp/...`.
fn portable_external_target_id(root: &Path, target: &Path) -> String {
    let root_components = root.components().collect::<Vec<_>>();
    let target_components = target.components().collect::<Vec<_>>();
    let common = root_components
        .iter()
        .zip(&target_components)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = &target_components[common..];
    let suffix = if suffix.len() > 3 {
        &suffix[suffix.len() - 3..]
    } else {
        suffix
    };
    let portable = suffix
        .iter()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    make_id(&["ext", &portable])
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

fn clean_comment(raw: &str) -> String {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("/**")
        .or_else(|| trimmed.strip_prefix("/*"))
        .unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix("*/").unwrap_or(trimmed);
    trimmed
        .lines()
        .map(|line| {
            line.trim()
                .trim_start_matches("//")
                .trim_start_matches('#')
                .trim_start_matches('*')
                .trim()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn rationale_label(value: &str) -> String {
    const WIDTH: usize = 80;
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= WIDTH {
        return normalized;
    }
    let candidate: String = normalized.chars().take(WIDTH - 1).collect();
    let word_boundary = candidate.rfind(char::is_whitespace);
    let mut core = word_boundary
        .filter(|boundary| *boundary >= (WIDTH / 3))
        .map_or(candidate.as_str(), |boundary| &candidate[..boundary])
        .trim_end()
        .trim_end_matches('.');
    if core.is_empty() {
        core = candidate.trim_end();
    }
    format!("{core}…")
}

fn normalize_name(value: &str) -> String {
    value
        .trim_start_matches('.')
        .trim_end_matches("()")
        .to_lowercase()
}

fn definition_key(value: &str) -> String {
    value
        .trim_start_matches('.')
        .trim_end_matches("()")
        .to_lowercase()
}

fn python_decorator_noise(value: &str) -> bool {
    matches!(
        value,
        "property"
            | "staticmethod"
            | "classmethod"
            | "abstractmethod"
            | "abstractproperty"
            | "cached_property"
            | "wraps"
            | "lru_cache"
            | "cache"
            | "singledispatch"
            | "singledispatchmethod"
            | "total_ordering"
            | "contextmanager"
            | "asynccontextmanager"
            | "overload"
            | "override"
            | "final"
            | "no_type_check"
            | "runtime_checkable"
            | "dataclass"
    )
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

fn go_predeclared_function(value: &str) -> bool {
    matches!(
        value,
        "append"
            | "cap"
            | "clear"
            | "close"
            | "complex"
            | "copy"
            | "delete"
            | "imag"
            | "len"
            | "make"
            | "max"
            | "min"
            | "new"
            | "panic"
            | "print"
            | "println"
            | "real"
            | "recover"
    )
}
