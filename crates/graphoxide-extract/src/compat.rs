//! Structured compatibility extractors for languages whose tree-sitter grammar
//! is not compiled into the minimal binary yet.
//!
//! These scanners preserve the upstream extractor contract (scoped symbol
//! nodes, semantic edge roles, and source locations) instead of routing source
//! through the old definition-only fallback regex.

use crate::project_path::{normalize_project_path, source_relative_project_path, ProjectPath};
use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

pub(crate) fn extract_compat(
    path: &Path,
    text: &str,
    source_file: &str,
    allow_path_probes: bool,
) -> anyhow::Result<Option<Extraction>> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "kt" | "kts" => extract_kotlin(path, text, source_file).map(Some),
        "scala" => extract_scala(path, text, source_file).map(Some),
        "php" => extract_php(path, text, source_file).map(Some),
        "swift" => crate::swift::extract_swift(path, text, source_file).map(Some),
        "ex" | "exs" => extract_elixir(path, text, source_file).map(Some),
        "m" | "mm" | "h" => extract_objc(path, text, source_file).map(Some),
        "jl" => extract_julia(path, text, source_file).map(Some),
        "f" | "f90" | "f95" | "f03" | "f08" => extract_fortran(path, text, source_file).map(Some),
        "ps1" | "psm1" => extract_powershell(path, text, source_file).map(Some),
        "psd1" => extract_powershell_manifest(path, text, source_file).map(Some),
        "groovy" | "gradle" => extract_groovy(path, text, source_file).map(Some),
        "dm" | "dme" => extract_dm(path, text, source_file, allow_path_probes).map(Some),
        "dmm" => extract_dmm(path, text, source_file).map(Some),
        "dmf" => extract_dmf(path, text, source_file).map(Some),
        "sln" => extract_sln(path, text, source_file).map(Some),
        "csproj" | "fsproj" | "vbproj" => extract_dotnet_project(path, text, source_file).map(Some),
        "xaml" => extract_xaml(path, text, source_file).map(Some),
        "razor" | "cshtml" => extract_razor(path, text, source_file).map(Some),
        "cls" | "trigger" => extract_apex(path, text, source_file).map(Some),
        "sv" | "svh" => extract_systemverilog(path, text, source_file).map(Some),
        _ => Ok(None),
    }
}

struct Builder<'a> {
    source_file: &'a str,
    stem: String,
    file_id: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    seen_nodes: HashSet<String>,
    seen_edges: HashSet<(String, String, String, String)>,
    definitions: HashMap<String, Vec<String>>,
    labels: HashMap<String, String>,
    interfaces: HashSet<String>,
}

impl<'a> Builder<'a> {
    fn new(path: &Path, source_file: &'a str) -> Self {
        let stem = Path::new(source_file)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let file_id = make_id(&[&stem]);
        let mut builder = Self {
            source_file,
            stem,
            file_id: file_id.clone(),
            nodes: Vec::new(),
            edges: Vec::new(),
            seen_nodes: HashSet::new(),
            seen_edges: HashSet::new(),
            definitions: HashMap::new(),
            labels: HashMap::new(),
            interfaces: HashSet::new(),
        };
        builder.insert_node(
            file_id,
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or(source_file),
            "file",
            1,
            source_file,
        );
        builder
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
        self.labels.insert(id.clone(), label.to_owned());
        self.definitions
            .entry(normalize_label(label))
            .or_default()
            .push(id.clone());
        self.nodes.push(Node {
            id,
            label: label.to_owned(),
            file_type: "code".into(),
            source_file: source.into(),
            source_location: Some(format!("L{line}")),
            community: None,
            extra: BTreeMap::from([
                ("_origin".into(), "fallback".into()),
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
        self.edge(
            &parent,
            &id,
            if kind == "function" && owner.is_some() {
                "method"
            } else {
                "contains"
            },
            None,
            line,
            Confidence::Extracted,
        );
        id
    }

    fn labeled_definition(
        &mut self,
        name: &str,
        label: &str,
        owner: Option<&str>,
        kind: &str,
        line: usize,
    ) -> String {
        let id = owner
            .map(|owner| make_id(&[owner, name]))
            .unwrap_or_else(|| make_id(&[&self.stem, name]));
        self.insert_node(id.clone(), label, kind, line, self.source_file);
        let parent = owner.unwrap_or(&self.file_id).to_owned();
        self.edge(
            &parent,
            &id,
            if kind == "function" && owner.is_some() {
                "method"
            } else {
                "contains"
            },
            None,
            line,
            Confidence::Extracted,
        );
        id
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
                .expect("new external node")
                .extra
                .insert("origin_file".into(), self.source_file.into());
        }
        id
    }

    fn module(&mut self, name: &str, line: usize) -> String {
        let id = make_id(&[name]);
        self.insert_node(id.clone(), name, "module", line, self.source_file);
        id
    }

    fn unique_definition(&self, name: &str) -> Option<String> {
        self.definitions
            .get(&normalize_label(name))
            .filter(|ids| ids.len() == 1)
            .and_then(|ids| ids.first())
            .cloned()
    }

    fn method_owner(&self, method: &str) -> Option<String> {
        self.edges
            .iter()
            .find(|edge| edge.relation == "method" && edge.true_target() == method)
            .map(|edge| edge.true_source().to_owned())
    }

    fn scoped_method(&self, method: &str, name: &str, scope: &str) -> Option<String> {
        let owner = self.method_owner(method)?;
        let mut frontier = if scope == "super" {
            self.direct_supertypes(&owner)
        } else {
            vec![owner]
        };
        let mut seen = HashSet::new();
        while !frontier.is_empty() {
            frontier.sort();
            frontier.dedup();
            frontier.retain(|candidate| seen.insert(candidate.clone()));
            if frontier.is_empty() {
                break;
            }
            let mut candidates = self
                .definitions
                .get(&normalize_label(name))
                .into_iter()
                .flatten()
                .filter(|candidate| {
                    self.edges.iter().any(|edge| {
                        edge.relation == "method"
                            && edge.true_target() == candidate.as_str()
                            && frontier
                                .iter()
                                .any(|owner| edge.true_source() == owner.as_str())
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            candidates.sort();
            candidates.dedup();
            match candidates.as_slice() {
                [only] => return Some(only.clone()),
                [] => {}
                _ => return None,
            }
            frontier = frontier
                .iter()
                .flat_map(|owner| self.direct_supertypes(owner))
                .collect();
        }
        None
    }

    fn direct_supertypes(&self, owner: &str) -> Vec<String> {
        self.edges
            .iter()
            .filter(|edge| {
                edge.true_source() == owner
                    && matches!(
                        edge.relation.as_str(),
                        "inherits" | "implements" | "mixes_in" | "extends"
                    )
            })
            .map(|edge| edge.true_target().to_owned())
            .collect()
    }

    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        context: Option<&str>,
        line: usize,
        confidence: Confidence,
    ) {
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
        self.edges.push(Edge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            confidence,
            source_file: self.source_file.into(),
            extra,
        });
    }

    fn reference(
        &mut self,
        source: &str,
        type_text: &str,
        context: &str,
        line: usize,
        language: &str,
    ) {
        let names = type_names(type_text);
        let Some((primary, generic_arguments)) = names.split_first() else {
            return;
        };
        if !type_noise(language, primary) {
            let target = self.external(primary, line);
            self.edge(
                source,
                &target,
                "references",
                Some(context),
                line,
                Confidence::Extracted,
            );
        }
        for argument in generic_arguments {
            if type_noise(language, argument) {
                continue;
            }
            let target = self.external(argument, line);
            self.edge(
                source,
                &target,
                "references",
                Some("generic_arg"),
                line,
                Confidence::Extracted,
            );
        }
    }
}

#[derive(Clone)]
struct Scope {
    id: String,
    start: usize,
    end: usize,
}

fn owner_at(scopes: &[Scope], offset: usize) -> Option<&str> {
    scopes
        .iter()
        .filter(|scope| scope.start < offset && offset < scope.end)
        .max_by_key(|scope| scope.start)
        .map(|scope| scope.id.as_str())
}

fn brace_range(text: &str, search_from: usize) -> Option<(usize, usize)> {
    let open = text[search_from..].find('{')? + search_from;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (offset, byte) in bytes[open..].iter().copied().enumerate() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active {
                quote = None;
            }
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            continue;
        }
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((open, open + offset + 1));
                }
            }
            _ => {}
        }
    }
    Some((open, text.len()))
}

fn declaration_body_range(text: &str, search_from: usize) -> Option<(usize, usize)> {
    for (offset, character) in text[search_from..].char_indices() {
        match character {
            ';' => return None,
            '{' => return brace_range(text, search_from + offset),
            _ if character.is_whitespace() => {}
            _ => return None,
        }
    }
    None
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn normalize_label(label: &str) -> String {
    label
        .trim_matches(|character| matches!(character, '.' | '(' | ')' | '<' | '>'))
        .to_ascii_lowercase()
}

fn type_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut token = String::new();
    let flush = |token: &mut String, names: &mut Vec<String>| {
        if token.is_empty() {
            return;
        }
        let raw = std::mem::take(token);
        let lower = raw.to_ascii_lowercase();
        if [
            "private",
            "protected",
            "public",
            "internal",
            "open",
            "override",
            "abstract",
            "final",
            "static",
            "readonly",
            "const",
            "lateinit",
            "vararg",
            "in",
            "out",
            "by",
        ]
        .contains(&lower.as_str())
        {
            return;
        }
        let tail = raw
            .rsplit("::")
            .next()
            .unwrap_or(&raw)
            .rsplit(['.', '\\'])
            .next()
            .unwrap_or(&raw)
            .trim_start_matches('$')
            .to_owned();
        if !tail.is_empty() && !names.contains(&tail) {
            names.push(tail);
        }
    };
    for character in text.chars() {
        if character.is_alphanumeric()
            || character == '_'
            || character == '$'
            || character == ':'
            || character == '.'
            || character == '\\'
        {
            token.push(character);
        } else {
            flush(&mut token, &mut names);
        }
    }
    flush(&mut token, &mut names);
    names
}

fn type_noise(language: &str, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if [
        "void", "unit", "bool", "boolean", "byte", "char", "short", "int", "long", "float",
        "double", "string", "array", "mixed", "null", "nothing", "any", "t",
    ]
    .contains(&lower.as_str())
    {
        return true;
    }
    language == "kotlin"
        && [
            "list",
            "mutablelist",
            "map",
            "mutablemap",
            "set",
            "mutableset",
        ]
        .contains(&lower.as_str())
}

fn extract_kotlin(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let imports = Regex::new(r"(?m)^\s*import\s+([A-Za-z_][A-Za-z0-9_.]*)")?;
    for capture in imports.captures_iter(text) {
        let module = capture[1].to_owned();
        let target = builder.external(&module, line_of(text, capture.get(0).unwrap().start()));
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "imports",
            Some("import"),
            line_of(text, capture.get(0).unwrap().start()),
            Confidence::Extracted,
        );
    }

    let declarations = Regex::new(
        r"(?m)^\s*(?:(data|open|abstract|sealed|enum)\s+)?(class|interface|object)\s+([A-Za-z_][A-Za-z0-9_]*)(?:<[^>]+>)?(?:\s*\([^\n{}]*\))?(?:\s*:\s*([^\n{]+))?",
    )?;
    let mut scopes = Vec::new();
    let mut declaration_rows = Vec::new();
    for capture in declarations.captures_iter(text) {
        let name = &capture[3];
        let line = line_of(text, capture.get(0).unwrap().start());
        let owner = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned);
        let id = builder.definition(name, owner.as_deref(), "class", line);
        if &capture[2] == "interface" {
            builder.interfaces.insert(normalize_label(name));
        }
        if let Some((open, end)) = brace_range(text, capture.get(0).unwrap().end()) {
            scopes.push(Scope {
                id: id.clone(),
                start: open,
                end,
            });
        }
        declaration_rows.push((
            capture.get(0).unwrap().start(),
            id.clone(),
            capture.get(4).map(|value| value.as_str().trim().to_owned()),
            capture
                .get(1)
                .map(|value| value.as_str() == "enum")
                .unwrap_or(false),
        ));
    }
    let enum_entries = Regex::new(r"(?m)^\s*([A-Z][A-Z0-9_]*)\s*,?")?;
    for (offset, id, parents, is_enum) in declaration_rows {
        let line = line_of(text, offset);
        if let Some(parents) = parents {
            for (index, parent_text) in parents.split(',').enumerate() {
                let names = type_names(parent_text);
                let Some(parent) = names.first() else {
                    continue;
                };
                let target = builder.external(parent, line);
                let relation = if parent_text.contains(" by ")
                    || builder.interfaces.contains(&normalize_label(parent))
                    || index > 0
                {
                    "implements"
                } else {
                    "inherits"
                };
                builder.edge(&id, &target, relation, None, line, Confidence::Extracted);
                for generic in names.iter().skip(1) {
                    if !type_noise("kotlin", generic) {
                        let target = builder.external(generic, line);
                        builder.edge(
                            &id,
                            &target,
                            "references",
                            Some("generic_arg"),
                            line,
                            Confidence::Extracted,
                        );
                    }
                }
            }
        }
        if is_enum && let Some(scope) = scopes.iter().find(|scope| scope.id == id) {
            for capture in enum_entries.captures_iter(&text[scope.start + 1..scope.end]) {
                let entry_line = line_of(text, scope.start + 1 + capture.get(0).unwrap().start());
                let entry = builder.definition(&capture[1], Some(&id), "enum_case", entry_line);
                builder.edge(
                    &id,
                    &entry,
                    "case_of",
                    None,
                    entry_line,
                    Confidence::Extracted,
                );
            }
        }
    }

    let functions = Regex::new(
        r"(?m)^\s*(?:(?:private|protected|public|internal|override|open|abstract|suspend|inline|operator)\s+)*fun\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)\s*(?::\s*([^={\n]+))?",
    )?;
    // Anonymous objects are expressions rather than named declarations. Build
    // lightweight function ranges first so each object can be contained by
    // its lexical function without prematurely emitting mis-owned members.
    let enclosing_functions = functions
        .captures_iter(text)
        .filter_map(|capture| {
            let matched = capture.get(0)?;
            let owner = owner_at(&scopes, matched.start());
            let id = owner
                .map(|owner| make_id(&[owner, &capture[1]]))
                .unwrap_or_else(|| make_id(&[&builder.stem, &capture[1]]));
            let (start, end) = brace_range(text, matched.end())?;
            Some(Scope { id, start, end })
        })
        .collect::<Vec<_>>();
    let object_literals = Regex::new(r"(?m)\bobject\s*:\s*([^\n{]+)\{")?;
    for (ordinal, capture) in object_literals.captures_iter(text).enumerate() {
        let matched = capture.get(0).expect("Kotlin object literal");
        let line = line_of(text, matched.start());
        let parents = capture[1]
            .split(',')
            .filter_map(|parent| {
                let name = type_names(parent).first()?.clone();
                let target = builder.external(&name, line);
                let relation = if builder.interfaces.contains(&normalize_label(&name)) {
                    "implements"
                } else {
                    "inherits"
                };
                Some((target, relation))
            })
            .collect::<Vec<_>>();
        let Some((first_target, _)) = parents.first() else {
            continue;
        };
        let label = builder
            .labels
            .get(first_target)
            .cloned()
            .unwrap_or_else(|| capture[1].trim().to_owned());
        let owner = owner_at(&enclosing_functions, matched.start())
            .or_else(|| owner_at(&scopes, matched.start()))
            .map(str::to_owned);
        let identity = format!("object_{}_{}", line, ordinal + 1);
        let id = builder.labeled_definition(&identity, &label, owner.as_deref(), "class", line);
        for (target, relation) in parents {
            builder.edge(&id, &target, relation, None, line, Confidence::Extracted);
        }
        if let Some((start, end)) = brace_range(text, matched.start()) {
            scopes.push(Scope { id, start, end });
        }
    }
    let mut function_scopes = Vec::new();
    for capture in functions.captures_iter(text) {
        let owner = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned);
        let line = line_of(text, capture.get(0).unwrap().start());
        let id = builder.definition(&capture[1], owner.as_deref(), "function", line);
        for parameter in capture[2].split(',') {
            if let Some((_, type_text)) = parameter.split_once(':') {
                builder.reference(&id, type_text, "parameter_type", line, "kotlin");
            }
        }
        if let Some(return_type) = capture.get(3) {
            builder.reference(&id, return_type.as_str(), "return_type", line, "kotlin");
        }
        if let Some((open, end)) = callable_body_range(text, capture.get(0).unwrap().end()) {
            function_scopes.push(Scope {
                id,
                start: open,
                end,
            });
        }
    }
    let fields = Regex::new(
        r"(?m)^\s*(?:private\s+|protected\s+|public\s+|override\s+)*(?:val|var)\s+[A-Za-z_][A-Za-z0-9_]*\s*:\s*([^=\n]+)",
    )?;
    for capture in fields.captures_iter(text) {
        if let Some(owner) = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned) {
            builder.reference(
                &owner,
                capture[1].trim(),
                "field",
                line_of(text, capture.get(0).unwrap().start()),
                "kotlin",
            );
        }
    }
    emit_calls(
        &mut builder,
        text,
        &function_scopes,
        &["if", "for", "while", "when", "return", "fun"],
        true,
    )?;
    Ok(builder.finish())
}

fn extract_scala(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let imports = Regex::new(r"(?m)^\s*import\s+([^\s{,]+)")?;
    for capture in imports.captures_iter(text) {
        let Some(module) = static_scala_import_target(&capture[1]) else {
            continue;
        };
        let target = builder.external(module, line_of(text, capture.get(0).unwrap().start()));
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "imports",
            Some("import"),
            line_of(text, capture.get(0).unwrap().start()),
            Confidence::Extracted,
        );
    }
    let declarations = Regex::new(
        r"(?m)^\s*(?:(case|abstract|sealed)\s+)?(class|trait|object)\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s*\(([^)]*)\))?(?:\s+extends\s+([^\n{]+))?",
    )?;
    let mut scopes = Vec::new();
    let mut rows = Vec::new();
    for capture in declarations.captures_iter(text) {
        let owner = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned);
        let line = line_of(text, capture.get(0).unwrap().start());
        let id = builder.definition(&capture[3], owner.as_deref(), "class", line);
        if &capture[2] == "trait" {
            builder.interfaces.insert(normalize_label(&capture[3]));
        }
        if let Some((open, end)) = brace_range(text, capture.get(0).unwrap().end()) {
            scopes.push(Scope {
                id: id.clone(),
                start: open,
                end,
            });
        }
        rows.push((
            id,
            line,
            capture.get(4).map(|value| value.as_str().to_owned()),
            capture.get(5).map(|value| value.as_str().to_owned()),
        ));
    }
    for (id, line, constructor, parents) in rows {
        if let Some(constructor) = constructor {
            for parameter in constructor.split(',') {
                if let Some((_, type_text)) = parameter.split_once(':') {
                    builder.reference(&id, type_text, "field", line, "scala");
                }
            }
        }
        if let Some(parents) = parents {
            for (index, parent) in parents.split(" with ").enumerate() {
                let Some(name) = type_names(parent).first().cloned() else {
                    continue;
                };
                let target = builder.external(&name, line);
                builder.edge(
                    &id,
                    &target,
                    if index == 0 { "inherits" } else { "mixes_in" },
                    None,
                    line,
                    Confidence::Extracted,
                );
            }
        }
    }
    let self_types =
        Regex::new(r"(?m)^\s*(?:this|[A-Za-z_][A-Za-z0-9_]*)\s*(?::\s*([^=\n]+))?\s*=>")?;
    for capture in self_types.captures_iter(text) {
        let matched = capture.get(0).expect("Scala self type");
        let Some(type_text) = capture.get(1) else {
            continue;
        };
        let Some(owner) = owner_at(&scopes, matched.start()).map(str::to_owned) else {
            continue;
        };
        let line = line_of(text, matched.start());
        for member in type_text.as_str().split(" with ") {
            let Some(name) = type_names(member).first().cloned() else {
                continue;
            };
            if type_noise("scala", &name) {
                continue;
            }
            let target = builder.external(&name, line);
            builder.edge(
                &owner,
                &target,
                "requires",
                None,
                line,
                Confidence::Extracted,
            );
        }
    }
    let functions = Regex::new(
        r"(?m)^\s*(?:(?:private|protected|override|final|implicit|async)\s+)*def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)\s*(?::\s*([^=\n{]+))?",
    )?;
    let mut function_scopes = Vec::new();
    for capture in functions.captures_iter(text) {
        let owner = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned);
        let line = line_of(text, capture.get(0).unwrap().start());
        let id = builder.definition(&capture[1], owner.as_deref(), "function", line);
        for parameter in capture[2].split(',') {
            if let Some((_, type_text)) = parameter.split_once(':') {
                builder.reference(&id, type_text, "parameter_type", line, "scala");
            }
        }
        if let Some(return_type) = capture.get(3) {
            builder.reference(&id, return_type.as_str(), "return_type", line, "scala");
        }
        if let Some((open, end)) = callable_body_range(text, capture.get(0).unwrap().end()) {
            function_scopes.push(Scope {
                id,
                start: open,
                end,
            });
        }
    }
    let fields = Regex::new(
        r"(?m)^\s*(?:private\s+|protected\s+)?(?:val|var)\s+[A-Za-z_][A-Za-z0-9_]*\s*:\s*([^=\n]+)",
    )?;
    for capture in fields.captures_iter(text) {
        if let Some(owner) = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned) {
            builder.reference(
                &owner,
                capture[1].trim(),
                "field",
                line_of(text, capture.get(0).unwrap().start()),
                "scala",
            );
        }
    }
    emit_calls(
        &mut builder,
        text,
        &function_scopes,
        &["if", "for", "while", "match", "return"],
        false,
    )?;
    Ok(builder.finish())
}

