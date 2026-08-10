//! Byte-only, bounded extractors for engineering diagram source files.
//!
//! The module deliberately accepts bytes and a logical source name rather than
//! a filesystem path.  It is intended to run on the compute pool after the
//! I/O plane has admitted a bounded source allocation.  Textual diagram
//! languages do not share one grammar. Graphviz DOT is delegated to its
//! complete bounded parser; the remaining adapters conservatively emit only
//! structure explicit in their source format. Invalid XML/JSON is an error
//! (and therefore a caller diagnostic), never a partly-successful semantic
//! extraction.

use anyhow::{bail, ensure, Context as _};
use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use quick_xml::{events::Event, Reader};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::LazyLock,
};

use crate::bytes::LineIndex;

/// Deliberately below the runtime's normal source admission limit: diagram
/// parsers have per-file semantic limits and must not consume all CPU-arena
/// capacity merely because one diagram is unusually large.
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODES: usize = 100_000;
const MAX_EDGES: usize = 250_000;
const MAX_LABEL_BYTES: usize = 512;

static DECLARATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?mi)^\s*(?:abstract\s+)?(?P<kind>class|interface|enum|entity|actor|participant|boundary|control|database|component|node|cloud|queue|usecase|state|package|namespace|rectangle|folder|frame)\s*(?:\"(?P<quoted>[^\"]+)\"|(?P<name>[A-Za-z_][A-Za-z0-9_.:-]*))"#)
        .expect("valid declaration expression")
});
static MERMAID_NODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?P<id>[A-Za-z_][A-Za-z0-9_.:-]*)(?:\s*[\[\(\{]\s*(?P<label>[^\]\)\}]+)\s*[\]\)\}])?"#,
    )
    .expect("valid Mermaid endpoint expression")
});
static STRUCTURIZR_ELEMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?mi)^\s*(?:(?P<alias>[A-Za-z_][A-Za-z0-9_]*)\s*=\s*)?(?P<kind>person|softwareSystem|container|component|deploymentNode)\s+\"(?P<label>[^\"]+)\""#)
        .expect("valid Structurizr element expression")
});
static STRUCTURIZR_REL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*(?P<a>[A-Za-z_][A-Za-z0-9_]*)\s*->\s*(?P<b>[A-Za-z_][A-Za-z0-9_]*)(?:\s+\"(?P<label>[^\"]+)\")?"#)
        .expect("valid Structurizr relation expression")
});
static DBML_TABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?mi)^\s*Table\s+(?P<name>[A-Za-z_][A-Za-z0-9_.]*)\s*\{"#)
        .expect("valid DBML table expression")
});
static DBML_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?mi)^\s*Ref(?:\s+\"[^\"]+\")?\s*:\s*(?P<a>[A-Za-z_][A-Za-z0-9_.]*)\s*(?P<op><|>|-|<>)\s*(?P<b>[A-Za-z_][A-Za-z0-9_.]*)"#)
        .expect("valid DBML reference expression")
});
static D2_OBJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*(?P<id>[A-Za-z_][A-Za-z0-9_.:-]*)\s*:\s*(?:\"(?P<label>[^\"]+)\"|(?P<label_plain>[A-Za-z_][A-Za-z0-9 _.-]*))\s*(?:\{|$)"#)
        .expect("valid D2 object expression")
});
static D2_EDGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*(?P<a>[A-Za-z_][A-Za-z0-9_.:-]*)\s*(?P<op><->|->|<-|--)\s*(?P<b>[A-Za-z_][A-Za-z0-9_.:-]*)(?:\s*:\s*\"?(?P<label>[^\"\n]+)\"?)?"#)
        .expect("valid D2 edge expression")
});
static GANTT_TASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?mi)^\s*(?P<label>[^:\n]+?)\s*:\s*(?P<timing>[^\n]+)$"#)
        .expect("valid Mermaid Gantt task expression")
});

/// Extract explicit diagram facts from a source allocation already admitted by
/// the I/O plane.  No filesystem operation is performed by this function.
pub(crate) fn extract_diagram_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    ensure!(
        bytes.len() <= MAX_BYTES,
        "diagram source exceeds {MAX_BYTES} byte limit"
    );
    let format = format_for(path, bytes);
    if format == DiagramFormat::Dot {
        return crate::dot::extract_dot_bytes(source_file, bytes);
    }
    let text = std::str::from_utf8(bytes)
        .with_context(|| format!("diagram source is not UTF-8: {source_file}"))?;
    let mut state = DiagramState::new(source_file, format, bytes);
    match format {
        DiagramFormat::Dot => unreachable!("DOT uses its dedicated grammar-aware parser"),
        DiagramFormat::Mermaid => parse_mermaid(text, &mut state),
        DiagramFormat::PlantUml => parse_plantuml(text, &mut state),
        DiagramFormat::D2 => parse_d2(text, &mut state),
        DiagramFormat::StructurizrDsl => parse_structurizr_dsl(text, &mut state),
        DiagramFormat::StructurizrJson => parse_structurizr_json(text, &mut state)?,
        DiagramFormat::Dbml => parse_dbml(text, &mut state),
        DiagramFormat::Bpmn => parse_xml(text, &mut state, XmlDialect::Bpmn)?,
        DiagramFormat::Xmi => parse_xml(text, &mut state, XmlDialect::Xmi)?,
        DiagramFormat::Drawio => parse_xml(text, &mut state, XmlDialect::Drawio)?,
        DiagramFormat::Excalidraw => parse_excalidraw(text, &mut state)?,
        DiagramFormat::Tldraw => parse_tldraw(text, &mut state)?,
        DiagramFormat::Unknown => {
            state.status = "unrecognized";
        }
    }
    Ok(state.finish())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagramFormat {
    Dot,
    Mermaid,
    PlantUml,
    D2,
    StructurizrDsl,
    StructurizrJson,
    Dbml,
    Bpmn,
    Xmi,
    Drawio,
    Excalidraw,
    Tldraw,
    Unknown,
}

