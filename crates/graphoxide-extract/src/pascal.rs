//! Deterministic Pascal/Delphi, Lazarus form, and Lazarus package extraction.
//!
//! Pascal is not part of the compiled tree-sitter set, so this extractor keeps
//! the structural subset deliberately small and syntax-aware: declarations,
//! class ownership, inheritance, imports, and intra-file calls. Form and
//! package files use their native declarative formats rather than the generic
//! text fallback.

use graphoxide_core::{make_id, normalize_id, Confidence, Edge, Extraction, Node};
use quick_xml::{events::Event, Reader};
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

const PASCAL_EXTENSIONS: &[&str] = &["pas", "pp", "dpr", "dpk", "lpr", "inc"];
const PROJECT_XML_MAX_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn supports_extension(extension: &str) -> bool {
    PASCAL_EXTENSIONS.contains(&extension) || matches!(extension, "dfm" | "lfm" | "lpk")
}

pub(crate) fn extract_pascal_family(
    path: &Path,
    source_file: &str,
    extension: &str,
) -> anyhow::Result<Extraction> {
    let bytes = fs::read(path)?;
    extract_pascal_family_with_path_probes(path, source_file, extension, &bytes, true)
}

/// Extract a Pascal-family document from already-read source bytes.
///
/// The path remains available only for stable identities. This entry point
/// performs no filesystem access, including sibling resolution.
pub(crate) fn extract_pascal_family_bytes(
    path: &Path,
    source_file: &str,
    extension: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    extract_pascal_family_with_path_probes(path, source_file, extension, bytes, false)
}

fn extract_pascal_family_with_path_probes(
    path: &Path,
    source_file: &str,
    extension: &str,
    bytes: &[u8],
    allow_path_probes: bool,
) -> anyhow::Result<Extraction> {
    match extension {
        "dfm" => extract_form(path, source_file, bytes, true),
        "lfm" => extract_form(path, source_file, bytes, false),
        "lpk" => extract_lazarus_package(path, source_file, bytes, allow_path_probes),
        extension if PASCAL_EXTENSIONS.contains(&extension) => {
            extract_pascal_source(path, source_file, bytes, allow_path_probes)
        }
        _ => Ok(Extraction::default()),
    }
}

