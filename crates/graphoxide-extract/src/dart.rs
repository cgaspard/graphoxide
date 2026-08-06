//! Deterministic Dart syntax extraction.
//!
//! Graphoxide does not currently ship a Dart tree-sitter grammar, so this
//! scanner mirrors Graphify's balanced-regex contract. It recognizes only
//! declarations and framework constructs with explicit lexical evidence.

use crate::project_path::{source_relative_project_path, ProjectPath};
use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

const TYPE_BLACKLIST: &[&str] = &[
    "String", "int", "double", "bool", "num", "dynamic", "Object", "List", "Map", "Set", "Future",
    "Stream", "void",
];

struct Builder<'a> {
    source_file: &'a str,
    stem: String,
    file_id: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    seen_nodes: HashSet<String>,
    seen_edges: HashSet<(String, String, String, String)>,
}

impl<'a> Builder<'a> {
    fn new(path: &Path, source_file: &'a str, parent: Option<&Path>) -> Self {
        let identity_path = parent.unwrap_or_else(|| Path::new(source_file));
        let stem = identity_path
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let file_id = if parent.is_some() {
            make_id(&[&identity_path.to_string_lossy()])
        } else {
            make_id(&[&stem])
        };
        let mut builder = Self {
            source_file,
            stem,
            file_id: file_id.clone(),
            nodes: Vec::new(),
            edges: Vec::new(),
            seen_nodes: HashSet::new(),
            seen_edges: HashSet::new(),
        };
        if parent.is_none() {
            builder.add_node(
                file_id,
                path.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(source_file),
                "file",
                Some(source_file),
                1,
                "code",
            );
        }
        builder
    }

    fn finish(self) -> Extraction {
        Extraction {
            nodes: self.nodes,
            edges: self.edges,
            hyperedges: Vec::new(),
        }
    }

    fn add_node(
        &mut self,
        id: String,
        label: &str,
        kind: &str,
        source_file: Option<&str>,
        line: usize,
        file_type: &str,
    ) -> String {
        if self.seen_nodes.insert(id.clone()) {
            self.nodes.push(Node {
                id: id.clone(),
                label: label.into(),
                file_type: file_type.into(),
                source_file: source_file.unwrap_or("").into(),
                source_location: Some(format!("L{line}")),
                community: None,
                extra: BTreeMap::from([
                    ("_origin".into(), "fallback".into()),
                    ("type".into(), kind.into()),
                ]),
            });
        }
        id
    }

    fn definition(&mut self, name: &str, kind: &str, line: usize) -> String {
        let id = make_id(&[&self.stem, name]);
        let label = if kind == "function" {
            format!("{name}()")
        } else {
            name.to_owned()
        };
        self.add_node(
            id.clone(),
            &label,
            kind,
            Some(self.source_file),
            line,
            "code",
        );
        let file_id = self.file_id.clone();
        self.edge(&file_id, &id, "defines", None, line);
        id
    }

    fn method(&mut self, owner: &str, name: &str, line: usize) -> String {
        let id = make_id(&[owner, name]);
        self.add_node(
            id.clone(),
            &format!("{name}()"),
            "function",
            Some(self.source_file),
            line,
            "code",
        );
        self.edge(owner, &id, "method", None, line);
        id
    }

    fn external(&mut self, name: &str, kind: &str, line: usize) -> String {
        let id = make_id(&[name]);
        self.add_node(id.clone(), name, kind, None, line, "code");
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            // Corpus-level resolution must know the runtime that produced an
            // otherwise source-less reference. Without this provenance a
            // common name such as `Service` is either left unresolved or can
            // be welded to an unrelated PHP/C# declaration.
            node.extra
                .insert("origin_file".into(), self.source_file.into());
        }
        id
    }

    fn local_module(&self, logical: &str) -> String {
        let stem = Path::new(logical)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        make_id(&[&stem])
    }

    fn concept(&mut self, id: String, label: &str, line: usize, local: bool) -> String {
        let source = local.then_some(self.source_file);
        self.add_node(id, label, "concept", source, line, "concept")
    }

    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        context: Option<&str>,
        line: usize,
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
            ("confidence_score".into(), 1.0.into()),
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
            confidence: Confidence::Extracted,
            source_file: self.source_file.into(),
            extra,
        });
    }
}

#[derive(Clone)]
struct Candidate {
    start: usize,
    id: String,
    name: String,
    is_class: bool,
}

struct TypeScope {
    id: String,
    body_start: usize,
    body_end: usize,
}