impl DiagramFormat {
    const fn name(self) -> &'static str {
        match self {
            Self::Dot => "graphviz",
            Self::Mermaid => "mermaid",
            Self::PlantUml => "plantuml",
            Self::D2 => "d2",
            Self::StructurizrDsl | Self::StructurizrJson => "structurizr",
            Self::Dbml => "dbml",
            Self::Bpmn => "bpmn",
            Self::Xmi => "xmi",
            Self::Drawio => "drawio",
            Self::Excalidraw => "excalidraw",
            Self::Tldraw => "tldraw",
            Self::Unknown => "unknown",
        }
    }
}

fn format_for(path: &Path, source: &[u8]) -> DiagramFormat {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let head = &source[..source.len().min(4096)];
    if matches!(extension.as_str(), "dot" | "gv" | "graphviz") {
        return DiagramFormat::Dot;
    }
    if matches!(extension.as_str(), "mmd" | "mermaid") {
        return DiagramFormat::Mermaid;
    }
    if matches!(extension.as_str(), "puml" | "plantuml" | "iuml" | "pu") {
        return DiagramFormat::PlantUml;
    }
    if extension == "d2" {
        return DiagramFormat::D2;
    }
    if extension == "dbml" {
        return DiagramFormat::Dbml;
    }
    if matches!(extension.as_str(), "bpmn" | "bpmn2") {
        return DiagramFormat::Bpmn;
    }
    if matches!(extension.as_str(), "xmi" | "uml" | "uml2" | "sysml") {
        return DiagramFormat::Xmi;
    }
    if extension == "drawio" || (extension == "xml" && contains(head, b"mxGraphModel")) {
        return DiagramFormat::Drawio;
    }
    if extension == "excalidraw" || contains(head, b"excalidraw") {
        return DiagramFormat::Excalidraw;
    }
    if matches!(extension.as_str(), "tldr" | "tldraw") || contains(head, b"tldraw") {
        return DiagramFormat::Tldraw;
    }
    if extension == "dsl" || extension == "structurizr" {
        return DiagramFormat::StructurizrDsl;
    }
    if extension == "json" && (name.contains("structurizr") || contains(head, b"softwareSystems")) {
        return DiagramFormat::StructurizrJson;
    }
    if contains(head, b"@start") {
        return DiagramFormat::PlantUml;
    }
    if contains(head, b"flowchart")
        || contains(head, b"sequenceDiagram")
        || contains(head, b"gantt")
    {
        return DiagramFormat::Mermaid;
    }
    DiagramFormat::Unknown
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    crate::bytes::contains_nonempty_subslice(haystack, needle)
}

struct DiagramState<'a> {
    source_file: &'a str,
    format: DiagramFormat,
    stem: String,
    root: String,
    lines: LineIndex,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    ids: BTreeMap<String, String>,
    edge_set: BTreeSet<(String, String, String, usize)>,
    status: &'static str,
    truncated: bool,
}

impl<'a> DiagramState<'a> {
    fn new(source_file: &'a str, format: DiagramFormat, source: &[u8]) -> Self {
        let stem = Path::new(source_file)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let root = make_id(&[&stem, "diagram"]);
        let lines = LineIndex::new(source);
        let mut state = Self {
            source_file,
            format,
            stem,
            root: root.clone(),
            lines,
            nodes: Vec::new(),
            edges: Vec::new(),
            ids: BTreeMap::new(),
            edge_set: BTreeSet::new(),
            // These adapters are deliberately conservative structural readers,
            // not complete language implementations.  Keep this explicit so
            // downstream capability reporting never overclaims semantics.
            status: "partial",
            truncated: false,
        };
        let root_admitted = crate::parser_budget::try_reserve_facts(1);
        if root_admitted {
            state.nodes.push(Node {
                id: root,
                label: Path::new(source_file)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(source_file)
                    .to_owned(),
                file_type: "document".into(),
                source_file: source_file.into(),
                source_location: Some("L1".into()),
                community: None,
                extra: BTreeMap::from([
                    ("_origin".into(), "diagram".into()),
                    ("diagram_format".into(), format.name().into()),
                    ("type".into(), "diagram".into()),
                ]),
            });
        } else {
            state.truncated = true;
        }
        state
    }

    fn line(&self, offset: usize) -> usize {
        self.lines.line_of(offset)
    }

    fn node(&mut self, local: &str, label: &str, kind: &str, offset: usize) -> String {
        let local = clean_identifier(local);
        if local.is_empty() {
            return self.root.clone();
        }
        if let Some(id) = self.ids.get(&local) {
            return id.clone();
        }
        if self.nodes.len() >= MAX_NODES {
            self.truncated = true;
            return self.root.clone();
        }
        if !crate::parser_budget::try_reserve_facts(1) {
            self.truncated = true;
            return self.root.clone();
        }
        let id = make_id(&[&self.stem, "diagram", self.format.name(), &local]);
        let line = self.line(offset);
        self.nodes.push(Node {
            id: id.clone(),
            label: bounded_label(label),
            file_type: "document".into(),
            source_file: self.source_file.into(),
            source_location: Some(format!("L{line}")),
            community: None,
            extra: BTreeMap::from([
                ("_origin".into(), "diagram".into()),
                ("diagram_format".into(), self.format.name().into()),
                ("diagram_kind".into(), kind.into()),
                ("type".into(), kind.into()),
            ]),
        });
        self.ids.insert(local, id.clone());
        self.edge(&self.root.clone(), &id, "contains", None, offset);
        id
    }