struct Builder<'a> {
    source_file: &'a str,
    stem: String,
    file_id: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    seen_nodes: HashSet<String>,
    seen_edges: HashSet<(String, String, String)>,
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
        };
        builder.node(
            file_id,
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(source_file),
            1,
            "file",
        );
        builder
    }

    fn node(&mut self, id: String, label: &str, line: usize, kind: &str) {
        if id.is_empty() || !self.seen_nodes.insert(id.clone()) {
            return;
        }
        self.nodes.push(Node {
            id,
            label: label.to_owned(),
            file_type: "code".into(),
            source_file: self.source_file.into(),
            source_location: Some(format!("L{line}")),
            community: None,
            extra: BTreeMap::from([
                ("_origin".into(), "fallback".into()),
                ("type".into(), kind.into()),
                (
                    "metadata".into(),
                    serde_json::json!({"language": "pascal", "kind": kind}),
                ),
            ]),
        });
    }

    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        line: usize,
        context: Option<&str>,
    ) {
        let key = (source.to_owned(), target.to_owned(), relation.to_owned());
        if source.is_empty() || target.is_empty() || !self.seen_edges.insert(key) {
            return;
        }
        let mut extra = BTreeMap::from([
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), 1.0.into()),
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
        ]);
        if let Some(context) = context {
            extra.insert("context".into(), context.into());
        }
        self.edges.push(Edge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            confidence: Confidence::Extracted,
            source_file: self.source_file.into(),
            extra,
        });
    }

    fn unresolved_call(&mut self, source: &str, callee: &str, line: usize) {
        let target = make_id(&[self.source_file, "__pascal_unresolved_call", source, callee]);
        let relation = "__pascal_raw_call";
        let key = (source.to_owned(), target.clone(), relation.to_owned());
        if source.is_empty() || callee.is_empty() || !self.seen_edges.insert(key) {
            return;
        }
        self.edges.push(Edge {
            source: source.into(),
            target: target.clone(),
            relation: relation.into(),
            confidence: Confidence::Extracted,
            source_file: self.source_file.into(),
            extra: BTreeMap::from([
                ("source_location".into(), format!("L{line}").into()),
                ("weight".into(), 1.0.into()),
                ("context".into(), "call".into()),
                ("callee".into(), callee.into()),
                ("_src".into(), source.into()),
                ("_tgt".into(), target.into()),
            ]),
        });
    }

    fn finish(self) -> Extraction {
        Extraction {
            nodes: self.nodes,
            edges: self.edges,
            hyperedges: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct TypeDeclaration {
    name: String,
    kind: String,
    bases: Vec<String>,
    body: String,
    body_offset: usize,
    line: usize,
}

#[derive(Debug)]
struct Procedure {
    id: String,
    owner: String,
    name: String,
    body: String,
    body_line: usize,
}

fn extract_pascal_source(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    allow_path_probes: bool,
) -> anyhow::Result<Extraction> {
    let raw = String::from_utf8_lossy(bytes).into_owned();
    let text = strip_pascal_comments(&raw);
    let mut builder = Builder::new(path, source_file);
    let mut module_id = builder.file_id.clone();

    let module_re =
        Regex::new(r"(?i)\b(?:unit|program|library|package)\s+([A-Za-z_][A-Za-z0-9_.]*)\s*;")?;
    if let Some(capture) = module_re.captures(&text) {
        let whole = capture.get(0).expect("module capture");
        let name = capture.get(1).expect("module name").as_str();
        module_id = make_id(&[&builder.stem, name]);
        let line = line_of(&text, whole.start());
        builder.node(module_id.clone(), name, line, "module");
        builder.edge(&builder.file_id.clone(), &module_id, "contains", line, None);
    }

    let uses_re = Regex::new(r"(?is)\buses\b\s*([^;]+);")?;
    for capture in uses_re.captures_iter(&text) {
        let whole = capture.get(0).expect("uses capture");
        let line = line_of(&text, whole.start());
        for unit in split_uses(capture.get(1).expect("uses list").as_str()) {
            let target = if allow_path_probes {
                resolve_pascal_unit(path, source_file, &unit)
            } else {
                make_id(&[&unit])
            };
            builder.edge(&module_id, &target, "imports", line, Some("import"));
            if !allow_path_probes && let Some(edge) = builder.edges.last_mut() {
                edge.extra.insert("pascal_unit".into(), unit.into());
            }
        }
    }

    let (type_scope, type_offset) = declaration_scope(&text);
    let type_re = Regex::new(
        r"(?is)\b([A-Za-z_][A-Za-z0-9_]*)(?:\s*<[^>]+>)?\s*=\s*(?:packed\s+)?(class|interface)\b(?:\s*\(\s*([^)]*)\s*\))?(.*?)\bend\s*;",
    )?;
    let mut declarations = Vec::new();
    for capture in type_re.captures_iter(type_scope) {
        let whole = capture.get(0).expect("type capture");
        let body = capture.get(4).expect("type body");
        declarations.push(TypeDeclaration {
            name: capture.get(1).expect("type name").as_str().to_owned(),
            kind: capture
                .get(2)
                .expect("type kind")
                .as_str()
                .to_ascii_lowercase(),
            bases: capture
                .get(3)
                .map(|value| split_bases(value.as_str()))
                .unwrap_or_default(),
            body: body.as_str().to_owned(),
            body_offset: type_offset + body.start(),
            line: line_of(&text, type_offset + whole.start()),
        });
    }

    let mut type_ids = HashMap::new();
    for declaration in &declarations {
        let id = make_id(&[&builder.stem, &declaration.name]);
        builder.node(
            id.clone(),
            &declaration.name,
            declaration.line,
            &declaration.kind,
        );
        builder.edge(&module_id, &id, "contains", declaration.line, None);
        type_ids.insert(declaration.name.to_ascii_lowercase(), id);
    }

    let method_re = Regex::new(
        r"(?i)\b(?:procedure|function|constructor|destructor)\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s*\([^)]*\))?(?:\s*:\s*[A-Za-z_][A-Za-z0-9_<>,.\s]*)?\s*;",
    )?;
    for declaration in &declarations {
        let owner = type_ids
            .get(&declaration.name.to_ascii_lowercase())
            .expect("declared type ID")
            .clone();
        for base in &declaration.bases {
            let base_id = if let Some(id) = type_ids.get(&base.to_ascii_lowercase()) {
                id.clone()
            } else if let Some(id) = allow_path_probes
                .then(|| resolve_pascal_class(path, source_file, base))
                .flatten()
            {
                id
            } else {
                let id = make_id(&[base]);
                builder.node(id.clone(), base, declaration.line, "class");
                if !allow_path_probes && let Some(node) = builder.nodes.last_mut() {
                    node.extra
                        .insert("pascal_unresolved_base".into(), base.clone().into());
                }
                id
            };
            builder.edge(&owner, &base_id, "inherits", declaration.line, None);
        }
        for capture in method_re.captures_iter(&declaration.body) {
            let whole = capture.get(0).expect("method capture");
            let name = capture.get(1).expect("method name").as_str();
            let line = line_of(&text, declaration.body_offset + whole.start());
            let method_id = make_id(&[&owner, name]);
            builder.node(method_id.clone(), &format!("{name}()"), line, "function");
            builder.edge(&owner, &method_id, "method", line, None);
        }
    }

    let (implementation, implementation_offset) = implementation_scope(&text);
    let procedure_re = Regex::new(
        r"(?im)\b(?:procedure|function|constructor|destructor)\s+([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)?)(?:\s*<[^>]+>)?(?:\s*\([^)]*\))?(?:\s*:\s*[A-Za-z_][A-Za-z0-9_<>,.\s]*)?\s*;",
    )?;
    let mut procedures = Vec::new();
    for capture in procedure_re.captures_iter(implementation) {
        let whole = capture.get(0).expect("procedure capture");
        let qualified = capture.get(1).expect("procedure name").as_str();
        let absolute_start = implementation_offset + whole.start();
        let (owner, relation, label, short_name) =
            if let Some((class, method)) = qualified.split_once('.') {
                let owner = type_ids
                    .get(&class.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_else(|| module_id.clone());
                let relation = if owner == module_id {
                    "contains"
                } else {
                    "method"
                };
                (owner, relation, format!("{method}()"), method)
            } else {
                (
                    module_id.clone(),
                    "contains",
                    format!("{qualified}()"),
                    qualified,
                )
            };
        let id = make_id(&[&builder.stem, qualified]);
        let line = line_of(&text, absolute_start);
        builder.node(id.clone(), &label, line, "function");
        builder.edge(&owner, &id, relation, line, None);
        let search_start = implementation_offset + whole.end();
        let (body_start, body_end) = find_pascal_body(&text, search_start);
        procedures.push(Procedure {
            id,
            owner,
            name: short_name.to_ascii_lowercase(),
            body: text.get(body_start..body_end).unwrap_or("").to_owned(),
            body_line: line_of(&text, body_start),
        });
    }

    emit_pascal_calls(&mut builder, &procedures);
    Ok(builder.finish())
}

fn emit_pascal_calls(builder: &mut Builder<'_>, procedures: &[Procedure]) {
    let mut owned: HashMap<&str, HashMap<&str, Vec<&str>>> = HashMap::new();
    let mut global: HashMap<&str, Vec<&str>> = HashMap::new();
    for procedure in procedures {
        owned
            .entry(&procedure.owner)
            .or_default()
            .entry(&procedure.name)
            .or_default()
            .push(&procedure.id);
        global
            .entry(&procedure.name)
            .or_default()
            .push(&procedure.id);
    }
    let mut bases: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &builder.edges {
        if edge.relation == "inherits" {
            bases
                .entry(edge.true_source())
                .or_default()
                .push(edge.true_target());
        }
    }
    let calls_re =
        Regex::new(r"(?i)\b([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*)\s*[\(;]")
            .expect("valid Pascal call regex");
    let mut pending = Vec::new();
    for procedure in procedures {
        for capture in calls_re.captures_iter(&procedure.body) {
            let whole = capture.get(0).expect("call capture");
            let raw = capture.get(1).expect("callee capture").as_str();
            let name = raw.rsplit('.').next().unwrap_or(raw).to_ascii_lowercase();
            if is_pascal_keyword(&name) {
                continue;
            }
            let target = resolve_pascal_callee(&procedure.owner, &name, &owned, &global, &bases);
            let line = procedure.body_line + line_of(&procedure.body, whole.start()) - 1;
            match target.filter(|target| *target != procedure.id) {
                Some(target) => {
                    pending.push((procedure.id.clone(), Some(target.to_owned()), name, line))
                }
                None if target.is_none() => {
                    pending.push((procedure.id.clone(), None, name, line));
                }
                None => {}
            }
        }
    }
    for (source, target, callee, line) in pending {
        if let Some(target) = target {
            builder.edge(&source, &target, "calls", line, Some("call"));
        } else {
            builder.unresolved_call(&source, &callee, line);
        }
    }
}

/// Bind byte-extracted Pascal unit and base-class facts against the complete
/// set of independently admitted project extractions.
///
/// The per-file parser emits lexical unit/base identities because it cannot
/// inspect sibling paths. This pass uses only normalized `source_file`
/// metadata and already-extracted nodes; it performs no filesystem I/O.
pub(crate) fn resolve_project_symbols(extractions: &mut [Extraction]) {
    let mut files = BTreeMap::<(String, String), BTreeSet<(String, String)>>::new();
    let mut types = BTreeMap::<(String, String), BTreeSet<String>>::new();

    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if !is_pascal_source(&node.source_file) {
                continue;
            }
            if node.extra.get("type").and_then(|value| value.as_str()) == Some("file") {
                let path = Path::new(&node.source_file);
                let directory = path
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .to_string_lossy()
                    .replace('\\', "/")
                    .to_ascii_lowercase();
                let stem = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !stem.is_empty() {
                    files
                        .entry((directory, stem))
                        .or_default()
                        .insert((node.id.clone(), node.source_file.clone()));
                }
            }
            if matches!(
                node.extra.get("type").and_then(|value| value.as_str()),
                Some("class" | "interface")
            ) && !node.extra.contains_key("pascal_unresolved_base")
            {
                types
                    .entry((node.source_file.clone(), normalize_id(&node.label)))
                    .or_default()
                    .insert(node.id.clone());
            }
        }
    }

    let mut imported_sources = vec![BTreeSet::<String>::new(); extractions.len()];
    for (index, extraction) in extractions.iter_mut().enumerate() {
        for edge in &mut extraction.edges {
            let Some(unit) = edge
                .extra
                .remove("pascal_unit")
                .and_then(|value| value.as_str().map(str::to_owned))
            else {
                continue;
            };
            let directory = Path::new(&edge.source_file)
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase();
            let unit = unit.to_ascii_lowercase();
            let Some(bindings) = files.get(&(directory, unit)) else {
                continue;
            };
            let Some((target, source_file)) = (bindings.len() == 1)
                .then(|| bindings.iter().next().expect("one Pascal unit binding"))
            else {
                continue;
            };
            edge.target = target.clone();
            edge.extra.insert("_tgt".into(), target.clone().into());
            imported_sources[index].insert(source_file.clone());
        }
    }

    for (index, extraction) in extractions.iter_mut().enumerate() {
        let mut remap = BTreeMap::new();
        for node in &extraction.nodes {
            let Some(base) = node
                .extra
                .get("pascal_unresolved_base")
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            let candidates = imported_sources[index]
                .iter()
                .filter_map(|source| types.get(&(source.clone(), normalize_id(base))))
                .flatten()
                .cloned()
                .collect::<BTreeSet<_>>();
            if candidates.len() == 1 {
                remap.insert(
                    node.id.clone(),
                    candidates
                        .into_iter()
                        .next()
                        .expect("one Pascal base binding"),
                );
            }
        }
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
        extraction
            .nodes
            .retain(|node| !remap.contains_key(&node.id));
        for node in &mut extraction.nodes {
            node.extra.remove("pascal_unresolved_base");
        }
    }
}

/// Consume private per-file Pascal call facts once the complete corpus is
/// available, resolving only through explicit inheritance topology.
pub(crate) fn resolve_inherited_calls(extractions: &mut [Extraction]) {
    let mut labels = BTreeMap::new();
    let mut bases = BTreeMap::<String, Vec<String>>::new();
    let mut owner_of = BTreeMap::<String, String>::new();
    let mut methods = BTreeMap::<String, BTreeMap<String, BTreeSet<String>>>::new();
    let mut existing_calls = BTreeSet::new();

    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            labels.insert(node.id.clone(), node.label.clone());
        }
    }
    for extraction in extractions.iter() {
        for edge in &extraction.edges {
            match edge.relation.as_str() {
                "inherits" if is_pascal_source(&edge.source_file) => {
                    bases
                        .entry(edge.true_source().to_owned())
                        .or_default()
                        .push(edge.true_target().to_owned());
                }
                "method" if is_pascal_source(&edge.source_file) => {
                    let owner = edge.true_source().to_owned();
                    let method = edge.true_target().to_owned();
                    owner_of.insert(method.clone(), owner.clone());
                    if let Some(label) = labels.get(&method) {
                        let name =
                            normalize_id(label.trim_start_matches('.').trim_end_matches("()"));
                        if !name.is_empty() {
                            methods
                                .entry(owner)
                                .or_default()
                                .entry(name)
                                .or_default()
                                .insert(method);
                        }
                    }
                }
                "calls" => {
                    existing_calls
                        .insert((edge.true_source().to_owned(), edge.true_target().to_owned()));
                }
                _ => {}
            }
        }
    }
    for values in bases.values_mut() {
        values.sort();
        values.dedup();
    }

    let mut raw_calls = Vec::new();
    for (index, extraction) in extractions.iter_mut().enumerate() {
        for edge in &extraction.edges {
            if edge.relation == "__pascal_raw_call" && is_pascal_source(&edge.source_file) {
                raw_calls.push((index, edge.clone()));
            }
        }
        extraction
            .edges
            .retain(|edge| edge.relation != "__pascal_raw_call");
    }

    for (index, raw) in raw_calls {
        let caller = raw.true_source();
        let Some(owner) = owner_of.get(caller) else {
            continue;
        };
        let callee = raw
            .extra
            .get("callee")
            .and_then(|value| value.as_str())
            .map(normalize_id)
            .unwrap_or_default();
        if callee.is_empty() {
            continue;
        }
        let Some(target) = resolve_ancestor_method(owner, &callee, &bases, &methods) else {
            continue;
        };
        if caller == target || !existing_calls.insert((caller.to_owned(), target.clone())) {
            continue;
        }
        let mut extra = raw.extra.clone();
        extra.remove("callee");
        extra.insert("_src".into(), caller.into());
        extra.insert("_tgt".into(), target.clone().into());
        extra.insert("confidence_score".into(), 1.0.into());
        extra.insert(
            "metadata".into(),
            serde_json::json!({"resolver": "pascal_inherited_calls"}),
        );
        crate::resolution::push_resolved_edge(
            &mut extractions[index].edges,
            Edge {
                source: caller.into(),
                target,
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                source_file: raw.source_file,
                extra,
            },
        );
    }
}

