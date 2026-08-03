//! Bash extraction and source-aware call facts.
//!
//! The upstream extractor uses tree-sitter for definitions and command context,
//! then resolves calls into sourced files at corpus scope.  We preserve that
//! split by emitting private `__bash_raw_call` edges; `resolution::resolve`
//! consumes them before an extraction leaves the project pipeline.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};
use tree_sitter::{Node as TsNode, Parser};

const SOURCE_COMMANDS: &[&str] = &["source", "."];
const SCRIPT_RUNNERS: &[&str] = &["bash", "sh", "zsh", "ksh", "dash"];

pub(crate) fn extract_bash(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let source = fs::read(path)?;
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_bash::LANGUAGE.into())?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter-bash returned no tree"))?;

    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let file_id = make_id(&[&stem]);
    // Graphify keeps the extension in the script-entry identity even though
    // the file node itself uses the extension-free stem. Keeping those two
    // namespaces distinct also makes invocation targets stable across full
    // and incremental extraction.
    let entry_id = format!("{}__entry", make_id(&[source_file]));
    let mut state = BashState {
        source: &source,
        source_file,
        physical_path: path,
        stem,
        file_id: file_id.clone(),
        entry_id: entry_id.clone(),
        nodes: Vec::new(),
        edges: Vec::new(),
        seen_nodes: HashSet::new(),
        seen_edges: HashSet::new(),
        functions: HashMap::new(),
        function_ranges: HashMap::new(),
        variable_bases: collect_variable_bases(tree.root_node(), &source, path),
    };
    state.add_node(
        file_id.clone(),
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(source_file),
        1,
        "file",
    );
    state.add_node(
        entry_id.clone(),
        &format!(
            "{} script",
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or(source_file)
        ),
        1,
        "bash_entrypoint",
    );
    state.add_edge(
        file_id.clone(),
        entry_id,
        "contains",
        1,
        Confidence::Extracted,
        None,
    );

    state.collect_definitions(tree.root_node(), &file_id);
    state.collect_commands(tree.root_node(), None);

    Ok(Extraction {
        nodes: state.nodes,
        edges: state.edges,
        hyperedges: Vec::new(),
    })
}

struct BashState<'a> {
    source: &'a [u8],
    source_file: &'a str,
    physical_path: &'a Path,
    stem: String,
    file_id: String,
    entry_id: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    seen_nodes: HashSet<String>,
    seen_edges: HashSet<(String, String, String, usize)>,
    functions: HashMap<String, String>,
    function_ranges: HashMap<(usize, usize), String>,
    variable_bases: HashMap<String, PathBuf>,
}