    fn node_with_meta(
        &mut self,
        local: &str,
        label: &str,
        kind: &str,
        offset: usize,
        meta: BTreeMap<String, Value>,
    ) -> String {
        let id = self.node(local, label, kind, offset);
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            node.extra.extend(meta);
        }
        id
    }

    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        label: Option<&str>,
        offset: usize,
    ) {
        if source == target || self.edges.len() >= MAX_EDGES {
            self.truncated |= self.edges.len() >= MAX_EDGES;
            return;
        }
        let line = self.line(offset);
        // Charge the attempted retained fact before allocating its owned
        // deduplication key. Duplicate attempts conservatively consume a
        // credit in the managed parser estimate.
        if !crate::parser_budget::try_reserve_facts(1) {
            self.truncated = true;
            return;
        }
        let key = (
            source.to_owned(),
            target.to_owned(),
            relation.to_owned(),
            line,
        );
        if self.edge_set.contains(&key) {
            return;
        }
        self.edge_set.insert(key);
        let mut extra = BTreeMap::from([
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
            ("source_location".into(), format!("L{line}").into()),
            ("diagram_format".into(), self.format.name().into()),
            ("weight".into(), 1.0.into()),
        ]);
        if let Some(label) = label.filter(|value| !value.trim().is_empty()) {
            extra.insert("label".into(), bounded_label(label).into());
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

    fn contain(&mut self, parent: &str, child: &str, offset: usize) {
        self.edge(parent, child, "contains", None, offset);
    }

    fn finish(mut self) -> Extraction {
        if let Some(root) = self.nodes.first_mut() {
            root.extra.insert("parse_status".into(), self.status.into());
            if self.truncated {
                root.extra.insert("truncated".into(), true.into());
            }
        }
        Extraction {
            nodes: self.nodes,
            edges: self.edges,
            hyperedges: Vec::new(),
        }
    }
}

fn bounded_label(value: &str) -> String {
    let value = value.trim().trim_matches(['\"', '\'', '`']);
    if value.len() <= MAX_LABEL_BYTES {
        return value.into();
    }
    let mut end = MAX_LABEL_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn clean_identifier(value: &str) -> String {
    value
        .trim()
        .trim_matches(['\"', '\'', '`', '[', ']', '(', ')', '{', '}', '<', '>'])
        .trim()
        .to_owned()
}

fn parse_mermaid(source: &str, state: &mut DiagramState<'_>) {
    let mut gantt_section: Option<String> = None;
    for (line_no, line) in source.lines().enumerate() {
        let offset = offset_of_line(source, line_no);
        let trim = line.trim();
        if trim.is_empty() || trim.starts_with("%%") {
            continue;
        }
        if let Some(name) = trim
            .strip_prefix("participant ")
            .or_else(|| trim.strip_prefix("actor "))
        {
            let (id, label) = split_alias(name);
            state.node(id, label, "participant", offset);
            continue;
        }
        if let Some(name) = trim.strip_prefix("class ") {
            state.node(name, name, "class", offset);
            continue;
        }
        if let Some(name) = trim.strip_prefix("state ") {
            let name = name.trim_end_matches('{').trim();
            if !name.is_empty() {
                state.node(name, name, "state", offset);
            }
            continue;
        }
        if let Some(name) = trim.strip_prefix("section ") {
            let id = state.node(name, name, "gantt_section", offset);
            gantt_section = Some(id);
            continue;
        }
        if trim.starts_with("gantt") && !trim.contains(":") {
            continue;
        }
        if let Some(captures) = GANTT_TASK
            .captures(trim)
            .filter(|_| gantt_section.is_some())
        {
            let label = captures.name("label").expect("label").as_str().trim();
            let timing = captures.name("timing").expect("timing").as_str().trim();
            let local = timing
                .split(',')
                .find_map(|part| {
                    let part = part.trim();
                    (!part.contains(' ') && !part.chars().all(|ch| ch.is_ascii_digit()))
                        .then_some(part)
                })
                .unwrap_or(label);
            let task = state.node_with_meta(
                local,
                label,
                "gantt_task",
                offset,
                BTreeMap::from([("timing".into(), bounded_label(timing).into())]),
            );
            if let Some(section) = &gantt_section {
                state.contain(section, &task, offset);
            }
            if let Some(after) = timing
                .split("after ")
                .nth(1)
                .and_then(|part| part.split(',').next())
            {
                let predecessor = state.node(after.trim(), after.trim(), "gantt_task", offset);
                state.edge(&predecessor, &task, "precedes", None, offset);
            }
            continue;
        }
        if let Some((a, op, b, label)) = parse_arrow(trim) {
            let (a_local, a_label) = mermaid_endpoint(a);
            let (b_local, b_label) = mermaid_endpoint(b);
            if a_local.is_empty() || b_local.is_empty() {
                continue;
            }
            let a_id = state.node(&a_local, &a_label, mermaid_kind(trim), offset);
            let b_id = state.node(&b_local, &b_label, mermaid_kind(trim), offset);
            let relation = if op.contains(">>") {
                "message_to"
            } else if op.contains("--") && !op.contains('>') {
                "connected_to"
            } else {
                "flows_to"
            };
            state.edge(&a_id, &b_id, relation, label, offset);
        }
    }
}

fn mermaid_kind(line: &str) -> &'static str {
    if line.contains("<|--") {
        "class"
    } else if line.contains("||--") {
        "entity"
    } else {
        "diagram_node"
    }
}

fn mermaid_endpoint(value: &str) -> (String, String) {
    let value = value.trim();
    if let Some(captures) = MERMAID_NODE.captures(value) {
        let id = captures
            .name("id")
            .map_or("", |value| value.as_str())
            .to_owned();
        let label = captures
            .name("label")
            .map_or_else(|| id.clone(), |value| value.as_str().trim().to_owned());
        return (id, label);
    }
    let cleaned = clean_identifier(value);
    (cleaned.clone(), cleaned)
}

fn parse_plantuml(source: &str, state: &mut DiagramState<'_>) {
    for (line_no, line) in source.lines().enumerate() {
        let offset = offset_of_line(source, line_no);
        let trim = line.trim();
        if trim.is_empty()
            || trim.starts_with('\'')
            || trim.starts_with("@")
            || trim.starts_with("!")
        {
            continue;
        }
        if let Some(captures) = DECLARATION.captures(trim) {
            let name = captures
                .name("quoted")
                .or_else(|| captures.name("name"))
                .expect("name")
                .as_str();
            let kind = captures
                .name("kind")
                .expect("kind")
                .as_str()
                .to_ascii_lowercase();
            state.node(name, name, &kind, offset);
            continue;
        }
        if let Some((a, op, b, label)) = parse_arrow(trim) {
            let a = clean_identifier(a);
            let b = clean_identifier(b);
            if a.is_empty() || b.is_empty() {
                continue;
            }
            let a_id = state.node(&a, &a, "diagram_node", offset);
            let b_id = state.node(&b, &b, "diagram_node", offset);
            let relation = if op.contains("<|--") {
                "inherits"
            } else if op.contains("--|>") {
                "implements"
            } else if op.contains("..") {
                "depends_on"
            } else {
                "flows_to"
            };
            state.edge(&a_id, &b_id, relation, label, offset);
        }
    }
}

fn parse_d2(source: &str, state: &mut DiagramState<'_>) {
    for captures in D2_OBJECT.captures_iter(source) {
        let whole = captures.get(0).expect("match");
        let id = captures.name("id").expect("id").as_str();
        let label = captures
            .name("label")
            .or_else(|| captures.name("label_plain"))
            .map_or(id, |value| value.as_str());
        state.node(id, label, "diagram_node", whole.start());
    }
    for captures in D2_EDGE.captures_iter(source) {
        let whole = captures.get(0).expect("match");
        let a = captures.name("a").expect("a").as_str();
        let b = captures.name("b").expect("b").as_str();
        let a_id = state.node(a, a, "diagram_node", whole.start());
        let b_id = state.node(b, b, "diagram_node", whole.start());
        let label = captures.name("label").map(|value| value.as_str().trim());
        let op = captures.name("op").expect("op").as_str();
        state.edge(
            &a_id,
            &b_id,
            if op == "--" {
                "connected_to"
            } else {
                "flows_to"
            },
            label,
            whole.start(),
        );
    }
}

fn parse_structurizr_dsl(source: &str, state: &mut DiagramState<'_>) {
    let mut aliases = BTreeMap::new();
    for captures in STRUCTURIZR_ELEMENT.captures_iter(source) {
        let whole = captures.get(0).expect("match");
        let label = captures.name("label").expect("label").as_str();
        let local = captures.name("alias").map_or(label, |value| value.as_str());
        let kind = captures.name("kind").expect("kind").as_str();
        let id = state.node(local, label, kind, whole.start());
        aliases.insert(local.to_owned(), id);
    }
    for captures in STRUCTURIZR_REL.captures_iter(source) {
        let whole = captures.get(0).expect("match");
        let a = captures.name("a").expect("a").as_str();
        let b = captures.name("b").expect("b").as_str();
        let a_id = aliases
            .get(a)
            .cloned()
            .unwrap_or_else(|| state.node(a, a, "element", whole.start()));
        let b_id = aliases
            .get(b)
            .cloned()
            .unwrap_or_else(|| state.node(b, b, "element", whole.start()));
        state.edge(
            &a_id,
            &b_id,
            "uses",
            captures.name("label").map(|value| value.as_str()),
            whole.start(),
        );
    }
}

fn parse_structurizr_json(source: &str, state: &mut DiagramState<'_>) -> anyhow::Result<()> {
    let value: Value = serde_json::from_str(source).context("parse Structurizr JSON")?;
    let model = value.get("model").unwrap_or(&value);
    let mut aliases = BTreeMap::new();
    walk_structurizr_json(model, None, state, &mut aliases, 0)?;
    Ok(())
}

fn walk_structurizr_json(
    value: &Value,
    parent: Option<String>,
    state: &mut DiagramState<'_>,
    aliases: &mut BTreeMap<String, String>,
    depth: usize,
) -> anyhow::Result<()> {
    ensure!(depth <= 64, "Structurizr JSON nesting exceeds limit");
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    let type_name = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("element");
    let is_element = object.contains_key("name")
        && matches!(
            type_name,
            "Person" | "SoftwareSystem" | "Container" | "Component" | "DeploymentNode" | "element"
        );
    let current = if is_element {
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unnamed");
        let local = object
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| object.get("name").and_then(Value::as_str))
            .unwrap_or(name);
        let id = state.node(local, name, &type_name.to_ascii_lowercase(), 0);
        aliases.insert(local.into(), id.clone());
        if let Some(parent) = parent.as_deref() {
            state.contain(parent, &id, 0);
        }
        Some(id)
    } else {
        parent
    };
    if let Some(relationships) = object.get("relationships").and_then(Value::as_array) {
        for relationship in relationships {
            let source = relationship
                .get("sourceId")
                .and_then(Value::as_str)
                .or_else(|| object.get("id").and_then(Value::as_str));
            let target = relationship.get("destinationId").and_then(Value::as_str);
            let (Some(source), Some(target)) = (source, target) else {
                continue;
            };
            let source = aliases
                .get(source)
                .cloned()
                .unwrap_or_else(|| state.node(source, source, "element", 0));
            let target = aliases
                .get(target)
                .cloned()
                .unwrap_or_else(|| state.node(target, target, "element", 0));
            state.edge(
                &source,
                &target,
                "uses",
                relationship.get("description").and_then(Value::as_str),
                0,
            );
        }
    }
    for key in [
        "softwareSystems",
        "containers",
        "components",
        "deploymentNodes",
        "children",
    ] {
        if let Some(items) = object.get(key).and_then(Value::as_array) {
            for item in items {
                walk_structurizr_json(item, current.clone(), state, aliases, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn parse_dbml(source: &str, state: &mut DiagramState<'_>) {
    for captures in DBML_TABLE.captures_iter(source) {
        let whole = captures.get(0).expect("match");
        let name = captures.name("name").expect("name").as_str();
        state.node(name, name, "table", whole.start());
    }
    for captures in DBML_REF.captures_iter(source) {
        let whole = captures.get(0).expect("match");
        let a = captures.name("a").expect("a").as_str();
        let b = captures.name("b").expect("b").as_str();
        let a_table = a.split('.').next().unwrap_or(a);
        let b_table = b.split('.').next().unwrap_or(b);
        let a_id = state.node(a_table, a_table, "table", whole.start());
        let b_id = state.node(b_table, b_table, "table", whole.start());
        let relation = match captures.name("op").expect("op").as_str() {
            ">" => "references",
            "<" => "referenced_by",
            "<>" => "related_to",
            _ => "connected_to",
        };
        state.edge(
            &a_id,
            &b_id,
            relation,
            Some(&format!("{a} {relation} {b}")),
            whole.start(),
        );
    }
}

#[derive(Clone, Copy)]
enum XmlDialect {
    Bpmn,
    Xmi,
    Drawio,
}

#[derive(Default)]
struct XmlParseState {
    parents: Vec<String>,
    drawio_parents: BTreeMap<String, String>,
    drawio_edges: Vec<(String, String, Option<String>, usize)>,
    bpmn_lanes: Vec<(String, String, usize)>,
}

fn parse_xml(
    source: &str,
    state: &mut DiagramState<'_>,
    dialect: XmlDialect,
) -> anyhow::Result<()> {
    let mut reader = Reader::from_str(source);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut xml = XmlParseState::default();
    let mut open_elements = 0usize;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) => {
                open_elements = open_elements.saturating_add(1);
                process_xml_start(
                    &event,
                    false,
                    reader.buffer_position() as usize,
                    dialect,
                    state,
                    &mut xml,
                )?;
            }
            Ok(Event::Empty(event)) => {
                process_xml_start(
                    &event,
                    true,
                    reader.buffer_position() as usize,
                    dialect,
                    state,
                    &mut xml,
                )?;
            }
            Ok(Event::End(event)) => {
                open_elements = open_elements
                    .checked_sub(1)
                    .context("unexpected XML end tag")?;
                let tag = local_name(event.name().as_ref());
                if matches!(
                    tag.as_str(),
                    "subProcess" | "process" | "lane" | "packagedElement" | "mxCell"
                ) {
                    xml.parents.pop();
                }
            }
            Ok(Event::DocType(_)) | Ok(Event::GeneralRef(_)) => {
                bail!("unsafe XML entity declaration in diagram")
            }
            Ok(Event::Eof) => {
                ensure!(
                    open_elements == 0,
                    "malformed XML diagram: unclosed element"
                );
                break;
            }
            Err(error) => return Err(anyhow::anyhow!("malformed XML diagram: {error}")),
            _ => {}
        }
        buffer.clear();
    }
    if matches!(dialect, XmlDialect::Drawio) {
        for (source, target, label, offset) in xml.drawio_edges {
            let source = state.node(&source, &source, "shape", offset);
            let target = state.node(&target, &target, "shape", offset);
            state.edge(&source, &target, "flows_to", label.as_deref(), offset);
        }
        for (child, parent) in xml.drawio_parents {
            let child = state.node(&child, &child, "shape", 0);
            let parent = state.node(&parent, &parent, "group", 0);
            state.contain(&parent, &child, 0);
        }
    }
    if matches!(dialect, XmlDialect::Bpmn) {
        for (lane, member, offset) in xml.bpmn_lanes {
            let lane = state.node(&lane, &lane, "lane", offset);
            let member = state.node(&member, &member, "flow_node", offset);
            state.contain(&lane, &member, offset);
        }
    }
    Ok(())
}

fn process_xml_start(
    event: &quick_xml::events::BytesStart<'_>,
    is_empty: bool,
    offset: usize,
    dialect: XmlDialect,
    state: &mut DiagramState<'_>,
    xml: &mut XmlParseState,
) -> anyhow::Result<()> {
    let tag = local_name(event.name().as_ref());
    let attrs = xml_attributes(event)?;
    match dialect {
        XmlDialect::Bpmn => parse_bpmn_event(
            &tag,
            &attrs,
            offset,
            state,
            &xml.parents,
            &mut xml.bpmn_lanes,
        ),
        XmlDialect::Xmi => parse_xmi_event(&tag, &attrs, offset, state, &xml.parents),
        XmlDialect::Drawio => parse_drawio_event(
            &tag,
            &attrs,
            offset,
            state,
            &mut xml.drawio_parents,
            &mut xml.drawio_edges,
        ),
    }
    // `quick-xml` returns Empty separately. For starts, retain a semantic
    // owner only for recognized containers; the pop on unknown end tags is
    // deliberately guarded by the same tag set in the caller.
    if !is_empty
        && matches!(
            tag.as_str(),
            "subProcess" | "process" | "lane" | "packagedElement" | "mxCell"
        )
        && let Some(id) = attrs.get("id").or_else(|| attrs.get("xmi:id"))
    {
        xml.parents.push(id.clone());
    }
    Ok(())
}

fn xml_attributes(
    event: &quick_xml::events::BytesStart<'_>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut attrs = BTreeMap::new();
    for attribute in event.attributes().with_checks(false) {
        let attribute = attribute.context("malformed XML attribute")?;
        let key = String::from_utf8_lossy(attribute.key.as_ref()).into_owned();
        let value = crate::decode_xml_attribute(attribute.value.as_ref())
            .context("decode XML attribute")?
            .into_owned();
        attrs.insert(key, value);
    }
    Ok(attrs)
}

fn local_name(name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    name.rsplit(':').next().unwrap_or(&name).to_owned()
}

fn parse_bpmn_event(
    tag: &str,
    attrs: &BTreeMap<String, String>,
    offset: usize,
    state: &mut DiagramState<'_>,
    parents: &[String],
    lanes: &mut Vec<(String, String, usize)>,
) {
    let id = attrs.get("id").cloned();
    let name = attrs
        .get("name")
        .cloned()
        .or_else(|| id.clone())
        .unwrap_or_else(|| tag.into());
    if tag == "sequenceFlow" || tag == "messageFlow" {
        if let (Some(source), Some(target)) = (attrs.get("sourceRef"), attrs.get("targetRef")) {
            let source = state.node(source, source, "flow_node", offset);
            let target = state.node(target, target, "flow_node", offset);
            state.edge(
                &source,
                &target,
                if tag == "messageFlow" {
                    "message_to"
                } else {
                    "flows_to"
                },
                attrs.get("name").map(String::as_str),
                offset,
            );
        }
        return;
    }
    if tag == "flowNodeRef" {
        if let Some(parent) = parents.last() {
            lanes.push((parent.clone(), name, offset));
        }
        return;
    }
    if let Some(id) = id.filter(|_| is_bpmn_node(tag)) {
        let node = state.node(&id, &name, bpmn_kind(tag), offset);
        if let Some(parent) = parents.last() {
            let parent = state.node(parent, parent, "container", offset);
            state.contain(&parent, &node, offset);
        }
    }
}

fn is_bpmn_node(tag: &str) -> bool {
    tag.ends_with("Event")
        || tag.ends_with("Task")
        || tag.ends_with("Gateway")
        || matches!(
            tag,
            "subProcess" | "callActivity" | "participant" | "lane" | "process"
        )
}
fn bpmn_kind(tag: &str) -> &'static str {
    if tag.ends_with("Event") {
        "event"
    } else if tag.ends_with("Task") || tag == "callActivity" {
        "task"
    } else if tag.ends_with("Gateway") {
        "gateway"
    } else if tag == "lane" {
        "lane"
    } else {
        "container"
    }
}

fn parse_xmi_event(
    tag: &str,
    attrs: &BTreeMap<String, String>,
    offset: usize,
    state: &mut DiagramState<'_>,
    parents: &[String],
) {
    let type_name = attrs
        .get("xmi:type")
        .or_else(|| attrs.get("type"))
        .map(String::as_str)
        .unwrap_or(tag);
    let id = attrs.get("xmi:id").or_else(|| attrs.get("id"));
    let name = attrs
        .get("name")
        .or(id)
        .map(String::as_str)
        .unwrap_or(type_name);
    if let Some(id) = id.filter(|_| is_xmi_element(type_name, tag)) {
        let node = state.node(id, name, xmi_kind(type_name), offset);
        if let Some(parent) = parents.last() {
            let parent = state.node(parent, parent, "package", offset);
            state.contain(&parent, &node, offset);
        }
        if let Some(general) = attrs.get("general") {
            let target = state.node(general, general, "class", offset);
            state.edge(&node, &target, "inherits", None, offset);
        }
        if let Some(type_ref) = attrs.get("type") {
            let target = state.node(type_ref, type_ref, "type", offset);
            state.edge(&node, &target, "has_type", None, offset);
        }
    }
}
fn is_xmi_element(type_name: &str, tag: &str) -> bool {
    tag == "packagedElement"
        || type_name.contains("Class")
        || type_name.contains("Interface")
        || type_name.contains("Component")
        || type_name.contains("State")
        || type_name.contains("UseCase")
        || type_name.contains("Package")
        || type_name.contains("Block")
}
fn xmi_kind(type_name: &str) -> &'static str {
    if type_name.contains("Interface") {
        "interface"
    } else if type_name.contains("Component") {
        "component"
    } else if type_name.contains("State") {
        "state"
    } else if type_name.contains("Package") {
        "package"
    } else if type_name.contains("Block") {
        "block"
    } else {
        "class"
    }
}