fn resolve_ancestor_method(
    owner: &str,
    callee: &str,
    bases: &BTreeMap<String, Vec<String>>,
    methods: &BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
) -> Option<String> {
    let mut seen = BTreeSet::new();
    let mut frontier = bases.get(owner).cloned().unwrap_or_default();
    while !frontier.is_empty() {
        let mut matches = BTreeSet::new();
        let mut next = Vec::new();
        for base in frontier {
            if !seen.insert(base.clone()) {
                continue;
            }
            if let Some(candidates) = methods
                .get(&base)
                .and_then(|class_methods| class_methods.get(callee))
            {
                matches.extend(candidates.iter().cloned());
            }
            next.extend(bases.get(&base).into_iter().flatten().cloned());
        }
        match matches.len() {
            0 => {}
            1 => return matches.into_iter().next(),
            _ => return None,
        }
        next.sort();
        next.dedup();
        frontier = next;
    }
    None
}

fn is_pascal_source(source_file: &str) -> bool {
    Path::new(source_file)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            PASCAL_EXTENSIONS
                .iter()
                .any(|pascal| extension.eq_ignore_ascii_case(pascal))
        })
}

fn resolve_pascal_callee<'a>(
    owner: &str,
    name: &str,
    owned: &'a HashMap<&str, HashMap<&str, Vec<&str>>>,
    global: &'a HashMap<&str, Vec<&str>>,
    bases: &HashMap<&str, Vec<&str>>,
) -> Option<&'a str> {
    if let Some(candidates) = owned.get(owner).and_then(|methods| methods.get(name)) {
        return (candidates.len() == 1).then_some(candidates[0]);
    }
    let mut seen = HashSet::new();
    let mut queue: VecDeque<&str> = bases.get(owner).into_iter().flatten().copied().collect();
    while let Some(base) = queue.pop_front() {
        if !seen.insert(base) {
            continue;
        }
        if let Some(candidates) = owned.get(base).and_then(|methods| methods.get(name)) {
            return (candidates.len() == 1).then_some(candidates[0]);
        }
        queue.extend(bases.get(base).into_iter().flatten().copied());
    }
    global
        .get(name)
        .filter(|candidates| candidates.len() == 1)
        .map(|candidates| candidates[0])
}