impl BashState<'_> {
    fn text(&self, node: TsNode<'_>) -> String {
        String::from_utf8_lossy(&self.source[node.start_byte()..node.end_byte()]).into_owned()
    }

    fn literal(&self, node: TsNode<'_>) -> Option<String> {
        let mut raw = self.text(node).trim().to_owned();
        if raw.len() >= 2 {
            let bytes = raw.as_bytes();
            if matches!(bytes[0], b'\'' | b'"') && bytes[0] == bytes[raw.len() - 1] {
                raw = raw[1..raw.len() - 1].to_owned();
            }
        }
        if raw.is_empty()
            || ["$", "`", "$(", "<(", ">(", "|", ";", "&"]
                .iter()
                .any(|token| raw.contains(token))
        {
            return None;
        }
        Some(raw)
    }

    fn function_name(&self, node: TsNode<'_>) -> Option<String> {
        if let Some(name) = node.child_by_field_name("name") {
            return self.literal(name);
        }
        let mut cursor = node.walk();
        let name = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "word")
            .and_then(|child| self.literal(child));
        name
    }

    fn add_node(&mut self, id: String, label: &str, line: usize, kind: &str) {
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
                ("_origin".into(), "ast".into()),
                ("type".into(), kind.into()),
                (
                    "metadata".into(),
                    serde_json::json!({"language": "bash", "kind": kind}),
                ),
            ]),
        });
    }

    fn add_edge(
        &mut self,
        source: String,
        target: String,
        relation: &str,
        line: usize,
        confidence: Confidence,
        context: Option<&str>,
    ) {
        if source.is_empty() || target.is_empty() || source == target {
            return;
        }
        if !self
            .seen_edges
            .insert((source.clone(), target.clone(), relation.to_owned(), line))
        {
            return;
        }
        let mut extra = BTreeMap::from([
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), confidence.default_score().into()),
        ]);
        if let Some(context) = context {
            extra.insert("context".into(), context.into());
        }
        self.edges.push(Edge {
            source,
            target,
            relation: relation.into(),
            confidence,
            source_file: self.source_file.into(),
            extra,
        });
    }

    fn collect_definitions(&mut self, node: TsNode<'_>, parent: &str) {
        if node.kind() == "function_definition" {
            let Some(name) = self.function_name(node) else {
                return;
            };
            let id = make_id(&[&self.stem, &name]);
            let line = node.start_position().row + 1;
            self.add_node(id.clone(), &format!("{name}()"), line, "function");
            self.add_edge(
                parent.to_owned(),
                id.clone(),
                "defines",
                line,
                Confidence::Extracted,
                None,
            );
            self.functions.insert(name, id.clone());
            self.function_ranges
                .insert((node.start_byte(), node.end_byte()), id.clone());
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                self.collect_definitions(child, &id);
            }
            return;
        }
        if node.kind() == "variable_assignment" {
            let raw = self.text(node);
            let name = node
                .child_by_field_name("name")
                .map(|name| self.text(name))
                .or_else(|| raw.split_once('=').map(|(name, _)| name.trim().to_owned()))
                .unwrap_or_default();
            if !name.is_empty()
                && name
                    .chars()
                    .all(|character| character == '_' || character.is_ascii_alphanumeric())
                && !name
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_digit())
            {
                let id = make_id(&[&self.stem, &name]);
                let line = node.start_position().row + 1;
                self.add_node(id.clone(), &name, line, "variable");
                self.add_edge(
                    parent.to_owned(),
                    id,
                    "defines",
                    line,
                    Confidence::Extracted,
                    None,
                );
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect_definitions(child, parent);
        }
    }

    fn collect_commands(&mut self, node: TsNode<'_>, owner: Option<&str>) {
        if node.kind() == "function_definition" {
            let id = self
                .function_ranges
                .get(&(node.start_byte(), node.end_byte()))
                .cloned();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                self.collect_commands(child, id.as_deref().or(owner));
            }
            return;
        }
        if node.kind() == "command" && !inside_expansion(node) {
            self.handle_command(node, owner);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect_commands(child, owner);
        }
    }

    fn handle_command(&mut self, node: TsNode<'_>, owner: Option<&str>) {
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| node.named_child(0));
        let Some(name_node) = name_node else { return };
        let Some(command) = self.literal(name_node) else {
            return;
        };
        let line = node.start_position().row + 1;
        let caller = owner.unwrap_or(&self.entry_id).to_owned();

        if let Some(target) = self.functions.get(&command).cloned() {
            self.add_edge(
                caller,
                target,
                "calls",
                line,
                Confidence::Extracted,
                Some("call"),
            );
            return;
        }

        let args = command_arguments(node, name_node);
        if SOURCE_COMMANDS.contains(&command.as_str()) {
            if let Some(argument) = args.first() {
                self.handle_source(argument, line);
            }
            return;
        }

        let script_argument = if SCRIPT_RUNNERS.contains(&command.as_str()) {
            args.first().and_then(|arg| self.literal(*arg))
        } else if command.ends_with(".sh") {
            Some(command.clone())
        } else {
            None
        };
        if let Some(raw) = script_argument.filter(|raw| raw.ends_with(".sh")) {
            let candidate = self
                .physical_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(raw);
            if let Ok(resolved) = candidate.canonicalize() {
                if resolved.is_file() {
                    let target_source =
                        logical_target_source_file(self.physical_path, self.source_file, &resolved);
                    self.add_edge(
                        caller,
                        format!("{}__entry", make_id(&[&target_source])),
                        "calls",
                        line,
                        Confidence::Extracted,
                        Some("script_invocation"),
                    );
                }
            }
            return;
        }

        // Keep unresolved Bash commands out of the public call graph.  The
        // corpus resolver consumes this private fact only when a sourced file
        // defines the callee; ordinary external commands are discarded.
        let placeholder = make_id(&["__bash_raw", &command]);
        self.add_edge(
            caller,
            placeholder,
            "__bash_raw_call",
            line,
            Confidence::Extracted,
            Some("call"),
        );
        if let Some(edge) = self.edges.last_mut() {
            edge.extra.insert("callee".into(), command.into());
        }
    }

    fn handle_source(&mut self, argument: &TsNode<'_>, line: usize) {
        let raw = strip_shell_quotes(&self.text(*argument));
        if raw.is_empty() {
            return;
        }
        let (candidate, confidence) = if raw.starts_with('.') || raw.starts_with('/') {
            (
                Some(
                    self.physical_path
                        .parent()
                        .unwrap_or_else(|| Path::new(""))
                        .join(&raw),
                ),
                Confidence::Extracted,
            )
        } else if raw.contains('$') {
            let Some(suffix) = bash_source_suffix(&raw) else {
                return;
            };
            let base = leading_variable(&raw)
                .and_then(|name| self.variable_bases.get(&name).cloned())
                .unwrap_or_else(|| {
                    self.physical_path
                        .parent()
                        .unwrap_or_else(|| Path::new(""))
                        .to_path_buf()
                });
            (Some(base.join(suffix)), Confidence::Inferred)
        } else {
            let sibling = self
                .physical_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(&raw);
            if sibling.is_file() {
                (Some(sibling), Confidence::Inferred)
            } else {
                // Retain the upstream opaque `imports` fact for a literal bare
                // source name, but never for an expansion that failed safety.
                self.add_edge(
                    self.file_id.clone(),
                    make_id(&[&raw]),
                    "imports",
                    line,
                    Confidence::Extracted,
                    Some("import"),
                );
                return;
            }
        };
        let Some(candidate) = candidate else { return };
        let Ok(resolved) = candidate.canonicalize() else {
            return;
        };
        if !resolved.is_file() {
            return;
        }
        let target_source =
            logical_target_source_file(self.physical_path, self.source_file, &resolved);
        let target_stem = Path::new(&target_source)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        self.add_edge(
            self.file_id.clone(),
            make_id(&[&target_stem]),
            "imports_from",
            line,
            confidence,
            Some("import"),
        );
    }
}