fn parse_drawio_event(
    tag: &str,
    attrs: &BTreeMap<String, String>,
    offset: usize,
    state: &mut DiagramState<'_>,
    parents: &mut BTreeMap<String, String>,
    edges: &mut Vec<(String, String, Option<String>, usize)>,
) {
    if tag != "mxCell" {
        return;
    }
    let Some(id) = attrs.get("id") else {
        return;
    };
    if matches!(id.as_str(), "0" | "1") {
        return;
    }
    let label = attrs
        .get("value")
        .map(|value| strip_markup(value))
        .unwrap_or_else(|| id.clone());
    if attrs.get("edge").is_some_and(|value| value == "1") {
        if let (Some(source), Some(target)) = (attrs.get("source"), attrs.get("target")) {
            edges.push((
                source.clone(),
                target.clone(),
                (!label.is_empty()).then_some(label),
                offset,
            ));
        }
    } else if attrs.get("vertex").is_some_and(|value| value == "1") {
        state.node(
            id,
            &label,
            if attrs
                .get("style")
                .is_some_and(|style| style.contains("swimlane"))
            {
                "lane"
            } else {
                "shape"
            },
            offset,
        );
        if let Some(parent) = attrs
            .get("parent")
            .filter(|parent| !matches!(parent.as_str(), "0" | "1"))
        {
            parents.insert(id.clone(), parent.clone());
        }
    }
}