fn strip_pascal_comments(text: &str) -> String {
    let token_re = Regex::new(r"(?s)'(?:''|[^'])*'|\{[^}]*\}|\(\*.*?\*\)|//[^\n]*")
        .expect("valid Pascal comment regex");
    token_re
        .replace_all(text, |capture: &regex::Captures<'_>| {
            let token = capture.get(0).expect("token capture").as_str();
            if token.starts_with('\'') {
                token.to_owned()
            } else {
                token
                    .chars()
                    .map(|character| if character == '\n' { '\n' } else { ' ' })
                    .collect()
            }
        })
        .into_owned()
}

fn declaration_scope(text: &str) -> (&str, usize) {
    let interface_re = Regex::new(r"(?im)^\s*interface\s*$").expect("valid interface regex");
    let implementation_re =
        Regex::new(r"(?im)^\s*implementation\s*$").expect("valid implementation regex");
    match (interface_re.find(text), implementation_re.find(text)) {
        (Some(interface), Some(implementation)) if interface.end() <= implementation.start() => (
            &text[interface.end()..implementation.start()],
            interface.end(),
        ),
        _ => (text, 0),
    }
}

fn implementation_scope(text: &str) -> (&str, usize) {
    let implementation_re =
        Regex::new(r"(?im)^\s*implementation\s*$").expect("valid implementation regex");
    if let Some(implementation) = implementation_re.find(text) {
        let start = implementation.end();
        let tail = &text[start..];
        let terminal_re = Regex::new(r"(?im)^\s*(?:initialization|finalization)\b")
            .expect("valid terminal-section regex");
        let end = terminal_re
            .find(tail)
            .map_or(text.len(), |terminal| start + terminal.start());
        (&text[start..end], start)
    } else {
        (text, 0)
    }
}