pub(crate) fn extract_dart(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let source = fs::read_to_string(path)?;
    let text = mask_comments(source.as_str());
    let parent = part_parent(path, source_file, &text);
    extract_dart_text(path, source_file, &text, parent.as_deref())
}

/// Extract Dart declarations from source bytes supplied by the I/O plane.
///
/// A `part of` directive is resolved against the portable source identity,
/// rather than probing the filesystem. The path-based wrapper retains the
/// legacy existence/canonicalization behavior for existing callers.
#[allow(dead_code)] // Activated by the byte-oriented engine dispatch.
pub(crate) fn extract_dart_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    let source = std::str::from_utf8(bytes)?;
    let text = mask_comments(source);
    let parent = part_parent_from_source_file(source_file, &text);
    extract_dart_text(path, source_file, &text, parent.as_deref())
}

fn extract_dart_text(
    path: &Path,
    source_file: &str,
    text: &str,
    parent: Option<&Path>,
) -> anyhow::Result<Extraction> {
    let mut builder = Builder::new(path, source_file, parent);
    let mut candidates = Vec::<Candidate>::new();
    let mut type_scopes = Vec::<TypeScope>::new();

    let classes = Regex::new(
        r"(?m)^\s*(?:(?:abstract|sealed|base|interface|final|mixin)\s+)*(?:class|mixin|enum|extension\s+type)\s+([A-Za-z_][A-Za-z0-9_]*)",
    )?;
    for capture in classes.captures_iter(text) {
        let matched = capture.get(0).expect("Dart declaration");
        let name = &capture[1];
        let line = line_of(text, matched.start());
        let id = builder.definition(name, "class", line);
        candidates.push(Candidate {
            start: matched.start(),
            id: id.clone(),
            name: name.to_owned(),
            is_class: true,
        });
        let rest_end = (matched.end() + 500).min(text.len());
        let rest = &text[matched.end()..rest_end];
        let (header, body) = declaration_header_and_body(text, matched.start(), rest);
        emit_class_relationships(&mut builder, &id, &header, line)?;
        if let Some((body_start, body_end)) = body {
            emit_framework_edges(&mut builder, text, &id, body_start, body_end, false)?;
            type_scopes.push(TypeScope {
                id,
                body_start,
                body_end,
            });
        }
    }

    let extensions = Regex::new(
        r"(?m)^[ ]{0,4}extension\s+([A-Za-z_][A-Za-z0-9_]*)?(?:<[^>]+>)?\s+on\s+([A-Za-z_][A-Za-z0-9_]*)",
    )?;
    for capture in extensions.captures_iter(text) {
        let matched = capture.get(0).expect("Dart extension");
        let line = line_of(text, matched.start());
        let target_name = &capture[2];
        let name = capture
            .get(1)
            .map(|value| value.as_str().to_owned())
            .unwrap_or_else(|| format!("{}_anonymous_extension", builder.stem));
        let label = capture
            .get(1)
            .map(|value| value.as_str().to_owned())
            .unwrap_or_else(|| format!("Extension on {target_name}"));
        let id = make_id(&[&builder.stem, &name]);
        builder.add_node(id.clone(), &label, "class", Some(source_file), line, "code");
        let file_id = builder.file_id.clone();
        builder.edge(&file_id, &id, "defines", None, line);
        let target = builder.external(target_name, "reference", line);
        builder.edge(&id, &target, "extends", None, line);
        candidates.push(Candidate {
            start: matched.start(),
            id: id.clone(),
            name,
            is_class: true,
        });
        if let Some((body_start, body_end)) = declaration_body_range(text, matched.start()) {
            type_scopes.push(TypeScope {
                id,
                body_start,
                body_end,
            });
        }
    }

    let functions = Regex::new(
        r"(?m)^[ ]{0,2}(?:(?:factory|static|async|external|abstract)[ ]+)?(?:\([^)]+\)|[A-Za-z0-9_<>,.?]+)(?:[ ]+[A-Za-z0-9_<>,.?]+){0,3}[ ]+([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)?)[ ]*\(",
    )?;
    let mut function_offsets = HashSet::new();
    for capture in functions.captures_iter(text) {
        let matched = capture.get(0).expect("Dart function");
        let raw_name = &capture[1];
        let name = raw_name.rsplit('.').next().unwrap_or(raw_name);
        if function_name_noise(name) || name.chars().next().is_some_and(char::is_uppercase) {
            continue;
        }
        function_offsets.insert(matched.start());
        let line = line_of(text, matched.start());
        let owner = enclosing_type(&type_scopes, matched.start());
        let id = if let Some(owner) = owner {
            builder.method(&owner.id, name, line)
        } else {
            builder.definition(name, "function", line)
        };
        candidates.push(Candidate {
            start: matched.start(),
            id: id.clone(),
            name: name.to_owned(),
            is_class: false,
        });
        let scope_end = owner.map_or(text.len(), |owner| owner.body_end);
        if let Some((body_start, body_end)) =
            callable_body_range(text, matched.end() - 1, scope_end)
        {
            emit_framework_edges(&mut builder, text, &id, body_start, body_end, true)?;
        }
    }
    let factories = Regex::new(
        r"(?m)^[ ]{0,2}factory\s+[A-Za-z_][A-Za-z0-9_]*\.([A-Za-z_][A-Za-z0-9_]*)\s*\(",
    )?;
    for capture in factories.captures_iter(text) {
        let matched = capture.get(0).expect("Dart factory");
        if function_offsets.contains(&matched.start()) {
            continue;
        }
        let line = line_of(text, matched.start());
        let owner = enclosing_type(&type_scopes, matched.start());
        let id = if let Some(owner) = owner {
            builder.method(&owner.id, &capture[1], line)
        } else {
            builder.definition(&capture[1], "function", line)
        };
        candidates.push(Candidate {
            start: matched.start(),
            id,
            name: capture[1].to_owned(),
            is_class: false,
        });
    }

    emit_annotations(&mut builder, text, &candidates)?;
    emit_typedefs(&mut builder, text)?;
    emit_variables(&mut builder, text)?;
    emit_directives(&mut builder, text)?;
    emit_generic_lookups(&mut builder, text)?;
    Ok(builder.finish())
}