fn parse_excalidraw(source: &str, state: &mut DiagramState<'_>) -> anyhow::Result<()> {
    let value: Value = serde_json::from_str(source).context("parse Excalidraw JSON")?;
    let elements = value
        .get("elements")
        .and_then(Value::as_array)
        .context("Excalidraw document has no elements array")?;
    let mut bindings = Vec::new();
    for element in elements {
        let Some(id) = element.get("id").and_then(Value::as_str) else {
            continue;
        };
        let kind = element
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("shape");
        let label = element.get("text").and_then(Value::as_str).unwrap_or(id);
        let node = state.node(id, label, kind, 0);
        if let Some(container) = element.get("containerId").and_then(Value::as_str) {
            let container = state.node(container, container, "container", 0);
            state.contain(&container, &node, 0);
        }
        if kind == "arrow" {
            let start = element
                .get("startBinding")
                .and_then(|binding| binding.get("elementId"))
                .and_then(Value::as_str);
            let end = element
                .get("endBinding")
                .and_then(|binding| binding.get("elementId"))
                .and_then(Value::as_str);
            if let (Some(start), Some(end)) = (start, end) {
                bindings.push((
                    start.to_owned(),
                    end.to_owned(),
                    (!label.is_empty()).then_some(label.to_owned()),
                ));
            }
        }
    }
    for (source, target, label) in bindings {
        let source = state.node(&source, &source, "shape", 0);
        let target = state.node(&target, &target, "shape", 0);
        state.edge(&source, &target, "flows_to", label.as_deref(), 0);
    }
    Ok(())
}