fn split_uses(raw: &str) -> Vec<String> {
    let in_re = Regex::new(r"(?i)\s+in\s+").expect("valid uses-in regex");
    let name_re = Regex::new(r"^[A-Za-z_][A-Za-z0-9_.]*$").expect("valid Pascal unit-name regex");
    raw.split(',')
        .filter_map(|chunk| {
            let name = in_re
                .split(chunk.trim())
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(';');
            name_re.is_match(name).then(|| name.to_owned())
        })
        .collect()
}

fn split_bases(raw: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut depth = 0_u32;
    for character in raw.chars() {
        match character {
            '<' => {
                depth += 1;
                current.push(character);
            }
            '>' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => push_base(&mut result, &mut current),
            _ => current.push(character),
        }
    }
    push_base(&mut result, &mut current);
    result
}

fn push_base(result: &mut Vec<String>, current: &mut String) {
    let base = current.split('<').next().unwrap_or("").trim().to_owned();
    current.clear();
    if Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$")
        .expect("valid Pascal base-name regex")
        .is_match(&base)
    {
        result.push(base);
    }
}

fn find_pascal_body(text: &str, start: usize) -> (usize, usize) {
    let begin_re = Regex::new(r"(?i)\bbegin\b").expect("valid begin regex");
    let Some(begin) = begin_re.find_at(text, start) else {
        return (start, start);
    };
    let token_re =
        Regex::new(r"(?i)\b(begin|end|case|try|asm|record)\b").expect("valid block-token regex");
    let body_start = begin.end();
    let mut depth = 1_u32;
    for capture in token_re.captures_iter(&text[body_start..]) {
        let whole = capture.get(0).expect("block-token capture");
        match capture
            .get(1)
            .expect("block-token value")
            .as_str()
            .to_ascii_lowercase()
            .as_str()
        {
            "end" => {
                depth -= 1;
                if depth == 0 {
                    return (body_start, body_start + whole.start());
                }
            }
            _ => depth += 1,
        }
    }
    (body_start, text.len())
}