fn static_scala_import_target(raw: &str) -> Option<&str> {
    let raw = raw.trim_end_matches(';');
    let path = raw
        .strip_suffix("._")
        .or_else(|| raw.strip_suffix(".*"))
        .or_else(|| raw.strip_suffix('.'))
        .unwrap_or(raw);
    if path.is_empty() {
        return None;
    }

    let mut segment_start = 0usize;
    let mut quoted = false;
    for (offset, character) in path.char_indices() {
        match character {
            '`' => quoted = !quoted,
            '.' if !quoted => {
                if !static_scala_identifier(&path[segment_start..offset]) {
                    return None;
                }
                segment_start = offset + 1;
            }
            _ => {}
        }
    }
    (!quoted && static_scala_identifier(&path[segment_start..])).then_some(if raw.ends_with('.') {
        path
    } else {
        raw
    })
}

fn static_scala_identifier(value: &str) -> bool {
    if let Some(inner) = value
        .strip_prefix('`')
        .and_then(|value| value.strip_suffix('`'))
    {
        return !inner.is_empty()
            && !inner
                .chars()
                .any(|character| character.is_control() || character == '`');
    }
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && characters
            .all(|character| character == '_' || character == '$' || character.is_alphanumeric())
}

fn php_qualified_name(raw: &str, namespace: &str, aliases: &HashMap<String, String>) -> String {
    let raw = raw.trim();
    let absolute = raw.starts_with('\\');
    let raw = raw.trim_start_matches('\\');
    let (head, tail) = raw.split_once('\\').unwrap_or((raw, ""));
    if !absolute && let Some(imported) = aliases.get(&head.to_ascii_lowercase()) {
        return if tail.is_empty() {
            imported.clone()
        } else {
            format!("{imported}\\{tail}")
        };
    }
    if absolute || namespace.is_empty() {
        raw.to_owned()
    } else {
        format!("{namespace}\\{raw}")
    }
}

fn mark_php_node(builder: &mut Builder<'_>, id: &str, fqn: &str) {
    if let Some(node) = builder.nodes.iter_mut().find(|node| node.id == id) {
        node.extra.insert(crate::php::PHP_FQN.into(), fqn.into());
    }
}

fn php_target(builder: &mut Builder<'_>, fqn: &str, line: usize) -> String {
    let candidates = builder
        .nodes
        .iter()
        .filter(|node| !node.source_file.is_empty())
        .filter(|node| {
            node.extra
                .get(crate::php::PHP_FQN)
                .and_then(|value| value.as_str())
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(fqn))
        })
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    if let [target] = candidates.as_slice() {
        return target.clone();
    }
    builder.external(fqn, line)
}

fn mark_php_target(
    builder: &mut Builder<'_>,
    source: &str,
    target: &str,
    relation: &str,
    fqn: &str,
) {
    if let Some(edge) = builder.edges.iter_mut().rev().find(|edge| {
        edge.true_source() == source && edge.true_target() == target && edge.relation == relation
    }) {
        edge.extra
            .insert(crate::php::PHP_TARGET_FQN.into(), fqn.into());
    }
    mark_php_node(builder, target, fqn);
}

fn php_type_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut token = String::new();
    let flush = |token: &mut String, names: &mut Vec<String>| {
        if token.is_empty() {
            return;
        }
        let name = std::mem::take(token).trim_start_matches('$').to_owned();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    };
    for character in text.chars() {
        if character.is_alphanumeric() || matches!(character, '_' | '$' | '\\') {
            token.push(character);
        } else {
            flush(&mut token, &mut names);
        }
    }
    flush(&mut token, &mut names);
    names
}

#[allow(clippy::too_many_arguments)]
fn emit_php_type_references(
    builder: &mut Builder<'_>,
    source: &str,
    type_text: &str,
    context: &str,
    line: usize,
    namespace: &str,
    aliases: &HashMap<String, String>,
) {
    for (index, name) in php_type_names(type_text).into_iter().enumerate() {
        if type_noise("php", &name) {
            continue;
        }
        let fqn = php_qualified_name(&name, namespace, aliases);
        let target = php_target(builder, &fqn, line);
        let relation_context = if index == 0 { context } else { "generic_arg" };
        builder.edge(
            source,
            &target,
            "references",
            Some(relation_context),
            line,
            Confidence::Extracted,
        );
        mark_php_target(builder, source, &target, "references", &fqn);
    }
}

fn extract_php(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let namespace = Regex::new(r"(?im)^\s*namespace\s+([A-Za-z_][A-Za-z0-9_\\]*)\s*[;{]")?
        .captures(text)
        .map(|capture| capture[1].trim_matches('\\').to_owned())
        .unwrap_or_default();
    let declarations = Regex::new(
        r"(?m)^\s*(?:abstract\s+|final\s+)?(class|interface|trait|enum)\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s+extends\s+([A-Za-z_\\][A-Za-z0-9_\\]*))?(?:\s+implements\s+([^\n{]+))?",
    )?;
    let mut scopes = Vec::new();
    let mut rows = Vec::new();
    for capture in declarations.captures_iter(text) {
        let owner = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned);
        let line = line_of(text, capture.get(0).unwrap().start());
        let id = builder.definition(&capture[2], owner.as_deref(), "class", line);
        let fqn = if namespace.is_empty() {
            capture[2].to_owned()
        } else {
            format!("{namespace}\\{}", &capture[2])
        };
        mark_php_node(&mut builder, &id, &fqn);
        if &capture[1] == "interface" {
            builder.interfaces.insert(normalize_label(&capture[2]));
        }
        if let Some((open, end)) = brace_range(text, capture.get(0).unwrap().end()) {
            scopes.push(Scope {
                id: id.clone(),
                start: open,
                end,
            });
        }
        rows.push((
            id,
            line,
            capture.get(3).map(|value| value.as_str().to_owned()),
            capture.get(4).map(|value| value.as_str().to_owned()),
        ));
    }
    let imports = Regex::new(
        r"(?im)^\s*use\s+([A-Za-z_\\][A-Za-z0-9_\\]*)(?:\s+as\s+([A-Za-z_][A-Za-z0-9_]*))?\s*;",
    )?;
    let mut aliases = HashMap::new();
    for capture in imports.captures_iter(text) {
        let matched = capture.get(0).expect("PHP use statement");
        if owner_at(&scopes, matched.start()).is_some() {
            continue;
        }
        let fqn = capture[1].trim_matches('\\').to_owned();
        let alias = capture
            .get(2)
            .map(|value| value.as_str())
            .or_else(|| fqn.rsplit('\\').next())
            .unwrap_or(&fqn)
            .to_owned();
        aliases.insert(alias.to_ascii_lowercase(), fqn.clone());
        let line = line_of(text, matched.start());
        let target = php_target(&mut builder, &fqn, line);
        let file_id = builder.file_id.clone();
        builder.edge(
            &file_id,
            &target,
            "imports",
            Some("import"),
            line,
            Confidence::Extracted,
        );
        mark_php_target(&mut builder, &file_id, &target, "imports", &fqn);
    }
    for (id, line, base, interfaces) in rows {
        if let Some(base) = base {
            let fqn = php_qualified_name(&base, &namespace, &aliases);
            let target = php_target(&mut builder, &fqn, line);
            builder.edge(&id, &target, "inherits", None, line, Confidence::Extracted);
            mark_php_target(&mut builder, &id, &target, "inherits", &fqn);
        }
        if let Some(interfaces) = interfaces {
            for interface in interfaces.split(',') {
                let fqn = php_qualified_name(interface, &namespace, &aliases);
                let target = php_target(&mut builder, &fqn, line);
                builder.edge(
                    &id,
                    &target,
                    "implements",
                    None,
                    line,
                    Confidence::Extracted,
                );
                mark_php_target(&mut builder, &id, &target, "implements", &fqn);
            }
        }
    }
    let trait_uses = Regex::new(r"(?m)^\s*use\s+([\\]?[A-Za-z_][A-Za-z0-9_\\]*)\s*;")?;
    for capture in trait_uses.captures_iter(text) {
        if let Some(owner) = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned) {
            let line = line_of(text, capture.get(0).unwrap().start());
            let fqn = php_qualified_name(&capture[1], &namespace, &aliases);
            let target = php_target(&mut builder, &fqn, line);
            builder.edge(
                &owner,
                &target,
                "mixes_in",
                None,
                line,
                Confidence::Extracted,
            );
            mark_php_target(&mut builder, &owner, &target, "mixes_in", &fqn);
        }
    }
    let functions = Regex::new(
        r"(?m)^\s*(?:(?:public|protected|private|static|final|abstract)\s+)*function\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)\s*(?::\s*([^\s{;]+))?",
    )?;
    let mut function_scopes = Vec::new();
    for capture in functions.captures_iter(text) {
        let owner = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned);
        let line = line_of(text, capture.get(0).unwrap().start());
        let id = builder.definition(&capture[1], owner.as_deref(), "function", line);
        for parameter in capture[2].split(',') {
            let before_variable = parameter.split('$').next().unwrap_or("").trim();
            let type_text = before_variable
                .split_whitespace()
                .filter(|word| !matches!(*word, "public" | "private" | "protected" | "readonly"))
                .collect::<Vec<_>>()
                .join(" ");
            if !type_text.is_empty() {
                emit_php_type_references(
                    &mut builder,
                    &id,
                    &type_text,
                    "parameter_type",
                    line,
                    &namespace,
                    &aliases,
                );
                if owner.is_some()
                    && parameter
                        .split_whitespace()
                        .any(|word| matches!(word, "public" | "private" | "protected"))
                {
                    emit_php_type_references(
                        &mut builder,
                        owner.as_deref().unwrap(),
                        &type_text,
                        "field",
                        line,
                        &namespace,
                        &aliases,
                    );
                }
            }
        }
        if let Some(return_type) = capture.get(3) {
            emit_php_type_references(
                &mut builder,
                &id,
                return_type.as_str(),
                "return_type",
                line,
                &namespace,
                &aliases,
            );
        }
        if let Some((open, end)) = declaration_body_range(text, capture.get(0).unwrap().end()) {
            function_scopes.push(Scope {
                id,
                start: open,
                end,
            });
        }
    }
    let fields = Regex::new(
        r"(?m)^\s*(?:(?:public|protected|private|static|readonly)\s+)*([A-Za-z_\\][A-Za-z0-9_\\<>]*)\s+\$[A-Za-z_][A-Za-z0-9_]*",
    )?;
    for capture in fields.captures_iter(text) {
        let offset = capture.get(0).unwrap().start();
        // PHP does not have typed local-variable declarations. Looking for
        // property-shaped text inside a method turns statements such as
        // `return $this->process()` into a bogus field type named `return`.
        if owner_at(&function_scopes, offset).is_some() {
            continue;
        }
        if let Some(owner) = owner_at(&scopes, offset).map(str::to_owned) {
            emit_php_type_references(
                &mut builder,
                &owner,
                &capture[1],
                "field",
                line_of(text, offset),
                &namespace,
                &aliases,
            );
        }
    }
    emit_calls(
        &mut builder,
        text,
        &function_scopes,
        &["if", "for", "foreach", "while", "switch", "function"],
        true,
    )?;
    emit_php_special_relations(&mut builder, text, &function_scopes, &namespace, &aliases)?;
    Ok(builder.finish())
}

#[allow(dead_code)]
fn extract_swift(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let imports = Regex::new(r"(?m)^\s*import\s+([A-Za-z_][A-Za-z0-9_.]*)")?;
    for capture in imports.captures_iter(text) {
        let line = line_of(text, capture.get(0).unwrap().start());
        let target = builder.module(&capture[1], line);
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "imports",
            Some("import"),
            line,
            Confidence::Extracted,
        );
    }

    let declarations = Regex::new(
        r"(?m)^\s*(?:(?:public|private|internal|open|final|indirect)\s+)*(protocol|class|struct|enum|actor|extension)\s+([A-Za-z_][A-Za-z0-9_]*)(?:<[^>]+>)?(?:\s*:\s*([^\n{]+))?",
    )?;
    for capture in declarations.captures_iter(text) {
        if &capture[1] == "protocol" {
            builder.interfaces.insert(normalize_label(&capture[2]));
        }
    }
    let mut scopes = Vec::new();
    let mut rows = Vec::new();
    for capture in declarations.captures_iter(text) {
        let kind = &capture[1];
        let name = &capture[2];
        let line = line_of(text, capture.get(0).unwrap().start());
        let id = if kind == "extension" {
            builder
                .unique_definition(name)
                .unwrap_or_else(|| builder.external(name, line))
        } else {
            let owner = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned);
            builder.definition(name, owner.as_deref(), "class", line)
        };
        if let Some((open, end)) = brace_range(text, capture.get(0).unwrap().end()) {
            scopes.push(Scope {
                id: id.clone(),
                start: open,
                end,
            });
        }
        rows.push((
            id,
            kind.to_owned(),
            line,
            capture.get(3).map(|value| value.as_str().to_owned()),
        ));
    }
    let enum_cases = Regex::new(r"(?m)^\s*case\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s*\(([^)]*)\))?")?;
    for (id, kind, line, parents) in &rows {
        if let Some(parents) = parents {
            for (index, parent_text) in parents.split(',').enumerate() {
                let Some(name) = type_names(parent_text).first().cloned() else {
                    continue;
                };
                let target = builder.external(&name, *line);
                let relation = if kind == "extension"
                    || kind == "protocol"
                    || builder.interfaces.contains(&normalize_label(&name))
                    || index > 0
                {
                    "implements"
                } else {
                    "inherits"
                };
                builder.edge(id, &target, relation, None, *line, Confidence::Extracted);
            }
        }
        if kind == "enum"
            && let Some(scope) = scopes.iter().find(|scope| scope.id == *id)
        {
            for capture in enum_cases.captures_iter(&text[scope.start + 1..scope.end]) {
                let offset = scope.start + 1 + capture.get(0).unwrap().start();
                let case_line = line_of(text, offset);
                let case_id = builder.definition(&capture[1], Some(id), "enum_case", case_line);
                builder.edge(
                    id,
                    &case_id,
                    "case_of",
                    None,
                    case_line,
                    Confidence::Extracted,
                );
                if let Some(associated) = capture.get(2) {
                    for value_type in associated.as_str().split(',') {
                        builder.reference(id, value_type, "type", case_line, "swift");
                    }
                }
            }
        }
    }

    let functions = Regex::new(
        r"(?m)^\s*(?:(?:public|private|internal|open|final|override|static|class|mutating|nonmutating|async)\s+)*func\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)\s*(?:async\s+|throws\s+|rethrows\s+)*(?:->\s*([^\n{]+))?",
    )?;
    let mut function_scopes = Vec::new();
    for capture in functions.captures_iter(text) {
        let owner = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned);
        let line = line_of(text, capture.get(0).unwrap().start());
        let id = builder.definition(&capture[1], owner.as_deref(), "function", line);
        emit_swift_parameters(&mut builder, &id, &capture[2], line);
        if let Some(return_type) = capture.get(3) {
            builder.reference(&id, return_type.as_str(), "return_type", line, "swift");
        }
        if let Some((open, end)) = same_line_brace_range(text, capture.get(0).unwrap().end()) {
            function_scopes.push(Scope {
                id,
                start: open,
                end,
            });
        }
    }
    let special_functions = Regex::new(
        r"(?m)^\s*(?:(?:public|private|internal|override|required|convenience)\s+)*(init|deinit|subscript)\s*(?:\(([^)]*)\))?\s*(?:->\s*([^\n{]+))?",
    )?;
    for capture in special_functions.captures_iter(text) {
        let owner = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned);
        let line = line_of(text, capture.get(0).unwrap().start());
        let id = builder.definition(&capture[1], owner.as_deref(), "function", line);
        if let Some(parameters) = capture.get(2) {
            emit_swift_parameters(&mut builder, &id, parameters.as_str(), line);
        }
        if let Some(return_type) = capture.get(3) {
            builder.reference(&id, return_type.as_str(), "return_type", line, "swift");
        }
        if let Some((open, end)) = same_line_brace_range(text, capture.get(0).unwrap().end()) {
            function_scopes.push(Scope {
                id,
                start: open,
                end,
            });
        }
    }
    let fields = Regex::new(
        r"(?m)^\s*(?:(?:public|private|internal|static|class|lazy|weak|unowned)\s+)*(?:let|var)\s+[A-Za-z_][A-Za-z0-9_]*\s*:\s*([^=\n{]+)",
    )?;
    for capture in fields.captures_iter(text) {
        if let Some(owner) = owner_at(&scopes, capture.get(0).unwrap().start()).map(str::to_owned) {
            builder.reference(
                &owner,
                capture[1].trim(),
                "field",
                line_of(text, capture.get(0).unwrap().start()),
                "swift",
            );
        }
    }
    emit_calls(
        &mut builder,
        text,
        &function_scopes,
        &["if", "for", "while", "switch", "return", "func", "print"],
        false,
    )?;
    Ok(builder.finish())
}