fn parse_tldraw(source: &str, state: &mut DiagramState<'_>) -> anyhow::Result<()> {
    let value: Value = serde_json::from_str(source).context("parse tldraw JSON")?;
    let records = value
        .get("records")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .context("tldraw document has no records array")?;
    let mut bindings = Vec::new();
    for record in records {
        let type_name = record.get("typeName").and_then(Value::as_str).unwrap_or("");
        if type_name == "shape" {
            let Some(id) = record.get("id").and_then(Value::as_str) else {
                continue;
            };
            let kind = record
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("shape");
            let label = record
                .pointer("/props/text")
                .and_then(Value::as_str)
                .unwrap_or(id);
            let node = state.node(id, label, kind, 0);
            if let Some(parent) = record
                .get("parentId")
                .and_then(Value::as_str)
                .filter(|parent| parent.starts_with("shape:"))
            {
                let parent = state.node(parent, parent, "group", 0);
                state.contain(&parent, &node, 0);
            }
        } else if type_name == "binding"
            && let (Some(from), Some(to)) = (
                record.get("fromId").and_then(Value::as_str),
                record.get("toId").and_then(Value::as_str),
            )
        {
            bindings.push((from.to_owned(), to.to_owned()));
        }
    }
    for (from, to) in bindings {
        let from = state.node(&from, &from, "shape", 0);
        let to = state.node(&to, &to, "shape", 0);
        state.edge(&from, &to, "flows_to", None, 0);
    }
    Ok(())
}