fn resolve_pascal_unit(path: &Path, source_file: &str, unit: &str) -> String {
    find_pascal_file(path, unit).map_or_else(
        || make_id(&[unit]),
        |found| logical_pascal_stem(path, source_file, &found, None),
    )
}

fn resolve_pascal_class(path: &Path, source_file: &str, class_name: &str) -> Option<String> {
    let unit = if class_name.starts_with(['T', 'I']) {
        &class_name[1..]
    } else {
        class_name
    };
    find_pascal_file(path, unit)
        .map(|found| logical_pascal_stem(path, source_file, &found, Some(class_name)))
}

fn find_pascal_file(from_path: &Path, unit: &str) -> Option<PathBuf> {
    let directory = from_path.parent()?;
    let wanted = unit.to_ascii_lowercase();
    fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|candidate| {
            candidate
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    PASCAL_EXTENSIONS
                        .iter()
                        .any(|valid| extension.eq_ignore_ascii_case(valid))
                })
                && candidate
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.to_ascii_lowercase() == wanted)
        })
}

fn logical_pascal_stem(
    physical_source: &Path,
    logical_source: &str,
    found: &Path,
    symbol: Option<&str>,
) -> String {
    let logical_parent = Path::new(logical_source)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let relative = physical_source
        .parent()
        .and_then(|parent| found.strip_prefix(parent).ok())
        .unwrap_or(found);
    let logical = logical_parent.join(relative).with_extension("");
    let logical = logical.to_string_lossy().replace('\\', "/");
    symbol.map_or_else(
        || make_id(&[&logical]),
        |symbol| make_id(&[&logical, symbol]),
    )
}

fn extract_form(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    delphi: bool,
) -> anyhow::Result<Extraction> {
    if delphi && bytes.starts_with(&[0xff, 0x0a]) {
        return Ok(Extraction::default());
    }
    let text = String::from_utf8_lossy(bytes);
    let mut builder = Builder::new(path, source_file);
    let object_re = Regex::new(
        r"(?i)^\s*(?:object|inherited|inline)\s+[A-Za-z_][A-Za-z0-9_]*\s*:\s*([A-Za-z_][A-Za-z0-9_]*)",
    )?;
    let event_re = Regex::new(r"(?i)^\s*On[A-Za-z_][A-Za-z0-9_]*\s*=\s*([A-Za-z_][A-Za-z0-9_]*)")?;
    let end_re = Regex::new(r"(?i)^\s*end\s*$")?;
    let mut stack = vec![builder.file_id.clone()];
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if let Some(capture) = object_re.captures(line) {
            let class_name = capture.get(1).expect("form class").as_str();
            let id = make_id(&[&builder.stem, class_name]);
            builder.node(id.clone(), class_name, line_number, "class");
            builder.edge(
                stack.last().expect("form stack"),
                &id,
                "contains",
                line_number,
                None,
            );
            stack.push(id);
        } else if let Some(capture) = event_re.captures(line) {
            if stack.len() == 1 {
                continue;
            }
            let handler = capture.get(1).expect("event handler").as_str();
            let id = make_id(&[&builder.stem, handler]);
            builder.node(id.clone(), &format!("{handler}()"), line_number, "function");
            builder.edge(
                stack.last().expect("form stack"),
                &id,
                "references",
                line_number,
                Some("event"),
            );
        } else if end_re.is_match(line) && stack.len() > 1 {
            stack.pop();
        }
    }
    Ok(builder.finish())
}