fn extract_elixir(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let module_re = Regex::new(r"(?m)^\s*defmodule\s+([A-Za-z_][A-Za-z0-9_.]*)\s+do\b")?;
    let module = module_re.captures(text).map(|capture| {
        let offset = capture.get(0).expect("module match").start();
        (
            builder.definition(&capture[1], None, "module", line_of(text, offset)),
            capture[1].to_owned(),
        )
    });

    let imports = Regex::new(r"(?m)^\s*(?:alias|import|require|use)\s+([^\n#]+)")?;
    for capture in imports.captures_iter(text) {
        let line = line_of(text, capture.get(0).expect("import match").start());
        let captured = capture[1].trim();
        let raw = if captured.contains(".{") {
            captured
                .find('}')
                .map(|end| &captured[..=end])
                .unwrap_or(captured)
        } else {
            captured.split(',').next().unwrap_or(captured).trim()
        };
        let modules = if let Some((base, members)) = raw.split_once(".{") {
            let Some(members) = members.strip_suffix('}') else {
                continue;
            };
            members
                .split(',')
                .map(|member| format!("{base}.{}", member.trim()))
                .collect::<Vec<_>>()
        } else {
            vec![raw.to_owned()]
        };
        for mut module_name in modules {
            if !static_elixir_module(&module_name) {
                continue;
            }
            if let Some(suffix) = module_name.strip_prefix("__MODULE__") {
                let Some((_, declared_module)) = module.as_ref() else {
                    continue;
                };
                module_name = format!("{declared_module}{suffix}");
            }
            let target = builder.external(&module_name, line);
            builder.edge(
                &builder.file_id.clone(),
                &target,
                "imports",
                Some("import"),
                line,
                Confidence::Extracted,
            );
        }
    }

    let functions = Regex::new(r"(?m)^\s*defp?\s+([a-z_][A-Za-z0-9_!?]*)\s*(?:\([^\n)]*\))?")?;
    let matches: Vec<_> = functions.captures_iter(text).collect();
    let mut scopes = Vec::new();
    for (index, capture) in matches.iter().enumerate() {
        let matched = capture.get(0).expect("function match");
        let line = line_of(text, matched.start());
        let id = builder.definition(
            &capture[1],
            module.as_ref().map(|(id, _)| id.as_str()),
            "function",
            line,
        );
        let end = matches
            .get(index + 1)
            .and_then(|next| next.get(0))
            .map(|next| next.start())
            .unwrap_or(text.len());
        scopes.push(Scope {
            id,
            start: matched.end(),
            end,
        });
    }
    emit_calls(
        &mut builder,
        text,
        &scopes,
        &[
            "def",
            "defp",
            "defmodule",
            "defmacro",
            "defmacrop",
            "defstruct",
            "defprotocol",
            "defimpl",
            "defguard",
            "alias",
            "import",
            "require",
            "use",
            "if",
            "unless",
            "case",
            "cond",
            "with",
            "for",
        ],
        false,
    )?;
    Ok(builder.finish())
}

fn static_elixir_module(value: &str) -> bool {
    if let Some(atom) = value.strip_prefix(':') {
        if let Some(inner) = atom
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        {
            return !inner.is_empty()
                && !inner.contains("#{")
                && !inner
                    .chars()
                    .any(|character| character.is_control() || character == '"');
        }
        let mut characters = atom.chars();
        return characters
            .next()
            .is_some_and(|first| first == '_' || first.is_alphabetic())
            && characters.all(|character| {
                character == '_'
                    || character == '@'
                    || matches!(character, '!' | '?')
                    || character.is_alphanumeric()
            });
    }

    value.split('.').enumerate().all(|(index, segment)| {
        if segment == "__MODULE__" {
            return index == 0;
        }
        let mut characters = segment.chars();
        characters.next().is_some_and(char::is_uppercase)
            && characters.all(|character| character == '_' || character.is_alphanumeric())
    })
}

#[derive(Clone)]
struct ObjcMethod {
    id: String,
    owner: String,
    name: String,
    body: Option<(usize, usize)>,
}

fn extract_objc(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    extract_objc_impl(path, text, source_file, true)
}

/// Extract Objective-C from I/O-owned bytes without probing quoted imports.
pub(crate) fn extract_objc_bytes(
    path: &Path,
    text: &str,
    source_file: &str,
) -> anyhow::Result<Extraction> {
    extract_objc_impl(path, text, source_file, false)
}

fn extract_objc_impl(
    path: &Path,
    text: &str,
    source_file: &str,
    allow_path_probes: bool,
) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    emit_objc_imports(&mut builder, path, text, source_file, allow_path_probes)?;

    let declarations = Regex::new(
        r"(?m)^\s*@(interface|implementation|protocol)\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s*:\s*([A-Za-z_][A-Za-z0-9_]*))?(?:\s*<([^>]+)>)?",
    )?;
    for capture in declarations.captures_iter(text) {
        if &capture[1] == "protocol" {
            builder.interfaces.insert(normalize_label(&capture[2]));
        }
    }

    let mut scopes = Vec::new();
    let mut rows = Vec::new();
    for capture in declarations.captures_iter(text) {
        let matched = capture.get(0).expect("Objective-C declaration");
        let line = line_of(text, matched.start());
        let name = &capture[2];
        let kind = &capture[1];
        let id = if let Some(existing) = builder.unique_definition(name) {
            existing
        } else if kind == "protocol" {
            builder.labeled_definition(name, &format!("<{name}>"), None, "protocol", line)
        } else {
            builder.definition(name, None, "class", line)
        };
        let end = text[matched.end()..]
            .find("@end")
            .map(|offset| matched.end() + offset + "@end".len())
            .unwrap_or(text.len());
        scopes.push(Scope {
            id: id.clone(),
            start: matched.start(),
            end,
        });
        rows.push((
            id,
            kind.to_owned(),
            line,
            capture.get(3).map(|value| value.as_str().to_owned()),
            capture.get(4).map(|value| value.as_str().to_owned()),
        ));
    }
    for (id, kind, line, superclass, protocols) in rows {
        if let Some(superclass) = superclass {
            let target = builder.external(&superclass, line);
            builder.edge(&id, &target, "inherits", None, line, Confidence::Extracted);
        }
        if let Some(protocols) = protocols {
            for protocol in protocols
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
            {
                let target = builder.external(protocol, line);
                builder.edge(
                    &id,
                    &target,
                    "implements",
                    None,
                    line,
                    Confidence::Extracted,
                );
            }
        }
        if kind == "protocol" {
            builder.interfaces.insert(normalize_label(
                builder.labels.get(&id).map(String::as_str).unwrap_or(""),
            ));
        }
    }

    let properties =
        Regex::new(r"(?m)^\s*@property\s*(?:\([^)]*\)\s*)?(.+?)\s+\**[A-Za-z_][A-Za-z0-9_]*\s*;")?;
    for capture in properties.captures_iter(text) {
        let matched = capture.get(0).expect("Objective-C property");
        if let Some(owner) = owner_at(&scopes, matched.start()).map(str::to_owned) {
            let line = line_of(text, matched.start());
            for type_name in type_names(capture[1].trim()) {
                if type_noise("objc", &type_name) {
                    continue;
                }
                let target = builder.external(&type_name, line);
                builder.edge(
                    &owner,
                    &target,
                    "references",
                    Some("field"),
                    line,
                    Confidence::Extracted,
                );
            }
        }
    }

    let method_lines = Regex::new(r"(?m)^\s*([+-])\s*\([^\n)]*\)\s*([^\n{;]+)(?:\{|;)")?;
    let selector_parts = Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\s*:")?;
    let first_identifier = Regex::new(r"^\s*([A-Za-z_][A-Za-z0-9_]*)")?;
    let mut methods = Vec::new();
    let mut method_ids = HashSet::new();
    for capture in method_lines.captures_iter(text) {
        let matched = capture.get(0).expect("Objective-C method");
        let Some(owner) = owner_at(&scopes, matched.start()).map(str::to_owned) else {
            continue;
        };
        let parts: Vec<_> = selector_parts
            .captures_iter(&capture[2])
            .map(|part| part[1].to_owned())
            .collect();
        let name = if parts.is_empty() {
            first_identifier
                .captures(&capture[2])
                .map(|part| part[1].to_owned())
        } else {
            Some(parts.concat())
        };
        let Some(name) = name else { continue };
        let line = line_of(text, matched.start());
        let label = format!("{}{}", &capture[1], name);
        let id = builder.labeled_definition(&name, &label, Some(&owner), "function", line);
        let body = matched
            .as_str()
            .contains('{')
            .then(|| brace_range(text, matched.start()))
            .flatten();
        method_ids.insert(id.clone());
        methods.push(ObjcMethod {
            id,
            owner,
            name,
            body,
        });
    }
    methods.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| right.body.is_some().cmp(&left.body.is_some()))
    });
    methods.dedup_by(|left, right| left.id == right.id && left.body == right.body);

    let mut owner_methods: HashMap<(String, String), String> = HashMap::new();
    let mut global_methods: HashMap<String, HashSet<String>> = HashMap::new();
    for method in &methods {
        owner_methods.insert(
            (method.owner.clone(), method.name.clone()),
            method.id.clone(),
        );
        global_methods
            .entry(method.name.clone())
            .or_default()
            .insert(method.id.clone());
    }
    emit_objc_body_relations(
        &mut builder,
        text,
        &methods,
        &owner_methods,
        &global_methods,
        &method_ids,
    )?;
    Ok(builder.finish())
}

fn emit_objc_imports(
    builder: &mut Builder<'_>,
    path: &Path,
    text: &str,
    source_file: &str,
    allow_path_probes: bool,
) -> anyhow::Result<()> {
    let imports = Regex::new(r#"(?m)^\s*#import\s*([<\"])([^>\"]+)[>\"]"#)?;
    for capture in imports.captures_iter(text) {
        let matched = capture.get(0).expect("Objective-C import");
        let line = line_of(text, matched.start());
        let mut logical_target = None;
        let target = if &capture[1] == "\"" {
            let raw = &capture[2];
            let logical = normalize_logical_path(
                &Path::new(source_file)
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .join(raw),
            );
            if allow_path_probes {
                let resolved = path.parent().unwrap_or_else(|| Path::new("")).join(raw);
                if resolved.is_file() {
                    let stem = Path::new(&logical)
                        .with_extension("")
                        .to_string_lossy()
                        .replace('\\', "/");
                    let base = make_id(&[&stem]);
                    let has_pair = ["m", "mm"]
                        .iter()
                        .any(|extension| resolved.with_extension(extension).is_file());
                    if has_pair {
                        make_id(&[&logical, &base])
                    } else {
                        base
                    }
                } else {
                    let module = Path::new(raw)
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or(raw);
                    builder.module(module, line)
                }
            } else {
                let Some(logical) = logical_project_import(source_file, raw) else {
                    continue;
                };
                let stem = Path::new(&logical)
                    .with_extension("")
                    .to_string_lossy()
                    .replace('\\', "/");
                let base = make_id(&[&stem]);
                logical_target = Some(logical);
                base
            }
        } else {
            let module = Path::new(&capture[2])
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(&capture[2]);
            builder.module(module, line)
        };
        let edge_count = builder.edges.len();
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "imports",
            Some("import"),
            line,
            Confidence::Extracted,
        );
        if builder.edges.len() > edge_count
            && let Some(logical) = logical_target
            && let Some(edge) = builder.edges.last_mut()
        {
            edge.extra.insert("target_file".into(), logical.into());
        }
    }
    let module_imports = Regex::new(r"(?m)^\s*@import\s+([A-Za-z_][A-Za-z0-9_.]*)\s*;")?;
    for capture in module_imports.captures_iter(text) {
        let line = line_of(text, capture.get(0).expect("module import").start());
        let module = capture[1].split('.').next().unwrap_or(&capture[1]);
        let target = builder.module(module, line);
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "imports",
            Some("import"),
            line,
            Confidence::Extracted,
        );
    }
    Ok(())
}

fn emit_objc_body_relations(
    builder: &mut Builder<'_>,
    text: &str,
    methods: &[ObjcMethod],
    owner_methods: &HashMap<(String, String), String>,
    global_methods: &HashMap<String, HashSet<String>>,
    method_ids: &HashSet<String>,
) -> anyhow::Result<()> {
    let messages = Regex::new(r"\[\s*(self|super|[A-Za-z_][A-Za-z0-9_]*)\s+([^\[\]]+)\]")?;
    let selector_parts = Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\s*:")?;
    let first_identifier = Regex::new(r"^\s*([A-Za-z_][A-Za-z0-9_]*)")?;
    let allocations = Regex::new(r"\[\[\s*([A-Z][A-Za-z0-9_]*)\s+alloc\]")?;
    let dot_access = Regex::new(r"\bself\.([A-Za-z_][A-Za-z0-9_]*)")?;
    let selectors = Regex::new(r"@selector\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)")?;
    for method in methods {
        let Some((start, end)) = method.body else {
            continue;
        };
        let body = &text[start..end];
        for capture in allocations.captures_iter(body) {
            let line = line_of(text, start + capture.get(0).expect("allocation").start());
            let target = builder.external(&capture[1], line);
            builder.edge(
                &method.id,
                &target,
                "references",
                Some("type"),
                line,
                Confidence::Extracted,
            );
        }
        for capture in messages.captures_iter(body) {
            let parts: Vec<_> = selector_parts
                .captures_iter(&capture[2])
                .map(|part| part[1].to_owned())
                .collect();
            let name = if parts.is_empty() {
                first_identifier
                    .captures(&capture[2])
                    .map(|part| part[1].to_owned())
            } else {
                Some(parts.concat())
            };
            let Some(name) = name else { continue };
            let receiver = &capture[1];
            let line = line_of(text, start + capture.get(0).expect("message").start());
            let direct_target = if matches!(receiver, "self" | "super") {
                objc_owner_method(
                    &method.owner,
                    &name,
                    receiver == "super",
                    owner_methods,
                    &builder.edges,
                )
            } else {
                None
            };
            if let Some(target) = direct_target.filter(|target| target != &method.id) {
                builder.edge(
                    &method.id,
                    &target,
                    "calls",
                    Some("call"),
                    line,
                    Confidence::Extracted,
                );
                continue;
            }

            let owner_type = builder.labels.get(&method.owner).map(|label| {
                label
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            });
            let receiver_type = if matches!(receiver, "self" | "super") {
                owner_type
            } else if receiver.chars().next().is_some_and(char::is_uppercase) {
                Some(receiver.to_string())
            } else {
                let message_start = capture.get(0).expect("message").start();
                crate::native::objc_receiver_type(&body[..message_start], receiver)
            };
            let Some(receiver_type) = receiver_type.filter(|name| !name.is_empty()) else {
                // A bare selector name is not receiver evidence. In particular,
                // do not connect `[thing run]` to whichever `run` happens to be
                // globally unique in the corpus.
                continue;
            };
            let edge_start = builder.edges.len();
            builder.edge(
                &method.id,
                &crate::native::unresolved_target(&name),
                "calls",
                Some("call"),
                line,
                Confidence::Extracted,
            );
            if let Some(edge) = builder.edges.get_mut(edge_start) {
                edge.extra
                    .insert(crate::native::CALL_FACT.into(), true.into());
                edge.extra.insert(
                    crate::native::INFERRED_RECEIVER.into(),
                    receiver
                        .chars()
                        .next()
                        .is_some_and(char::is_lowercase)
                        .into(),
                );
                edge.extra.insert("unresolved_call".into(), true.into());
                edge.extra.insert("member_call".into(), true.into());
                edge.extra.insert("callee".into(), name.into());
                edge.extra.insert("receiver".into(), receiver.into());
                edge.extra
                    .insert("receiver_type".into(), receiver_type.into());
            }
        }
        for capture in dot_access.captures_iter(body) {
            let Some(target) = owner_methods
                .get(&(method.owner.clone(), capture[1].to_owned()))
                .filter(|target| method_ids.contains(*target))
            else {
                continue;
            };
            if target != &method.id {
                builder.edge(
                    &method.id,
                    target,
                    "accesses",
                    None,
                    line_of(text, start + capture.get(0).expect("dot access").start()),
                    Confidence::Extracted,
                );
            }
        }
        for capture in selectors.captures_iter(body) {
            let Some(target) = global_methods
                .get(&capture[1])
                .filter(|ids| ids.len() == 1)
                .and_then(|ids| ids.iter().next())
                .filter(|target| *target != &method.id)
            else {
                continue;
            };
            builder.edge(
                &method.id,
                target,
                "calls",
                Some("call"),
                line_of(text, start + capture.get(0).expect("selector").start()),
                Confidence::Extracted,
            );
        }
    }
    Ok(())
}

fn objc_owner_method(
    owner: &str,
    name: &str,
    skip_owner: bool,
    owner_methods: &HashMap<(String, String), String>,
    edges: &[Edge],
) -> Option<String> {
    let mut pending = vec![owner.to_owned()];
    let mut seen = HashSet::new();
    let mut first = true;
    while let Some(candidate) = pending.pop() {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        if !(skip_owner && first)
            && let Some(method) = owner_methods.get(&(candidate.clone(), name.to_owned()))
        {
            return Some(method.clone());
        }
        first = false;
        pending.extend(
            edges
                .iter()
                .filter(|edge| edge.relation == "inherits" && edge.true_source() == candidate)
                .map(|edge| edge.true_target().to_owned()),
        );
    }
    None
}

fn normalize_logical_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let absolute = text.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for component in text.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|part| *part != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push(component);
                }
            }
            _ => parts.push(component),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

fn logical_project_import(source_file: &str, raw: &str) -> Option<String> {
    if looks_absolute_path(raw) || windows_drive_prefix(raw) {
        return None;
    }
    let portable_raw = raw.replace('\\', "/");
    let raw = Path::new(&portable_raw);
    let source = Path::new(source_file);
    if raw.is_absolute() || source.is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    for component in source
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .components()
    {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_owned()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return None,
        }
    }
    for component in raw.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => parts.push(part.to_owned()),
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| {
        parts
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    })
}

fn windows_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn looks_absolute_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path.starts_with('/')
        || path
            .as_bytes()
            .get(1..3)
            .is_some_and(|bytes| bytes[0] == b':' && bytes[1] == b'/')
}

fn portable_project_reference(
    physical_source: &Path,
    logical_source: &str,
    reference: &str,
) -> (String, bool) {
    let reference = reference.replace('\\', "/");
    if looks_absolute_path(&reference) {
        if reference.starts_with('/') && !looks_absolute_path(logical_source) {
            let logical_depth = Path::new(logical_source)
                .components()
                .filter(|component| !matches!(component, std::path::Component::CurDir))
                .count();
            let mut physical_root = physical_source.to_path_buf();
            for _ in 0..logical_depth {
                physical_root.pop();
            }
            let physical_root = normalize_logical_path(&physical_root);
            let absolute_reference = normalize_logical_path(Path::new(&reference));
            if absolute_reference == physical_root {
                return (String::new(), false);
            }
            if let Some(relative) = absolute_reference
                .strip_prefix(&physical_root)
                .and_then(|suffix| suffix.strip_prefix('/'))
            {
                return (relative.to_owned(), false);
            }
        }
        let basename = reference
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .unwrap_or(&reference)
            .to_owned();
        return (basename, true);
    }

    let logical = normalize_logical_path(
        &Path::new(logical_source)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(&reference),
    );
    let walk_up_depth = logical
        .split('/')
        .take_while(|component| *component == "..")
        .count();
    if walk_up_depth > 3 {
        let basename = logical.rsplit('/').next().unwrap_or(&logical).to_owned();
        (basename, true)
    } else {
        (logical, walk_up_depth > 0)
    }
}

fn portable_project_id(logical_path: &str, external: bool) -> String {
    if external {
        return make_id(&["ext", logical_path]);
    }
    let stem = Path::new(logical_path)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    make_id(&[&stem])
}