fn part_parent(path: &Path, source_file: &str, text: &str) -> Option<PathBuf> {
    let directive = Regex::new(r#"(?m)^\s*part\s+of\s+['\"]([^'\"]+)['\"]"#).ok()?;
    let parent = directive.captures(text)?.get(1)?.as_str();
    if normalized_dart_project_uri(source_file, parent, true).is_none() {
        // The legacy single-file entrypoint supplies an absolute source name.
        // Validate its static sibling reference against a synthetic logical
        // basename, then require the physical sibling below before retaining
        // the compatibility parent identity.
        let basename = path.file_name()?.to_str()?;
        normalized_dart_project_uri(basename, parent, true)?;
    }
    let candidate = path.parent()?.join(parent);
    candidate
        .is_file()
        .then(|| fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.to_path_buf()))
}

#[allow(dead_code)] // Used by the byte-oriented Dart dispatch.
fn part_parent_from_source_file(source_file: &str, text: &str) -> Option<PathBuf> {
    let directive = Regex::new(r#"(?m)^\s*part\s+of\s+['\"]([^'\"]+)['\"]"#).ok()?;
    let parent = directive.captures(text)?.get(1)?.as_str();
    normalized_dart_project_uri(source_file, parent, true).map(PathBuf::from)
}

/// Return a normalized project-relative identity for a static Dart URI.
///
/// Source bytes are untrusted, so URI spelling is interpreted with portable
/// `/` separators only. This prevents host-specific absolute, drive-relative,
/// UNC, interpolation, and parent-escape spellings from acquiring the identity
/// of an independently admitted Dart source.
fn normalized_dart_project_uri(
    source_file: &str,
    uri: &str,
    require_dart_extension: bool,
) -> Option<String> {
    let uri = static_dart_uri_literal(uri)?;
    let bytes = uri.as_bytes();
    if uri.starts_with(['/', '\\'])
        || bytes.get(1) == Some(&b':')
        || uri.contains([':', '?', '#'])
        || (require_dart_extension && !uri.ends_with(".dart"))
    {
        return None;
    }

    match source_relative_project_path(source_file, uri)? {
        ProjectPath::Contained(logical) => Some(logical),
        ProjectPath::EscapesRoot(_) => None,
    }
}

fn static_dart_uri_literal(uri: &str) -> Option<&str> {
    (!uri.is_empty()
        && uri.trim() == uri
        && !uri.chars().any(|character| {
            character.is_control() || matches!(character, '$' | '`' | '\\' | '\'' | '"' | '{' | '}')
        }))
    .then_some(uri)
}

fn portable_dart_uri_segment(segment: &str) -> bool {
    let device_stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .to_ascii_lowercase();
    !segment.is_empty()
        && !segment.ends_with(['.', ' '])
        && !matches!(
            device_stem.as_str(),
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        )
        && !segment.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '$' | '`' | '{' | '}'
                )
        })
}

enum DartDirectiveUri<'a> {
    Project { logical: String },
    External(&'a str),
}