fn extract_lazarus_package(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    allow_path_probes: bool,
) -> anyhow::Result<Extraction> {
    anyhow::ensure!(
        bytes.len() <= PROJECT_XML_MAX_BYTES,
        "package XML is larger than {PROJECT_XML_MAX_BYTES} bytes"
    );
    let lower = bytes.to_ascii_lowercase();
    anyhow::ensure!(
        !lower.windows(9).any(|window| window == b"<!doctype")
            && !lower.windows(8).any(|window| window == b"<!entity"),
        "refusing XML with DOCTYPE/ENTITY declaration"
    );
    let mut package_name = None;
    let mut dependencies = Vec::new();
    let mut units = Vec::new();
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event()? {
            Event::Start(event) | Event::Empty(event) => {
                let local_name = xml_local_name(event.name().as_ref()).to_vec();
                let value = xml_value(&event)?;
                match local_name.as_slice() {
                    b"Name" if package_name.is_none() => package_name = value,
                    b"PackageName" => dependencies.extend(value),
                    b"UnitName" => units.extend(value),
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    let mut builder = Builder::new(path, source_file);
    let package_name = package_name.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("package")
            .to_owned()
    });
    let package_id = make_id(&[&builder.stem, &package_name]);
    builder.node(package_id.clone(), &package_name, 1, "package");
    builder.edge(&builder.file_id.clone(), &package_id, "contains", 1, None);
    for dependency in dependencies {
        let id = make_id(&[&dependency]);
        builder.node(id.clone(), &dependency, 1, "package");
        builder.edge(&package_id, &id, "imports", 1, Some("import"));
    }
    for unit in units {
        let id = if allow_path_probes {
            resolve_pascal_unit(path, source_file, &unit)
        } else {
            make_id(&[&unit])
        };
        builder.node(id.clone(), &unit, 1, "module");
        builder.edge(&package_id, &id, "contains", 1, None);
    }
    Ok(builder.finish())
}

fn xml_local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| matches!(byte, b':' | b'}'))
        .next()
        .unwrap_or(name)
}

fn xml_value(event: &quick_xml::events::BytesStart<'_>) -> anyhow::Result<Option<String>> {
    for attribute in event.attributes() {
        let attribute = attribute?;
        if xml_local_name(attribute.key.as_ref()).eq_ignore_ascii_case(b"Value") {
            return Ok(Some(
                crate::decode_xml_attribute(attribute.value.as_ref())?.into_owned(),
            ));
        }
    }
    Ok(None)
}

fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn is_pascal_keyword(name: &str) -> bool {
    matches!(
        name,
        "begin"
            | "end"
            | "if"
            | "then"
            | "else"
            | "while"
            | "do"
            | "for"
            | "to"
            | "downto"
            | "repeat"
            | "until"
            | "case"
            | "of"
            | "try"
            | "finally"
            | "except"
            | "with"
            | "inherited"
            | "result"
            | "var"
            | "const"
            | "type"
            | "nil"
            | "true"
            | "false"
            | "exit"
            | "break"
            | "continue"
            | "uses"
            | "unit"
            | "program"
            | "library"
            | "interface"
            | "implementation"
            | "initialization"
            | "finalization"
            | "procedure"
            | "function"
            | "constructor"
            | "destructor"
            | "class"
            | "record"
            | "object"
            | "array"
            | "string"
            | "integer"
            | "boolean"
            | "real"
            | "char"
            | "writeln"
            | "write"
            | "readln"
            | "read"
            | "assigned"
            | "length"
            | "high"
            | "low"
            | "inc"
            | "dec"
            | "new"
            | "dispose"
            | "setlength"
            | "copy"
            | "pos"
            | "trim"
            | "format"
            | "inttostr"
            | "strtoint"
            | "ord"
            | "chr"
            | "sizeof"
            | "create"
            | "free"
            | "destroy"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_path_and_bytes_match(extension: &str, contents: &[u8]) {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join(format!("fixture.{extension}"));
        fs::write(&path, contents).expect("write fixture");

        let source_file = format!("fixture.{extension}");
        let path_extraction =
            extract_pascal_family(&path, &source_file, extension).expect("path extraction");
        let bytes_extraction =
            extract_pascal_family_bytes(&path, &source_file, extension, contents)
                .expect("byte extraction");

        assert_eq!(
            serde_json::to_value(path_extraction).expect("serialize path extraction"),
            serde_json::to_value(bytes_extraction).expect("serialize byte extraction"),
        );
    }

    #[test]
    fn byte_entrypoint_matches_path_entrypoint_for_pascal_family_formats() {
        assert_path_and_bytes_match(
            "pas",
            b"unit Fixture;\ninterface\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n",
        );
        assert_path_and_bytes_match(
            "dfm",
            b"object Form1: TForm1\n  OnCreate = FormCreate\nend\n",
        );
        assert_path_and_bytes_match(
            "lpk",
            b"<Package><Name Value=\"Fixture\"/><RequiredPkgs><PackageName Value=\"LCL\"/></RequiredPkgs></Package>",
        );
    }

    #[test]
    fn byte_entrypoint_preserves_binary_dfm_handling() {
        let extraction = extract_pascal_family_bytes(
            Path::new("fixture.dfm"),
            "fixture.dfm",
            "dfm",
            &[0xff, 0x0a],
        )
        .expect("binary DFM is supported");
        assert!(extraction.nodes.is_empty());
        assert!(extraction.edges.is_empty());
    }

    #[test]
    fn byte_entrypoint_does_not_probe_pascal_siblings() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("main.pas");
        fs::write(directory.path().join("Sibling.pas"), b"unit Sibling;")
            .expect("write sibling fixture");

        let extraction = extract_pascal_family_bytes(
            &path,
            "src/main.pas",
            "pas",
            b"unit Main; uses Sibling; interface implementation end.",
        )
        .expect("byte extraction");

        assert!(extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "imports" && edge.target == make_id(&["Sibling"])));
    }

    #[test]
    fn byte_project_resolution_binds_units_bases_and_inherited_calls() {
        let mut extractions = vec![
            extract_pascal_family_bytes(
                Path::new("/graphoxide-missing-project/pascal/Runner.pas"),
                "pascal/Runner.pas",
                "pas",
                b"unit Runner; interface uses Worker; type TRunner = class(TWorker) public procedure Execute; end; implementation procedure TRunner.Execute; begin Process; end; end.",
            )
            .expect("extract runner bytes"),
            extract_pascal_family_bytes(
                Path::new("/graphoxide-missing-project/pascal/Worker.pas"),
                "pascal/Worker.pas",
                "pas",
                b"unit Worker; interface type TWorker = class public procedure Process; end; implementation procedure TWorker.Process; begin end; end.",
            )
            .expect("extract worker bytes"),
        ];

        resolve_project_symbols(&mut extractions);
        resolve_inherited_calls(&mut extractions);
        let edges = extractions
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .collect::<Vec<_>>();
        assert!(edges
            .iter()
            .any(|edge| { edge.relation == "imports" && edge.true_target() == "pascal_worker" }));
        assert!(edges.iter().any(|edge| {
            edge.relation == "inherits"
                && edge.true_source() == "pascal_runner_trunner"
                && edge.true_target() == "pascal_worker_tworker"
        }));
        assert!(edges.iter().any(|edge| {
            edge.relation == "calls"
                && edge.true_source() == "pascal_runner_trunner_execute"
                && edge.true_target() == "pascal_worker_tworker_process"
        }));
        assert!(extractions
            .iter()
            .flat_map(|value| &value.nodes)
            .all(|node| {
                node.id != "tworker" && !node.extra.contains_key("pascal_unresolved_base")
            }));
    }

    #[test]
    fn duplicate_pascal_unit_candidates_remain_unresolved() {
        let mut extractions = vec![
            extract_pascal_family_bytes(
                Path::new("/graphoxide-missing-project/pascal/Runner.pas"),
                "pascal/Runner.pas",
                "pas",
                b"unit Runner; interface uses Worker; type TRunner = class(TWorker) public procedure Execute; end; implementation procedure TRunner.Execute; begin Process; end; end.",
            )
            .expect("extract runner bytes"),
            extract_pascal_family_bytes(
                Path::new("/graphoxide-missing-project/pascal/Worker.pas"),
                "pascal/Worker.pas",
                "pas",
                b"unit Worker; interface type TWorker = class public procedure Process; end; implementation procedure TWorker.Process; begin end; end.",
            )
            .expect("extract first worker bytes"),
            extract_pascal_family_bytes(
                Path::new("/graphoxide-missing-project/pascal/worker.pp"),
                "pascal/worker.pp",
                "pp",
                b"unit worker; interface type TWorker = class public procedure Process; end; implementation procedure TWorker.Process; begin end; end.",
            )
            .expect("extract ambiguous worker bytes"),
        ];

        resolve_project_symbols(&mut extractions);
        resolve_inherited_calls(&mut extractions);
        let runner = &extractions[0];
        assert!(runner
            .edges
            .iter()
            .any(|edge| edge.relation == "imports" && edge.true_target() == "worker"));
        assert!(runner
            .edges
            .iter()
            .any(|edge| edge.relation == "inherits" && edge.true_target() == "tworker"));
        assert!(!runner.edges.iter().any(|edge| {
            edge.relation == "calls" && edge.true_target().ends_with("tworker_process")
        }));
        assert!(runner
            .edges
            .iter()
            .all(|edge| !edge.extra.contains_key("pascal_unit")));
        assert!(runner
            .nodes
            .iter()
            .all(|node| !node.extra.contains_key("pascal_unresolved_base")));
    }
}