fn extract_julia(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let module_re = Regex::new(r"(?m)^\s*module\s+([A-Za-z_][A-Za-z0-9_]*)")?;
    let module = module_re.captures(text).map(|capture| {
        builder.definition(
            &capture[1],
            None,
            "module",
            line_of(text, capture.get(0).expect("Julia module").start()),
        )
    });

    let imports = Regex::new(r"(?m)^\s*(?:using|import)\s+([^\n#]+)")?;
    for capture in imports.captures_iter(text) {
        let line = line_of(text, capture.get(0).expect("Julia import").start());
        let raw = capture[1].trim();
        let module_clause = raw.split(':').next().unwrap_or(raw);
        for candidate in module_clause.split(',') {
            let Some(module_name) = static_julia_module(candidate) else {
                continue;
            };
            let target = builder.external(module_name, line);
            builder.edge(
                &builder.file_id.clone(),
                &target,
                "imports",
                Some("import"),
                line,
                Confidence::Extracted,
            );
        }
    }

    let abstract_types = Regex::new(
        r"(?m)^\s*abstract\s+type\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s*<:\s*([A-Za-z_][A-Za-z0-9_.]*))?",
    )?;
    let mut abstract_rows = Vec::new();
    for capture in abstract_types.captures_iter(text) {
        let line = line_of(text, capture.get(0).expect("Julia abstract type").start());
        let id = builder.definition(&capture[1], module.as_deref(), "class", line);
        abstract_rows.push((
            id,
            line,
            capture.get(2).map(|value| value.as_str().to_owned()),
        ));
    }

    let structs = Regex::new(
        r"(?m)^\s*(?:mutable\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s*<:\s*([A-Za-z_][A-Za-z0-9_.]*))?",
    )?;
    let field_re = Regex::new(r"(?m)^\s*[A-Za-z_][A-Za-z0-9_]*\s*::\s*([^\s#]+)")?;
    let mut struct_rows = Vec::new();
    for capture in structs.captures_iter(text) {
        let matched = capture.get(0).expect("Julia struct");
        let line = line_of(text, matched.start());
        let id = builder.definition(&capture[1], module.as_deref(), "class", line);
        let end = text[matched.end()..]
            .find("\nend")
            .map(|offset| matched.end() + offset)
            .unwrap_or(text.len());
        for field in field_re.captures_iter(&text[matched.end()..end]) {
            builder.reference(
                &id,
                &field[1],
                "field",
                line_of(
                    text,
                    matched.end() + field.get(0).expect("Julia field").start(),
                ),
                "julia",
            );
        }
        struct_rows.push((
            id,
            line,
            capture.get(2).map(|value| value.as_str().to_owned()),
        ));
    }
    for (id, line, parent) in abstract_rows.into_iter().chain(struct_rows) {
        if let Some(parent) = parent {
            let target = builder.external(&parent, line);
            builder.edge(&id, &target, "inherits", None, line, Confidence::Extracted);
        }
    }

    let long_functions = Regex::new(r"(?m)^\s*function\s+([A-Za-z_][A-Za-z0-9_!]*)\s*\(")?;
    let short_functions = Regex::new(r"(?m)^\s*([A-Za-z_][A-Za-z0-9_!]*)\s*\([^\n)]*\)\s*=")?;
    let mut declarations = Vec::new();
    for capture in long_functions.captures_iter(text) {
        declarations.push((
            capture.get(0).expect("Julia function").start(),
            capture.get(0).expect("Julia function").end(),
            capture[1].to_owned(),
        ));
    }
    for capture in short_functions.captures_iter(text) {
        declarations.push((
            capture.get(0).expect("Julia short function").start(),
            capture.get(0).expect("Julia short function").end(),
            capture[1].to_owned(),
        ));
    }
    declarations.sort_by_key(|row| row.0);
    let mut function_scopes = Vec::new();
    for (index, (start, signature_end, name)) in declarations.iter().enumerate() {
        let id = builder.definition(name, module.as_deref(), "function", line_of(text, *start));
        let line_end = text[*signature_end..]
            .find('\n')
            .map(|offset| signature_end + offset)
            .unwrap_or(text.len());
        let next = declarations
            .get(index + 1)
            .map(|row| row.0)
            .unwrap_or(text.len());
        function_scopes.push(Scope {
            id,
            start: *signature_end,
            end: if text[*start..line_end].contains('=') {
                line_end
            } else {
                next
            },
        });
    }
    emit_calls(
        &mut builder,
        text,
        &function_scopes,
        &["if", "for", "while", "function", "return"],
        false,
    )?;
    Ok(builder.finish())
}

fn static_julia_module(value: &str) -> Option<&str> {
    let value = value.trim().trim_start_matches('.');
    if value.is_empty() {
        return None;
    }
    value
        .split('.')
        .all(|segment| {
            let mut characters = segment.chars();
            characters
                .next()
                .is_some_and(|first| first == '_' || first.is_alphabetic())
                && characters.all(|character| {
                    character == '_'
                        || matches!(character, '!' | '?')
                        || character.is_alphanumeric()
                })
        })
        .then_some(value)
}

#[derive(Clone)]
struct FortranProcedure {
    id: String,
    name: String,
    parameters: HashSet<String>,
    result_name: Option<String>,
    start: usize,
    end: usize,
}

fn extract_fortran(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let modules = Regex::new(r"(?im)^\s*module\s+([A-Za-z_][A-Za-z0-9_]*)")?;
    let module = modules.captures(text).map(|capture| {
        let name = capture[1].to_ascii_lowercase();
        builder.definition(
            &name,
            None,
            "module",
            line_of(text, capture.get(0).expect("Fortran module").start()),
        )
    });
    let programs = Regex::new(r"(?im)^\s*program\s+([A-Za-z_][A-Za-z0-9_]*)")?;
    for capture in programs.captures_iter(text) {
        builder.definition(
            &capture[1].to_ascii_lowercase(),
            None,
            "program",
            line_of(text, capture.get(0).expect("Fortran program").start()),
        );
    }
    let imports = Regex::new(r"(?im)^\s*use(?:\s*,[^:]+::)?\s+([A-Za-z_][A-Za-z0-9_]*)")?;
    for capture in imports.captures_iter(text) {
        let line = line_of(text, capture.get(0).expect("Fortran use").start());
        let target = builder.external(&capture[1].to_ascii_lowercase(), line);
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "imports",
            Some("use"),
            line,
            Confidence::Extracted,
        );
    }
    let derived_types = Regex::new(r"(?im)^\s*type\s*(?:::)?\s+([A-Za-z_][A-Za-z0-9_]*)\s*$")?;
    for capture in derived_types.captures_iter(text) {
        builder.definition(
            &capture[1].to_ascii_lowercase(),
            module.as_deref(),
            "class",
            line_of(text, capture.get(0).expect("Fortran type").start()),
        );
    }

    let procedures = Regex::new(
        r"(?im)^\s*(?:(?:pure|elemental|recursive|impure)\s+)*(subroutine|function)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)(?:\s*result\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\))?",
    )?;
    let mut procedure_rows = Vec::new();
    for capture in procedures.captures_iter(text) {
        let matched = capture.get(0).expect("Fortran procedure");
        let name = capture[2].to_ascii_lowercase();
        let id = builder.definition(
            &name,
            module.as_deref(),
            "function",
            line_of(text, matched.start()),
        );
        let end_pattern = Regex::new(&format!(
            r"(?im)^\s*end\s+{}(?:\s+{})?\s*$",
            &capture[1],
            regex::escape(&capture[2])
        ))?;
        let end = end_pattern
            .find(&text[matched.end()..])
            .map(|end| matched.end() + end.end())
            .unwrap_or(text.len());
        procedure_rows.push(FortranProcedure {
            id,
            name,
            parameters: capture[3]
                .split(',')
                .map(|parameter| parameter.trim().to_ascii_lowercase())
                .filter(|parameter| !parameter.is_empty())
                .collect(),
            result_name: capture
                .get(4)
                .map(|value| value.as_str().to_ascii_lowercase()),
            start: matched.end(),
            end,
        });
    }
    let typed_variables = Regex::new(
        r"(?im)^\s*type\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)(?:\s*,[^:]*)?\s*::\s*([^\n!]+)",
    )?;
    for procedure in &procedure_rows {
        for capture in typed_variables.captures_iter(&text[procedure.start..procedure.end]) {
            let line = line_of(
                text,
                procedure.start + capture.get(0).expect("Fortran typed variable").start(),
            );
            for variable in capture[2].split(',') {
                let variable = variable
                    .split(['(', '='])
                    .next()
                    .unwrap_or(variable)
                    .trim()
                    .to_ascii_lowercase();
                let context = if procedure.parameters.contains(&variable) {
                    Some("parameter_type")
                } else if procedure.result_name.as_deref() == Some(variable.as_str()) {
                    Some("return_type")
                } else {
                    None
                };
                if let Some(context) = context {
                    let target = builder.external(&capture[1].to_ascii_lowercase(), line);
                    builder.edge(
                        &procedure.id,
                        &target,
                        "references",
                        Some(context),
                        line,
                        Confidence::Extracted,
                    );
                }
            }
        }
    }
    let explicit_calls = Regex::new(r"(?im)\bcall\s+([A-Za-z_][A-Za-z0-9_]*)")?;
    let expression_calls = Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(")?;
    for procedure in &procedure_rows {
        let body = &text[procedure.start..procedure.end];
        for capture in explicit_calls
            .captures_iter(body)
            .chain(expression_calls.captures_iter(body))
        {
            let name = capture[1].to_ascii_lowercase();
            let Some(target) = builder.unique_definition(&name) else {
                continue;
            };
            if target == procedure.id || name == procedure.name {
                continue;
            }
            builder.edge(
                &procedure.id,
                &target,
                "calls",
                Some("call"),
                line_of(
                    text,
                    procedure.start + capture.get(0).expect("Fortran call").start(),
                ),
                Confidence::Extracted,
            );
        }
    }
    Ok(builder.finish())
}

fn extract_powershell(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    emit_powershell_imports(&mut builder, text)?;
    let classes =
        Regex::new(r"(?im)^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s*:\s*([^\n{]+))?\s*\{")?;
    let mut scopes = Vec::new();
    let mut class_rows = Vec::new();
    for capture in classes.captures_iter(text) {
        let matched = capture.get(0).expect("PowerShell class");
        let line = line_of(text, matched.start());
        let id = builder.definition(&capture[1], None, "class", line);
        if let Some((start, end)) = brace_range(text, matched.start()) {
            scopes.push(Scope {
                id: id.clone(),
                start,
                end,
            });
        }
        class_rows.push((
            id,
            line,
            capture.get(2).map(|value| value.as_str().to_owned()),
        ));
    }
    for (id, line, bases) in class_rows {
        if let Some(bases) = bases {
            for (index, base) in bases.split(',').map(str::trim).enumerate() {
                if base.is_empty() {
                    continue;
                }
                let target = builder.external(base, line);
                builder.edge(
                    &id,
                    &target,
                    if index == 0 { "inherits" } else { "implements" },
                    None,
                    line,
                    Confidence::Extracted,
                );
            }
        }
    }
    let properties = Regex::new(r"(?im)^\s*\[([^\]]+)\]\s*\$[A-Za-z_][A-Za-z0-9_]*\s*(?:=|$)")?;
    for capture in properties.captures_iter(text) {
        let matched = capture.get(0).expect("PowerShell property");
        if let Some(owner) = owner_at(&scopes, matched.start()).map(str::to_owned) {
            emit_builtin_reference(
                &mut builder,
                &owner,
                &capture[1],
                "field",
                line_of(text, matched.start()),
            );
        }
    }

    let methods =
        Regex::new(r"(?im)^\s*(?:\[([^\]]+)\]\s+)?([A-Za-z_][A-Za-z0-9_-]*)\s*\(([^\n)]*)\)\s*\{")?;
    let parameter_types = Regex::new(r"\[([^\]]+)\]\s*\$[A-Za-z_][A-Za-z0-9_]*")?;
    let mut function_scopes = Vec::new();
    for capture in methods.captures_iter(text) {
        let matched = capture.get(0).expect("PowerShell method");
        let Some(owner) = owner_at(&scopes, matched.start()).map(str::to_owned) else {
            continue;
        };
        let line = line_of(text, matched.start());
        let id = builder.definition(&capture[2], Some(&owner), "function", line);
        if let Some(return_type) = capture.get(1) {
            emit_builtin_reference(&mut builder, &id, return_type.as_str(), "return_type", line);
        }
        for parameter in parameter_types.captures_iter(&capture[3]) {
            emit_builtin_reference(&mut builder, &id, &parameter[1], "parameter_type", line);
        }
        if let Some((start, end)) = brace_range(text, matched.start()) {
            function_scopes.push(Scope { id, start, end });
        }
    }
    let functions = Regex::new(r"(?im)^\s*function\s+([A-Za-z_][A-Za-z0-9_-]*)\s*\{")?;
    for capture in functions.captures_iter(text) {
        let matched = capture.get(0).expect("PowerShell function");
        let id = builder.definition(
            &capture[1],
            None,
            "function",
            line_of(text, matched.start()),
        );
        if let Some((start, end)) = brace_range(text, matched.start()) {
            function_scopes.push(Scope { id, start, end });
        }
    }
    emit_powershell_calls(&mut builder, text, &function_scopes)?;
    Ok(builder.finish())
}

fn emit_builtin_reference(
    builder: &mut Builder<'_>,
    source: &str,
    name: &str,
    context: &str,
    line: usize,
) {
    let target = builder.external(name.trim(), line);
    builder.edge(
        source,
        &target,
        "references",
        Some(context),
        line,
        Confidence::Extracted,
    );
}

#[derive(Clone, Copy)]
enum PowershellImportMode {
    Namespace,
    PathOrBare,
    SourceRelative,
}

enum PowershellImportTarget<'a> {
    External(&'a str),
    Project(String),
}

fn emit_powershell_imports(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    let patterns = [
        (
            Regex::new(r"(?im)^\s*using\s+namespace\s+([^\r\n#]+)")?,
            false,
            PowershellImportMode::Namespace,
        ),
        (
            Regex::new(r"(?im)^\s*using\s+(?:module|assembly)\s+([^\r\n#]+)")?,
            false,
            PowershellImportMode::PathOrBare,
        ),
        (
            Regex::new(r"(?im)^\s*Import-Module\s+(?:(?:-Name|-N)\s+)?([^\r\n#]+)")?,
            true,
            PowershellImportMode::PathOrBare,
        ),
        (
            Regex::new(r"(?im)^\s*\.\s+([^\r\n#]+)")?,
            true,
            PowershellImportMode::SourceRelative,
        ),
    ];
    for (pattern, allow_arguments, mode) in patterns {
        for capture in pattern.captures_iter(text) {
            let matched = capture.get(0).expect("PowerShell import");
            let Some(raw) = static_powershell_import_target(&capture[1], allow_arguments) else {
                continue;
            };
            let line = line_of(text, matched.start());
            emit_powershell_import(builder, raw, line, mode);
        }
    }
    Ok(())
}

fn emit_powershell_import(
    builder: &mut Builder<'_>,
    raw: &str,
    line: usize,
    mode: PowershellImportMode,
) {
    let Some(classified) = classify_powershell_import(builder.source_file, raw, mode) else {
        return;
    };
    let (target, target_file) = match classified {
        PowershellImportTarget::External(raw) => {
            let Some(name) = powershell_external_import_name(raw) else {
                return;
            };
            (builder.external(&name, line), None)
        }
        PowershellImportTarget::Project(logical) => {
            let stem = Path::new(&logical)
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            let target = make_id(&[&stem]);
            let label = Path::new(&logical)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(&logical);
            let before = builder.nodes.len();
            builder.insert_node(target.clone(), label, "reference", line, "");
            if builder.nodes.len() > before {
                builder
                    .nodes
                    .last_mut()
                    .expect("new PowerShell project reference")
                    .extra
                    .insert("origin_file".into(), builder.source_file.into());
            }
            (target, Some(logical))
        }
    };
    let edge_count = builder.edges.len();
    builder.edge(
        &builder.file_id.clone(),
        &target,
        "imports_from",
        Some("import"),
        line,
        Confidence::Extracted,
    );
    if builder.edges.len() > edge_count
        && let Some(target_file) = target_file
        && let Some(edge) = builder.edges.last_mut()
    {
        edge.extra.insert("target_file".into(), target_file.into());
    }
}

fn classify_powershell_import<'a>(
    source_file: &str,
    raw: &'a str,
    mode: PowershellImportMode,
) -> Option<PowershellImportTarget<'a>> {
    let is_path = matches!(mode, PowershellImportMode::SourceRelative)
        || (matches!(mode, PowershellImportMode::PathOrBare) && powershell_path_bearing(raw));
    if !is_path {
        return Some(PowershellImportTarget::External(raw));
    }
    let leaf = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    if raw.starts_with('~') || matches!(leaf, "" | "." | "..") {
        return None;
    }
    // The legacy single-file entrypoint supplies an absolute source spelling.
    // Scope that compatibility case to the file's basename: sibling targets
    // retain their historical portable IDs, while parent traversal still
    // fails closed because no admitted project root is available.
    let logical_source = normalize_project_path(source_file)
        .filter(|source| !source.is_empty())
        .or_else(|| {
            let basename = source_file.rsplit(['/', '\\']).next()?;
            normalize_project_path(basename).filter(|source| !source.is_empty())
        })?;
    match source_relative_project_path(&logical_source, raw)? {
        ProjectPath::Contained(logical) => Some(PowershellImportTarget::Project(logical)),
        ProjectPath::EscapesRoot(_) => None,
    }
}

fn powershell_path_bearing(raw: &str) -> bool {
    raw.chars()
        .next()
        .is_some_and(|first| matches!(first, '.' | '/' | '\\' | '~'))
        || raw.contains(['/', '\\', ':'])
        || raw.rsplit_once('.').is_some_and(|(_, extension)| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "ps1" | "psm1" | "psd1" | "dll" | "exe" | "cdxml"
            )
        })
}

fn powershell_external_import_name(raw: &str) -> Option<String> {
    (!raw.is_empty()).then(|| raw.to_owned())
}

fn static_powershell_import_target(raw: &str, allow_arguments: bool) -> Option<&str> {
    let raw = raw.trim();
    let (target, remainder) = match raw.as_bytes().first().copied() {
        Some(quote @ (b'\'' | b'"')) => {
            let body = &raw[1..];
            let end = body.find(char::from(quote))?;
            (&body[..end], body[end + 1..].trim())
        }
        Some(_) => {
            let end = raw.find(char::is_whitespace).unwrap_or(raw.len());
            (&raw[..end], raw[end..].trim())
        }
        None => return None,
    };
    if target.is_empty()
        || target.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '$' | '`'
                        | '\''
                        | '"'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '['
                        | ']'
                        | ';'
                        | '|'
                        | '&'
                        | ','
                        | '+'
                        | '='
                        | '*'
                        | '?'
                        | '<'
                        | '>'
                )
        })
        || (!allow_arguments && !remainder.is_empty())
        || remainder
            .chars()
            .next()
            .is_some_and(|character| matches!(character, '+' | ',' | '|' | '&' | ';'))
    {
        return None;
    }
    Some(target)
}

fn emit_powershell_calls(
    builder: &mut Builder<'_>,
    text: &str,
    functions: &[Scope],
) -> anyhow::Result<()> {
    let commands = Regex::new(r"(?m)^\s*([A-Za-z_][A-Za-z0-9_-]*)\b")?;
    for function in functions {
        for capture in commands.captures_iter(&text[function.start..function.end]) {
            let name = &capture[1];
            if [
                "using",
                "return",
                "if",
                "else",
                "elseif",
                "foreach",
                "for",
                "while",
                "do",
                "switch",
                "try",
                "catch",
                "finally",
                "throw",
                "break",
                "continue",
                "exit",
                "param",
                "begin",
                "process",
                "end",
                "Import-Module",
            ]
            .iter()
            .any(|keyword| keyword.eq_ignore_ascii_case(name))
            {
                continue;
            }
            let Some(target) = builder.unique_definition(name) else {
                continue;
            };
            if target == function.id {
                continue;
            }
            builder.edge(
                &function.id,
                &target,
                "calls",
                Some("call"),
                line_of(
                    text,
                    function.start + capture.get(0).expect("PowerShell call").start(),
                ),
                Confidence::Extracted,
            );
        }
    }
    Ok(())
}