fn inside_expansion(mut node: TsNode<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "command_substitution" | "process_substitution"
        ) {
            return true;
        }
        node = parent;
    }
    false
}

fn command_arguments<'tree>(node: TsNode<'tree>, name: TsNode<'tree>) -> Vec<TsNode<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| {
            child.id() != name.id()
                && matches!(
                    child.kind(),
                    "word" | "string" | "raw_string" | "concatenation"
                )
        })
        .collect()
}

fn strip_shell_quotes(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 {
        let first = raw.as_bytes()[0];
        if matches!(first, b'\'' | b'"') && raw.as_bytes()[raw.len() - 1] == first {
            return raw[1..raw.len() - 1].to_owned();
        }
    }
    raw.to_owned()
}

fn leading_expansion_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| {
        Regex::new(r"^(?:(?:\$\{[^}]*\}|\$[A-Za-z_][A-Za-z0-9_]*)/?)+")
            .expect("valid Bash leading expansion regex")
    })
}

fn leading_variable_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| {
        Regex::new(r"^\$\{([A-Za-z_][A-Za-z0-9_]*)[^}]*\}|^\$([A-Za-z_][A-Za-z0-9_]*)")
            .expect("valid Bash variable regex")
    })
}

fn dirname_idiom_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| {
        Regex::new(r"dirname[^)]*\)((?:/\.\.)*)").expect("valid Bash dirname regex")
    })
}