fn static_dart_directive_uri<'a>(source_file: &str, uri: &'a str) -> Option<DartDirectiveUri<'a>> {
    let uri = static_dart_uri_literal(uri)?;
    if let Some(rest) = uri
        .strip_prefix("package:")
        .or_else(|| uri.strip_prefix("dart:"))
    {
        return (!rest.is_empty()
            && !rest.starts_with('/')
            && rest
                .split('/')
                .all(|part| part != "." && part != ".." && portable_dart_uri_segment(part)))
        .then_some(DartDirectiveUri::External(uri));
    }
    normalized_dart_project_uri(source_file, uri, false)
        .map(|logical| DartDirectiveUri::Project { logical })
}

fn mask_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut index = 0usize;
    let mut quote = None;
    let mut escaped = false;
    while index < bytes.len() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if bytes[index] == b'\\' {
                escaped = true;
            } else if bytes[index] == active {
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(bytes[index], b'\'' | b'"') {
            quote = Some(bytes[index]);
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"//") {
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
            while index < bytes.len() && !bytes[index..].starts_with(b"*/") {
                if !matches!(bytes[index], b'\r' | b'\n') {
                    masked[index] = b' ';
                }
                index += 1;
            }
            for _ in 0..2 {
                if index < bytes.len() {
                    masked[index] = b' ';
                    index += 1;
                }
            }
            continue;
        }
        index += 1;
    }
    String::from_utf8(masked).unwrap_or_else(|_| source.to_owned())
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn brace_range(text: &str, search_from: usize) -> Option<(usize, usize)> {
    let open = text[search_from..].find('{')? + search_from;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (relative, byte) in bytes[open..].iter().copied().enumerate() {
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
                    return Some((open, open + relative + 1));
                }
            }
            _ => {}
        }
    }
    Some((open, text.len()))
}

fn declaration_body_range(text: &str, declaration_start: usize) -> Option<(usize, usize)> {
    let brace = text[declaration_start..].find('{')? + declaration_start;
    let semicolon = text[declaration_start..]
        .find(';')
        .map(|offset| declaration_start + offset);
    if semicolon.is_some_and(|semicolon| semicolon < brace) {
        return None;
    }
    brace_range(text, brace)
}

fn enclosing_type(scopes: &[TypeScope], offset: usize) -> Option<&TypeScope> {
    scopes
        .iter()
        .filter(|scope| scope.body_start < offset && offset < scope.body_end)
        .min_by_key(|scope| scope.body_end - scope.body_start)
}

fn callable_body_range(
    text: &str,
    parameter_open: usize,
    scope_end: usize,
) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    if bytes.get(parameter_open) != Some(&b'(') {
        return None;
    }
    let limit = scope_end.min(bytes.len());
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let mut cursor = parameter_open;
    while cursor < limit {
        let byte = bytes[cursor];
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active {
                quote = None;
            }
            cursor += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            cursor += 1;
            continue;
        }
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    cursor += 1;
                    break;
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    if depth != 0 {
        return None;
    }

    while cursor < limit {
        if bytes[cursor..].starts_with(b"=>") || bytes[cursor] == b';' {
            return None;
        }
        if bytes[cursor] == b'{' {
            let body = brace_range(text, cursor)?;
            return (body.1 <= limit).then_some(body);
        }
        cursor += 1;
    }
    None
}