fn extract_powershell_manifest(
    path: &Path,
    text: &str,
    source_file: &str,
) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let mut imports = Vec::new();
    let root = Regex::new(r#"(?im)^\s*RootModule\s*=\s*['"]([^'"]+)['"]"#)?;
    for capture in root.captures_iter(text) {
        imports.push((
            capture[1].to_owned(),
            line_of(text, capture.get(0).expect("RootModule").start()),
        ));
    }
    let nested = Regex::new(r"(?ims)^\s*NestedModules\s*=\s*@\((.*?)\)")?;
    let required = Regex::new(r"(?ims)^\s*RequiredModules\s*=\s*@\((.*?)^\s*\)")?;
    let strings = Regex::new(r#"['\"]([^'\"]+)['\"]"#)?;
    for capture in nested
        .captures_iter(text)
        .chain(required.captures_iter(text))
    {
        let line = line_of(
            text,
            capture.get(0).expect("PowerShell manifest modules").start(),
        );
        for value in strings.captures_iter(&capture[1]) {
            if !value[1]
                .chars()
                .all(|character| character.is_ascii_digit() || matches!(character, '.' | '-' | '+'))
            {
                imports.push((value[1].to_owned(), line));
            }
        }
    }
    for (raw, line) in imports {
        let Some(raw) = static_powershell_manifest_target(&raw) else {
            continue;
        };
        emit_powershell_import(&mut builder, raw, line, PowershellImportMode::PathOrBare);
    }
    Ok(builder.finish())
}

fn static_powershell_manifest_target(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    (!raw.is_empty()
        && !raw.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '$' | '`'
                        | '\''
                        | '"'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '['
                        | ']'
                        | ';'
                        | '|'
                        | '&'
                        | ','
                        | '+'
                        | '='
                        | '*'
                        | '?'
                        | '<'
                        | '>'
                )
        }))
    .then_some(raw)
}

#[derive(Clone)]
struct DmProc {
    id: String,
    body_start: usize,
    body_end: usize,
}

fn ensure_dm_type(
    builder: &mut Builder<'_>,
    types: &mut HashMap<String, String>,
    type_path: &str,
    line: usize,
) -> String {
    if let Some(id) = types.get(type_path) {
        return id.clone();
    }
    let id = builder.labeled_definition(type_path, type_path, None, "class", line);
    types.insert(type_path.to_owned(), id.clone());
    id
}

#[allow(clippy::too_many_arguments)]
fn add_dm_proc(
    builder: &mut Builder<'_>,
    procs: &mut Vec<DmProc>,
    proc_ids: &mut HashMap<String, Vec<String>>,
    name: &str,
    owner: Option<(&str, &str)>,
    line: usize,
    body_start: usize,
) {
    let (label, owner_id) = owner
        .map(|(owner_path, owner_id)| (format!("{owner_path}/{name}()"), Some(owner_id)))
        .unwrap_or_else(|| (format!("{name}()"), None));
    let id = builder.labeled_definition(name, &label, owner_id, "function", line);
    proc_ids
        .entry(name.to_ascii_lowercase())
        .or_default()
        .push(id.clone());
    procs.push(DmProc {
        id,
        body_start,
        body_end: body_start,
    });
}

fn dm_logical_source(path: &Path, source_file: &str, allow_path_probes: bool) -> Option<String> {
    normalize_project_path(source_file)
        .filter(|source| !source.is_empty())
        .or_else(|| {
            allow_path_probes
                .then(|| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .and_then(normalize_project_path)
                        .filter(|source| !source.is_empty())
                })
                .flatten()
        })
}

fn dm_include_target(logical_source: &str, reference: &str) -> Option<String> {
    let leaf = reference.rsplit(['/', '\\']).next().unwrap_or(reference);
    if matches!(leaf, "" | "." | "..") {
        return None;
    }
    match source_relative_project_path(logical_source, reference)? {
        ProjectPath::Contained(logical) => Some(logical),
        ProjectPath::EscapesRoot(_) => None,
    }
}

fn dm_physical_path_matches_source(path: &Path, logical_source: &str) -> bool {
    let mut physical = Some(path);
    for logical in logical_source.rsplit('/') {
        let Some(current) = physical else {
            return false;
        };
        let Some(name) = current.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if name != logical {
            return false;
        }
        physical = current.parent();
    }
    true
}

/// The byte entrypoint never calls this helper. The legacy path entrypoint may
/// retain its historical eager include resolution, but only after the logical
/// target has been admitted and the physical target is proven to be a regular
/// file beneath the corresponding physical root.
fn dm_legacy_include_source(
    path: &Path,
    logical_source: &str,
    reference: &str,
    logical_target: &str,
) -> Option<String> {
    let (root, candidate) = if dm_physical_path_matches_source(path, logical_source) {
        let mut root = path.to_path_buf();
        for _ in logical_source.split('/') {
            if !root.pop() {
                return None;
            }
        }
        let candidate = root.join(logical_target);
        (root, candidate)
    } else {
        // Some legacy callers pair a project-relative identity with a physical
        // single-file path whose directory suffix does not match it. Preserve
        // safe sibling/subdirectory lookup, but never infer a root for `..`.
        if reference.split(['/', '\\']).any(|part| part == "..") {
            return None;
        }
        let root = path.parent()?;
        let mut candidate = root.to_path_buf();
        for part in reference.split(['/', '\\']) {
            if part != "." {
                candidate.push(part);
            }
        }
        (root.to_path_buf(), candidate)
    };

    let root = if root.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        root
    };
    let Ok(root) = fs::canonicalize(root) else {
        return None;
    };
    let physical_source = candidate.to_string_lossy().replace('\\', "/");
    let Ok(canonical_candidate) = fs::canonicalize(candidate) else {
        return None;
    };
    (canonical_candidate.starts_with(&root) && canonical_candidate.is_file())
        .then_some(physical_source)
}

fn extract_dm(
    path: &Path,
    text: &str,
    source_file: &str,
    allow_path_probes: bool,
) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let include = Regex::new(r#"^#include\s+[\"']([^\"']+)[\"']"#)?;
    let path_declaration = Regex::new(r"^(/[A-Za-z_][A-Za-z0-9_/]*)(?:\s*\([^)]*\))?\s*$")?;
    let nested_proc = Regex::new(r"^(?:proc/)?([A-Za-z_][A-Za-z0-9_]*)\s*\([^)]*\)\s*$")?;
    let mut types = HashMap::new();
    let mut proc_ids: HashMap<String, Vec<String>> = HashMap::new();
    let mut procs = Vec::new();
    let mut declaration_offsets = Vec::new();
    let mut current_owner: Option<(String, String)> = None;
    let mut offset = 0usize;
    let logical_source = dm_logical_source(path, source_file, allow_path_probes);

    for (line_index, raw_line) in text.split_inclusive('\n').enumerate() {
        let line_number = line_index + 1;
        let line = raw_line.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim();
        let body_start = offset + raw_line.len();
        if let Some(capture) = include.captures(trimmed) {
            if let Some(logical_source) = logical_source.as_deref()
                && let Some(logical_target) = dm_include_target(logical_source, &capture[1])
            {
                let stem = Path::new(&logical_target)
                    .with_extension("")
                    .to_string_lossy()
                    .replace('\\', "/");
                let target = make_id(&[&stem]);
                let label = Path::new(&logical_target)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(&logical_target);
                let resolved_source = if allow_path_probes {
                    dm_legacy_include_source(path, logical_source, &capture[1], &logical_target)
                } else {
                    None
                };
                let resolved = resolved_source.is_some();
                let prior_nodes = builder.nodes.len();
                builder.insert_node(
                    target.clone(),
                    label,
                    if resolved { "file" } else { "reference" },
                    line_number,
                    resolved_source.as_deref().unwrap_or(&logical_target),
                );
                if builder.nodes.len() > prior_nodes {
                    let node = builder.nodes.last_mut().expect("new DM include node");
                    node.extra.insert(
                        crate::project_path::EXACT_PROJECT_RELATIVE_PLACEHOLDER.into(),
                        true.into(),
                    );
                    node.extra
                        .insert("target_file".into(), logical_target.clone().into());
                }
                let prior_edges = builder.edges.len();
                builder.edge(
                    &builder.file_id.clone(),
                    &target,
                    if resolved { "imports_from" } else { "imports" },
                    Some("import"),
                    line_number,
                    Confidence::Extracted,
                );
                if builder.edges.len() > prior_edges {
                    let edge = builder.edges.last_mut().expect("new DM include edge");
                    edge.extra
                        .insert("target_file".into(), logical_target.into());
                    if !resolved {
                        edge.extra.insert("external".into(), true.into());
                    }
                }
            }
            offset += raw_line.len();
            continue;
        }

        let is_top_level = line.len() == line.trim_start().len();
        if is_top_level {
            if let Some(capture) = path_declaration.captures(trimmed) {
                let full_path = &capture[1];
                declaration_offsets.push(offset);
                if let Some(name) = full_path.strip_prefix("/proc/") {
                    current_owner = None;
                    add_dm_proc(
                        &mut builder,
                        &mut procs,
                        &mut proc_ids,
                        name,
                        None,
                        line_number,
                        body_start,
                    );
                } else if let Some((owner_path, name)) = full_path.rsplit_once("/proc/") {
                    let owner_id =
                        ensure_dm_type(&mut builder, &mut types, owner_path, line_number);
                    current_owner = None;
                    add_dm_proc(
                        &mut builder,
                        &mut procs,
                        &mut proc_ids,
                        name,
                        Some((owner_path, &owner_id)),
                        line_number,
                        body_start,
                    );
                } else if trimmed.contains('(') {
                    if let Some((owner_path, name)) = full_path.rsplit_once('/') {
                        let owner_id =
                            ensure_dm_type(&mut builder, &mut types, owner_path, line_number);
                        current_owner = None;
                        add_dm_proc(
                            &mut builder,
                            &mut procs,
                            &mut proc_ids,
                            name,
                            Some((owner_path, &owner_id)),
                            line_number,
                            body_start,
                        );
                    }
                } else {
                    let owner_id = ensure_dm_type(&mut builder, &mut types, full_path, line_number);
                    current_owner = Some((full_path.to_owned(), owner_id));
                }
                offset += raw_line.len();
                continue;
            }
            if !trimmed.is_empty() && !trimmed.starts_with("//") {
                current_owner = None;
            }
        } else if let Some((owner_path, owner_id)) = current_owner.clone() {
            let indentation = line
                .chars()
                .take_while(|character| character.is_whitespace())
                .fold(0usize, |depth, character| {
                    depth + if character == '\t' { 4 } else { 1 }
                });
            if indentation <= 4
                && let Some(capture) = nested_proc.captures(trimmed)
            {
                declaration_offsets.push(offset);
                add_dm_proc(
                    &mut builder,
                    &mut procs,
                    &mut proc_ids,
                    &capture[1],
                    Some((&owner_path, &owner_id)),
                    line_number,
                    body_start,
                );
            }
        }
        offset += raw_line.len();
    }

    declaration_offsets.sort_unstable();
    for proc in &mut procs {
        proc.body_end = declaration_offsets
            .iter()
            .copied()
            .find(|candidate| *candidate >= proc.body_start)
            .unwrap_or(text.len());
    }

    let member_calls = Regex::new(r"\b[A-Za-z_][A-Za-z0-9_]*\.([A-Za-z_][A-Za-z0-9_]*)\s*\(")?;
    let direct_calls = Regex::new(r"(?m)(?:^|[^A-Za-z0-9_.])([A-Za-z_][A-Za-z0-9_]*)\s*\(")?;
    let constructions = Regex::new(r"\bnew\s+(/[A-Za-z_][A-Za-z0-9_/]*)\s*\(")?;
    for proc in &procs {
        let body = &text[proc.body_start..proc.body_end];
        for capture in member_calls.captures_iter(body) {
            let name = &capture[1];
            let candidates = proc_ids
                .get(&name.to_ascii_lowercase())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if candidates.len() == 1 && candidates[0] != proc.id {
                builder.edge(
                    &proc.id,
                    &candidates[0],
                    "calls",
                    Some("call"),
                    line_of(
                        text,
                        proc.body_start + capture.get(0).expect("DM member call").start(),
                    ),
                    Confidence::Extracted,
                );
            }
        }
        for capture in direct_calls.captures_iter(body) {
            let name = &capture[1];
            if matches!(name, "if" | "for" | "while" | "switch" | "new") {
                continue;
            }
            let candidates = proc_ids
                .get(&name.to_ascii_lowercase())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if candidates.len() == 1 && candidates[0] != proc.id {
                builder.edge(
                    &proc.id,
                    &candidates[0],
                    "calls",
                    Some("call"),
                    line_of(
                        text,
                        proc.body_start + capture.get(0).expect("DM call").start(),
                    ),
                    Confidence::Extracted,
                );
            }
        }
        for capture in constructions.captures_iter(body) {
            if let Some(target) = types.get(&capture[1]) {
                builder.edge(
                    &proc.id,
                    target,
                    "instantiates",
                    Some("call"),
                    line_of(
                        text,
                        proc.body_start + capture.get(0).expect("DM construction").start(),
                    ),
                    Confidence::Extracted,
                );
            }
        }
    }
    Ok(builder.finish())
}

pub(crate) fn extract_dmi(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let bytes = fs::read(path)?;
    extract_dmi_bytes(path, source_file, &bytes)
}

pub(crate) fn extract_dmi_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(builder.finish());
    }
    let mut description = None;
    let mut offset = 8usize;
    while offset.checked_add(12).is_some_and(|end| end <= bytes.len()) {
        let length = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("four-byte PNG chunk length"),
        ) as usize;
        let payload_start = offset + 8;
        let Some(payload_end) = payload_start.checked_add(length) else {
            break;
        };
        let Some(chunk_end) = payload_end.checked_add(4) else {
            break;
        };
        if chunk_end > bytes.len() {
            break;
        }
        if &bytes[offset + 4..offset + 8] == b"tEXt" {
            let payload = &bytes[payload_start..payload_end];
            if let Some(null) = payload.iter().position(|byte| *byte == 0)
                && &payload[..null] == b"Description"
            {
                description = Some(String::from_utf8_lossy(&payload[null + 1..]).into_owned());
                break;
            }
        }
        offset = chunk_end;
    }
    if let Some(description) = description {
        for (line_index, line) in description.lines().enumerate() {
            let Some(value) = line.trim().strip_prefix("state =") else {
                continue;
            };
            let state = value.trim().trim_matches('"');
            if state.is_empty() {
                continue;
            }
            let id = make_id(&[&builder.stem, "state", state]);
            builder.insert_node(
                id.clone(),
                &format!("\"{state}\""),
                "enum_case",
                line_index + 1,
                source_file,
            );
            builder.edge(
                &builder.file_id.clone(),
                &id,
                "contains",
                None,
                line_index + 1,
                Confidence::Extracted,
            );
        }
    }
    Ok(builder.finish())
}

fn split_dmm_tile(body: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut buffer = String::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for character in body.chars() {
        if escaped {
            buffer.push(character);
            escaped = false;
            continue;
        }
        if in_string {
            buffer.push(character);
            if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                buffer.push(character);
            }
            '(' | '{' | '[' => {
                depth += 1;
                buffer.push(character);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                buffer.push(character);
            }
            ',' if depth == 0 => {
                entries.push(buffer.trim().to_owned());
                buffer.clear();
            }
            _ => buffer.push(character),
        }
    }
    if !buffer.trim().is_empty() {
        entries.push(buffer.trim().to_owned());
    }
    entries
}

fn extract_dmm(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let grid = Regex::new(r"(?m)^\(\s*\d+\s*,\s*\d+\s*,\s*\d+\s*\)\s*=")?;
    let dictionary = grid
        .find(text)
        .map(|matched| &text[..matched.start()])
        .unwrap_or(text);
    let mut buffer = String::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut open_line = 1usize;
    let mut seen_targets = HashSet::new();
    for (line_index, line) in dictionary.lines().enumerate() {
        for character in line.chars() {
            if escaped {
                escaped = false;
            } else if in_string {
                if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
            } else {
                match character {
                    '"' => in_string = true,
                    '(' => {
                        if depth == 0 {
                            open_line = line_index + 1;
                        }
                        depth += 1;
                    }
                    ')' => depth -= 1,
                    _ => {}
                }
            }
            buffer.push(character);
        }
        buffer.push('\n');
        if depth != 0 || buffer.trim().is_empty() {
            continue;
        }
        let chunk = std::mem::take(&mut buffer);
        let (Some(left), Some(right)) = (chunk.find('('), chunk.rfind(')')) else {
            continue;
        };
        if right <= left {
            continue;
        }
        for entry in split_dmm_tile(&chunk[left + 1..right]) {
            let type_path = entry.split('{').next().unwrap_or("").trim();
            if !type_path.starts_with('/') {
                continue;
            }
            let target = make_id(&[type_path]);
            if !seen_targets.insert(target.clone()) {
                continue;
            }
            builder.insert_node(target.clone(), type_path, "reference", open_line, "");
            builder.edge(
                &builder.file_id.clone(),
                &target,
                "uses",
                Some("map"),
                open_line,
                Confidence::Extracted,
            );
        }
    }
    Ok(builder.finish())
}