fn parse_arrow(line: &str) -> Option<(&str, &str, &str, Option<&str>)> {
    const OPERATORS: &[&str] = &[
        "<|--", "--|>", "<->", "-->>", "->>", "==>", "<--", "-.->", "..>", "-->", "<--", "<-",
        "->", "--",
    ];
    let mut candidate: Option<(usize, &str)> = None;
    for operator in OPERATORS {
        let Some(position) = line.find(operator) else {
            continue;
        };
        if candidate.is_none_or(|(current, existing)| {
            position < current || (position == current && operator.len() > existing.len())
        }) {
            candidate = Some((position, operator));
        }
    }
    let (position, operator) = candidate?;
    let source = line[..position].trim();
    let rest = line[position + operator.len()..].trim();
    if source.is_empty() || rest.is_empty() {
        return None;
    }
    let (target, label) = rest
        .split_once(':')
        .map_or((rest, None), |(target, label)| {
            (target.trim(), Some(label.trim()))
        });
    (!target.is_empty()).then_some((source, operator, target, label))
}

fn split_alias(value: &str) -> (&str, &str) {
    let value = value.trim();
    if let Some((id, label)) = value.split_once(" as ") {
        (id.trim(), label.trim().trim_matches('\"'))
    } else {
        (value, value.trim_matches('\"'))
    }
}