fn split_types(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for character in text.chars() {
        match character {
            '<' => {
                depth += 1;
                current.push(character);
            }
            '>' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => {
                let value = current.trim();
                if !value.is_empty() {
                    parts.push(value.to_owned());
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_owned());
    }
    parts
}

fn angle_group(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if !text.starts_with('<') {
        return None;
    }
    let mut depth = 0usize;
    for (offset, character) in text.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((&text[1..offset], &text[offset + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

fn paren_tail(text: &str) -> &str {
    let text = text.trim_start();
    if !text.starts_with('(') {
        return text;
    }
    let mut depth = 0usize;
    for (offset, character) in text.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return &text[offset + 1..];
                }
            }
            _ => {}
        }
    }
    text
}

fn declaration_header_and_body(
    text: &str,
    declaration_start: usize,
    rest: &str,
) -> (String, Option<(usize, usize)>) {
    let mut rest = rest.trim_start();
    if let Some((_, tail)) = angle_group(rest) {
        rest = tail.trim_start();
    }
    rest = paren_tail(rest);
    let header_end = rest.find(['{', ';']).unwrap_or(rest.len());
    let header = rest[..header_end].trim().to_owned();
    let brace = text[declaration_start..]
        .find('{')
        .map(|offset| declaration_start + offset);
    let semicolon = text[declaration_start..]
        .find(';')
        .map(|offset| declaration_start + offset);
    let body = brace
        .filter(|brace| semicolon.is_none_or(|semicolon| *brace < semicolon))
        .and_then(|_| brace_range(text, declaration_start));
    (header, body)
}

fn clean_outer_type(value: &str) -> String {
    value.split('<').next().unwrap_or(value).trim().to_owned()
}

fn emit_class_relationships(
    builder: &mut Builder<'_>,
    class: &str,
    header: &str,
    line: usize,
) -> anyhow::Result<()> {
    let base = Regex::new(r"^\s*(?:extends|on)\s+([A-Za-z0-9_.$]+)")?;
    let mut remaining = header;
    if let Some(capture) = base.captures(header) {
        let matched = capture.get(0).expect("Dart base type");
        let base_name = &capture[1];
        let target = builder.external(base_name, "reference", line);
        builder.edge(class, &target, "inherits", None, line);
        remaining = &header[matched.end()..];
        if let Some((generics, tail)) = angle_group(remaining) {
            for generic in split_types(generics) {
                let name = clean_outer_type(&generic);
                if !name.is_empty() && !TYPE_BLACKLIST.contains(&name.as_str()) {
                    let target = builder.external(&name, "reference", line);
                    builder.edge(class, &target, "references", None, line);
                }
            }
            remaining = tail;
        }
    }
    let remaining = remaining.trim_start();
    let (mixins, interfaces) = if let Some(with) = remaining.strip_prefix("with ") {
        if let Some(position) = with.find("implements") {
            (
                &with[..position],
                Some(&with[position + "implements".len()..]),
            )
        } else {
            (with, None)
        }
    } else if let Some(interfaces) = remaining.strip_prefix("implements ") {
        ("", Some(interfaces))
    } else {
        ("", None)
    };
    for mixin in split_types(mixins) {
        let name = clean_outer_type(&mixin);
        if name.is_empty() {
            continue;
        }
        let target = builder.external(&name, "reference", line);
        builder.edge(class, &target, "mixes_in", None, line);
    }
    if let Some(interfaces) = interfaces {
        for interface in split_types(interfaces) {
            let name = clean_outer_type(&interface);
            if name.is_empty() {
                continue;
            }
            let target = builder.external(&name, "reference", line);
            builder.edge(class, &target, "implements", None, line);
        }
    }
    Ok(())
}

fn emit_framework_edges(
    builder: &mut Builder<'_>,
    text: &str,
    source: &str,
    body_start: usize,
    body_end: usize,
    navigation: bool,
) -> anyhow::Result<()> {
    let body = &text[body_start..body_end];
    let patterns = [
        (
            r"\bon<([A-Za-z_][A-Za-z0-9_]*)>\s*\(",
            "calls",
            "bloc_event",
        ),
        (
            r"\bref\.(?:watch|read|listen)\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)",
            "references",
            "riverpod_reference",
        ),
        (
            r"\b(?:read|watch|select|of)\s*<([A-Za-z_][A-Za-z0-9_]*)>",
            "references",
            "bloc_lookup",
        ),
    ];
    for (pattern, relation, context) in patterns {
        let regex = Regex::new(pattern)?;
        for capture in regex.captures_iter(body) {
            let matched = capture.get(0).expect("Dart framework reference");
            let name = &capture[1];
            if TYPE_BLACKLIST.contains(&name) {
                continue;
            }
            let line = line_of(text, body_start + matched.start());
            let target = builder.external(name, "reference", line);
            builder.edge(source, &target, relation, Some(context), line);
        }
    }
    let emissions = Regex::new(r"\b(?:emit|yield)\s*\(?\s*(?:const\s+)?([A-Z][A-Za-z0-9_]*)\b")?;
    for capture in emissions.captures_iter(body) {
        let matched = capture.get(0).expect("Dart emission");
        let name = &capture[1];
        if TYPE_BLACKLIST.contains(&name) {
            continue;
        }
        let line = line_of(text, body_start + matched.start());
        let target = builder.external(name, "reference", line);
        builder.edge(source, &target, "calls", Some("emit_state"), line);
    }
    let additions = Regex::new(
        r"\b(?:[A-Za-z_][A-Za-z0-9_]*[Bb]loc[A-Za-z0-9_]*|context\.read<[A-Za-z_][A-Za-z0-9_]*>\(\))\.add\(\s*(?:const\s+)?([A-Z][A-Za-z0-9_]*)\b",
    )?;
    for capture in additions.captures_iter(body) {
        let matched = capture.get(0).expect("Dart Bloc addition");
        let line = line_of(text, body_start + matched.start());
        let target = builder.external(&capture[1], "reference", line);
        builder.edge(source, &target, "calls", Some("bloc_add_event"), line);
    }
    if !navigation {
        return Ok(());
    }
    let paths = Regex::new(
        r#"\b(?:go|push|goNamed|pushNamed|replace|replaceNamed)\s*\(\s*(?:context\s*,\s*)?['\"]([A-Za-z0-9_/?=&%\-]+)['\"]"#,
    )?;
    for capture in paths.captures_iter(body) {
        let matched = capture.get(0).expect("Dart route path");
        let route = &capture[1];
        let normalized = route.replace(['/', '?', '=', '&'], "_");
        let id = make_id(&["route", &normalized]);
        let line = line_of(text, body_start + matched.start());
        let target = builder.concept(id, &format!("Route {route}"), line, false);
        builder.edge(source, &target, "navigates", Some("route_path"), line);
    }
    let constants = Regex::new(
        r"\b(?:go|push|goNamed|pushNamed|replace|replaceNamed)\s*\(\s*(?:context\s*,\s*)?([A-Z][A-Za-z0-9_]*\.[A-Za-z0-9_]+)",
    )?;
    for capture in constants.captures_iter(body) {
        let matched = capture.get(0).expect("Dart route constant");
        let route = &capture[1];
        let id = make_id(&["route", &route.replace('.', "_")]);
        let line = line_of(text, body_start + matched.start());
        let target = builder.concept(id, route, line, false);
        builder.edge(source, &target, "navigates", Some("route_const"), line);
    }
    let objects = Regex::new(
        r"\b(?:push|replace)\s*\(\s*(?:context\s*,\s*)?.*?\b([A-Z][A-Za-z0-9_]*(?:Route|Screen|Page))\b",
    )?;
    for capture in objects.captures_iter(body) {
        let matched = capture.get(0).expect("Dart route object");
        let route = &capture[1];
        let line = line_of(text, body_start + matched.start());
        let target = builder.external(route, "reference", line);
        builder.edge(source, &target, "navigates", Some("route_object"), line);
    }
    Ok(())
}

fn function_name_noise(name: &str) -> bool {
    matches!(
        name,
        "if" | "for"
            | "while"
            | "switch"
            | "catch"
            | "return"
            | "void"
            | "dynamic"
            | "final"
            | "const"
            | "get"
            | "set"
    )
}

fn emit_annotations(
    builder: &mut Builder<'_>,
    text: &str,
    candidates: &[Candidate],
) -> anyhow::Result<()> {
    let annotations = Regex::new(r"@([A-Za-z_][A-Za-z0-9_]*)(?:\([^)]*\))?")?;
    for capture in annotations.captures_iter(text) {
        let matched = capture.get(0).expect("Dart annotation");
        let annotation = &capture[1];
        if matches!(
            annotation,
            "override" | "deprecated" | "required" | "protected" | "mustCallSuper"
        ) {
            continue;
        }
        let Some(target) = candidates
            .iter()
            .filter(|candidate| candidate.start >= matched.end())
            .min_by_key(|candidate| candidate.start)
        else {
            continue;
        };
        if target.start.saturating_sub(matched.end()) > 300 {
            continue;
        }
        let between = &text[matched.end()..target.start];
        if between.contains([';', '{', '}']) {
            continue;
        }
        let line = line_of(text, matched.start());
        let annotation_id = make_id(&["annotation", &annotation.to_ascii_lowercase()]);
        let annotation_id = builder.concept(annotation_id, &format!("@{annotation}"), line, false);
        builder.edge(&target.id, &annotation_id, "configures", None, line);
        if annotation.eq_ignore_ascii_case("riverpod") {
            let provider = if target.is_class {
                let mut characters = target.name.chars();
                characters
                    .next()
                    .map(|first| first.to_lowercase().collect::<String>() + characters.as_str())
                    .unwrap_or_default()
                    + "Provider"
            } else {
                format!("{}Provider", target.name)
            };
            let provider_id = builder.concept(make_id(&[&provider]), &provider, line, true);
            builder.edge(
                &target.id,
                &provider_id,
                "defines",
                Some("riverpod_provider"),
                line,
            );
        }
    }
    Ok(())
}

fn emit_typedefs(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    let typedefs = Regex::new(
        r"(?m)^\s*typedef\s+([A-Za-z_][A-Za-z0-9_]*)(?:<[^>]+>)?\s*=\s*([A-Za-z0-9_<>,.?\s]+);",
    )?;
    for capture in typedefs.captures_iter(text) {
        let matched = capture.get(0).expect("Dart typedef");
        let line = line_of(text, matched.start());
        let id = builder.definition(&capture[1], "type_alias", line);
        let target = clean_outer_type(&capture[2]);
        let target = target.rsplit('.').next().unwrap_or(&target);
        if !target.is_empty() && !TYPE_BLACKLIST.contains(&target) {
            let target = builder.external(target, "reference", line);
            builder.edge(&id, &target, "references", Some("typedef"), line);
        }
    }
    Ok(())
}

fn emit_variables(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    let variables = Regex::new(
        r"(?m)^[ ]{0,2}(?:late[ ]+)?(?:(?:final|const|var)[ ]+)?(?:\([^)]+\)[ ]+|([A-Za-z0-9_<>,.?]+(?:[ ]+[A-Za-z0-9_<>,.?]+){0,3})[ ]+)?(?:([A-Za-z_][A-Za-z0-9_]*)|(?:[A-Za-z_][A-Za-z0-9_]*[ ]*)?\(([^)]+)\))[ ]*(?:=|$|;)",
    )?;
    for capture in variables.captures_iter(text) {
        let matched = capture.get(0).expect("Dart variable");
        let declaration = matched.as_str().trim_start();
        let variable_type = capture.get(1).map(|value| value.as_str().trim());
        if !["late", "final", "const", "var"]
            .iter()
            .any(|modifier| declaration.starts_with(modifier))
            && variable_type.is_none()
        {
            continue;
        }
        let line = line_of(text, matched.start());
        if let Some(name) = capture.get(2).map(|value| value.as_str()) {
            if function_name_noise(name) {
                continue;
            }
            builder.definition(name, "variable", line);
            if let Some(variable_type) = variable_type {
                let clean = clean_outer_type(variable_type);
                let clean = clean.rsplit('.').next().unwrap_or(&clean);
                if !clean.is_empty() && !TYPE_BLACKLIST.contains(&clean) {
                    let target = builder.external(clean, "reference", line);
                    let file_id = builder.file_id.clone();
                    builder.edge(&file_id, &target, "references", Some("variable_type"), line);
                }
            }
            continue;
        }
        let Some(names) = capture.get(3).map(|value| value.as_str()) else {
            continue;
        };
        for raw in names.split(',') {
            let name = raw.rsplit(':').next().unwrap_or(raw).trim();
            if name.is_empty()
                || name.chars().next().is_some_and(char::is_uppercase)
                || !name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
            {
                continue;
            }
            builder.definition(name, "variable", line);
        }
    }
    Ok(())
}

fn emit_directives(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    let quoted_uris = Regex::new(r#"['\"]([^'\"]+)['\"]"#)?;
    for (keyword, relation) in [("import", "imports"), ("export", "exports")] {
        let regex = Regex::new(&format!(r#"(?m)^\s*{keyword}\s+['\"]([^'\"]+)['\"]"#))?;
        for capture in regex.captures_iter(text) {
            let matched = capture.get(0).expect("Dart directive");
            let line_end = text[matched.end()..]
                .find(['\r', '\n'])
                .map_or(text.len(), |offset| matched.end() + offset);
            let directive = &text[matched.start()..line_end];
            let tail = text[matched.end()..line_end].trim();
            let valid_tail = tail.is_empty()
                || tail == ";"
                || ["deferred", "as", "show", "hide", "if"]
                    .iter()
                    .any(|keyword| {
                        tail.strip_prefix(keyword).is_some_and(|rest| {
                            rest.chars().next().is_some_and(|character| {
                                character.is_whitespace() || character == '('
                            })
                        })
                    });
            if !valid_tail
                || directive
                    .chars()
                    .any(|character| matches!(character, '$' | '`' | '+' | '=' | '{' | '}'))
                || quoted_uris.captures_iter(directive).any(|quoted| {
                    static_dart_directive_uri(builder.source_file, &quoted[1]).is_none()
                })
            {
                continue;
            }
            let Some(uri) = static_dart_directive_uri(builder.source_file, &capture[1]) else {
                continue;
            };
            let line = line_of(text, matched.start());
            let (target, target_file) = match uri {
                DartDirectiveUri::Project { logical } => {
                    (builder.local_module(&logical), Some(logical))
                }
                DartDirectiveUri::External(uri) => (builder.external(uri, "module", line), None),
            };
            let file_id = builder.file_id.clone();
            builder.edge(&file_id, &target, relation, None, line);
            if let Some(target_file) = target_file
                && let Some(edge) = builder
                    .edges
                    .iter_mut()
                    .rev()
                    .find(|edge| edge.relation == relation && edge.true_target() == target)
            {
                edge.extra.insert("target_file".into(), target_file.into());
            }
        }
    }
    Ok(())
}

fn emit_generic_lookups(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    let lookups = Regex::new(
        r"\b[A-Za-z_][A-Za-z0-9_]*<([A-Za-z0-9_.]+(?:<[A-Za-z0-9_.,\s<>]+>)?)\s*>\s*\(",
    )?;
    for capture in lookups.captures_iter(text) {
        let matched = capture.get(0).expect("Dart type lookup");
        let name = capture[1]
            .split('<')
            .next()
            .unwrap_or(&capture[1])
            .rsplit('.')
            .next()
            .unwrap_or(&capture[1])
            .trim();
        if name.is_empty() || TYPE_BLACKLIST.contains(&name) {
            continue;
        }
        let line = line_of(text, matched.start());
        let target = builder.external(name, "reference", line);
        let file_id = builder.file_id.clone();
        builder.edge(&file_id, &target, "references", Some("type_lookup"), line);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn byte_entrypoint_does_not_require_a_source_file() {
        let extraction = extract_dart_bytes(
            Path::new("missing.dart"),
            "lib/missing.dart",
            b"class Widget {}\nvoid main() {}\n",
        )
        .expect("extract in-memory Dart source");
        assert!(extraction.nodes.iter().any(|node| node.label == "Widget"));
        assert!(extraction.nodes.iter().any(|node| node.label == "main()"));
    }

    #[test]
    fn byte_part_of_requires_a_static_contained_project_uri() {
        let static_part = extract_dart_bytes(
            Path::new("/missing/lib/parts/child.dart"),
            "lib/parts/child.dart",
            "part of '.././données.dart';\nclass Child {}\n".as_bytes(),
        )
        .expect("extract static Dart part");
        assert!(static_part
            .nodes
            .iter()
            .any(|node| node.id == "lib_données_child"));
        assert!(!static_part
            .nodes
            .iter()
            .any(|node| node.id == "lib_parts_child" && node.extra["type"] == "file"));

        for uri in [
            "${page}.dart",
            "$page.dart",
            "../../../page.dart",
            "/page.dart",
            "C:page.dart",
            "C:/page.dart",
            "//server/share/page.dart",
            r"\\server\share\page.dart",
            "../NUL.dart",
            "../page.dart.",
            "../dir//page.dart",
            "../dir/node:page.dart",
        ] {
            let source = format!("part of '{uri}';\nclass Child {{}}\n");
            let extraction = extract_dart_bytes(
                Path::new("/missing/lib/parts/child.dart"),
                "lib/parts/child.dart",
                source.as_bytes(),
            )
            .expect("extract rejected Dart part URI");
            assert!(
                extraction
                    .nodes
                    .iter()
                    .any(|node| node.id == "lib_parts_child" && node.extra["type"] == "file"),
                "unsafe part URI acquired a parent identity: {uri}"
            );
            assert!(
                extraction
                    .nodes
                    .iter()
                    .any(|node| node.id == "lib_parts_child_child"),
                "unsafe part URI changed declaration identity: {uri}"
            );
        }
    }

    #[test]
    fn import_and_export_directives_require_static_uris() {
        let extraction = extract_dart_bytes(
            Path::new("/missing/lib/consumer.dart"),
            "lib/consumer.dart",
            br#"import './page.dart';
export 'package:example/api.dart';
import '${page}.dart';
export "$target.dart";
import 'page.dart' + suffix;
import '../../../escape.dart';
import '/absolute.dart';
import 'C:drive.dart';
import './NUL.dart';
import './page.dart.';
import './dir//page.dart';
import './dir/node:page.dart';
"#,
        )
        .expect("extract Dart directives");
        let targets = extraction
            .edges
            .iter()
            .filter(|edge| matches!(edge.relation.as_str(), "imports" | "exports"))
            .map(|edge| edge.true_target())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            targets,
            BTreeSet::from(["lib_page", "package_example_api_dart"])
        );
    }

    #[test]
    fn local_uri_rejects_nonportable_source_identities() {
        for source_file in [
            "/absolute/consumer.dart",
            r"C:\absolute\consumer.dart",
            r"\\server\share\consumer.dart",
            "lib/NUL.dart",
        ] {
            assert_eq!(
                normalized_dart_project_uri(source_file, "./page.dart", true),
                None,
                "source_file={source_file:?}",
            );
        }
    }
}