fn extract_dmf(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let window = Regex::new(r#"^\s*window\s+\"([^\"]+)\"\s*$"#)?;
    let element = Regex::new(r#"^\s*elem\s+\"([^\"]+)\"\s*$"#)?;
    let control_type = Regex::new(r"^\s*type\s*=\s*(\S+)\s*$")?;
    let mut current_window = None;
    let mut current_element: Option<(String, String)> = None;
    for (line_index, line) in text.lines().enumerate() {
        let line_number = line_index + 1;
        if let Some(capture) = window.captures(line) {
            let name = &capture[1];
            let id = builder.labeled_definition(
                &format!("window/{name}"),
                &format!("window \"{name}\""),
                None,
                "window",
                line_number,
            );
            current_window = Some(id);
            current_element = None;
            continue;
        }
        if let (Some(capture), Some(owner)) = (element.captures(line), current_window.as_deref()) {
            let name = capture[1].to_owned();
            let owner = owner.to_owned();
            let id = builder.labeled_definition(
                &format!("elem/{name}"),
                &format!("elem \"{name}\""),
                Some(&owner),
                "element",
                line_number,
            );
            current_element = Some((id, name));
            continue;
        }
        if let (Some(capture), Some((id, name))) =
            (control_type.captures(line), current_element.as_ref())
        {
            let label = format!("elem \"{name}\" [{}]", &capture[1]);
            if let Some(node) = builder.nodes.iter_mut().find(|node| node.id == *id) {
                node.label = label.clone();
            }
            builder.labels.insert(id.clone(), label);
        }
    }
    Ok(builder.finish())
}

fn extract_sln(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let project = Regex::new(
        r#"(?m)^Project\(\"[^\"]*\"\)\s*=\s*\"([^\"]+)\"\s*,\s*\"([^\"]+)\"\s*,\s*\"\{?([^\"}]+)\}?\""#,
    )?;
    let mut guid_to_id = HashMap::new();
    for capture in project.captures_iter(text) {
        let name = &capture[1];
        let relative = capture[2].replace('\\', "/");
        let is_solution_folder = relative == name;
        let (logical_path, external) = if is_solution_folder {
            (relative, false)
        } else {
            portable_project_reference(path, source_file, &relative)
        };
        let id = if is_solution_folder {
            make_id(&[&logical_path])
        } else {
            portable_project_id(&logical_path, external)
        };
        let line = line_of(text, capture.get(0).expect("solution project").start());
        builder.insert_node(id.clone(), name, "project", line, &logical_path);
        builder.edge(
            &builder.file_id.clone(),
            &id,
            "contains",
            None,
            line,
            Confidence::Extracted,
        );
        guid_to_id.insert(capture[3].to_ascii_lowercase(), id);
    }

    let project_line = Regex::new(
        r#"Project\(\"[^\"]*\"\)\s*=\s*\"[^\"]+\"\s*,\s*\"[^\"]+\"\s*,\s*\"\{([^}]+)\}\""#,
    )?;
    let dependency = Regex::new(r"\{([0-9A-Fa-f-]+)\}\s*=\s*\{([0-9A-Fa-f-]+)\}")?;
    let mut current_project = None;
    let mut in_dependencies = false;
    for (line_index, line) in text.lines().enumerate() {
        if let Some(capture) = project_line.captures(line) {
            current_project = Some(capture[1].to_ascii_lowercase());
            continue;
        }
        if line.trim() == "EndProject" {
            current_project = None;
            continue;
        }
        if line.contains("ProjectSection(ProjectDependencies)") {
            in_dependencies = true;
            continue;
        }
        if line.contains("EndProjectSection") {
            in_dependencies = false;
            continue;
        }
        if !in_dependencies {
            continue;
        }
        let (Some(current), Some(capture)) = (current_project.as_ref(), dependency.captures(line))
        else {
            continue;
        };
        let target_guid = capture[1].to_ascii_lowercase();
        if let (Some(source), Some(target)) =
            (guid_to_id.get(current), guid_to_id.get(&target_guid))
        {
            builder.edge(
                source,
                target,
                "imports",
                None,
                line_index + 1,
                Confidence::Extracted,
            );
        }
    }
    Ok(builder.finish())
}

fn xml_attribute(attributes: &str, name: &str) -> Option<String> {
    let pattern = format!(r#"(?i)\b{}\s*=\s*\"([^\"]*)\""#, regex::escape(name));
    Regex::new(&pattern)
        .ok()?
        .captures(attributes)
        .map(|capture| capture[1].to_owned())
}

fn extract_dotnet_project(
    path: &Path,
    text: &str,
    source_file: &str,
) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let framework = Regex::new(r"(?is)<TargetFrameworks?>\s*([^<]+?)\s*</TargetFrameworks?>")?;
    for capture in framework.captures_iter(text) {
        for value in capture[1]
            .split(';')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let id = make_id(&["framework", value]);
            let line = line_of(text, capture.get(0).expect("target framework").start());
            builder.insert_node(id.clone(), value, "framework", line, source_file);
            builder.edge(
                &builder.file_id.clone(),
                &id,
                "references",
                None,
                line,
                Confidence::Extracted,
            );
        }
    }
    let element = Regex::new(r"(?is)<(PackageReference|ProjectReference)\b([^>]*)>")?;
    for capture in element.captures_iter(text) {
        let kind = &capture[1].to_ascii_lowercase();
        let attributes = &capture[2];
        let Some(include) = xml_attribute(attributes, "Include") else {
            continue;
        };
        let line = line_of(text, capture.get(0).expect("project XML element").start());
        if kind == "packagereference" {
            let version = xml_attribute(attributes, "Version");
            let label = version
                .filter(|version| !version.is_empty())
                .map(|version| format!("{include} ({version})"))
                .unwrap_or_else(|| include.clone());
            let id = make_id(&["nuget", &include]);
            builder.insert_node(id.clone(), &label, "package", line, source_file);
            builder.edge(
                &builder.file_id.clone(),
                &id,
                "imports",
                None,
                line,
                Confidence::Extracted,
            );
        } else {
            let normalized = include.replace('\\', "/");
            let (logical_path, external) =
                portable_project_reference(path, source_file, &normalized);
            let id = portable_project_id(&logical_path, external);
            let label = normalized.rsplit('/').next().unwrap_or(&normalized);
            builder.insert_node(id.clone(), label, "project", line, &logical_path);
            builder.edge(
                &builder.file_id.clone(),
                &id,
                "imports",
                None,
                line,
                Confidence::Extracted,
            );
        }
    }
    let sdk = Regex::new(r#"(?is)<Project\b[^>]*\bSdk\s*=\s*\"([^\"]+)\""#)?;
    if let Some(capture) = sdk.captures(text) {
        let value = &capture[1];
        let id = make_id(&["sdk", value]);
        let line = line_of(text, capture.get(0).expect("project SDK").start());
        builder.insert_node(id.clone(), value, "sdk", line, source_file);
        builder.edge(
            &builder.file_id.clone(),
            &id,
            "references",
            None,
            line,
            Confidence::Extracted,
        );
    }
    Ok(builder.finish())
}

fn extract_xaml(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let root_element = Regex::new(r"(?s)^\s*<([A-Za-z_][A-Za-z0-9_.:-]*)")?;
    let root_name = root_element
        .captures(text)
        .map(|capture| {
            capture[1]
                .rsplit(':')
                .next()
                .unwrap_or(&capture[1])
                .to_owned()
        })
        .unwrap_or_else(|| "Xaml".into());
    let root_id = builder.definition(&root_name, None, "element", 1);
    let class = Regex::new(r#"\bx:Class\s*=\s*\"([^\"]+)\""#)?;
    let class_row = class.captures(text).map(|capture| {
        let qualified = capture[1].to_owned();
        let simple = qualified
            .rsplit('.')
            .next()
            .unwrap_or(&qualified)
            .to_owned();
        let line = line_of(text, capture.get(0).expect("XAML class").start());
        (simple, line)
    });
    let class_id = class_row.as_ref().map(|(simple, line)| {
        let id = builder.labeled_definition(simple, simple, None, "class", *line);
        builder.edge(
            &root_id,
            &id,
            "references",
            Some("x_class"),
            *line,
            Confidence::Extracted,
        );
        id
    });

    let mut handlers = HashMap::new();
    let codebehind = path.with_file_name(format!(
        "{}.cs",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
    ));
    let logical_codebehind =
        normalize_logical_path(&Path::new(source_file).with_file_name(format!(
            "{}.cs",
            Path::new(source_file)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
        )));
    if let (Some(owner), Ok(code)) = (class_id.as_deref(), fs::read_to_string(&codebehind)) {
        let methods = Regex::new(
            r"(?m)^\s*(?:public|private|protected|internal)\s+(?:static\s+)?[A-Za-z_][A-Za-z0-9_.<>?]*\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(\s*object\??\s+\w+\s*,\s*[A-Za-z0-9_.]+EventArgs(?:<[^>]*>)?\s+\w+\s*\)",
        )?;
        for capture in methods.captures_iter(&code) {
            let name = &capture[1];
            let line = line_of(&code, capture.get(0).expect("XAML handler").start());
            let id = builder.labeled_definition(
                name,
                &format!(".{name}()"),
                Some(owner),
                "function",
                line,
            );
            if let Some(node) = builder.nodes.iter_mut().find(|node| node.id == id) {
                node.source_file.clone_from(&logical_codebehind);
            }
            handlers.insert(name.to_owned(), id);
        }
    }

    let named_element = Regex::new(
        r#"(?s)<([A-Za-z_][A-Za-z0-9_.:-]*)\b[^>]*\b(?:x:)?Name\s*=\s*\"([^\"]+)\"[^>]*>"#,
    )?;
    for capture in named_element.captures_iter(text) {
        let name = &capture[2];
        let line = line_of(text, capture.get(0).expect("named XAML element").start());
        builder.labeled_definition(name, name, Some(&root_id), "element", line);
    }

    let attribute =
        Regex::new(r#"([A-Za-z_][A-Za-z0-9_.:-]*)\s*=\s*\"([A-Za-z_][A-Za-z0-9_]*)\""#)?;
    for capture in attribute.captures_iter(text) {
        let Some(target) = handlers.get(&capture[2]) else {
            continue;
        };
        let line = line_of(text, capture.get(0).expect("XAML event").start());
        builder.edge(
            &root_id,
            target,
            "references",
            Some("event"),
            line,
            Confidence::Extracted,
        );
    }
    Ok(builder.finish())
}

fn extract_razor(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let using = Regex::new(r"(?m)^@using\s+([A-Za-z_][A-Za-z0-9_.]*)")?;
    for capture in using.captures_iter(text) {
        let line = line_of(text, capture.get(0).expect("Razor using").start());
        let target = builder.external(&capture[1], line);
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "imports",
            Some("import"),
            line,
            Confidence::Extracted,
        );
    }
    let inject = Regex::new(r"(?m)^@inject\s+([A-Za-z_][A-Za-z0-9_.<>\[\]]*)\s+\w+")?;
    for capture in inject.captures_iter(text) {
        let line = line_of(text, capture.get(0).expect("Razor injection").start());
        let target = builder.external(&capture[1], line);
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "imports",
            Some("import"),
            line,
            Confidence::Extracted,
        );
    }
    let inherits = Regex::new(r"(?m)^@inherits\s+([A-Za-z_][A-Za-z0-9_.<>\[\]]*)")?;
    for capture in inherits.captures_iter(text) {
        let line = line_of(text, capture.get(0).expect("Razor inherits").start());
        let target = builder.external(&capture[1], line);
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "inherits",
            None,
            line,
            Confidence::Extracted,
        );
    }
    let components = Regex::new(r"<([A-Z][A-Za-z0-9]+)(?:\s|/|>)")?;
    let html = [
        "DOCTYPE", "Html", "Head", "Body", "Div", "Span", "Table", "Form", "Input", "Button",
        "Select", "Option", "Label", "Textarea", "Script", "Style", "Link", "Meta", "Title",
        "Header", "Footer", "Nav", "Main", "Section", "Article", "Aside",
    ];
    for capture in components.captures_iter(text) {
        let name = &capture[1];
        if html.contains(&name) {
            continue;
        }
        let line = line_of(text, capture.get(0).expect("Razor component").start());
        let target = builder.external(name, line);
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "calls",
            Some("call"),
            line,
            Confidence::Extracted,
        );
    }
    let methods = Regex::new(
        r"(?m)^\s*(?:public|private|protected|internal|static|async|override|virtual|abstract)(?:\s+(?:public|private|protected|internal|static|async|override|virtual|abstract))*\s+[A-Za-z_][A-Za-z0-9_.<>\[\], ]*\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
    )?;
    for capture in methods.captures_iter(text) {
        builder.definition(
            &capture[1],
            None,
            "function",
            line_of(text, capture.get(0).expect("Razor method").start()),
        );
    }
    Ok(builder.finish())
}

fn extract_apex(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let class = Regex::new(
        r"(?i)^(?:(?:public|private|protected|global|abstract|virtual|with|without|inherited|sharing|static|final)\s+)*class\s+([A-Za-z_]\w*)(?:\s+extends\s+([A-Za-z_]\w*))?(?:\s+implements\s+([^\{]+))?",
    )?;
    let interface = Regex::new(
        r"(?i)^(?:(?:public|private|protected|global|abstract|virtual|static)\s+)*interface\s+([A-Za-z_]\w*)(?:\s+extends\s+([^\{]+))?",
    )?;
    let enumeration = Regex::new(
        r"(?i)^(?:(?:public|private|protected|global|static)\s+)*enum\s+([A-Za-z_]\w*)",
    )?;
    let trigger = Regex::new(r"(?i)^trigger\s+([A-Za-z_]\w*)\s+on\s+([A-Za-z_]\w*)\s*\(")?;
    let method = Regex::new(
        r"(?i)^(?:(?:public|private|protected|global|webservice|static|abstract|virtual|override|final|testmethod)\s+)+[A-Za-z_]\w*(?:\s*<[^>]+>)?(?:\[\])?\s+([A-Za-z_]\w*)\s*\(",
    )?;
    let soql = Regex::new(r"(?i)\[\s*SELECT\b[^\]]+\bFROM\s+([A-Za-z_]\w*)")?;
    let dml = Regex::new(r"(?i)\b(insert|update|delete|upsert|merge|undelete)\s+[A-Za-z_]\w*")?;
    let mut current_class = None;
    for (line_index, line) in text.lines().enumerate() {
        let line_number = line_index + 1;
        let stripped = line.trim();
        if let Some(capture) = trigger.captures(stripped) {
            let id = builder.definition(&capture[1], None, "trigger", line_number);
            let target = builder.external(&capture[2], line_number);
            builder.edge(
                &id,
                &target,
                "uses",
                None,
                line_number,
                Confidence::Inferred,
            );
            current_class = Some(id);
            continue;
        }
        if let Some(capture) = class.captures(stripped) {
            let id = builder.definition(&capture[1], None, "class", line_number);
            if let Some(parent) = capture.get(2) {
                let target = builder.external(parent.as_str(), line_number);
                builder.edge(
                    &id,
                    &target,
                    "extends",
                    None,
                    line_number,
                    Confidence::Inferred,
                );
            }
            if let Some(interfaces) = capture.get(3) {
                for name in interfaces.as_str().split(',').map(str::trim) {
                    if name.is_empty() {
                        continue;
                    }
                    let target = builder.external(name, line_number);
                    builder.edge(
                        &id,
                        &target,
                        "implements",
                        None,
                        line_number,
                        Confidence::Inferred,
                    );
                }
            }
            current_class = Some(id);
            continue;
        }
        if let Some(capture) = interface.captures(stripped) {
            let id = builder.labeled_definition(
                &capture[1],
                &capture[1],
                current_class.as_deref(),
                "interface",
                line_number,
            );
            if let Some(parents) = capture.get(2) {
                for name in parents.as_str().split(',').map(str::trim) {
                    if name.is_empty() {
                        continue;
                    }
                    let target = builder.external(name, line_number);
                    builder.edge(
                        &id,
                        &target,
                        "extends",
                        None,
                        line_number,
                        Confidence::Inferred,
                    );
                }
            }
            continue;
        }
        if let Some(capture) = enumeration.captures(stripped) {
            builder.labeled_definition(
                &capture[1],
                &capture[1],
                current_class.as_deref(),
                "enum",
                line_number,
            );
            continue;
        }
        if let (Some(owner), Some(capture)) = (current_class.as_deref(), method.captures(stripped))
        {
            builder.definition(&capture[1], Some(owner), "function", line_number);
        }
        for capture in soql.captures_iter(line) {
            let target = builder.external(&capture[1], line_number);
            builder.edge(
                current_class.as_deref().unwrap_or(&builder.file_id.clone()),
                &target,
                "uses",
                None,
                line_number,
                Confidence::Inferred,
            );
        }
        for capture in dml.captures_iter(line) {
            let operation = capture[1].to_ascii_lowercase();
            let id = make_id(&["dml", &operation]);
            builder.insert_node(
                id.clone(),
                &operation,
                "operation",
                line_number,
                source_file,
            );
            builder.edge(
                current_class.as_deref().unwrap_or(&builder.file_id.clone()),
                &id,
                "uses",
                None,
                line_number,
                Confidence::Inferred,
            );
        }
    }
    Ok(builder.finish())
}

fn sv_type_references(
    type_text: &str,
    type_parameters: &HashSet<String>,
) -> Vec<(String, &'static str)> {
    let builtins = [
        "bit",
        "logic",
        "reg",
        "wire",
        "int",
        "integer",
        "shortint",
        "longint",
        "byte",
        "time",
        "real",
        "shortreal",
        "void",
        "string",
        "type",
        "event",
        "mailbox",
        "semaphore",
        "process",
        "chandle",
    ];
    let mut references = Vec::new();
    let identifier = Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("SystemVerilog identifier");
    let Some(primary) = identifier.find(type_text).map(|value| value.as_str()) else {
        return references;
    };
    if !builtins.contains(&primary) && !type_parameters.contains(primary) {
        references.push((primary.to_owned(), "type"));
    }
    if let Some(hash) = type_text.find('#') {
        for name in identifier
            .find_iter(&type_text[hash + 1..])
            .map(|value| value.as_str())
        {
            if !builtins.contains(&name)
                && !type_parameters.contains(name)
                && !references.iter().any(|(existing, _)| existing == name)
            {
                references.push((name.to_owned(), "generic_arg"));
            }
        }
    }
    references
}