fn strip_markup(value: &str) -> String {
    let mut plain = String::with_capacity(value.len());
    let mut inside = false;
    for character in value.chars() {
        match character {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => plain.push(character),
            _ => {}
        }
    }
    plain
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .trim()
        .to_owned()
}

fn offset_of_line(source: &str, target: usize) -> usize {
    source
        .as_bytes()
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(target.saturating_sub(1))
        .map_or(0, |(offset, _)| offset + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(path: &str, source: &str) -> Extraction {
        extract_diagram_bytes(Path::new(path), path, source.as_bytes()).expect("extract")
    }
    fn relations(extraction: &Extraction) -> Vec<&str> {
        extraction
            .edges
            .iter()
            .map(|edge| edge.relation.as_str())
            .collect()
    }

    #[test]
    fn graphviz_edges_and_labels_are_explicit() {
        assert_eq!(
            crate::format_registry::format_registry()
                .find_by_path(Path::new("architecture.dot"))
                .map(|spec| spec.adapter()),
            Some(crate::format_registry::ByteAdapterKind::Diagram)
        );
        let extraction = extract(
            "architecture.dot",
            "digraph { api [label=\"API\"]; api -> db [label=\"queries\"]; }",
        );
        assert!(extraction.nodes.iter().any(|node| node.label == "API"));
        assert!(relations(&extraction).contains(&"flows_to"));
    }

    #[test]
    fn mermaid_sequence_and_gantt_emit_typed_relations() {
        let sequence = extract(
            "sequence.mmd",
            "sequenceDiagram\nparticipant Alice\nAlice->>Bob: request",
        );
        assert!(relations(&sequence).contains(&"message_to"));
        let gantt = extract(
            "schedule.mmd",
            "gantt\nsection Build\nCompile :compile, 2026-01-01, 1d\nTest :test, after compile, 1d",
        );
        assert!(relations(&gantt).contains(&"precedes"));
    }

    #[test]
    fn plantuml_d2_structurizr_and_dbml_are_byte_only() {
        assert!(relations(&extract("model.puml", "class A\nA <|-- B")).contains(&"inherits"));
        assert!(
            relations(&extract("model.d2", "web: Web\nweb -> db: stores")).contains(&"flows_to")
        );
        assert!(relations(&extract("workspace.dsl", "model { api = softwareSystem \"API\"\ndb = softwareSystem \"DB\"\napi -> db \"uses\" }")).contains(&"uses"));
        assert!(relations(&extract(
            "schema.dbml",
            "Table users { id int }\nRef: users.id > posts.user_id"
        ))
        .contains(&"references"));
    }

    #[test]
    fn xml_diagrams_reject_malformed_input() {
        assert!(
            extract_diagram_bytes(Path::new("bad.bpmn"), "bad.bpmn", b"<definitions><task>")
                .is_err()
        );
        let bpmn = extract("flow.bpmn", "<definitions><process id=\"p\"><task id=\"a\" name=\"A\"/><task id=\"b\"/><sequenceFlow sourceRef=\"a\" targetRef=\"b\"/></process></definitions>");
        assert!(relations(&bpmn).contains(&"flows_to"));
    }

    #[test]
    fn xmi_drawio_and_structurizr_json_keep_explicit_structure() {
        let xmi = extract(
            "model.xmi",
            "<xmi:XMI xmlns:xmi=\"urn:xmi\"><packagedElement xmi:type=\"uml:Class\" xmi:id=\"A\" name=\"API\"/></xmi:XMI>",
        );
        assert!(xmi.nodes.iter().any(|node| node.label == "API"));
        let drawio = extract(
            "design.drawio",
            "<mxGraphModel><root><mxCell id=\"2\" value=\"API\" vertex=\"1\" parent=\"1\"/><mxCell id=\"3\" value=\"DB\" vertex=\"1\" parent=\"1\"/><mxCell id=\"4\" edge=\"1\" source=\"2\" target=\"3\"/></root></mxGraphModel>",
        );
        assert!(relations(&drawio).contains(&"flows_to"));
        let structurizr = extract(
            "workspace.json",
            r#"{"model":{"softwareSystems":[{"id":"api","name":"API","type":"SoftwareSystem","relationships":[{"destinationId":"db","description":"uses"}]},{"id":"db","name":"DB","type":"SoftwareSystem"}]}}"#,
        );
        assert!(relations(&structurizr).contains(&"uses"));
    }

    #[test]
    fn canvas_json_bindings_are_explicit() {
        let excalidraw = extract(
            "design.excalidraw",
            r#"{"type":"excalidraw","elements":[{"id":"a","type":"rectangle"},{"id":"b","type":"rectangle"},{"id":"e","type":"arrow","startBinding":{"elementId":"a"},"endBinding":{"elementId":"b"}}]}"#,
        );
        assert!(relations(&excalidraw).contains(&"flows_to"));
        let tldraw = extract(
            "design.tldr",
            r#"{"records":[{"typeName":"shape","id":"shape:a","type":"geo"},{"typeName":"shape","id":"shape:b","type":"geo"},{"typeName":"binding","fromId":"shape:a","toId":"shape:b"}]}"#,
        );
        assert!(relations(&tldraw).contains(&"flows_to"));
    }
}