fn assignment_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| {
        Regex::new(r"(?m)^\s*([A-Za-z_][A-Za-z0-9_]*)=(.+?)\s*$")
            .expect("valid Bash assignment regex")
    })
}

fn bash_source_suffix(raw: &str) -> Option<PathBuf> {
    let suffix = leading_expansion_regex()
        .replace(raw, "")
        .trim_start_matches('/')
        .to_owned();
    if suffix.is_empty() || suffix.contains('$') {
        return None;
    }
    let path = PathBuf::from(suffix);
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    Some(path)
}

fn leading_variable(raw: &str) -> Option<String> {
    let capture = leading_variable_regex().captures(raw)?;
    capture
        .get(1)
        .or_else(|| capture.get(2))
        .map(|value| value.as_str().to_owned())
}

fn collect_variable_bases(
    _root: TsNode<'_>,
    source: &[u8],
    physical_path: &Path,
) -> HashMap<String, PathBuf> {
    let text = String::from_utf8_lossy(source);
    let script_dir = physical_path.parent().unwrap_or_else(|| Path::new(""));
    assignment_regex()
        .captures_iter(&text)
        .filter_map(|capture| {
            assignment_base(&capture[2], script_dir).map(|base| (capture[1].to_owned(), base))
        })
        .collect()
}

fn assignment_base(raw: &str, script_dir: &Path) -> Option<PathBuf> {
    let value = strip_shell_quotes(raw);
    if value.is_empty() {
        return None;
    }
    if value.contains("dirname") && (value.contains("BASH_SOURCE") || value.contains("$0")) {
        let hops = dirname_idiom_regex()
            .captures(&value)
            .and_then(|capture| capture.get(1))
            .map(|value| value.as_str().matches("..").count())
            .unwrap_or(0);
        let mut base = script_dir.to_path_buf();
        for _ in 0..hops {
            base.pop();
        }
        return Some(base);
    }
    if value.contains('$') || value.contains('`') {
        return None;
    }
    let candidate = PathBuf::from(value);
    Some(if candidate.is_absolute() {
        candidate
    } else {
        script_dir.join(candidate)
    })
}

fn inferred_scan_root(physical_path: &Path, source_file: &str) -> Option<PathBuf> {
    if Path::new(source_file).is_absolute() {
        return None;
    }
    let mut root = physical_path.to_path_buf();
    for component in Path::new(source_file).components() {
        if matches!(component, Component::Normal(_)) && !root.pop() {
            return None;
        }
    }
    Some(root)
}

fn logical_target_source_file(physical_path: &Path, source_file: &str, target: &Path) -> String {
    if let Some(root) = inferred_scan_root(physical_path, source_file) {
        if let Ok(relative) = target.strip_prefix(&root) {
            return relative.to_string_lossy().replace('\\', "/");
        }
        let mut tail = target.components().rev().take(3).collect::<Vec<_>>();
        tail.reverse();
        return PathBuf::from("ext")
            .join(tail.iter().collect::<PathBuf>())
            .to_string_lossy()
            .replace('\\', "/");
    }
    target.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_rejects_dynamic_and_parent_paths() {
        assert_eq!(
            bash_source_suffix("${ROOT}/lib/a.sh"),
            Some(PathBuf::from("lib/a.sh"))
        );
        assert_eq!(bash_source_suffix("$ROOT"), None);
        assert_eq!(bash_source_suffix("${ROOT}/lib/${NAME}.sh"), None);
        assert_eq!(bash_source_suffix("${ROOT}/../secret.sh"), None);
    }
}