fn extract_systemverilog(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let classes =
        Regex::new(r"(?s)\b(?:interface\s+)?class\s+([A-Za-z_]\w*)([^;]*)\s*;(.*?)\bendclass\b")?;
    let mut class_rows = Vec::new();
    for capture in classes.captures_iter(text) {
        let matched = capture.get(0).expect("SystemVerilog class");
        let line = line_of(text, matched.start());
        let id = builder.definition(&capture[1], None, "class", line);
        class_rows.push((
            id,
            capture[2].to_owned(),
            capture[3].to_owned(),
            matched.start(),
        ));
    }
    let type_parameter = Regex::new(r"\btype\s+([A-Za-z_]\w*)")?;
    let extension = Regex::new(r"\bextends\s+([A-Za-z_]\w*)")?;
    let implementation = Regex::new(r"\bimplements\s+([^;{]+)")?;
    let function = Regex::new(
        r"(?s)\bfunction\s+([A-Za-z_]\w*(?:\s*#\s*\([^;]*?\))?)\s+([A-Za-z_]\w*)\s*\((.*?)\)\s*;",
    )?;
    let field = Regex::new(
        r"(?m)^\s*(?:(?:rand|randc|local|protected|static|const|automatic|var)\s+)*([A-Za-z_]\w*(?:\s*#\s*\([^;]+?\))?)\s+[A-Za-z_]\w*\s*;",
    )?;
    let parameter = Regex::new(
        r"^(?:(?:input|output|inout|ref|const\s+ref)\s+)?([A-Za-z_]\w*(?:\s*#\s*\([^;]+?\))?)\s+[A-Za-z_]\w*",
    )?;
    let function_bodies = Regex::new(r"(?s)\bfunction\b.*?\bendfunction\b")?;
    for (class_id, header, body, class_offset) in class_rows {
        let type_parameters: HashSet<_> = type_parameter
            .captures_iter(&header)
            .map(|capture| capture[1].to_owned())
            .collect();
        let class_line = line_of(text, class_offset);
        if let Some(capture) = extension.captures(&header) {
            let target = builder.external(&capture[1], class_line);
            builder.edge(
                &class_id,
                &target,
                "inherits",
                None,
                class_line,
                Confidence::Extracted,
            );
        }
        if let Some(capture) = implementation.captures(&header) {
            for name in capture[1]
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                let name = name.split('#').next().unwrap_or(name).trim();
                let target = builder.external(name, class_line);
                builder.edge(
                    &class_id,
                    &target,
                    "implements",
                    None,
                    class_line,
                    Confidence::Extracted,
                );
            }
        }
        let body_without_functions = function_bodies.replace_all(&body, "");
        for capture in field.captures_iter(&body_without_functions) {
            let line = class_line
                + body_without_functions[..capture.get(0).expect("SV field").start()]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count();
            for (name, role) in sv_type_references(&capture[1], &type_parameters) {
                let target = builder.external(&name, line);
                builder.edge(
                    &class_id,
                    &target,
                    "references",
                    Some(if role == "generic_arg" {
                        "generic_arg"
                    } else {
                        "field"
                    }),
                    line,
                    Confidence::Extracted,
                );
            }
        }
        for capture in function.captures_iter(&body) {
            let line = class_line
                + body[..capture.get(0).expect("SV function").start()]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count();
            let function_id = builder.labeled_definition(
                &capture[2],
                &capture[2],
                Some(&class_id),
                "function",
                line,
            );
            for (name, role) in sv_type_references(&capture[1], &type_parameters) {
                let target = builder.external(&name, line);
                builder.edge(
                    &function_id,
                    &target,
                    "references",
                    Some(if role == "generic_arg" {
                        "generic_arg"
                    } else {
                        "return_type"
                    }),
                    line,
                    Confidence::Extracted,
                );
            }
            for raw_parameter in capture[3].split(',') {
                let Some(parameter) = parameter.captures(raw_parameter.trim()) else {
                    continue;
                };
                for (name, role) in sv_type_references(&parameter[1], &type_parameters) {
                    let target = builder.external(&name, line);
                    builder.edge(
                        &function_id,
                        &target,
                        "references",
                        Some(if role == "generic_arg" {
                            "generic_arg"
                        } else {
                            "parameter_type"
                        }),
                        line,
                        Confidence::Extracted,
                    );
                }
            }
        }
    }

    let package = Regex::new(r"(?m)^\s*package\s+([A-Za-z_]\w*)\s*;")?;
    for capture in package.captures_iter(text) {
        builder.definition(
            &capture[1],
            None,
            "package",
            line_of(text, capture.get(0).expect("SV package").start()),
        );
    }
    let modules = Regex::new(r"(?s)\bmodule\s+([A-Za-z_]\w*)\s*;(.*?)\bendmodule\b")?;
    let module_function = Regex::new(r"(?m)^\s*function\s+\w+\s+([A-Za-z_]\w*)\s*\(")?;
    let task = Regex::new(r"(?m)^\s*task\s+([A-Za-z_]\w*)\s*;")?;
    let import = Regex::new(r"(?m)^\s*import\s+([A-Za-z_]\w*)::")?;
    let instantiation = Regex::new(r"(?m)^\s*([A-Za-z_]\w*)\s+[A-Za-z_]\w*\s*\([^;]*\)\s*;")?;
    for capture in modules.captures_iter(text) {
        let matched = capture.get(0).expect("SV module");
        let line = line_of(text, matched.start());
        let module_id = builder.definition(&capture[1], None, "module", line);
        let body = &capture[2];
        for function_capture in module_function.captures_iter(body) {
            builder.labeled_definition(
                &function_capture[1],
                &format!("{}()", &function_capture[1]),
                Some(&module_id),
                "function",
                line + line_of(
                    body,
                    function_capture.get(0).expect("module function").start(),
                ) - 1,
            );
        }
        for task_capture in task.captures_iter(body) {
            let task_line =
                line + line_of(body, task_capture.get(0).expect("module task").start()) - 1;
            builder.labeled_definition(
                &task_capture[1],
                &task_capture[1],
                Some(&module_id),
                "task",
                task_line,
            );
        }
        for import_capture in import.captures_iter(body) {
            let import_line =
                line + line_of(body, import_capture.get(0).expect("module import").start()) - 1;
            let target = builder.external(&import_capture[1], import_line);
            builder.edge(
                &module_id,
                &target,
                "imports_from",
                Some("import"),
                import_line,
                Confidence::Extracted,
            );
        }
        for instantiation_capture in instantiation.captures_iter(body) {
            let name = &instantiation_capture[1];
            if matches!(name, "function" | "task" | "return" | "if" | "for") {
                continue;
            }
            let instantiation_line =
                line + line_of(
                    body,
                    instantiation_capture
                        .get(0)
                        .expect("module instantiation")
                        .start(),
                ) - 1;
            let target = builder.external(name, instantiation_line);
            builder.edge(
                &module_id,
                &target,
                "instantiates",
                None,
                instantiation_line,
                Confidence::Extracted,
            );
        }
    }
    Ok(builder.finish())
}

fn extract_groovy(path: &Path, text: &str, source_file: &str) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file);
    let imports = Regex::new(r"(?m)^\s*import\s+(?:static\s+)?([A-Za-z_][A-Za-z0-9_.*]*)")?;
    for capture in imports.captures_iter(text) {
        let line = line_of(text, capture.get(0).expect("Groovy import").start());
        let target = builder.external(capture[1].trim_end_matches(".*"), line);
        builder.edge(
            &builder.file_id.clone(),
            &target,
            "imports",
            Some("import"),
            line,
            Confidence::Extracted,
        );
    }

    let classes = Regex::new(
        r"(?m)^\s*(?:(?:public|protected|private|abstract|final|static)\s+)*(class|interface|trait|enum)\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s+extends\s+([A-Za-z_][A-Za-z0-9_.]*))?(?:\s+implements\s+([^\n{]+))?\s*\{",
    )?;
    let mut scopes = Vec::new();
    let mut rows = Vec::new();
    for capture in classes.captures_iter(text) {
        let matched = capture.get(0).expect("Groovy class");
        let line = line_of(text, matched.start());
        let id = builder.definition(&capture[2], None, "class", line);
        if &capture[1] == "interface" {
            builder.interfaces.insert(normalize_label(&capture[2]));
        }
        if let Some((start, end)) = brace_range(text, matched.start()) {
            scopes.push(Scope {
                id: id.clone(),
                start,
                end,
            });
        }
        rows.push((
            id,
            line,
            capture.get(3).map(|value| value.as_str().to_owned()),
            capture.get(4).map(|value| value.as_str().to_owned()),
        ));
    }
    for (id, line, parent, interfaces) in rows {
        if let Some(parent) = parent {
            let target = builder.external(&parent, line);
            builder.edge(&id, &target, "inherits", None, line, Confidence::Extracted);
        }
        if let Some(interfaces) = interfaces {
            for interface in interfaces.split(',').map(str::trim) {
                if interface.is_empty() {
                    continue;
                }
                let target = builder.external(interface, line);
                builder.edge(
                    &id,
                    &target,
                    "implements",
                    None,
                    line,
                    Confidence::Extracted,
                );
            }
        }
    }

    let mut function_scopes = Vec::new();
    let features = Regex::new(r#"(?m)^\s*def\s+(?:"([^"]+)"|'([^']+)')\s*\("#)?;
    for capture in features.captures_iter(text) {
        let matched = capture.get(0).expect("Spock feature");
        let Some(owner) = owner_at(&scopes, matched.start()).map(str::to_owned) else {
            continue;
        };
        let name = capture
            .get(1)
            .or_else(|| capture.get(2))
            .expect("Spock feature name")
            .as_str();
        let id = builder.labeled_definition(
            name,
            &format!("\"{name}\""),
            Some(&owner),
            "function",
            line_of(text, matched.start()),
        );
        if let Some((start, end)) = same_line_brace_range(text, matched.start()) {
            function_scopes.push(Scope { id, start, end });
        }
    }
    let methods = Regex::new(
        r"(?m)^\s*(?:(?:public|protected|private|abstract|final|static|synchronized)\s+)*(?:(?:def|[A-Za-z_][A-Za-z0-9_.<>\[\]]*)\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*\([^\n)]*\)\s*(?:\{|;)",
    )?;
    for capture in methods.captures_iter(text) {
        let matched = capture.get(0).expect("Groovy method");
        if owner_at(&function_scopes, matched.start()).is_some()
            || matches!(
                &capture[1],
                "if" | "for" | "while" | "switch" | "catch" | "return"
            )
        {
            continue;
        }
        let Some(owner) = owner_at(&scopes, matched.start()).map(str::to_owned) else {
            continue;
        };
        let id = builder.definition(
            &capture[1],
            Some(&owner),
            "function",
            line_of(text, matched.start()),
        );
        if let Some((start, end)) = same_line_brace_range(text, matched.start()) {
            function_scopes.push(Scope { id, start, end });
        }
    }
    emit_calls(
        &mut builder,
        text,
        &function_scopes,
        &["if", "for", "while", "switch", "return"],
        false,
    )?;
    Ok(builder.finish())
}

fn emit_swift_parameters(builder: &mut Builder<'_>, owner: &str, parameters: &str, line: usize) {
    for parameter in parameters.split(',') {
        if let Some((_, type_text)) = parameter.split_once(':') {
            builder.reference(owner, type_text, "parameter_type", line, "swift");
        }
    }
}

fn same_line_brace_range(text: &str, search_from: usize) -> Option<(usize, usize)> {
    let line_end = text[search_from..]
        .find('\n')
        .map(|offset| search_from + offset)
        .unwrap_or(text.len());
    text[search_from..line_end]
        .contains('{')
        .then(|| brace_range(text, search_from))
        .flatten()
}

fn callable_body_range(text: &str, search_from: usize) -> Option<(usize, usize)> {
    let line_end = text[search_from..]
        .find('\n')
        .map(|offset| search_from + offset)
        .unwrap_or(text.len());
    let remainder = &text[search_from..line_end];
    let first = remainder
        .char_indices()
        .find(|(_, character)| !character.is_whitespace())?;
    match first.1 {
        '{' => brace_range(text, search_from + first.0),
        '=' => {
            let after_equals = search_from + first.0 + first.1.len_utf8();
            let next = text[after_equals..line_end]
                .char_indices()
                .find(|(_, character)| !character.is_whitespace());
            if next.is_some_and(|(_, character)| character == '{') {
                brace_range(text, after_equals)
            } else {
                Some((
                    after_equals,
                    expression_body_end(text, search_from, line_end),
                ))
            }
        }
        _ => None,
    }
}

fn expression_body_end(text: &str, declaration_offset: usize, first_line_end: usize) -> usize {
    let declaration_line_start = text[..declaration_offset]
        .rfind('\n')
        .map_or(0, |offset| offset + 1);
    let declaration_indent = text[declaration_line_start..]
        .bytes()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    let mut line_start = first_line_end;
    if text.as_bytes().get(line_start) == Some(&b'\n') {
        line_start += 1;
    }
    while line_start < text.len() {
        let line_end = text[line_start..]
            .find('\n')
            .map(|offset| line_start + offset)
            .unwrap_or(text.len());
        let line = &text[line_start..line_end];
        if !line.trim().is_empty() {
            let indent = line
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            if indent <= declaration_indent {
                return line_start;
            }
        }
        line_start = line_end;
        if text.as_bytes().get(line_start) == Some(&b'\n') {
            line_start += 1;
        }
    }
    text.len()
}

fn emit_calls(
    builder: &mut Builder<'_>,
    text: &str,
    functions: &[Scope],
    keywords: &[&str],
    emit_unresolved: bool,
) -> anyhow::Result<()> {
    let calls = Regex::new(
        r"(?:(\$?[A-Za-z_][A-Za-z0-9_]*)\s*(->|::|\.)\s*)?([A-Za-z_][A-Za-z0-9_]*)\s*\(",
    )?;
    for function in functions {
        for capture in calls.captures_iter(&text[function.start..function.end]) {
            let absolute_offset = function.start + capture.get(0).expect("call match").start();
            if owner_at(functions, absolute_offset) != Some(function.id.as_str()) {
                // Do not attribute calls from a nested function or anonymous-
                // object member to every enclosing function range.
                continue;
            }
            let name = &capture[3];
            if keywords.contains(&name) || normalize_label(name) == "return" {
                continue;
            }
            let member_call = capture.get(2).is_some();
            let receiver = member_call
                .then(|| {
                    capture
                        .get(1)
                        .map(|value| value.as_str().trim_start_matches('$'))
                })
                .flatten();
            let receiver_scope = receiver.and_then(|receiver| match receiver {
                "this" | "self" => Some("current"),
                "super" | "base" => Some("super"),
                _ => None,
            });
            let receiver_owner = receiver_scope.and_then(|_| builder.method_owner(&function.id));
            // A name is only enough to resolve a direct call. Member syntax
            // carries a receiver contract; without a proven receiver type,
            // binding it to the only same-named method in the corpus is unsafe.
            let target = receiver_scope
                .and_then(|scope| builder.scoped_method(&function.id, name, scope))
                .or_else(|| {
                    (!member_call)
                        .then(|| builder.unique_definition(name))
                        .flatten()
                });
            if target.is_none() && !emit_unresolved && receiver_owner.is_none() {
                continue;
            }
            let unresolved = target.is_none();
            let target = target.unwrap_or_else(|| make_id(&["__graphoxide_call", name]));
            if target == function.id {
                continue;
            }
            let edge_start = builder.edges.len();
            builder.edge(
                &function.id,
                &target,
                "calls",
                Some("call"),
                line_of(text, absolute_offset),
                Confidence::Extracted,
            );
            if unresolved && builder.edges.len() > edge_start {
                builder.edges[edge_start]
                    .extra
                    .insert("unresolved_call".into(), true.into());
                builder.edges[edge_start]
                    .extra
                    .insert("callee".into(), name.into());
                builder.edges[edge_start]
                    .extra
                    .insert("member_call".into(), member_call.into());
                if let Some(receiver) = receiver {
                    builder.edges[edge_start]
                        .extra
                        .insert("receiver".into(), receiver.into());
                }
                if let Some(receiver_owner) = receiver_owner {
                    builder.edges[edge_start]
                        .extra
                        .insert("receiver_owner".into(), receiver_owner.into());
                }
                if let Some(receiver_scope) = receiver_scope {
                    builder.edges[edge_start]
                        .extra
                        .insert("receiver_scope".into(), receiver_scope.into());
                }
            }
        }
    }
    Ok(())
}

fn mask_php_comments_and_strings(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut masked = bytes.to_vec();
    let mut index = 0usize;
    while index < bytes.len() {
        if matches!(bytes[index], b'\'' | b'"') {
            let quote = bytes[index];
            masked[index] = b' ';
            index += 1;
            let mut escaped = false;
            while index < bytes.len() {
                let byte = bytes[index];
                if byte != b'\n' && byte != b'\r' {
                    masked[index] = b' ';
                }
                index += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == quote {
                    break;
                }
            }
            continue;
        }
        let line_comment = bytes[index..].starts_with(b"//")
            || (bytes[index] == b'#' && bytes.get(index + 1) != Some(&b'['));
        if line_comment {
            while index < bytes.len() && bytes[index] != b'\n' {
                masked[index] = b' ';
                index += 1;
            }
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            masked[index] = b' ';
            index += 1;
            if index < bytes.len() {
                masked[index] = b' ';
                index += 1;
            }
            while index < bytes.len() {
                if bytes[index..].starts_with(b"*/") {
                    masked[index] = b' ';
                    index += 1;
                    if index < bytes.len() {
                        masked[index] = b' ';
                        index += 1;
                    }
                    break;
                }
                if bytes[index] != b'\n' && bytes[index] != b'\r' {
                    masked[index] = b' ';
                }
                index += 1;
            }
            continue;
        }
        index += 1;
    }
    String::from_utf8(masked).unwrap_or_else(|_| text.to_owned())
}

fn emit_php_special_relations(
    builder: &mut Builder<'_>,
    text: &str,
    functions: &[Scope],
    namespace: &str,
    aliases: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let static_properties = Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)::\$[A-Za-z_][A-Za-z0-9_]*")?;
    let configs = Regex::new(r#"\bconfig\s*\(\s*["']([A-Za-z_][A-Za-z0-9_]*)[.]"#)?;
    for function in functions {
        let body = &text[function.start..function.end];
        for capture in static_properties.captures_iter(body) {
            let target = builder.external(
                &capture[1],
                line_of(text, function.start + capture.get(0).unwrap().start()),
            );
            builder.edge(
                &function.id,
                &target,
                "uses_static_prop",
                None,
                line_of(text, function.start + capture.get(0).unwrap().start()),
                Confidence::Extracted,
            );
        }
        for capture in configs.captures_iter(body) {
            let label = capitalize(&capture[1]);
            let target = builder.external(
                &label,
                line_of(text, function.start + capture.get(0).unwrap().start()),
            );
            builder.edge(
                &function.id,
                &target,
                "uses_config",
                None,
                line_of(text, function.start + capture.get(0).unwrap().start()),
                Confidence::Extracted,
            );
        }
    }
    let code = mask_php_comments_and_strings(text);
    let bindings = Regex::new(
        r"(?:bind|singleton|scoped|instance)\s*\(\s*([\\]?[A-Za-z_][A-Za-z0-9_\\]*)::class\s*,\s*([\\]?[A-Za-z_][A-Za-z0-9_\\]*)::class",
    )?;
    for capture in bindings.captures_iter(&code) {
        let line = line_of(&code, capture.get(0).unwrap().start());
        let source_fqn = php_qualified_name(&capture[1], namespace, aliases);
        let target_fqn = php_qualified_name(&capture[2], namespace, aliases);
        let source = php_target(builder, &source_fqn, line);
        let target = php_target(builder, &target_fqn, line);
        mark_php_node(builder, &source, &source_fqn);
        builder.edge(
            &source,
            &target,
            "bound_to",
            None,
            line,
            Confidence::Extracted,
        );
        if let Some(edge) = builder.edges.iter_mut().rev().find(|edge| {
            edge.true_source() == source
                && edge.true_target() == target
                && edge.relation == "bound_to"
        }) {
            edge.extra
                .insert(crate::php::PHP_SOURCE_FQN.into(), source_fqn.clone().into());
        }
        mark_php_target(builder, &source, &target, "bound_to", &target_fqn);
    }
    let listeners = Regex::new(r"(?ms)([\\]?[A-Za-z_][A-Za-z0-9_\\]*)::class\s*=>\s*\[(.*?)\]")?;
    let class_ref = Regex::new(r"([\\]?[A-Za-z_][A-Za-z0-9_\\]*)::class")?;
    for capture in listeners.captures_iter(&code) {
        let line = line_of(&code, capture.get(0).unwrap().start());
        let source_fqn = php_qualified_name(&capture[1], namespace, aliases);
        let source = php_target(builder, &source_fqn, line);
        mark_php_node(builder, &source, &source_fqn);
        for listener in class_ref.captures_iter(&capture[2]) {
            let target_fqn = php_qualified_name(&listener[1], namespace, aliases);
            let target = php_target(builder, &target_fqn, line);
            builder.edge(
                &source,
                &target,
                "listened_by",
                None,
                line,
                Confidence::Extracted,
            );
            if let Some(edge) = builder.edges.iter_mut().rev().find(|edge| {
                edge.true_source() == source
                    && edge.true_target() == target
                    && edge.relation == "listened_by"
            }) {
                edge.extra
                    .insert(crate::php::PHP_SOURCE_FQN.into(), source_fqn.clone().into());
            }
            mark_php_target(builder, &source, &target, "listened_by", &target_fqn);
        }
    }
    Ok(())
}

fn capitalize(value: &str) -> String {
    let mut characters = value.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod byte_tests {
    use super::*;
    use std::{collections::BTreeSet, fs};
    use tempfile::TempDir;

    fn imported_labels(extraction: &Extraction) -> BTreeSet<String> {
        let labels = extraction
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node.label.as_str()))
            .collect::<HashMap<_, _>>();
        extraction
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports")
            .map(|edge| {
                labels
                    .get(edge.true_target())
                    .copied()
                    .unwrap_or(edge.true_target())
                    .to_owned()
            })
            .collect()
    }

    fn dm_import_edges(extraction: &Extraction) -> Vec<&Edge> {
        extraction
            .edges
            .iter()
            .filter(|edge| {
                edge.extra.get("context").and_then(|value| value.as_str()) == Some("import")
            })
            .collect()
    }

    #[test]
    fn dm_byte_includes_use_contained_source_relative_identity_without_probing() {
        let fixture = TempDir::new().expect("create DM fixture");
        let source_path = fixture.path().join("src/nested/main.dm");
        fs::create_dir_all(source_path.parent().expect("DM source parent"))
            .expect("create DM source parent");
        fs::create_dir_all(fixture.path().join("src/shared"))
            .expect("create intended include parent");
        fs::write(&source_path, "ignored physical source").expect("write DM source");
        fs::write(
            fixture.path().join("src/shared/helpers.dm"),
            "/proc/Intended()\n",
        )
        .expect("write intended include");
        fs::write(
            fixture.path().join("src/nested/helpers.dm"),
            "/proc/Decoy()\n",
        )
        .expect("write same-basename decoy");

        let source = "#include \"../shared/helpers.dm\"\n/proc/RunTest()\n\treturn\n";
        let extraction = extract_dm(&source_path, source, "src/nested/main.dm", false)
            .expect("extract DM bytes");
        let missing_physical = extract_dm(
            Path::new("/graphoxide-byte-only/src/nested/main.dm"),
            source,
            "src/nested/main.dm",
            false,
        )
        .expect("extract DM bytes without a physical source");
        assert_eq!(
            serde_json::to_value(&extraction).expect("serialize DM extraction"),
            serde_json::to_value(&missing_physical).expect("serialize pathless DM extraction"),
        );

        let edge = dm_import_edges(&extraction)
            .into_iter()
            .next()
            .expect("contained DM include edge");
        let target = make_id(&["src/shared/helpers"]);
        assert_eq!(edge.relation, "imports");
        assert_eq!(edge.true_target(), target);
        assert_eq!(
            edge.extra
                .get("target_file")
                .and_then(|value| value.as_str()),
            Some("src/shared/helpers.dm"),
        );
        assert!(extraction
            .nodes
            .iter()
            .any(|node| { node.id == target && node.source_file == "src/shared/helpers.dm" }));
        assert_ne!(edge.true_target(), make_id(&["src/nested/helpers"]));
    }

    #[test]
    fn dm_include_paths_reject_nonportable_and_root_escaping_spellings() {
        let source = r#"#include "/absolute.dm"
#include "\\server\share\unc.dm"
#include "C:\drive.dm"
#include "C:drive-relative.dm"
#include "scheme:value.dm"
#include "CON.dm"
#include "dir//repeated.dm"
#include "dir\\repeated.dm"
#include "trailing-dot.dm."
#include "trailing-space.dm "
#include "../../outside.dm"
"#;
        let extraction = extract_dm(Path::new("src/main.dm"), source, "src/main.dm", false)
            .expect("extract unsafe DM includes");
        assert!(dm_import_edges(&extraction).is_empty());
        assert_eq!(
            extraction.nodes.len(),
            1,
            "unsafe includes must create no stubs"
        );
    }

    #[test]
    fn dm_exact_include_identity_does_not_bind_a_same_basename_decoy() {
        let mut extractions = vec![
            extract_dm(
                Path::new("src/nested/main.dm"),
                "#include \"../shared/helpers.dm\"\n",
                "src/nested/main.dm",
                false,
            )
            .expect("extract DM importer"),
            extract_dm(
                Path::new("vendor/helpers.dm"),
                "/proc/Decoy()\n\treturn\n",
                "vendor/helpers.dm",
                false,
            )
            .expect("extract same-basename decoy"),
        ];
        crate::resolution::resolve(&mut extractions);

        let edge = dm_import_edges(&extractions[0])
            .into_iter()
            .next()
            .expect("DM include edge after corpus resolution");
        assert_eq!(edge.true_target(), make_id(&["src/shared/helpers"]));
        assert_ne!(edge.true_target(), make_id(&["vendor/helpers"]));
        assert!(extractions[0].nodes.iter().any(|node| {
            node.id == edge.true_target() && node.source_file == "src/shared/helpers.dm"
        }));
    }

    #[test]
    fn dm_exact_include_prefers_the_real_file_regardless_of_extraction_order() {
        let mut extractions = vec![
            extract_dm(
                Path::new("src/shared/helpers.dm"),
                "/proc/Intended()\n\treturn\n",
                "src/shared/helpers.dm",
                false,
            )
            .expect("extract intended DM target first"),
            extract_dm(
                Path::new("src/nested/main.dm"),
                "#include \"../shared/helpers.dm\"\n",
                "src/nested/main.dm",
                false,
            )
            .expect("extract DM importer second"),
            extract_dm(
                Path::new("vendor/helpers.dm"),
                "/proc/Decoy()\n\treturn\n",
                "vendor/helpers.dm",
                false,
            )
            .expect("extract same-basename DM decoy"),
        ];
        crate::resolution::resolve(&mut extractions);

        let target = make_id(&["src/shared/helpers"]);
        let edge = extractions
            .iter()
            .flat_map(dm_import_edges)
            .find(|edge| edge.source_file == "src/nested/main.dm")
            .expect("resolved DM import edge");
        assert_eq!(edge.true_target(), target);
        let target_nodes = extractions
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .filter(|node| node.id == target)
            .collect::<Vec<_>>();
        assert_eq!(
            target_nodes.len(),
            1,
            "exact file retained a duplicate stub"
        );
        assert_eq!(target_nodes[0].source_file, "src/shared/helpers.dm");
        assert_eq!(
            target_nodes[0]
                .extra
                .get("type")
                .and_then(serde_json::Value::as_str),
            Some("file"),
        );
    }

    #[test]
    fn dm_legacy_path_resolution_is_portable_and_never_crosses_its_root() {
        let fixture = TempDir::new().expect("create DM legacy fixture");
        let source_path = fixture.path().join("src/nested/main.dm");
        let intended = fixture.path().join("src/shared/helpers.dm");
        let decoy = fixture.path().join("src/nested/helpers.dm");
        fs::create_dir_all(source_path.parent().expect("DM source parent"))
            .expect("create source parent");
        fs::create_dir_all(intended.parent().expect("intended parent"))
            .expect("create intended parent");
        fs::write(&source_path, "ignored physical source").expect("write source");
        fs::write(&intended, "/proc/Intended()\n").expect("write intended include");
        fs::write(&decoy, "/proc/Decoy()\n").expect("write decoy include");

        let extraction = extract_dm(
            &source_path,
            "#include \"../shared/helpers.dm\"\n",
            "src/nested/main.dm",
            true,
        )
        .expect("extract legacy DM path");
        let edge = dm_import_edges(&extraction)
            .into_iter()
            .next()
            .expect("resolved legacy DM include");
        assert_eq!(edge.relation, "imports_from");
        assert_eq!(edge.true_target(), make_id(&["src/shared/helpers"]));
        let intended_source = intended.to_string_lossy().replace('\\', "/");
        assert!(extraction
            .nodes
            .iter()
            .any(|node| { node.id == edge.true_target() && node.source_file == intended_source }));
        assert_ne!(
            edge.true_target(),
            make_id(&[&intended.to_string_lossy().replace('\\', "/")]),
            "legacy path probing must not put a machine-specific path in the target ID",
        );

        let outside = fixture.path().join("outside.dm");
        fs::write(&outside, "/proc/Outside()\n").expect("write physical outside file");
        let root_source = fixture.path().join("project/main.dm");
        fs::create_dir_all(root_source.parent().expect("root source parent"))
            .expect("create root source parent");
        fs::write(&root_source, "ignored physical source").expect("write root source");
        let escaping = extract_dm(
            &root_source,
            "#include \"../outside.dm\"\n",
            "main.dm",
            true,
        )
        .expect("extract escaping legacy DM include");
        assert!(dm_import_edges(&escaping).is_empty());
        assert_eq!(escaping.nodes.len(), 1);
    }

    #[test]
    fn dm_legacy_path_resolution_accepts_a_relative_physical_source() {
        let current = std::env::current_dir().expect("current test directory");
        let fixture = tempfile::Builder::new()
            .prefix("graphoxide-relative-dm-")
            .tempdir_in(&current)
            .expect("create relative DM fixture");
        let source = fixture.path().join("src/main.dm");
        let include = fixture.path().join("src/helpers.dm");
        fs::create_dir_all(source.parent().expect("relative source parent"))
            .expect("create relative source parent");
        fs::write(&source, "ignored physical source").expect("write relative DM source");
        fs::write(&include, "/proc/Helper()\n").expect("write relative DM include");
        let relative_source = source
            .strip_prefix(&current)
            .expect("fixture beneath current directory");
        let logical_source = relative_source.to_string_lossy().replace('\\', "/");

        let extraction = extract_dm(
            relative_source,
            "#include \"helpers.dm\"\n",
            &logical_source,
            true,
        )
        .expect("extract relative legacy DM path");
        let edge = dm_import_edges(&extraction)
            .into_iter()
            .next()
            .expect("relative DM include edge");
        assert_eq!(edge.relation, "imports_from");
        assert!(!edge.extra.contains_key("external"));
    }

    #[test]
    fn scala_imports_require_static_module_paths() {
        let extraction = extract_scala(
            Path::new("Consumer.scala"),
            r#"import scala.collection.mutable.ListBuffer
import foo.bar._
import foo.selectors.{Baz, Qux}
import foo.`odd-name`
import adapter()
import $module.Member
import unquote(adapter)
import getAdapter().Plugin
import provider().Thing
import foo.${bar}
import "dynamic.module"
class Consumer
"#,
            "Consumer.scala",
        )
        .expect("extract Scala static modules");

        assert_eq!(
            imported_labels(&extraction),
            BTreeSet::from([
                "foo.`odd-name`".into(),
                "foo.bar._".into(),
                "foo.selectors".into(),
                "scala.collection.mutable.ListBuffer".into(),
            ])
        );
    }

    #[test]
    fn elixir_imports_require_static_module_forms() {
        let extraction = extract_elixir(
            Path::new("consumer.ex"),
            r#"defmodule Consumer do
  use MyApp.Adapter, otp_app: :consumer
  alias MyApp.Schemas.{Account, Token}
  alias __MODULE__.Nested
  import Ecto.Query
  require :logger
  use @adapter
  use unquote(adapter)
  use adapter
  use Module.concat([MyApp, Adapter])
end
"#,
            "consumer.ex",
        )
        .expect("extract Elixir static modules");

        assert_eq!(
            imported_labels(&extraction),
            BTreeSet::from([
                ":logger".into(),
                "Consumer.Nested".into(),
                "Ecto.Query".into(),
                "MyApp.Adapter".into(),
                "MyApp.Schemas.Account".into(),
                "MyApp.Schemas.Token".into(),
            ])
        );
    }

    #[test]
    fn julia_imports_require_static_module_paths() {
        let extraction = extract_julia(
            Path::new("consumer.jl"),
            r#"module Consumer
using LinearAlgebra
import Base: show, display
using Base.Threads
using ..ParentModule
using Alpha, Beta.Gamma
using $mod
using $(module_name)
using getmodule()
using "dynamic.module"
end
"#,
            "consumer.jl",
        )
        .expect("extract Julia static modules");

        assert_eq!(
            imported_labels(&extraction),
            BTreeSet::from([
                "Alpha".into(),
                "Base".into(),
                "Base.Threads".into(),
                "Beta.Gamma".into(),
                "LinearAlgebra".into(),
                "ParentModule".into(),
            ])
        );
    }

    #[test]
    fn objective_c_bytes_keep_full_symbols_and_logical_imports_without_path_io() {
        let extraction = extract_objc_bytes(
            Path::new("/graphoxide-missing-project/native/LegacyWorker.m"),
            "#import \"LegacyWorker.h\"\n@implementation LegacyWorker\n- (id)process:(id)value { return value; }\n@end\n",
            "native/LegacyWorker.m",
        )
        .expect("extract admitted Objective-C bytes");

        assert!(extraction.nodes.iter().any(|node| {
            node.id == "native_legacyworker_legacyworker" && node.label == "LegacyWorker"
        }));
        assert!(extraction.nodes.iter().any(|node| {
            node.id == "native_legacyworker_legacyworker_process" && node.label == "-process"
        }));
        assert!(extraction.edges.iter().any(|edge| {
            edge.relation == "imports"
                && edge.true_target() == "native_legacyworker"
                && edge
                    .extra
                    .get("target_file")
                    .and_then(|value| value.as_str())
                    == Some("native/LegacyWorker.h")
        }));
    }

    #[test]
    fn objective_c_byte_imports_reject_absolute_and_parent_escaping_paths() {
        let extraction = extract_objc_bytes(
            Path::new("/graphoxide-missing-project/native/Worker.m"),
            "#import \"../../Escape.h\"\n#import \"/Absolute.h\"\n@implementation Worker\n@end\n",
            "native/Worker.m",
        )
        .expect("extract admitted Objective-C bytes");
        assert!(
            extraction
                .edges
                .iter()
                .all(|edge| edge.relation != "imports"),
            "unsafe quoted imports must not create logical project bindings"
        );
    }

    #[test]
    fn logical_project_import_allows_contained_parent_navigation_only() {
        assert_eq!(
            logical_project_import("native/impl/Worker.m", "../Worker.h").as_deref(),
            Some("native/Worker.h")
        );
        assert_eq!(
            logical_project_import("native/Worker.m", "../../Escape.h"),
            None
        );
        assert_eq!(
            logical_project_import("native/Worker.m", "/Absolute.h"),
            None
        );
        for absolute in [
            r"C:\Absolute.h",
            "C:/Absolute.h",
            "C:DriveRelative.h",
            r"\\server\share\Absolute.h",
            "//server/share/Absolute.h",
        ] {
            assert_eq!(logical_project_import("native/Worker.m", absolute), None);
        }
        assert_eq!(
            logical_project_import("native/impl/Worker.m", r"..\Worker.h").as_deref(),
            Some("native/Worker.h")
        );
    }

    #[test]
    fn powershell_imports_require_static_lexical_targets() {
        let extraction = extract_powershell(
            Path::new("/graphoxide-missing-project/consumer.ps1"),
            r#"using module MyModule
Import-Module -Name .\module.psm1
. ./script.ps1
Import-Module $Module
Import-Module "$Root/module.psm1"
Import-Module "${Module}"
Import-Module "$(Get-ModuleName)"
Import-Module "$Root$Name/module.psm1"
. $Root/script.ps1
. "${Root}/script.ps1"
using module $Module
"#,
            "consumer.ps1",
        )
        .expect("extract PowerShell import facts");
        let targets = extraction
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports_from")
            .map(|edge| edge.true_target())
            .collect::<BTreeSet<_>>();
        assert_eq!(targets, BTreeSet::from(["module", "mymodule", "script"]));
    }

    #[test]
    fn powershell_nested_path_import_binds_the_exact_same_family_file() {
        let fixture = TempDir::new().expect("PowerShell path fixture");
        let nested_dir = fixture.path().join("nested");
        fs::create_dir_all(&nested_dir).expect("create nested module directory");
        let consumer = fixture.path().join("consumer.ps1");
        let root_module = fixture.path().join("Tools.psm1");
        let nested_module = nested_dir.join("Tools.psm1");
        fs::write(&consumer, "Import-Module ./nested/Tools.psm1\n")
            .expect("write PowerShell consumer");
        fs::write(&root_module, "function Invoke-RootTool {}\n")
            .expect("write root PowerShell module");
        fs::write(&nested_module, "function Invoke-NestedTool {}\n")
            .expect("write nested PowerShell module");

        let result = crate::extract_files(
            &[consumer, root_module, nested_module],
            Some(fixture.path()),
            true,
        )
        .expect("extract PowerShell collision corpus");
        let file_id = |source_file: &str| {
            result
                .extractions
                .iter()
                .flat_map(|extraction| &extraction.nodes)
                .find(|node| {
                    node.source_file == source_file
                        && node.extra.get("type").and_then(|value| value.as_str()) == Some("file")
                })
                .map(|node| node.id.clone())
                .unwrap_or_else(|| panic!("missing file node for {source_file}"))
        };
        let root_id = file_id("Tools.psm1");
        let nested_id = file_id("nested/Tools.psm1");
        let import = result
            .extractions
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .find(|edge| edge.source_file == "consumer.ps1" && edge.relation == "imports_from")
            .expect("nested PowerShell path import");

        assert_eq!(import.true_target(), nested_id);
        assert_ne!(import.true_target(), root_id);
    }

    #[test]
    fn powershell_project_paths_omit_unsafe_forms_in_scripts_and_manifests() {
        let script = extract_powershell(
            Path::new("/graphoxide-missing-project/scripts/consumer.ps1"),
            r#"using namespace System.IO
using module MyModule
Import-Module Pester.Core
using module ../modules/Using.psm1
using assembly '../lib/Support.dll'
Import-Module -Name .\local\Import.psm1
. ../shared/Setup.ps1
using module /outside/Escape.psm1
using assembly 'C:\outside\Escape.dll'
Import-Module -Name '\\server\share\Escape.psm1'
Import-Module C:relative\Escape.psm1
Import-Module ~/Escape.psm1
Import-Module .
Import-Module .\directory\
using module ../modules/
. ../../Escape.ps1
. \rooted\Escape.ps1
. .
. ./shared/
"#,
            "scripts/consumer.ps1",
        )
        .expect("extract safe and unsafe PowerShell script imports");
        let script_targets = script
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports_from")
            .map(|edge| edge.true_target().to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            script_targets,
            BTreeSet::from([
                "lib_support".into(),
                "modules_using".into(),
                "mymodule".into(),
                "pester_core".into(),
                "scripts_local_import".into(),
                "shared_setup".into(),
                "system_io".into(),
            ])
        );
        let external_labels = script
            .nodes
            .iter()
            .filter(|node| node.source_file.is_empty())
            .map(|node| node.label.as_str())
            .collect::<BTreeSet<_>>();
        assert!(external_labels.contains("System.IO"));
        assert!(external_labels.contains("MyModule"));
        assert!(external_labels.contains("Pester.Core"));
        let script_target_files = script
            .edges
            .iter()
            .filter_map(|edge| edge.extra.get("target_file")?.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            script_target_files,
            BTreeSet::from([
                "lib/Support.dll",
                "modules/Using.psm1",
                "scripts/local/Import.psm1",
                "shared/Setup.ps1",
            ])
        );

        let manifest = extract_powershell_manifest(
            Path::new("/graphoxide-missing-project/manifests/Module.psd1"),
            r#"@{
    RootModule = '../modules/Root.psm1'
    NestedModules = @(
        '../nested/Nested.psm1',
        '../../Escape.psm1',
        '/outside/Escape.psm1',
        'C:\outside\Escape.psm1',
        '.',
        '../nested/'
    )
    RequiredModules = @(
        'PSReadLine',
        '\\server\share\Escape.psm1',
        'C:relative\Escape.psm1',
        '../'
    )
}
"#,
            "manifests/Module.psd1",
        )
        .expect("extract safe and unsafe PowerShell manifest imports");
        let manifest_targets = manifest
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports_from")
            .map(|edge| edge.true_target().to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            manifest_targets,
            BTreeSet::from([
                "modules_root".into(),
                "nested_nested".into(),
                "psreadline".into(),
            ])
        );

        let unsafe_root = extract_powershell_manifest(
            Path::new("/graphoxide-missing-project/manifests/Unsafe.psd1"),
            "@{\n    RootModule = '/outside/Escape.psm1'\n}\n",
            "manifests/Unsafe.psd1",
        )
        .expect("extract unsafe PowerShell RootModule");
        assert!(unsafe_root
            .edges
            .iter()
            .all(|edge| edge.relation != "imports_from"));

        let degenerate_root = extract_powershell_manifest(
            Path::new("/graphoxide-missing-project/manifests/Degenerate.psd1"),
            "@{\n    RootModule = '.'\n}\n",
            "manifests/Degenerate.psd1",
        )
        .expect("extract degenerate PowerShell RootModule");
        assert!(degenerate_root
            .edges
            .iter()
            .all(|edge| edge.relation != "imports_from"));
    }
}
