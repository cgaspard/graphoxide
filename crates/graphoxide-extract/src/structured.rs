//! Bounded byte-only extraction for structured text and documentation files.
//!
//! This module deliberately owns no path-based I/O.  The runtime passes a
//! borrowed source buffer and a logical source path; callers can retain the
//! resulting facts and diagnostics without granting the CPU parser a
//! filesystem capability.  Parsers are strict where a locked dependency is
//! available and otherwise produce explicitly marked structural facts rather
//! than guessing a semantic value.

use base64::Engine as _;
use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use quick_xml::{events::Event, Reader};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    path::Path,
    sync::LazyLock,
};

const DEFAULT_MAX_INPUT_BYTES: usize =
    crate::format_registry::STRUCTURED_TEXT_LIMITS.max_input_bytes as usize;
const DEFAULT_MAX_FACTS: usize = crate::format_registry::STRUCTURED_TEXT_LIMITS.max_records;
const DEFAULT_MAX_DEPTH: usize = crate::format_registry::STRUCTURED_TEXT_LIMITS.max_nesting;
const DEFAULT_MAX_ROWS: usize = crate::format_registry::STRUCTURED_TEXT_LIMITS.max_records;
const DEFAULT_MAX_SCALAR_BYTES: usize = 64 * 1024;
const MAX_SENSITIVE_KEY_BYTES: usize = 256;
const MAX_SENSITIVE_VALUE_BYTES: usize = DEFAULT_MAX_SCALAR_BYTES;
pub(crate) const REDACTED_STRUCTURED_VALUE: &str = "<redacted>";

const JSON_EXTENSIONS: &[&str] = &[
    "json",
    "jsonc",
    "geojson",
    "topojson",
    "webmanifest",
    "har",
    "ipynb",
];
const JSON5_EXTENSIONS: &[&str] = &["json5"];
const JSON_LINES_EXTENSIONS: &[&str] = &["jsonl", "ndjson"];
const TOML_EXTENSIONS: &[&str] = &["toml"];
const YAML_EXTENSIONS: &[&str] = &["yaml", "yml"];
const XML_EXTENSIONS: &[&str] = &["xml", "xsd", "xsl", "xslt", "svg", "wsdl", "rng"];
const CSV_EXTENSIONS: &[&str] = &["csv", "ccsv"];
const TSV_EXTENSIONS: &[&str] = &["tsv", "tab"];
const INI_EXTENSIONS: &[&str] = &["ini", "cfg", "conf", "config", "properties", "env", "envrc"];
const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "qmd"];
const RST_EXTENSIONS: &[&str] = &["rst", "rest"];
const ASCIIDOC_EXTENSIONS: &[&str] = &["adoc", "asciidoc", "asc"];
const HTML_EXTENSIONS: &[&str] = &["html", "htm", "xhtml"];

static MARKDOWN_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"!?\[[^\]]*\]\(\s*<?([^\)\s>]+)>?(?:\s+[^\)]*)?\)"#)
        .expect("valid Markdown link regex")
});
static RST_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"`[^`<>]+\s*<([^>]+)>`_").expect("valid reStructuredText link regex")
});
static HTML_HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<h([1-6])\b[^>]*>(.*?)</h[1-6]\s*>").expect("valid HTML heading regex")
});
static HTML_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\bhref\s*=\s*[\"']([^\"']+)[\"']"#).expect("valid HTML link regex")
});
static HTML_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").expect("valid HTML tag regex"));

/// Limits applied before and during structured parsing.
///
/// Defaults come from the structured registry contract and remain independent
/// from runtime buffer credits: a source can be admissible to the I/O plane yet
/// too large for this parser's fact expansion policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StructuredLimits {
    pub max_input_bytes: usize,
    pub max_facts: usize,
    pub max_depth: usize,
    pub max_rows: usize,
    pub max_scalar_bytes: usize,
}

impl Default for StructuredLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_facts: DEFAULT_MAX_FACTS,
            max_depth: DEFAULT_MAX_DEPTH,
            max_rows: DEFAULT_MAX_ROWS,
            max_scalar_bytes: DEFAULT_MAX_SCALAR_BYTES,
        }
    }
}

impl StructuredLimits {
    fn valid(self) -> bool {
        self.max_input_bytes > 0
            && self.max_facts > 0
            && self.max_depth > 0
            && self.max_rows > 0
            && self.max_scalar_bytes > 0
    }
}

/// A stable, source-free explanation of a structural parsing limitation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructuredDiagnostic {
    pub code: &'static str,
    pub line: usize,
    pub message: String,
}

impl StructuredDiagnostic {
    fn as_value(&self) -> Value {
        serde_json::json!({
            "code": self.code,
            "line": self.line,
            "message": self.message,
        })
    }
}

/// Facts and diagnostics produced from one ready byte buffer.
#[derive(Debug, Clone, Default)]
pub(crate) struct StructuredExtraction {
    pub extraction: Extraction,
    pub diagnostics: Vec<StructuredDiagnostic>,
}

/// Extract structured facts from an already-read input.
///
/// `None` means the filename is not owned by this adapter.  A recognized
/// filename always returns `Some`, including invalid UTF-8 and malformed
/// syntax, so callers can retain deterministic diagnostics instead of falling
/// through to a lossy generic text parser.
pub(crate) fn extract_structured_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> Option<StructuredExtraction> {
    extract_structured_bytes_with_limits(path, source_file, bytes, StructuredLimits::default())
}

/// Limit-configurable variant of [`extract_structured_bytes`].
pub(crate) fn extract_structured_bytes_with_limits(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    limits: StructuredLimits,
) -> Option<StructuredExtraction> {
    let format = StructuredFormat::for_path(path)?;
    let mut state = State::new(format, path, source_file, limits);
    if !limits.valid() {
        state.diagnostic(
            "invalid_limits",
            1,
            "structured extractor limits must be non-zero",
        );
        return Some(state.finish());
    }
    if bytes.len() > limits.max_input_bytes {
        state.diagnostic(
            "input_too_large",
            1,
            format!(
                "{} bytes exceeds structured input limit of {} bytes",
                bytes.len(),
                limits.max_input_bytes
            ),
        );
        return Some(state.finish());
    }

    match format {
        StructuredFormat::Json { mcp } => extract_json(&mut state, bytes, mcp),
        StructuredFormat::Json5 => extract_json5(&mut state, bytes),
        StructuredFormat::JsonLines => extract_json_lines(&mut state, bytes),
        StructuredFormat::Toml => extract_toml(&mut state, bytes),
        StructuredFormat::Yaml => extract_yaml_structure(&mut state, bytes),
        StructuredFormat::Xml => extract_xml(&mut state, bytes),
        StructuredFormat::Delimited(delimiter) => extract_delimited(&mut state, bytes, delimiter),
        StructuredFormat::Ini => extract_ini(&mut state, bytes),
        StructuredFormat::Markdown => extract_markdown(&mut state, bytes),
        StructuredFormat::Rst => extract_rst(&mut state, bytes),
        StructuredFormat::AsciiDoc => extract_asciidoc(&mut state, bytes),
        StructuredFormat::Html => extract_html(&mut state, bytes),
    }
    Some(state.finish())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuredFormat {
    Json { mcp: bool },
    Json5,
    JsonLines,
    Toml,
    Yaml,
    Xml,
    Delimited(u8),
    Ini,
    Markdown,
    Rst,
    AsciiDoc,
    Html,
}

impl StructuredFormat {
    fn for_path(path: &Path) -> Option<Self> {
        let file_name = path.file_name()?.to_str()?.to_ascii_lowercase();
        if matches!(
            file_name.as_str(),
            ".mcp.json" | "claude_desktop_config.json" | "mcp.json" | "mcp_servers.json"
        ) {
            return Some(Self::Json { mcp: true });
        }
        if matches!(file_name.as_str(), ".prettierrc" | ".eslintrc" | ".babelrc") {
            return Some(Self::Json { mcp: false });
        }
        if file_name == ".yarnrc.yml" {
            return Some(Self::Yaml);
        }
        if matches!(
            file_name.as_str(),
            ".env" | ".envrc" | ".editorconfig" | ".npmrc" | ".yarnrc" | ".gitmodules"
        ) {
            return Some(Self::Ini);
        }
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        if JSON5_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Json5)
        } else if JSON_LINES_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::JsonLines)
        } else if JSON_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Json { mcp: false })
        } else if TOML_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Toml)
        } else if YAML_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Yaml)
        } else if XML_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Xml)
        } else if CSV_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Delimited(b','))
        } else if TSV_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Delimited(b'\t'))
        } else if extension == "psv" {
            Some(Self::Delimited(b'|'))
        } else if INI_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Ini)
        } else if MARKDOWN_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Markdown)
        } else if RST_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Rst)
        } else if ASCIIDOC_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::AsciiDoc)
        } else if HTML_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Html)
        } else {
            None
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Json { mcp: true } => "mcp_json",
            Self::Json { mcp: false } => "json",
            Self::Json5 => "json5_structural",
            Self::JsonLines => "json_lines",
            Self::Toml => "toml",
            Self::Yaml => "yaml_structural",
            Self::Xml => "xml",
            Self::Delimited(b',') => "csv",
            Self::Delimited(b'|') => "psv",
            Self::Delimited(_) => "tsv",
            Self::Ini => "ini",
            Self::Markdown => "markdown",
            Self::Rst => "restructuredtext",
            Self::AsciiDoc => "asciidoc",
            Self::Html => "html",
        }
    }

    const fn file_type(self) -> &'static str {
        match self {
            Self::Markdown | Self::Rst | Self::AsciiDoc | Self::Html => "document",
            _ => "code",
        }
    }
}

struct State<'a> {
    format: StructuredFormat,
    source_file: &'a str,
    stem: String,
    file_id: String,
    limits: StructuredLimits,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    seen_ids: BTreeSet<String>,
    node_positions: BTreeMap<String, usize>,
    diagnostics: Vec<StructuredDiagnostic>,
    fact_limit_reported: bool,
    path_limit_reported: bool,
    parser_budget_exhausted: bool,
}

#[derive(Clone, Copy, Default)]
struct ChildOptions<'a> {
    value: Option<ChildValue<'a>>,
    structural_only: bool,
    value_truncated: bool,
    sensitive_context: bool,
    redacted_value_type: Option<&'static str>,
}

#[derive(Clone, Copy)]
enum ChildValue<'a> {
    Json(&'a Value),
    String(&'a str),
}

impl<'a> ChildOptions<'a> {
    const STRUCTURAL: Self = Self {
        value: None,
        structural_only: true,
        value_truncated: false,
        sensitive_context: false,
        redacted_value_type: None,
    };

    fn string(value: &'a str) -> Self {
        Self {
            value: Some(ChildValue::String(value)),
            structural_only: false,
            value_truncated: false,
            sensitive_context: false,
            redacted_value_type: None,
        }
    }

    fn truncated_value() -> Self {
        Self {
            value: None,
            structural_only: false,
            value_truncated: true,
            sensitive_context: false,
            redacted_value_type: None,
        }
    }

    fn truncated_sensitive_string() -> Self {
        Self {
            value: None,
            structural_only: false,
            value_truncated: true,
            sensitive_context: true,
            redacted_value_type: Some("string"),
        }
    }

    const fn in_sensitive_context(mut self, sensitive_context: bool) -> Self {
        self.sensitive_context = sensitive_context;
        self
    }
}

impl<'a> State<'a> {
    fn new(
        format: StructuredFormat,
        path: &Path,
        source_file: &'a str,
        limits: StructuredLimits,
    ) -> Self {
        let stem = Path::new(source_file)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let file_id = make_id(&[&stem]);
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(source_file);
        let root_reserved = crate::parser_budget::try_reserve_facts(1);
        debug_assert!(root_reserved, "parser plans always reserve one root fact");
        let root = root_reserved.then(|| {
            let mut root_extra = BTreeMap::from([
                ("_origin".into(), Value::String("structured".into())),
                ("type".into(), Value::String("structured_file".into())),
                (
                    "structured_format".into(),
                    Value::String(format.id().into()),
                ),
            ]);
            root_extra.insert("structured_version".into(), Value::from(2_u64));
            root_extra.insert("structured_redaction_policy".into(), Value::from(1_u64));
            if let Some(spec) = crate::format_registry::format_registry().find_by_path(path) {
                root_extra.insert(
                    "format_capability".into(),
                    Value::String(spec.capability.as_str().into()),
                );
                if spec.capability == crate::format_registry::FormatCapability::StructuralPartial {
                    root_extra.insert("parse_status".into(), Value::String("partial".into()));
                }
            }
            Node {
                id: file_id.clone(),
                label: label.into(),
                file_type: format.file_type().into(),
                source_file: source_file.into(),
                source_location: Some("L1".into()),
                community: None,
                extra: root_extra,
            }
        });
        Self {
            format,
            source_file,
            stem,
            file_id: file_id.clone(),
            limits,
            nodes: root.into_iter().collect(),
            edges: Vec::new(),
            seen_ids: if root_reserved {
                BTreeSet::from([file_id.clone()])
            } else {
                BTreeSet::new()
            },
            node_positions: if root_reserved {
                BTreeMap::from([(file_id, 0)])
            } else {
                BTreeMap::new()
            },
            diagnostics: Vec::new(),
            fact_limit_reported: false,
            path_limit_reported: false,
            parser_budget_exhausted: !root_reserved,
        }
    }

    fn finish(mut self) -> StructuredExtraction {
        if self.parser_budget_exhausted
            && let Some(root) = self.nodes.first_mut()
        {
            root.extra.insert("parse_status".into(), "partial".into());
            root.extra
                .insert("parser_diagnostic".into(), "parser_arena_fact_limit".into());
        }
        if !self.diagnostics.is_empty()
            && let Some(root) = self.nodes.first_mut()
        {
            root.extra.insert(
                "structured_diagnostics".into(),
                Value::Array(
                    self.diagnostics
                        .iter()
                        .map(StructuredDiagnostic::as_value)
                        .collect(),
                ),
            );
        }
        StructuredExtraction {
            extraction: Extraction {
                nodes: self.nodes,
                edges: self.edges,
                hyperedges: Vec::new(),
            },
            diagnostics: self.diagnostics,
        }
    }

    fn diagnostic(&mut self, code: &'static str, line: usize, message: impl Into<String>) {
        if !self.reserve_parser_facts(1) {
            return;
        }
        self.diagnostics.push(StructuredDiagnostic {
            code,
            line: line.max(1),
            message: message.into(),
        });
    }

    fn reserve_parser_facts(&mut self, facts: usize) -> bool {
        if crate::parser_budget::try_reserve_facts(facts) {
            return true;
        }
        self.parser_budget_exhausted = true;
        false
    }

    fn report_path_limit(&mut self, line: usize, context: &str) {
        if self.path_limit_reported {
            return;
        }
        self.path_limit_reported = true;
        self.diagnostic(
            "path_limit",
            line,
            format!(
                "structured path limit of {} bytes reached in {context}",
                self.limits.max_scalar_bytes
            ),
        );
    }

    fn has_capacity(&mut self, facts: usize, line: usize) -> bool {
        let retained = self.nodes.len().saturating_add(self.edges.len());
        if retained
            .checked_add(facts)
            .is_some_and(|total| total <= self.limits.max_facts)
        {
            return true;
        }
        if !self.fact_limit_reported {
            self.fact_limit_reported = true;
            self.diagnostic(
                "fact_limit",
                line,
                format!("structured fact limit of {} reached", self.limits.max_facts),
            );
        }
        false
    }

    fn child(
        &mut self,
        parent: &str,
        path: &str,
        label: &str,
        kind: &str,
        line: usize,
        options: ChildOptions<'_>,
    ) -> Option<String> {
        if self.nodes.is_empty() {
            return None;
        }
        // A map key, XML name, heading, or reference can itself contain a
        // credential. Decide that before the raw component participates in a
        // published path or identifier. Callers also pass identity-safe path
        // components so descendants cannot inherit the original spelling; this
        // central replacement is the final guard for non-hierarchical labels.
        let label_redacted = structured_string_is_sensitive(label);
        let redacted_path = label_redacted.then(|| {
            format!(
                "$redacted/{kind}[{}]",
                self.nodes.len().saturating_add(self.edges.len())
            )
        });
        let path = redacted_path.as_deref().unwrap_or(path);
        if path.len() > self.limits.max_scalar_bytes {
            self.report_path_limit(line, "structured parser");
            return None;
        }
        let mut id = make_id(&[&self.stem, path]);
        if id.is_empty() {
            id = make_id(&[&self.stem, kind, &line.to_string()]);
        }
        if self.seen_ids.contains(&id) {
            id = make_id(&[&self.stem, path, &line.to_string()]);
        }
        if self.seen_ids.contains(&id) {
            self.diagnostic(
                "duplicate_fact",
                line,
                "duplicate structured path was ignored",
            );
            return None;
        }
        let retain_edge = !parent.is_empty() && parent != id;
        let facts = 1 + usize::from(retain_edge);
        if !self.has_capacity(facts, line) || !self.reserve_parser_facts(facts) {
            return None;
        }
        let mut extra = BTreeMap::from([
            ("_origin".into(), Value::String("structured".into())),
            ("type".into(), Value::String(kind.into())),
            (
                "structured_format".into(),
                Value::String(self.format.id().into()),
            ),
            ("structured_path".into(), Value::String(path.into())),
        ]);
        if options.structural_only {
            extra.insert("structured_unparsed".into(), Value::Bool(true));
        }
        if options.value_truncated {
            extra.insert("structured_value_truncated".into(), Value::Bool(true));
        }
        if let Some(value) = options.value {
            if options.sensitive_context {
                extra.insert(
                    "structured_value".into(),
                    Value::String(REDACTED_STRUCTURED_VALUE.into()),
                );
                extra.insert("structured_value_redacted".into(), Value::Bool(true));
                extra.insert(
                    "structured_value_type".into(),
                    Value::String(child_value_kind(value).into()),
                );
            } else {
                let fits = match value {
                    ChildValue::Json(value) => {
                        serialized_len_at_most(value, self.limits.max_scalar_bytes)
                    }
                    ChildValue::String(value) => {
                        serialized_len_at_most(value, self.limits.max_scalar_bytes)
                    }
                };
                if !fits {
                    extra.insert("structured_value_truncated".into(), Value::Bool(true));
                    self.diagnostic(
                        "scalar_limit",
                        line,
                        format!(
                            "structured scalar at {path:?} exceeds {} bytes",
                            self.limits.max_scalar_bytes
                        ),
                    );
                } else if child_value_is_sensitive(value) {
                    extra.insert(
                        "structured_value".into(),
                        Value::String(REDACTED_STRUCTURED_VALUE.into()),
                    );
                    extra.insert("structured_value_redacted".into(), Value::Bool(true));
                    extra.insert(
                        "structured_value_type".into(),
                        Value::String(child_value_kind(value).into()),
                    );
                } else {
                    let value = match value {
                        ChildValue::Json(value) => value.clone(),
                        ChildValue::String(value) => Value::String(value.into()),
                    };
                    extra.insert("structured_value".into(), value);
                }
            }
        } else if let Some(value_type) = options.redacted_value_type {
            extra.insert(
                "structured_value".into(),
                Value::String(REDACTED_STRUCTURED_VALUE.into()),
            );
            extra.insert("structured_value_redacted".into(), Value::Bool(true));
            extra.insert(
                "structured_value_type".into(),
                Value::String(value_type.into()),
            );
        } else if options.sensitive_context {
            extra.insert("structured_descendants_redacted".into(), Value::Bool(true));
        }
        if label_redacted {
            extra.insert("structured_label_redacted".into(), Value::Bool(true));
            extra.insert("structured_path_redacted".into(), Value::Bool(true));
        }
        self.nodes.push(Node {
            id: id.clone(),
            label: if label_redacted {
                REDACTED_STRUCTURED_VALUE.into()
            } else {
                label.into()
            },
            file_type: self.format.file_type().into(),
            source_file: self.source_file.into(),
            source_location: Some(format!("L{}", line.max(1))),
            community: None,
            extra,
        });
        self.node_positions.insert(id.clone(), self.nodes.len() - 1);
        self.seen_ids.insert(id.clone());
        if retain_edge {
            self.retain_edge(parent, &id, "contains", line);
        }
        Some(id)
    }

    fn edge(&mut self, source: &str, target: &str, relation: &str, line: usize) {
        if source.is_empty()
            || target.is_empty()
            || source == target
            || !self.has_capacity(1, line)
            || !self.reserve_parser_facts(1)
        {
            return;
        }
        self.retain_edge(source, target, relation, line);
    }

    fn retain_edge(&mut self, source: &str, target: &str, relation: &str, line: usize) {
        self.edges.push(Edge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            confidence: Confidence::Extracted,
            source_file: self.source_file.into(),
            extra: BTreeMap::from([
                ("_src".into(), Value::String(source.into())),
                ("_tgt".into(), Value::String(target.into())),
                (
                    "source_location".into(),
                    Value::String(format!("L{}", line.max(1))),
                ),
                ("weight".into(), Value::from(1.0)),
            ]),
        });
    }

    fn add_root_value(&mut self, value: &Value, line: usize) {
        if !serialized_len_at_most(value, self.limits.max_scalar_bytes) {
            self.diagnostic(
                "scalar_limit",
                line,
                format!(
                    "structured root scalar exceeds {} bytes",
                    self.limits.max_scalar_bytes
                ),
            );
            return;
        }
        if json_value_is_sensitive(value) {
            if let Some(root) = self.nodes.first_mut() {
                root.extra.insert(
                    "structured_value".into(),
                    Value::String(REDACTED_STRUCTURED_VALUE.into()),
                );
                root.extra
                    .insert("structured_value_redacted".into(), Value::Bool(true));
                root.extra.insert(
                    "structured_value_type".into(),
                    Value::String(value_kind(value).into()),
                );
            }
            return;
        }
        if let Some(root) = self.nodes.first_mut() {
            root.extra.insert("structured_value".into(), value.clone());
        }
    }

    fn append_text(&mut self, id: &str, text: &str, line: usize, sensitive_context: bool) {
        if text.trim().is_empty() {
            return;
        }
        let Some(position) = self.node_positions.get(id).copied() else {
            return;
        };
        let node = &mut self.nodes[position];
        if node.extra.get("structured_text_redacted") == Some(&Value::Bool(true)) {
            return;
        }
        if sensitive_context {
            node.extra.insert(
                "structured_text".into(),
                Value::String(REDACTED_STRUCTURED_VALUE.into()),
            );
            node.extra
                .insert("structured_text_redacted".into(), Value::Bool(true));
            node.extra.insert(
                "structured_text_type".into(),
                Value::String("string".into()),
            );
            return;
        }
        let existing = node
            .extra
            .remove("structured_text")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        let separator = if existing.is_empty() { "" } else { " " };
        let candidate = format!("{existing}{separator}{}", text.trim());
        if candidate.len() > self.limits.max_scalar_bytes {
            node.extra
                .insert("structured_text_truncated".into(), Value::Bool(true));
            self.diagnostic(
                "scalar_limit",
                line,
                format!("XML text exceeds {} bytes", self.limits.max_scalar_bytes),
            );
            return;
        }
        // XML can split one logical scalar across Text and CDATA events. Scan
        // the normal retained spelling and a bounded whitespace-free spelling
        // so a provider token cannot be evaded as `ghp_<![CDATA[...]]>`.
        let compact_candidate = candidate
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        if string_is_sensitive(&candidate)
            || (compact_candidate.len() != candidate.len()
                && string_is_sensitive(&compact_candidate))
        {
            node.extra.insert(
                "structured_text".into(),
                Value::String(REDACTED_STRUCTURED_VALUE.into()),
            );
            node.extra
                .insert("structured_text_redacted".into(), Value::Bool(true));
            node.extra.insert(
                "structured_text_type".into(),
                Value::String("string".into()),
            );
            return;
        }
        node.extra
            .insert("structured_text".into(), Value::String(candidate));
    }
}

fn extract_json(state: &mut State<'_>, bytes: &[u8], mcp: bool) {
    match graphoxide_core::parse_jsonc_slice(bytes) {
        Ok(value) => {
            debug_assert_eq!(
                mcp,
                matches!(state.format, StructuredFormat::Json { mcp: true })
            );
            walk_value(
                state,
                &value,
                &state.file_id.clone(),
                "$",
                0,
                1,
                WalkPolicy::PARSED,
            );
        }
        Err(error) => {
            let line = error.line();
            state.diagnostic(
                "json_parse_error",
                line,
                format!("JSON/JSONC parse error: {error}"),
            );
            extract_json_structural_fallback(state, bytes);
        }
    }
}

fn extract_json5(state: &mut State<'_>, bytes: &[u8]) {
    match graphoxide_core::parse_jsonc_slice(bytes) {
        Ok(value) => walk_value(
            state,
            &value,
            &state.file_id.clone(),
            "$",
            0,
            1,
            WalkPolicy::PARSED,
        ),
        Err(error) => {
            state.diagnostic(
                "json5_structural_only",
                error.line(),
                format!(
                    "full JSON5 decoding is unavailable; retaining bounded structural keys: {error}"
                ),
            );
            extract_json_structural_fallback(state, bytes);
        }
    }
}

fn extract_json_lines(state: &mut State<'_>, bytes: &[u8]) {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            state.diagnostic(
                "invalid_utf8",
                1,
                format!("JSON Lines input is not UTF-8: {error}"),
            );
            return;
        }
    };

    let mut record_index = 0usize;
    for (line_index, raw_line) in text.lines().enumerate() {
        let line = line_index + 1;
        let record = raw_line.trim();
        if record.is_empty() {
            continue;
        }
        if record_index >= state.limits.max_rows {
            state.diagnostic(
                "row_limit",
                line,
                format!(
                    "JSON Lines record limit of {} reached",
                    state.limits.max_rows
                ),
            );
            break;
        }

        let value = match serde_json::from_str::<Value>(record) {
            Ok(value) => value,
            Err(error) => {
                state.diagnostic(
                    "json_lines_parse_error",
                    line,
                    format!("JSON Lines record {record_index} is invalid: {error}"),
                );
                record_index += 1;
                continue;
            }
        };
        let path = format!("$[{record_index}]");
        let scalar = is_scalar(&value).then_some(&value);
        let Some(id) = state.child(
            &state.file_id.clone(),
            &path,
            &format!("[{record_index}]"),
            "json_record",
            line,
            ChildOptions {
                value: scalar.map(ChildValue::Json),
                structural_only: false,
                value_truncated: false,
                sensitive_context: false,
                redacted_value_type: None,
            },
        ) else {
            return;
        };
        if !is_scalar(&value) {
            walk_value(state, &value, &id, &path, 1, line, WalkPolicy::PARSED);
        }
        record_index += 1;
    }
}

fn extract_toml(state: &mut State<'_>, bytes: &[u8]) {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            state.diagnostic("invalid_utf8", 1, format!("TOML is not UTF-8: {error}"));
            return;
        }
    };
    match toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|value| serde_json::to_value(value).ok())
    {
        Some(value) => walk_value(
            state,
            &value,
            &state.file_id.clone(),
            "$",
            0,
            1,
            WalkPolicy::PARSED,
        ),
        None => {
            state.diagnostic("toml_parse_error", 1, "TOML parse failed");
            extract_key_value_structure(state, text, '=', "toml_structural");
        }
    }
}

#[derive(Clone, Copy)]
struct WalkPolicy {
    structural_only: bool,
    sensitive_context: bool,
}

impl WalkPolicy {
    const PARSED: Self = Self {
        structural_only: false,
        sensitive_context: false,
    };

    const fn with_sensitive_context(self, sensitive_context: bool) -> Self {
        Self {
            sensitive_context,
            ..self
        }
    }
}

fn walk_value(
    state: &mut State<'_>,
    value: &Value,
    parent: &str,
    path: &str,
    depth: usize,
    line: usize,
    policy: WalkPolicy,
) {
    if depth >= state.limits.max_depth {
        state.diagnostic(
            "depth_limit",
            line,
            format!(
                "structured depth limit of {} reached",
                state.limits.max_depth
            ),
        );
        return;
    }
    match value {
        Value::Object(object) => {
            for (ordinal, (key, child)) in object.iter().enumerate() {
                let child_line = line;
                let child_sensitive = policy.sensitive_context
                    || key_is_sensitive(key)
                    || (is_scalar(child) && key_is_sensitive_scalar(key))
                    || mcp_key_redacts_descendants(state.format, key);
                let path_component = identity_safe_component(key, "key", ordinal);
                let Some(child_path) =
                    bounded_object_path(path, &path_component, state.limits.max_scalar_bytes)
                else {
                    state.report_path_limit(child_line, "structured value traversal");
                    continue;
                };
                let scalar = is_scalar(child).then_some(child);
                let Some(id) = state.child(
                    parent,
                    &child_path,
                    key,
                    value_kind(child),
                    child_line,
                    ChildOptions {
                        value: scalar.map(ChildValue::Json),
                        structural_only: policy.structural_only,
                        value_truncated: false,
                        sensitive_context: child_sensitive,
                        redacted_value_type: None,
                    },
                ) else {
                    return;
                };
                if !is_scalar(child) {
                    walk_value(
                        state,
                        child,
                        &id,
                        &child_path,
                        depth + 1,
                        child_line,
                        policy.with_sensitive_context(child_sensitive),
                    );
                }
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                let Some(child_path) =
                    bounded_array_path(path, index, state.limits.max_scalar_bytes)
                else {
                    state.report_path_limit(line, "structured value traversal");
                    continue;
                };
                let scalar = is_scalar(child).then_some(child);
                let Some(id) = state.child(
                    parent,
                    &child_path,
                    &format!("[{index}]"),
                    if is_scalar(child) {
                        "array_item"
                    } else {
                        value_kind(child)
                    },
                    line,
                    ChildOptions {
                        value: scalar.map(ChildValue::Json),
                        structural_only: policy.structural_only,
                        value_truncated: false,
                        sensitive_context: policy.sensitive_context,
                        redacted_value_type: None,
                    },
                ) else {
                    return;
                };
                if !is_scalar(child) {
                    walk_value(state, child, &id, &child_path, depth + 1, line, policy);
                }
            }
        }
        scalar => state.add_root_value(scalar, line),
    }
}

fn extract_json_structural_fallback(state: &mut State<'_>, bytes: &[u8]) {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            state.diagnostic(
                "invalid_utf8",
                1,
                format!("JSON fallback requires UTF-8: {error}"),
            );
            return;
        }
    };
    let mut parent_stack: Vec<(usize, String, String, bool)> = Vec::new();
    let mut blocked_indent = None;
    let mut depth_limit_reported = false;
    for (index, raw_line) in text.lines().enumerate() {
        let line = index + 1;
        let line_without_comments = strip_json_comments(raw_line);
        let trimmed = line_without_comments.trim();
        if trimmed.is_empty() || matches!(trimmed, "{" | "}" | "[" | "]" | ",") {
            continue;
        }
        let indent = raw_line.len() - raw_line.trim_start().len();
        if let Some(blocked_at) = blocked_indent {
            if indent > blocked_at {
                continue;
            }
            blocked_indent = None;
        }
        let Some(colon) = find_unquoted_byte(trimmed.as_bytes(), b':') else {
            continue;
        };
        let raw_key = trimmed[..colon]
            .trim()
            .trim_matches(|character: char| matches!(character, '{' | '[' | ',' | ' ' | '\t'))
            .trim();
        let key = raw_key.trim_matches(['\"', '\'']);
        if key.is_empty() || !key.chars().all(is_json5_key_char) {
            continue;
        }
        while parent_stack
            .last()
            .is_some_and(|(parent_indent, _, _, _)| *parent_indent >= indent)
        {
            parent_stack.pop();
        }
        let value = trimmed[colon + 1..].trim().trim_end_matches(',').trim();
        let opens_container = value.ends_with('{') || value.ends_with('[') || value.is_empty();
        if parent_stack.len() >= state.limits.max_depth {
            if !depth_limit_reported {
                depth_limit_reported = true;
                state.diagnostic(
                    "depth_limit",
                    line,
                    format!(
                        "structured depth limit of {} reached in JSON structural fallback",
                        state.limits.max_depth
                    ),
                );
            }
            if opens_container {
                blocked_indent = Some(indent);
            }
            continue;
        }
        let parent_sensitive = parent_stack
            .last()
            .is_some_and(|(_, _, _, sensitive)| *sensitive);
        let (parent, prefix) = parent_stack
            .last()
            .map(|(_, id, path, _)| (id.clone(), path.as_str()))
            .unwrap_or_else(|| (state.file_id.clone(), "$"));
        let sensitive = parent_sensitive || key_is_sensitive(key);
        let path_component = identity_safe_component(key, "json-key", line);
        let Some(path) =
            bounded_object_path(prefix, &path_component, state.limits.max_scalar_bytes)
        else {
            state.report_path_limit(line, "JSON structural fallback");
            if opens_container {
                blocked_indent = Some(indent);
            }
            continue;
        };
        let Some(id) = state.child(
            &parent,
            &path,
            key,
            "json5_key",
            line,
            ChildOptions::STRUCTURAL.in_sensitive_context(sensitive),
        ) else {
            return;
        };
        if opens_container {
            parent_stack.push((indent, id, path, sensitive));
        }
    }
}

fn extract_yaml_structure(state: &mut State<'_>, bytes: &[u8]) {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            state.diagnostic("invalid_utf8", 1, format!("YAML is not UTF-8: {error}"));
            return;
        }
    };
    let mut parents: Vec<(usize, String, String, bool)> = Vec::new();
    let mut document = 0usize;
    let mut blocked_indent = None;
    let mut depth_limit_reported = false;
    for (index, raw_line) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = strip_yaml_comment(raw_line).trim();
        if trimmed.is_empty() || trimmed.starts_with('%') || trimmed == "..." {
            continue;
        }
        if trimmed == "---" {
            document += 1;
            parents.clear();
            blocked_indent = None;
            continue;
        }
        let indent = raw_line.len() - raw_line.trim_start().len();
        if let Some(blocked_at) = blocked_indent {
            if indent > blocked_at {
                continue;
            }
            blocked_indent = None;
        }
        while parents
            .last()
            .is_some_and(|(parent_indent, _, _, _)| *parent_indent >= indent)
        {
            parents.pop();
        }
        let content = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim();
        let (label, value, is_mapping) = match find_unquoted_byte(content.as_bytes(), b':') {
            Some(colon) if !content[..colon].trim().is_empty() => {
                let key = content[..colon].trim().trim_matches(['\"', '\'']);
                (key, content[colon + 1..].trim(), true)
            }
            _ if trimmed.starts_with("- ") => ("item", content, false),
            _ => continue,
        };
        let opens_container = value.is_empty() || value.starts_with('|') || value.starts_with('>');
        if parents.len() >= state.limits.max_depth {
            if !depth_limit_reported {
                depth_limit_reported = true;
                state.diagnostic(
                    "depth_limit",
                    line,
                    format!(
                        "structured depth limit of {} reached in YAML structural fallback",
                        state.limits.max_depth
                    ),
                );
            }
            if opens_container {
                blocked_indent = Some(indent);
            }
            continue;
        }
        let item_suffix = (!is_mapping).then(|| format!("item_{line}"));
        let suffix = item_suffix.as_deref().unwrap_or(label);
        let path_component = identity_safe_component(suffix, "yaml-key", line);
        let document_prefix;
        let parent_sensitive = parents
            .last()
            .is_some_and(|(_, _, _, sensitive)| *sensitive);
        let sensitive = parent_sensitive || (is_mapping && key_is_sensitive(label));
        let (parent, prefix) = if let Some((_, id, path, _)) = parents.last() {
            (id.clone(), path.as_str())
        } else {
            document_prefix = format!("$doc{document}");
            (state.file_id.clone(), document_prefix.as_str())
        };
        let Some(path) =
            bounded_object_path(prefix, &path_component, state.limits.max_scalar_bytes)
        else {
            state.report_path_limit(line, "YAML structural fallback");
            if opens_container {
                blocked_indent = Some(indent);
            }
            continue;
        };
        let Some(id) = state.child(
            &parent,
            &path,
            label,
            if is_mapping { "yaml_key" } else { "yaml_item" },
            line,
            ChildOptions::STRUCTURAL.in_sensitive_context(sensitive),
        ) else {
            return;
        };
        if opens_container {
            parents.push((indent, id, path, sensitive));
        }
    }
}

fn extract_xml(state: &mut State<'_>, bytes: &[u8]) {
    let sibling_sensitive_elements = xml_sibling_sensitive_elements(bytes, state.limits);
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut stack: Vec<XmlFrame> = Vec::new();
    let mut element_ordinal = 0usize;
    let mut skipped_depth = 0usize;
    let mut depth_limit_reported = false;
    'xml: loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) => {
                let current_ordinal = element_ordinal;
                element_ordinal = element_ordinal.saturating_add(1);
                if skipped_depth > 0 {
                    skipped_depth += 1;
                } else if stack.len() >= state.limits.max_depth {
                    if !depth_limit_reported {
                        depth_limit_reported = true;
                        state.diagnostic(
                            "depth_limit",
                            line_number(bytes, reader.buffer_position() as usize),
                            format!(
                                "structured depth limit of {} reached in XML",
                                state.limits.max_depth
                            ),
                        );
                    }
                    skipped_depth = 1;
                } else {
                    'admit: {
                        let source_line = line_number(bytes, reader.buffer_position() as usize);
                        let (parent, path, name, base_sensitive) = match prepare_xml_element(
                            state,
                            event.name().as_ref(),
                            &mut stack,
                            source_line,
                            current_ordinal,
                        ) {
                            XmlElementPreparation::Ready {
                                parent,
                                path,
                                name,
                                sensitive,
                            } => (parent, path, name, sensitive),
                            XmlElementPreparation::PathLimited => {
                                skipped_depth = 1;
                                break 'admit;
                            }
                            XmlElementPreparation::InvalidName => {
                                state.diagnostic(
                                    "xml_name_error",
                                    source_line,
                                    "XML element name is not UTF-8",
                                );
                                break 'xml;
                            }
                        };
                        let selector_sensitive = xml_selector_is_sensitive(&event);
                        let sensitive = base_sensitive || selector_sensitive;
                        let sibling_sensitive =
                            sibling_sensitive_elements.contains(&current_ordinal);
                        let Some(id) = state.child(
                            &parent,
                            &path,
                            &name,
                            "xml_element",
                            source_line,
                            ChildOptions::default()
                                .in_sensitive_context(sensitive || sibling_sensitive),
                        ) else {
                            break 'xml;
                        };
                        for (attribute_ordinal, attribute) in
                            event.attributes().with_checks(false).enumerate()
                        {
                            match attribute {
                                Ok(attribute) => {
                                    let (attribute_name, attribute_path) =
                                        match prepare_xml_attribute(
                                            state,
                                            &path,
                                            attribute.key.as_ref(),
                                            source_line,
                                            attribute_ordinal,
                                        ) {
                                            XmlAttributePreparation::Ready { name, path } => {
                                                (name, path)
                                            }
                                            XmlAttributePreparation::PathLimited => continue,
                                            XmlAttributePreparation::InvalidName => {
                                                state.diagnostic(
                                                    "xml_attribute_error",
                                                    source_line,
                                                    "XML attribute name is not UTF-8",
                                                );
                                                continue;
                                            }
                                        };
                                    let value = std::str::from_utf8(attribute.value.as_ref())
                                        .ok()
                                        .and_then(|raw| quick_xml::escape::unescape(raw).ok());
                                    if value.is_none() {
                                        state.diagnostic(
                                            "xml_attribute_error",
                                            source_line,
                                            "XML attribute value could not be decoded",
                                        );
                                    };
                                    let _ = state.child(
                                        &id,
                                        &attribute_path,
                                        &attribute_name,
                                        "xml_attribute",
                                        source_line,
                                        ChildOptions {
                                            value: value.as_deref().map(ChildValue::String),
                                            structural_only: false,
                                            value_truncated: false,
                                            sensitive_context: base_sensitive
                                                || key_is_sensitive_scalar(&attribute_name)
                                                || (selector_sensitive
                                                    && !xml_attribute_is_selector(&attribute_name))
                                                || (sibling_sensitive
                                                    && !xml_attribute_is_selector(&attribute_name)),
                                            redacted_value_type: None,
                                        },
                                    );
                                }
                                Err(error) => state.diagnostic(
                                    "xml_attribute_error",
                                    source_line,
                                    format!("XML attribute error: {error}"),
                                ),
                            }
                        }
                        stack.push(XmlFrame {
                            id,
                            path,
                            line: source_line,
                            children: BTreeMap::new(),
                            sensitive,
                            sibling_sensitive,
                        });
                    }
                }
            }
            Ok(Event::Empty(event)) => {
                let current_ordinal = element_ordinal;
                element_ordinal = element_ordinal.saturating_add(1);
                if skipped_depth > 0 {
                    // Empty elements do not change the depth of an already skipped subtree.
                } else if stack.len() >= state.limits.max_depth {
                    if !depth_limit_reported {
                        depth_limit_reported = true;
                        state.diagnostic(
                            "depth_limit",
                            line_number(bytes, reader.buffer_position() as usize),
                            format!(
                                "structured depth limit of {} reached in XML",
                                state.limits.max_depth
                            ),
                        );
                    }
                } else {
                    'admit_empty: {
                        let line = line_number(bytes, reader.buffer_position() as usize);
                        let (parent, path, name, base_sensitive) = match prepare_xml_element(
                            state,
                            event.name().as_ref(),
                            &mut stack,
                            line,
                            current_ordinal,
                        ) {
                            XmlElementPreparation::Ready {
                                parent,
                                path,
                                name,
                                sensitive,
                            } => (parent, path, name, sensitive),
                            XmlElementPreparation::PathLimited => break 'admit_empty,
                            XmlElementPreparation::InvalidName => {
                                state.diagnostic(
                                    "xml_name_error",
                                    line,
                                    "XML element name is not UTF-8",
                                );
                                break 'xml;
                            }
                        };
                        let selector_sensitive = xml_selector_is_sensitive(&event);
                        let sensitive = base_sensitive || selector_sensitive;
                        let sibling_sensitive =
                            sibling_sensitive_elements.contains(&current_ordinal);
                        let Some(id) = state.child(
                            &parent,
                            &path,
                            &name,
                            "xml_element",
                            line,
                            ChildOptions::default()
                                .in_sensitive_context(sensitive || sibling_sensitive),
                        ) else {
                            break 'xml;
                        };
                        for (attribute_ordinal, attribute) in event
                            .attributes()
                            .with_checks(false)
                            .enumerate()
                            .filter_map(|(ordinal, attribute)| {
                                attribute.ok().map(|attribute| (ordinal, attribute))
                            })
                        {
                            let (attribute_name, attribute_path) = match prepare_xml_attribute(
                                state,
                                &path,
                                attribute.key.as_ref(),
                                line,
                                attribute_ordinal,
                            ) {
                                XmlAttributePreparation::Ready { name, path } => (name, path),
                                XmlAttributePreparation::PathLimited => continue,
                                XmlAttributePreparation::InvalidName => {
                                    state.diagnostic(
                                        "xml_attribute_error",
                                        line,
                                        "XML attribute name is not UTF-8",
                                    );
                                    continue;
                                }
                            };
                            let value = std::str::from_utf8(attribute.value.as_ref())
                                .ok()
                                .and_then(|raw| quick_xml::escape::unescape(raw).ok());
                            let _ = state.child(
                                &id,
                                &attribute_path,
                                &attribute_name,
                                "xml_attribute",
                                line,
                                ChildOptions {
                                    value: value.as_deref().map(ChildValue::String),
                                    structural_only: false,
                                    value_truncated: false,
                                    sensitive_context: base_sensitive
                                        || key_is_sensitive_scalar(&attribute_name)
                                        || (selector_sensitive
                                            && !xml_attribute_is_selector(&attribute_name))
                                        || (sibling_sensitive
                                            && !xml_attribute_is_selector(&attribute_name)),
                                    redacted_value_type: None,
                                },
                            );
                        }
                    }
                }
            }
            Ok(Event::Text(event)) => {
                if skipped_depth == 0
                    && let Some(frame) = stack.last()
                    && let Ok(decoded) = event.decode()
                    && let Ok(text) = quick_xml::escape::unescape(&decoded)
                {
                    state.append_text(
                        &frame.id,
                        &text,
                        frame.line,
                        frame.sensitive || frame.sibling_sensitive,
                    );
                }
            }
            Ok(Event::CData(event)) => {
                if skipped_depth == 0
                    && let Some(frame) = stack.last()
                    && let Ok(text) = event.decode()
                {
                    state.append_text(
                        &frame.id,
                        &text,
                        frame.line,
                        frame.sensitive || frame.sibling_sensitive,
                    );
                }
            }
            Ok(Event::End(_)) => {
                if skipped_depth > 0 {
                    skipped_depth -= 1;
                } else {
                    stack.pop();
                }
            }
            Ok(Event::DocType(_)) | Ok(Event::GeneralRef(_)) => {
                state.diagnostic(
                    "xml_unsafe_construct",
                    line_number(bytes, reader.buffer_position() as usize),
                    "DOCTYPE and general entity references are not expanded",
                );
                break;
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                state.diagnostic(
                    "xml_parse_error",
                    line_number(bytes, reader.buffer_position() as usize),
                    "XML input is not well formed",
                );
                break;
            }
            _ => {}
        }
        buffer.clear();
    }
}

fn extract_delimited(state: &mut State<'_>, bytes: &[u8], delimiter: u8) {
    // Delimited data has no recursive nesting, so the registry's max_nesting
    // ceiling is its per-record field ceiling. This keeps even delimiter-only
    // records bounded before facts are admitted.
    let max_fields = state.limits.max_depth;
    let max_field_bytes = state.limits.max_scalar_bytes;
    let mut headers: Option<Vec<(String, bool)>> = None;
    let mut field_limit_reported = false;
    let mut scalar_limit_reported = false;
    let parse_result = parse_delimited(
        bytes,
        delimiter,
        state.limits.max_rows,
        max_fields,
        max_field_bytes,
        |row| {
            if row.field_limit_reached && !field_limit_reported {
                field_limit_reported = true;
                state.diagnostic(
                    "field_limit",
                    row.line,
                    format!("delimited field limit of {max_fields} per row reached"),
                );
            }
            if row.scalar_limit_reached && !scalar_limit_reported {
                scalar_limit_reported = true;
                state.diagnostic(
                    "scalar_limit",
                    row.line,
                    format!("delimited field exceeds {max_field_bytes} bytes"),
                );
            }

            if headers.is_none() {
                let Some(headers_id) = state.child(
                    &state.file_id.clone(),
                    "$headers",
                    "headers",
                    "table_header",
                    row.line,
                    ChildOptions::default(),
                ) else {
                    return false;
                };
                let mut retained_headers = Vec::with_capacity(row.fields.len());
                for (column, header) in row.fields.iter().enumerate() {
                    let header_value_sensitive =
                        row.truncated_fields[column] || string_is_sensitive(header);
                    let cell_values_sensitive =
                        header_value_sensitive || key_is_sensitive_scalar(header);
                    let label = if header.is_empty() || header_value_sensitive {
                        format!("column {column}")
                    } else {
                        header.clone()
                    };
                    let options = if row.truncated_fields[column] {
                        ChildOptions::truncated_sensitive_string()
                    } else {
                        ChildOptions::string(header).in_sensitive_context(header_value_sensitive)
                    };
                    if state
                        .child(
                            &headers_id,
                            &format!("$headers[{column}]"),
                            &label,
                            "table_column",
                            row.line,
                            options,
                        )
                        .is_none()
                    {
                        return false;
                    }
                    retained_headers.push((label, cell_values_sensitive));
                }
                headers = Some(retained_headers);
                return true;
            }

            let Some(row_id) = state.child(
                &state.file_id.clone(),
                &format!("$rows[{}]", row.index),
                &format!("row {}", row.index),
                "table_row",
                row.line,
                ChildOptions::default(),
            ) else {
                return false;
            };
            let headers = headers.as_ref().expect("headers initialized above");
            for (column, value) in row.fields.into_iter().enumerate() {
                let fallback_header = (format!("column {column}"), false);
                let (label, cell_values_sensitive) =
                    headers.get(column).unwrap_or(&fallback_header);
                let options = if row.truncated_fields[column] {
                    if *cell_values_sensitive {
                        ChildOptions::truncated_sensitive_string()
                    } else {
                        ChildOptions::truncated_value()
                    }
                } else {
                    ChildOptions::string(&value)
                }
                .in_sensitive_context(*cell_values_sensitive);
                if state
                    .child(
                        &row_id,
                        &format!("$rows[{}][{column}]", row.index),
                        label.as_str(),
                        "table_cell",
                        row.line,
                        options,
                    )
                    .is_none()
                {
                    return false;
                }
            }
            true
        },
    );
    if let Err(error) = parse_result {
        state.diagnostic(error.code, error.line, error.message);
    }
}

fn extract_ini(state: &mut State<'_>, bytes: &[u8]) {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            state.diagnostic(
                "invalid_utf8",
                1,
                format!("INI input is not UTF-8: {error}"),
            );
            return;
        }
    };
    let mut parent = state.file_id.clone();
    let mut prefix = "$".to_owned();
    let mut parent_sensitive = false;
    for (index, raw_line) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
            continue;
        }
        if let Some(section) = trimmed
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
        {
            let section = section.trim();
            if section.is_empty() {
                state.diagnostic("ini_parse_error", line, "empty INI section name");
                continue;
            }
            let path_component = identity_safe_component(section, "ini-section", line);
            let Some(path) =
                bounded_object_path("$", &path_component, state.limits.max_scalar_bytes)
            else {
                state.report_path_limit(line, "INI section");
                continue;
            };
            let section_sensitive = key_is_sensitive(section);
            let Some(section_id) = state.child(
                &state.file_id.clone(),
                &path,
                section,
                "ini_section",
                line,
                ChildOptions::default().in_sensitive_context(section_sensitive),
            ) else {
                return;
            };
            parent = section_id;
            prefix = path;
            parent_sensitive = section_sensitive;
            continue;
        }
        let content = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let split = find_unquoted_byte(content.as_bytes(), b'=')
            .or_else(|| find_unquoted_byte(content.as_bytes(), b':'));
        let Some(split) = split else {
            state.diagnostic("ini_parse_error", line, "expected key=value entry");
            continue;
        };
        let key = content[..split].trim();
        if key.is_empty() {
            state.diagnostic("ini_parse_error", line, "empty INI key");
            continue;
        }
        let raw_value = content[split + 1..].trim();
        let value = raw_value.trim_matches(['\"', '\'']);
        let line_sensitive = structured_string_is_sensitive(content);
        let path_component = identity_safe_component(key, "ini-key", line);
        let Some(path) =
            bounded_object_path(&prefix, &path_component, state.limits.max_scalar_bytes)
        else {
            state.report_path_limit(line, "INI key");
            continue;
        };
        let _ = state.child(
            &parent,
            &path,
            key,
            "ini_key",
            line,
            ChildOptions::string(value).in_sensitive_context(
                parent_sensitive || key_is_sensitive_scalar(key) || line_sensitive,
            ),
        );
    }
}

fn extract_key_value_structure(state: &mut State<'_>, text: &str, delimiter: char, kind: &str) {
    for (index, raw_line) in text.lines().enumerate() {
        let line = index + 1;
        let Some((key, _)) = raw_line.split_once(delimiter) else {
            continue;
        };
        let key = key.trim().trim_matches(['\"', '\'']);
        if key.is_empty() {
            continue;
        }
        let path_component = identity_safe_component(key, "fallback-key", line);
        let Some(path) = bounded_object_path("$", &path_component, state.limits.max_scalar_bytes)
        else {
            state.report_path_limit(line, "key-value fallback");
            continue;
        };
        let _ = state.child(
            &state.file_id.clone(),
            &path,
            key,
            kind,
            line,
            ChildOptions::STRUCTURAL.in_sensitive_context(key_is_sensitive(key)),
        );
    }
}

fn extract_markdown(state: &mut State<'_>, bytes: &[u8]) {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            state.diagnostic("invalid_utf8", 1, format!("Markdown is not UTF-8: {error}"));
            return;
        }
    };
    let mut headings = Vec::<(usize, String)>::new();
    let mut in_fence = false;
    for (index, raw_line) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            let language = trimmed.trim_start_matches(['`', '~']).trim();
            if !language.is_empty() {
                let _ = state.child(
                    &state.file_id.clone(),
                    &format!("$code[{line}]"),
                    language,
                    "code_fence",
                    line,
                    ChildOptions::string(language),
                );
            }
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = trimmed
            .as_bytes()
            .iter()
            .take_while(|byte| **byte == b'#')
            .count();
        if (1..=6).contains(&hashes)
            && trimmed
                .as_bytes()
                .get(hashes)
                .is_some_and(u8::is_ascii_whitespace)
        {
            let label = trimmed[hashes..].trim().trim_end_matches('#').trim();
            if !label.is_empty() {
                while headings.last().is_some_and(|(level, _)| *level >= hashes) {
                    headings.pop();
                }
                let parent = headings
                    .last()
                    .map(|(_, id)| id.clone())
                    .unwrap_or_else(|| state.file_id.clone());
                let path = format!("$heading[{line}]");
                if let Some(id) = state.child(
                    &parent,
                    &path,
                    label,
                    "document_heading",
                    line,
                    ChildOptions::default(),
                ) {
                    headings.push((hashes, id));
                }
            }
        }
        for capture in MARKDOWN_LINK.captures_iter(raw_line) {
            let Some(target) = capture.get(1).map(|value| value.as_str().trim()) else {
                continue;
            };
            let Some(reference) = add_document_reference(state, target, line, "markdown_link")
            else {
                continue;
            };
            let source = headings
                .last()
                .map(|(_, id)| id.clone())
                .unwrap_or_else(|| state.file_id.clone());
            state.edge(&source, &reference, "references", line);
        }
    }
}

fn extract_rst(state: &mut State<'_>, bytes: &[u8]) {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            state.diagnostic(
                "invalid_utf8",
                1,
                format!("reStructuredText is not UTF-8: {error}"),
            );
            return;
        }
    };
    let lines = text.lines().collect::<Vec<_>>();
    let mut active = state.file_id.clone();
    for index in 0..lines.len() {
        let line = index + 1;
        let current = lines[index].trim();
        if let Some(name) = current
            .strip_prefix(".. _")
            .and_then(|value| value.strip_suffix(':'))
        {
            let path = format!("$anchor[{line}]");
            if let Some(id) = state.child(
                &state.file_id.clone(),
                &path,
                name.trim(),
                "document_anchor",
                line,
                ChildOptions::default(),
            ) {
                active = id;
            }
        } else if let Some(directive) = current
            .strip_prefix(".. ")
            .and_then(|value| value.split_once("::"))
        {
            let path = format!("$directive[{line}]");
            if let Some(id) = state.child(
                &active,
                &path,
                directive.0.trim(),
                "rst_directive",
                line,
                ChildOptions::string(directive.1.trim()),
            ) {
                active = id;
            }
        } else if let Some(next) = lines.get(index + 1) {
            let underline = next.trim();
            let marker = underline.chars().next();
            if !current.is_empty()
                && underline.len() >= current.len()
                && marker.is_some_and(|marker| matches!(marker, '=' | '-' | '~' | '^' | '"' | '#'))
                && marker
                    .is_some_and(|marker| underline.chars().all(|candidate| candidate == marker))
            {
                let path = format!("$heading[{line}]");
                if let Some(id) = state.child(
                    &state.file_id.clone(),
                    &path,
                    current,
                    "document_heading",
                    line,
                    ChildOptions::default(),
                ) {
                    active = id;
                }
            }
        }
        for capture in RST_LINK.captures_iter(current) {
            if let Some(target) = capture.get(1)
                && let Some(reference) =
                    add_document_reference(state, target.as_str(), line, "rst_link")
            {
                state.edge(&active, &reference, "references", line);
            }
        }
    }
}

fn extract_asciidoc(state: &mut State<'_>, bytes: &[u8]) {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            state.diagnostic("invalid_utf8", 1, format!("AsciiDoc is not UTF-8: {error}"));
            return;
        }
    };
    let mut headings = Vec::<(usize, String)>::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw_line.trim();
        if let Some(anchor) = trimmed
            .strip_prefix("[[")
            .and_then(|value| value.strip_suffix("]]"))
        {
            let _ = state.child(
                &state.file_id.clone(),
                &format!("$anchor[{line}]"),
                anchor.trim(),
                "document_anchor",
                line,
                ChildOptions::default(),
            );
            continue;
        }
        let level = trimmed
            .as_bytes()
            .iter()
            .take_while(|byte| **byte == b'=')
            .count();
        if level > 0
            && trimmed
                .as_bytes()
                .get(level)
                .is_some_and(u8::is_ascii_whitespace)
        {
            let label = trimmed[level..].trim();
            while headings
                .last()
                .is_some_and(|(parent_level, _)| *parent_level >= level)
            {
                headings.pop();
            }
            let parent = headings
                .last()
                .map(|(_, id)| id.clone())
                .unwrap_or_else(|| state.file_id.clone());
            if let Some(id) = state.child(
                &parent,
                &format!("$heading[{line}]"),
                label,
                "document_heading",
                line,
                ChildOptions::default(),
            ) {
                headings.push((level, id));
            }
        }
        if let Some((label, target)) = trimmed.split_once("link:") {
            let target = target.split('[').next().unwrap_or(target).trim();
            if let Some(reference) = add_document_reference(state, target, line, "asciidoc_link") {
                let source = headings
                    .last()
                    .map(|(_, id)| id.clone())
                    .unwrap_or_else(|| state.file_id.clone());
                state.edge(&source, &reference, "references", line);
            }
            let _ = label;
        }
    }
}

fn extract_html(state: &mut State<'_>, bytes: &[u8]) {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            state.diagnostic("invalid_utf8", 1, format!("HTML is not UTF-8: {error}"));
            return;
        }
    };
    let mut headings = Vec::<(usize, String)>::new();
    for capture in HTML_HEADING.captures_iter(text) {
        let Some(level) = capture
            .get(1)
            .and_then(|value| value.as_str().parse::<usize>().ok())
        else {
            continue;
        };
        let Some(body) = capture.get(2) else {
            continue;
        };
        let label = HTML_TAG.replace_all(body.as_str(), "").trim().to_owned();
        if label.is_empty() {
            continue;
        }
        let line = line_number(bytes, body.start());
        while headings
            .last()
            .is_some_and(|(parent_level, _)| *parent_level >= level)
        {
            headings.pop();
        }
        let parent = headings
            .last()
            .map(|(_, id)| id.clone())
            .unwrap_or_else(|| state.file_id.clone());
        if let Some(id) = state.child(
            &parent,
            &format!("$heading[{line}]"),
            &label,
            "document_heading",
            line,
            ChildOptions::default(),
        ) {
            headings.push((level, id));
        }
    }
    for capture in HTML_LINK.captures_iter(text) {
        let Some(target) = capture.get(1).map(|value| value.as_str().trim()) else {
            continue;
        };
        let line = capture
            .get(1)
            .map(|value| line_number(bytes, value.start()))
            .unwrap_or(1);
        if let Some(reference) = add_document_reference(state, target, line, "html_link") {
            let source = headings
                .last()
                .map(|(_, id)| id.clone())
                .unwrap_or_else(|| state.file_id.clone());
            state.edge(&source, &reference, "references", line);
        }
    }
}

fn add_document_reference(
    state: &mut State<'_>,
    target: &str,
    line: usize,
    kind: &str,
) -> Option<String> {
    let target = target.trim();
    if target.is_empty() || target.starts_with("data:") {
        return None;
    }
    state.child(
        &state.file_id.clone(),
        &format!("$reference[{line}][{}]", make_id(&[target])),
        target,
        kind,
        line,
        ChildOptions::string(target),
    )
}

struct DelimitedError {
    code: &'static str,
    line: usize,
    message: String,
}

struct DelimitedRow {
    index: usize,
    line: usize,
    fields: Vec<String>,
    truncated_fields: Vec<bool>,
    field_limit_reached: bool,
    scalar_limit_reached: bool,
}

fn parse_delimited<F>(
    bytes: &[u8],
    delimiter: u8,
    max_rows: usize,
    max_fields: usize,
    max_field_bytes: usize,
    mut visit: F,
) -> Result<(), DelimitedError>
where
    F: FnMut(DelimitedRow) -> bool,
{
    if let Err(error) = std::str::from_utf8(bytes) {
        return Err(DelimitedError {
            code: "invalid_utf8",
            line: 1,
            message: format!("delimited input is not UTF-8: {error}"),
        });
    }
    let mut fields = Vec::with_capacity(max_fields.min(32));
    let mut truncated_fields = Vec::with_capacity(max_fields.min(32));
    let mut field = Vec::with_capacity(max_field_bytes.min(256));
    let mut quoted = false;
    let mut line = 1usize;
    let mut row_line = line;
    let mut row_index = 0usize;
    let mut column = 0usize;
    let mut row_started = false;
    let mut field_limit_reached = false;
    let mut scalar_limit_reached = false;
    let mut current_field_truncated = false;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            if byte == b'\"' {
                if bytes.get(index + 1) == Some(&b'\"') {
                    push_delimited_byte(
                        &mut field,
                        byte,
                        column,
                        max_fields,
                        max_field_bytes,
                        &mut scalar_limit_reached,
                        &mut current_field_truncated,
                    );
                    index += 2;
                    continue;
                }
                quoted = false;
            } else {
                if byte == b'\n' {
                    line += 1;
                }
                push_delimited_byte(
                    &mut field,
                    byte,
                    column,
                    max_fields,
                    max_field_bytes,
                    &mut scalar_limit_reached,
                    &mut current_field_truncated,
                );
            }
            index += 1;
            continue;
        }
        match byte {
            b'\"' if field.is_empty() => {
                quoted = true;
                row_started = true;
            }
            value if value == delimiter => {
                row_started = true;
                finish_delimited_field(
                    &mut fields,
                    &mut truncated_fields,
                    &mut field,
                    column,
                    max_fields,
                    &mut field_limit_reached,
                    &mut current_field_truncated,
                );
                column += 1;
                field_limit_reached |= column >= max_fields;
            }
            b'\n' => {
                finish_delimited_field(
                    &mut fields,
                    &mut truncated_fields,
                    &mut field,
                    column,
                    max_fields,
                    &mut field_limit_reached,
                    &mut current_field_truncated,
                );
                if row_index >= max_rows {
                    return Err(DelimitedError {
                        code: "row_limit",
                        line: row_line,
                        message: format!("delimited row limit of {max_rows} reached"),
                    });
                }
                let row = DelimitedRow {
                    index: row_index,
                    line: row_line,
                    fields: std::mem::take(&mut fields),
                    truncated_fields: std::mem::take(&mut truncated_fields),
                    field_limit_reached,
                    scalar_limit_reached,
                };
                if !visit(row) {
                    return Ok(());
                }
                row_index += 1;
                line += 1;
                row_line = line;
                column = 0;
                row_started = false;
                field_limit_reached = false;
                scalar_limit_reached = false;
            }
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {}
            _ => {
                row_started = true;
                push_delimited_byte(
                    &mut field,
                    byte,
                    column,
                    max_fields,
                    max_field_bytes,
                    &mut scalar_limit_reached,
                    &mut current_field_truncated,
                );
            }
        }
        index += 1;
    }
    if quoted {
        return Err(DelimitedError {
            code: "csv_parse_error",
            line,
            message: "unterminated quoted field".into(),
        });
    }
    if row_started {
        finish_delimited_field(
            &mut fields,
            &mut truncated_fields,
            &mut field,
            column,
            max_fields,
            &mut field_limit_reached,
            &mut current_field_truncated,
        );
        if row_index >= max_rows {
            return Err(DelimitedError {
                code: "row_limit",
                line: row_line,
                message: format!("delimited row limit of {max_rows} reached"),
            });
        }
        let _ = visit(DelimitedRow {
            index: row_index,
            line: row_line,
            fields,
            truncated_fields,
            field_limit_reached,
            scalar_limit_reached,
        });
    }
    Ok(())
}

fn push_delimited_byte(
    field: &mut Vec<u8>,
    byte: u8,
    column: usize,
    max_fields: usize,
    max_field_bytes: usize,
    scalar_limit_reached: &mut bool,
    current_field_truncated: &mut bool,
) {
    if column >= max_fields {
        return;
    }
    if field.len() < max_field_bytes {
        field.push(byte);
    } else {
        *scalar_limit_reached = true;
        *current_field_truncated = true;
    }
}

fn finish_delimited_field(
    fields: &mut Vec<String>,
    truncated_fields: &mut Vec<bool>,
    field: &mut Vec<u8>,
    column: usize,
    max_fields: usize,
    field_limit_reached: &mut bool,
    current_field_truncated: &mut bool,
) {
    if column >= max_fields {
        *field_limit_reached = true;
        field.clear();
        *current_field_truncated = false;
        return;
    }
    while std::str::from_utf8(field).is_err() {
        field.pop();
    }
    fields.push(String::from_utf8(std::mem::take(field)).expect("validated UTF-8 field prefix"));
    truncated_fields.push(std::mem::take(current_field_truncated));
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Array(_) | Value::Object(_))
}

fn child_value_is_sensitive(value: ChildValue<'_>) -> bool {
    match value {
        ChildValue::Json(value) => json_value_is_sensitive(value),
        ChildValue::String(value) => string_is_sensitive(value),
    }
}

fn child_value_kind(value: ChildValue<'_>) -> &'static str {
    match value {
        ChildValue::Json(value) => value_kind(value),
        ChildValue::String(_) => "string",
    }
}

fn json_value_is_sensitive(value: &Value) -> bool {
    value.as_str().is_some_and(string_is_sensitive)
}

/// Recognize only high-confidence secret-bearing keys. The classifier is
/// deliberately ASCII and allocation-bounded: structured keys longer than the
/// policy ceiling fail closed instead of forcing work proportional to a large
/// attacker-controlled identifier.
fn canonical_key(key: &str) -> Option<String> {
    if key.len() > MAX_SENSITIVE_KEY_BYTES {
        return None;
    }
    let mut canonical = String::with_capacity(key.len());
    canonical.extend(
        key.bytes()
            .filter(u8::is_ascii_alphanumeric)
            .map(|byte| byte.to_ascii_lowercase() as char),
    );
    Some(canonical)
}

fn key_is_sensitive(key: &str) -> bool {
    let Some(canonical) = canonical_key(key) else {
        return true;
    };
    if canonical.is_empty() {
        return false;
    }
    if matches!(
        canonical.as_str(),
        "publictoken"
            | "paginationtoken"
            | "continuationtoken"
            | "pagetoken"
            | "nextpagetoken"
            | "publickey"
    ) {
        return false;
    }

    matches!(
        canonical.as_str(),
        "authorization"
            | "proxyauthorization"
            | "password"
            | "passwords"
            | "passwd"
            | "passphrase"
            | "pwd"
            | "secret"
            | "secrets"
            | "token"
            | "tokens"
            | "credential"
            | "credentials"
            | "cookie"
            | "cookies"
            | "setcookie"
            | "connectionstring"
            | "databaseurl"
            | "dsn"
            | "awssecretaccesskey"
            | "awsaccesskeyid"
            | "secretaccesskey"
            | "accesskeyid"
    ) || [
        "password",
        "passwd",
        "passphrase",
        "secret",
        "token",
        "apikey",
        "privatekey",
        "secretkey",
        "secretaccesskey",
        "credential",
        "credentials",
        "connectionstring",
    ]
    .iter()
    .any(|suffix| canonical.ends_with(suffix))
}

fn key_is_sensitive_scalar(key: &str) -> bool {
    if key_is_sensitive(key) {
        return true;
    }
    canonical_key(key).is_none_or(|canonical| canonical == "auth")
}

fn strip_ascii_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .as_bytes()
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))?;
    value.get(prefix.len()..)
}

fn looks_like_jwt(value: &str) -> bool {
    let mut parts = value.split('.');
    let Some(header) = parts.next() else {
        return false;
    };
    let Some(payload) = parts.next() else {
        return false;
    };
    let Some(signature) = parts.next() else {
        return false;
    };
    if parts.next().is_some()
        || !header.starts_with("eyJ")
        || payload.is_empty()
        || signature.is_empty()
    {
        return false;
    }
    [header, payload, signature].iter().all(|part| {
        part.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    })
}

fn token68(value: &str, minimum: usize) -> bool {
    value.len() >= minimum
        && !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
}

fn basic_credentials(value: &str) -> bool {
    const MAX_BASIC_CREDENTIAL_BYTES: usize = 8 * 1024;
    if value.len() > MAX_BASIC_CREDENTIAL_BYTES {
        return true;
    }
    // A syntactically decoded Basic credential is high-confidence even when a
    // test account uses a short username/password or one side of the colon is
    // empty. Requiring production-looking length here lets valid credentials
    // through while adding no useful prose protection beyond base64 + colon.
    if !token68(value, 4) {
        return false;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value));
    decoded.ok().is_some_and(|decoded| decoded.contains(&b':'))
}

fn looks_like_credentialed_url(value: &str) -> bool {
    let Some(rest) =
        strip_ascii_prefix(value, "https://").or_else(|| strip_ascii_prefix(value, "http://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let Some(userinfo) = authority.rsplit_once('@').map(|(userinfo, _)| userinfo) else {
        return false;
    };
    userinfo
        .split_once(':')
        .is_some_and(|(user, password)| !user.is_empty() || !password.is_empty())
        || sensitive_line(userinfo)
}

fn sensitive_line(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed == REDACTED_STRUCTURED_VALUE {
        return true;
    }

    let first_line = trimmed.lines().next().unwrap_or(trimmed);
    if first_line.starts_with("-----BEGIN ") && first_line.contains("PRIVATE KEY-----") {
        return true;
    }

    if strip_ascii_prefix(trimmed, "bearer ").is_some_and(|credential| token68(credential, 16)) {
        return true;
    }
    if strip_ascii_prefix(trimmed, "basic ").is_some_and(basic_credentials) {
        return true;
    }
    if looks_like_jwt(trimmed) {
        return true;
    }

    for prefix in [
        "ghp_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "sk-proj-",
        "sk-live-",
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "rk_test_",
    ] {
        if trimmed.starts_with(prefix) && trimmed.len() >= prefix.len() + 8 {
            return true;
        }
    }
    if trimmed.len() == 20
        && (trimmed.starts_with("AKIA") || trimmed.starts_with("ASIA"))
        && trimmed
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return true;
    }
    if looks_like_credentialed_url(trimmed) {
        return true;
    }

    let assignment = strip_ascii_prefix(trimmed, "export ").unwrap_or(trimmed);
    let delimiter = assignment
        .find('=')
        .into_iter()
        .chain(assignment.find(':'))
        .min();
    delimiter.is_some_and(|delimiter| {
        key_is_sensitive_scalar(assignment[..delimiter].trim())
            && !assignment[delimiter + 1..].trim().is_empty()
    })
}

pub(crate) fn structured_string_is_sensitive(value: &str) -> bool {
    if value.len() > MAX_SENSITIVE_VALUE_BYTES {
        return true;
    }
    value.lines().any(sensitive_line)
}

fn string_is_sensitive(value: &str) -> bool {
    structured_string_is_sensitive(value)
}

/// Return a deterministic structural spelling that never incorporates a
/// credential-shaped attacker-controlled component. The ordinal is supplied by
/// the owning parser rather than derived from the secret, so paths and IDs do
/// not become stable hashes of credential material either.
fn identity_safe_component(value: &str, namespace: &str, ordinal: usize) -> String {
    if structured_string_is_sensitive(value) {
        format!("<redacted-{namespace}-{ordinal}>")
    } else {
        value.to_owned()
    }
}

fn object_path(parent: &str, key: &str) -> String {
    if parent == "$" {
        format!("$.{key}")
    } else {
        format!("{parent}.{key}")
    }
}

fn bounded_object_path(parent: &str, key: &str, max_bytes: usize) -> Option<String> {
    let path_bytes = parent.len().checked_add(1)?.checked_add(key.len())?;
    (path_bytes <= max_bytes).then(|| object_path(parent, key))
}

fn bounded_array_path(parent: &str, index: usize, max_bytes: usize) -> Option<String> {
    let suffix = format!("[{index}]");
    let path_bytes = parent.len().checked_add(suffix.len())?;
    (path_bytes <= max_bytes).then(|| format!("{parent}{suffix}"))
}

fn bounded_xml_path(
    parent: &str,
    name: &str,
    occurrence: usize,
    max_bytes: usize,
) -> Option<String> {
    let occurrence_digits = occurrence.checked_ilog10().unwrap_or(0) as usize + 1;
    let path_bytes = parent
        .len()
        .checked_add(1)?
        .checked_add(name.len())?
        .checked_add(2)?
        .checked_add(occurrence_digits)?;
    (path_bytes <= max_bytes).then(|| format!("{parent}/{name}[{occurrence}]"))
}

fn bounded_xml_attribute_path(parent: &str, name: &str, max_bytes: usize) -> Option<String> {
    let path_bytes = parent.len().checked_add(2)?.checked_add(name.len())?;
    (path_bytes <= max_bytes).then(|| format!("{parent}/@{name}"))
}

#[derive(Default)]
struct SerializedSize {
    bytes: usize,
}

impl Write for SerializedSize {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Measure the JSON representation without allocating a second serialized
/// buffer. Callers use this before cloning a scalar into retained graph facts.
fn serialized_len_at_most<T: Serialize + ?Sized>(value: &T, maximum: usize) -> bool {
    let mut size = SerializedSize::default();
    serde_json::to_writer(&mut size, value).is_ok() && size.bytes <= maximum
}

fn xml_name(raw: &[u8]) -> Option<String> {
    std::str::from_utf8(raw)
        .ok()
        .map(|name| name.rsplit([':', '}']).next().unwrap_or(name).to_owned())
}

fn xml_attribute_is_selector(name: &str) -> bool {
    name.eq_ignore_ascii_case("name")
        || name.eq_ignore_ascii_case("key")
        || name.eq_ignore_ascii_case("property")
}

fn xml_local_name_bytes(raw: &[u8]) -> &[u8] {
    raw.iter()
        .rposition(|byte| matches!(byte, b':' | b'}'))
        .and_then(|index| raw.get(index + 1..))
        .unwrap_or(raw)
}

fn xml_selector_attribute_name(raw: &[u8]) -> bool {
    if raw.len() > MAX_SENSITIVE_KEY_BYTES {
        return false;
    }
    let local = xml_local_name_bytes(raw);
    local.eq_ignore_ascii_case(b"name")
        || local.eq_ignore_ascii_case(b"key")
        || local.eq_ignore_ascii_case(b"property")
}

fn xml_selector_is_sensitive(event: &quick_xml::events::BytesStart<'_>) -> bool {
    event
        .attributes()
        .with_checks(false)
        .flatten()
        .any(|attribute| {
            if !xml_selector_attribute_name(attribute.key.as_ref()) {
                return false;
            }
            let raw = attribute.value.as_ref();
            if raw.len() > MAX_SENSITIVE_KEY_BYTES {
                return true;
            }
            let Ok(raw) = std::str::from_utf8(raw) else {
                return true;
            };
            quick_xml::escape::unescape(raw)
                .map(|value| key_is_sensitive_scalar(&value))
                .unwrap_or(true)
        })
}

fn xml_element_is_selector(raw: &[u8]) -> bool {
    if raw.len() > MAX_SENSITIVE_KEY_BYTES {
        return false;
    }
    let local = xml_local_name_bytes(raw);
    local.eq_ignore_ascii_case(b"name")
        || local.eq_ignore_ascii_case(b"key")
        || local.eq_ignore_ascii_case(b"property")
}

struct XmlSelectorFrame {
    ordinal: usize,
    selector: bool,
    text: String,
    text_overflowed: bool,
}

fn append_xml_selector_text(frame: &mut XmlSelectorFrame, text: &str) {
    if frame.text_overflowed {
        return;
    }
    if text.is_empty() {
        return;
    }
    let Some(next_len) = frame.text.len().checked_add(text.len()) else {
        frame.text_overflowed = true;
        frame.text.clear();
        return;
    };
    if next_len > MAX_SENSITIVE_KEY_BYTES {
        frame.text_overflowed = true;
        frame.text.clear();
        return;
    }
    frame.text.push_str(text);
}

/// Recognize order-independent `<entry><key>password</key><value>…</value>`
/// layouts without retaining a decoded XML tree. The first pass keeps at most
/// one bounded selector string per admitted nesting level and at most one
/// parent ordinal per fact that the second pass could publish.
fn xml_sibling_sensitive_elements(bytes: &[u8], limits: StructuredLimits) -> BTreeSet<usize> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut stack = Vec::<XmlSelectorFrame>::new();
    let mut sensitive = BTreeSet::new();
    let mut element_ordinal = 0usize;
    let mut skipped_depth = 0usize;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) => {
                let ordinal = element_ordinal;
                element_ordinal = element_ordinal.saturating_add(1);
                if skipped_depth > 0 {
                    skipped_depth = skipped_depth.saturating_add(1);
                } else if stack.len() >= limits.max_depth {
                    skipped_depth = 1;
                } else {
                    stack.push(XmlSelectorFrame {
                        ordinal,
                        selector: xml_element_is_selector(event.name().as_ref()),
                        text: String::new(),
                        text_overflowed: false,
                    });
                }
            }
            Ok(Event::Empty(_)) => {
                element_ordinal = element_ordinal.saturating_add(1);
            }
            Ok(Event::Text(event)) if skipped_depth == 0 => {
                if let Some(frame) = stack.last_mut() {
                    match event.decode() {
                        Ok(decoded) => match quick_xml::escape::unescape(&decoded) {
                            Ok(text) => append_xml_selector_text(frame, &text),
                            Err(_) if frame.selector => frame.text_overflowed = true,
                            Err(_) => {}
                        },
                        Err(_) if frame.selector => frame.text_overflowed = true,
                        Err(_) => {}
                    }
                }
            }
            Ok(Event::CData(event)) if skipped_depth == 0 => {
                if let Some(frame) = stack.last_mut() {
                    match event.decode() {
                        Ok(text) => append_xml_selector_text(frame, &text),
                        Err(_) if frame.selector => frame.text_overflowed = true,
                        Err(_) => {}
                    }
                }
            }
            Ok(Event::End(_)) => {
                if skipped_depth > 0 {
                    skipped_depth -= 1;
                } else if let Some(frame) = stack.pop() {
                    let selector_sensitive = frame.selector
                        && (frame.text_overflowed || key_is_sensitive_scalar(frame.text.trim()));
                    if selector_sensitive
                        && sensitive.len() < limits.max_facts
                        && let Some(parent) = stack.last()
                    {
                        sensitive.insert(parent.ordinal);
                    }
                    if let Some(parent) = stack.last_mut()
                        && parent.selector
                    {
                        if frame.text_overflowed {
                            parent.text_overflowed = true;
                            parent.text.clear();
                        } else {
                            append_xml_selector_text(parent, &frame.text);
                        }
                    }
                }
            }
            Ok(Event::DocType(_) | Event::GeneralRef(_) | Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
    sensitive
}

struct XmlFrame {
    id: String,
    path: String,
    line: usize,
    children: BTreeMap<String, usize>,
    sensitive: bool,
    sibling_sensitive: bool,
}

enum XmlElementPreparation {
    Ready {
        parent: String,
        path: String,
        name: String,
        sensitive: bool,
    },
    PathLimited,
    InvalidName,
}

fn prepare_xml_element(
    state: &mut State<'_>,
    raw_name: &[u8],
    stack: &mut [XmlFrame],
    line: usize,
    identity_ordinal: usize,
) -> XmlElementPreparation {
    if raw_name.len() > state.limits.max_scalar_bytes {
        state.report_path_limit(line, "XML");
        return XmlElementPreparation::PathLimited;
    }
    let Some(name) = xml_name(raw_name) else {
        return XmlElementPreparation::InvalidName;
    };
    let Some(occurrence) = stack
        .last()
        .map(|frame| frame.children.get(&name).copied().unwrap_or(0))
        .unwrap_or(0)
        .checked_add(1)
    else {
        state.report_path_limit(line, "XML");
        return XmlElementPreparation::PathLimited;
    };
    let (parent, prefix) = stack
        .last()
        .map(|frame| (frame.id.clone(), frame.path.as_str()))
        .unwrap_or_else(|| (state.file_id.clone(), "$"));
    let path_name = identity_safe_component(&name, "xml-element", identity_ordinal);
    let Some(path) = bounded_xml_path(
        prefix,
        &path_name,
        occurrence,
        state.limits.max_scalar_bytes,
    ) else {
        state.report_path_limit(line, "XML");
        return XmlElementPreparation::PathLimited;
    };
    if let Some(frame) = stack.last_mut() {
        frame.children.insert(name.clone(), occurrence);
    }
    let parent_sensitive = stack.last().is_some_and(|frame| {
        frame.sensitive || (frame.sibling_sensitive && !xml_element_is_selector(name.as_bytes()))
    });
    let sensitive = parent_sensitive || key_is_sensitive(&name);
    XmlElementPreparation::Ready {
        parent,
        path,
        name,
        sensitive,
    }
}

enum XmlAttributePreparation {
    Ready { name: String, path: String },
    PathLimited,
    InvalidName,
}

fn prepare_xml_attribute(
    state: &mut State<'_>,
    parent: &str,
    raw_name: &[u8],
    line: usize,
    identity_ordinal: usize,
) -> XmlAttributePreparation {
    if raw_name.len() > state.limits.max_scalar_bytes {
        state.report_path_limit(line, "XML");
        return XmlAttributePreparation::PathLimited;
    }
    let Some(name) = xml_name(raw_name) else {
        return XmlAttributePreparation::InvalidName;
    };
    let path_name = identity_safe_component(&name, "xml-attribute", identity_ordinal);
    let Some(path) = bounded_xml_attribute_path(parent, &path_name, state.limits.max_scalar_bytes)
    else {
        state.report_path_limit(line, "XML");
        return XmlAttributePreparation::PathLimited;
    };
    XmlAttributePreparation::Ready { name, path }
}

fn line_number(bytes: &[u8], offset: usize) -> usize {
    crate::bytes::line_number(bytes, offset)
}

fn find_unquoted_byte(bytes: &[u8], needle: u8) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        match quote {
            Some(_) if escaped => escaped = false,
            Some(active) if byte == b'\\' && active == b'\"' => escaped = true,
            Some(active) if byte == active => quote = None,
            Some(_) => {}
            None if matches!(byte, b'\"' | b'\'') => quote = Some(byte),
            None if byte == needle => return Some(index),
            None => {}
        }
    }
    None
}

fn strip_json_comments(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index + 1 < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(_) if escaped => escaped = false,
            Some(active) if byte == b'\\' && active == b'\"' => escaped = true,
            Some(active) if byte == active => quote = None,
            Some(_) => {}
            None if matches!(byte, b'\"' | b'\'') => quote = Some(byte),
            None if byte == b'/' && bytes[index + 1] == b'/' => return &line[..index],
            None => {}
        }
        index += 1;
    }
    line
}

fn strip_yaml_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote = None;
    let mut index = 0;
    while index < bytes.len() {
        match quote {
            Some(active) if bytes[index] == active => quote = None,
            Some(_) => {}
            None if matches!(bytes[index], b'\"' | b'\'') => quote = Some(bytes[index]),
            None if bytes[index] == b'#'
                && (index == 0 || bytes[index - 1].is_ascii_whitespace()) =>
            {
                return &line[..index]
            }
            None => {}
        }
        index += 1;
    }
    line
}

fn is_json5_key_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '$' | '-' | '.')
}

fn mcp_key_redacts_descendants(format: StructuredFormat, key: &str) -> bool {
    matches!(format, StructuredFormat::Json { mcp: true }) && matches!(key, "args" | "env")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn extract(name: &str, input: &[u8]) -> StructuredExtraction {
        extract_structured_bytes(Path::new(name), name, input)
            .expect("registered structured extension")
    }

    fn values(extraction: &StructuredExtraction) -> Vec<Value> {
        extraction
            .extraction
            .nodes
            .iter()
            .filter_map(|node| node.extra.get("structured_value").cloned())
            .collect()
    }

    fn rendered(extraction: &StructuredExtraction) -> String {
        serde_json::to_string(&extraction.extraction).expect("serialize structured extraction")
    }

    fn node_with_path<'a>(extraction: &'a StructuredExtraction, path: &str) -> &'a Node {
        extraction
            .extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("structured_path").and_then(Value::as_str) == Some(path))
            .unwrap_or_else(|| panic!("missing structured node at {path:?}"))
    }

    fn semantic_extraction(name: &str, input: &[u8]) -> Result<StructuredExtraction, String> {
        let spec = crate::format_registry::format_registry()
            .find_by_path(Path::new(name))
            .ok_or_else(|| format!("{name} has no registered format"))?;
        if spec.capability != crate::format_registry::FormatCapability::SemanticFull {
            return Err(format!(
                "{} advertises {}, not semantic_full",
                spec.id.as_str(),
                spec.capability.as_str()
            ));
        }
        let output = extract_structured_bytes(Path::new(name), name, input)
            .ok_or_else(|| format!("{} has no structured adapter route", spec.id.as_str()))?;
        if output.extraction.nodes.len() <= 1 {
            return Err(format!(
                "{} produced no domain facts beyond its file root",
                spec.id.as_str()
            ));
        }
        if output.extraction.nodes.iter().any(|node| {
            node.extra.get("structured_unparsed") == Some(&Value::Bool(true))
                || matches!(
                    node.extra.get("parse_status").and_then(Value::as_str),
                    Some("inventory_only" | "rejected")
                )
        }) {
            return Err(format!(
                "{} produced fallback or inventory facts for a semantic fixture",
                spec.id.as_str()
            ));
        }
        Ok(output)
    }

    #[test]
    fn jsonc_captures_typed_values_without_io() {
        let output = extract(
            "settings.jsonc",
            br#"{ // comment
              "enabled": true,
              "count": 4,
              "labels": ["one", "two",],
            }"#,
        );
        assert!(output.diagnostics.is_empty());
        assert!(values(&output).contains(&Value::Bool(true)));
        assert!(values(&output).contains(&Value::from(4)));
        assert!(output
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "labels" && node.extra["type"] == "array"));
    }

    #[test]
    fn valid_json5_is_truthfully_reported_as_structural_partial() {
        let output = extract(
            "settings.json5",
            b"{\n  // JSON5 permits unquoted keys and single-quoted strings\n  unquoted: 'value',\n  nested: {\n    item: 1,\n  },\n}\n",
        );
        assert!(output
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "json5_structural_only"));
        assert_eq!(
            output.extraction.nodes[0].extra["format_capability"],
            "structural_partial"
        );
        assert!(output
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "unquoted" && node.extra["structured_unparsed"] == true));
    }

    #[test]
    fn json_lines_decodes_each_valid_record_and_retains_line_diagnostics() {
        let output = extract(
            "events.ndjson",
            b"{\"service\":\"api\",\"replicas\":2}\n\n{\"service\":\"worker\",\"replicas\":1}\n",
        );
        assert!(output.diagnostics.is_empty());
        assert_eq!(
            output.extraction.nodes[0].extra["format_capability"],
            "semantic_full"
        );
        assert!(values(&output).contains(&Value::String("api".into())));
        assert!(values(&output).contains(&Value::from(1)));
        assert_eq!(
            output
                .extraction
                .nodes
                .iter()
                .filter(|node| node.extra["type"] == "json_record")
                .count(),
            2
        );
        assert!(!output.extraction.nodes.iter().any(|node| {
            node.extra
                .get("structured_unparsed")
                .and_then(Value::as_bool)
                == Some(true)
        }));

        let partial = extract(
            "events.jsonl",
            b"{\"service\":\"api\"}\nnot-json\n{\"service\":\"worker\"}\n",
        );
        assert!(partial
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "json_lines_parse_error" && diagnostic.line == 2));
        assert_eq!(
            partial
                .extraction
                .nodes
                .iter()
                .filter(|node| node.extra["type"] == "json_record")
                .count(),
            2
        );
    }

    #[test]
    fn semantic_contract_requires_parsed_domain_facts_for_valid_representations() {
        let fixtures: &[(&str, &[u8], &str)] = &[
            ("service.json", br#"{"service":{"replicas":3}}"#, "replicas"),
            (
                "settings.jsonc",
                b"{ // comment\n \"enabled\": true,\n}\n",
                "enabled",
            ),
            (
                "map.geojson",
                br#"{"type":"Point","coordinates":[1,2]}"#,
                "coordinates",
            ),
            (
                "map.topojson",
                br#"{"type":"Topology","objects":{"district":{}}}"#,
                "district",
            ),
            (
                "session.har",
                br#"{"log":{"entries":[{"request":{"url":"https://example.test"}}]}}"#,
                "url",
            ),
            (
                "app.webmanifest",
                br#"{"name":"Graphoxide","start_url":"/"}"#,
                "start_url",
            ),
            (
                "notebook.ipynb",
                br#"{"cells":[{"cell_type":"code","source":["x = 1"]}]}"#,
                "cell_type",
            ),
            (
                "events.jsonl",
                b"{\"service\":\"api\"}\n{\"service\":\"worker\"}\n",
                "service",
            ),
            (
                "events.ndjson",
                b"{\"event\":\"started\"}\n{\"event\":\"stopped\"}\n",
                "event",
            ),
            ("service.toml", b"[service]\nreplicas = 3\n", "replicas"),
            (
                "service.xml",
                b"<service><replicas>3</replicas></service>",
                "replicas",
            ),
            (".prettierrc", br#"{"printWidth":100}"#, "printWidth"),
        ];

        for (name, input, expected_label) in fixtures {
            let output =
                semantic_extraction(name, input).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(
                output
                    .extraction
                    .nodes
                    .iter()
                    .any(|node| node.label == *expected_label),
                "{name} did not produce expected domain fact {expected_label:?}"
            );
        }
    }

    #[test]
    fn partial_and_inventory_capabilities_cannot_satisfy_semantic_contract() {
        for (name, input) in [
            ("service.yaml", b"service:\n  replicas: 3\n".as_slice()),
            ("settings.json5", b"{service: 'api'}".as_slice()),
            ("notes.txt", b"service api".as_slice()),
            ("config.cue", b"service: { replicas: 3 }".as_slice()),
            ("table.csv", b"name,replicas\napi,3\n".as_slice()),
            ("service.ini", b"[service]\nreplicas=3\n".as_slice()),
            (
                "schema.xsd",
                br#"<schema><element name="service"/></schema>"#.as_slice(),
            ),
            ("guide.rst", b"Guide\n=====\n".as_slice()),
            ("guide.adoc", b"= Guide\n".as_slice()),
            ("guide.html", b"<h1>Guide</h1>".as_slice()),
        ] {
            let error = semantic_extraction(name, input)
                .expect_err("non-semantic capability must not pass semantic fixture checks");
            assert!(error.contains("not semantic_full"), "{name}: {error}");
        }
    }

    #[test]
    fn mcp_values_are_redacted_but_server_structure_remains() {
        let output = extract(
            ".mcp.json",
            br#"{"mcpServers":{"private":{"command":"graphoxide","args":["--token","secret"],"env":{"TOKEN":"secret"},"url":"https://safe.example"},"credential-command":{"command":"ghp_1234567890abcdef"}}}"#,
        );
        let rendered = rendered(&output);
        assert!(rendered.contains("graphoxide"));
        assert!(!rendered.contains("ghp_1234567890abcdef"));
        assert!(!rendered.contains("\"secret\""));
        assert!(rendered.contains("\"TOKEN\""));
        assert!(rendered.contains("mcpServers"));
        assert!(rendered.contains("https://safe.example"));
        let args = node_with_path(&output, "$.mcpServers.private.args");
        assert_eq!(args.extra["type"], "array");
        assert_eq!(args.extra["structured_descendants_redacted"], true);
        let environment = node_with_path(&output, "$.mcpServers.private.env");
        assert_eq!(environment.extra["type"], "object");
        assert_eq!(environment.extra["structured_descendants_redacted"], true);
        let token = node_with_path(&output, "$.mcpServers.private.env.TOKEN");
        assert_eq!(token.extra["structured_value"], REDACTED_STRUCTURED_VALUE);
        assert_eq!(token.extra["structured_value_redacted"], true);
        assert_eq!(token.extra["structured_value_type"], "string");
    }

    #[test]
    fn sensitive_classifier_covers_credentials_without_redacting_prose_or_safe_keys() {
        for key in [
            "password",
            "api_key",
            "AWS_SECRET_ACCESS_KEY",
            "client-secret",
            "databaseUrl",
        ] {
            assert!(key_is_sensitive(key), "expected sensitive key: {key}");
        }
        assert!(key_is_sensitive_scalar("_auth"));
        for key in [
            "authentication",
            "public_token",
            "pagination_token",
            "public_key",
            "token_count",
        ] {
            assert!(!key_is_sensitive(key), "unexpected sensitive key: {key}");
        }

        for value in [
            "Bearer abcdefghijklmnop",
            "Basic dXNlcjpwYXNzd29yZA==",
            "Basic YTpi",
            "Basic OnBhc3M=",
            "ghp_1234567890abcdef",
            "sk_live_1234567890abcdef",
            "https://user:password@example.test/path",
            "https://:password@example.test/path",
            "https://ghp_1234567890abcdef@example.test/path",
            "SAFE=visible\nTOKEN=multiline-secret",
            "-----BEGIN PRIVATE KEY-----\nprivate-material",
        ] {
            assert!(
                structured_string_is_sensitive(value),
                "expected sensitive value: {value:?}"
            );
        }
        assert!(structured_string_is_sensitive(
            &"x".repeat(MAX_SENSITIVE_VALUE_BYTES + 1)
        ));
        for value in [
            "Basic authentication settings",
            "Bearer token documentation",
            "https://example.test/path",
            "https://alice@example.test/path",
            "SAFE=visible",
            "public-token-value",
        ] {
            assert!(
                !structured_string_is_sensitive(value),
                "unexpected sensitive value: {value:?}"
            );
        }
    }

    #[test]
    fn credential_shaped_keys_never_reach_paths_or_secret_derived_ids() {
        let first_secret = "ghp_aaaaaaaaaaaaaaaa";
        let second_secret = "ghp_bbbbbbbbbbbbbbbb";
        let first = extract(
            "settings.json",
            format!(r#"{{"{first_secret}":{{"child":"visible"}}}}"#).as_bytes(),
        );
        let second = extract(
            "settings.json",
            format!(r#"{{"{second_secret}":{{"child":"visible"}}}}"#).as_bytes(),
        );
        let first_rendered = rendered(&first);
        let second_rendered = rendered(&second);
        assert!(!first_rendered.contains(first_secret));
        assert!(!second_rendered.contains(second_secret));
        assert_eq!(
            first
                .extraction
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            second
                .extraction
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>()
        );
        assert!(first.extraction.nodes.iter().any(|node| {
            node.extra.get("structured_path_redacted") == Some(&Value::Bool(true))
                && node.label == REDACTED_STRUCTURED_VALUE
        }));
        assert!(first_rendered.contains("visible"));
    }

    #[test]
    fn xml_errors_and_split_events_cannot_publish_secret_material() {
        let diagnostic_secret = "ghp_diagnosticsecret1";
        let malformed = extract(
            "settings.xml",
            format!("<root></{diagnostic_secret}>").as_bytes(),
        );
        let malformed_rendered = rendered(&malformed);
        assert!(!malformed_rendered.contains(diagnostic_secret));
        assert!(malformed_rendered.contains("XML input is not well formed"));

        let split = extract(
            "settings.xml",
            br#"<root>
                <entry><key>pass<![CDATA[word]]></key><value>XML_SPLIT_SELECTOR_SECRET</value></entry>
                <value>ghp_<![CDATA[1234567890abcdef]]></value>
            </root>"#,
        );
        let split_rendered = rendered(&split);
        assert!(!split_rendered.contains("XML_SPLIT_SELECTOR_SECRET"));
        assert!(!split_rendered.contains("1234567890abcdef"));
        assert!(split.extraction.nodes.iter().any(|node| {
            node.extra.get("structured_text_redacted") == Some(&Value::Bool(true))
        }));
    }

    #[test]
    fn ini_bare_credentialed_urls_are_redacted_before_colon_splitting() {
        for credentialed in [
            "https://user:password@example.test/path",
            "https://:password@example.test/path",
            "https://ghp_1234567890abcdef@example.test/path",
        ] {
            let output = extract(
                "settings.ini",
                format!("{credentialed}\nmode=visible-mode\n").as_bytes(),
            );
            let output_rendered = rendered(&output);
            assert!(!output_rendered.contains(credentialed));
            assert!(!output_rendered.contains("user:password"));
            assert!(!output_rendered.contains("1234567890abcdef"));
            assert!(output_rendered.contains("visible-mode"));
            assert!(output.extraction.nodes.iter().any(|node| {
                node.extra.get("structured_value_redacted") == Some(&Value::Bool(true))
            }));
        }
    }

    #[test]
    fn secret_like_document_labels_are_redacted_centrally() {
        let heading_secret = "ghp_1234567890abcdef";
        let link_secret = "https://user:password@example.test/path";
        let output = extract(
            "README.md",
            format!("# {heading_secret}\n\n[private]({link_secret})\n").as_bytes(),
        );
        let rendered = rendered(&output);
        assert!(!rendered.contains(heading_secret));
        assert!(!rendered.contains(link_secret));
        assert!(output.extraction.nodes.iter().any(|node| {
            node.label == REDACTED_STRUCTURED_VALUE
                && node.extra.get("structured_label_redacted") == Some(&Value::Bool(true))
        }));
    }

    #[test]
    fn malformed_and_limit_diagnostics_never_echo_secret_values() {
        let malformed = [
            (
                "settings.json",
                br#"{"password":"MALFORMED_JSON_SECRET""#.as_slice(),
                "MALFORMED_JSON_SECRET",
            ),
            (
                "settings.toml",
                br#"password = "MALFORMED_TOML_SECRET"#.as_slice(),
                "MALFORMED_TOML_SECRET",
            ),
            (
                "settings.xml",
                br#"<entry name="password">MALFORMED_XML_SECRET"#.as_slice(),
                "MALFORMED_XML_SECRET",
            ),
            (
                "accounts.csv",
                b"password\n\"MALFORMED_CSV_SECRET".as_slice(),
                "MALFORMED_CSV_SECRET",
            ),
        ];
        for (name, input, secret) in malformed {
            let output = extract(name, input);
            assert!(
                !rendered(&output).contains(secret),
                "{name} diagnostic retained {secret}"
            );
        }

        let oversized_secret = format!(
            "prefix-LIMIT_SECRET_SENTINEL-{}",
            "x".repeat(DEFAULT_MAX_SCALAR_BYTES)
        );
        let output = extract(
            "settings.json",
            serde_json::to_string(&serde_json::json!({"description": oversized_secret}))
                .expect("serialize oversized fixture")
                .as_bytes(),
        );
        let rendered = rendered(&output);
        assert!(!rendered.contains("LIMIT_SECRET_SENTINEL"));
        assert!(rendered.contains("structured_value_truncated"));
    }

    #[test]
    fn json_toml_ini_and_tables_redact_values_but_preserve_safe_structure() {
        type RedactionFixture<'a> = (&'a str, &'a [u8], &'a [&'a str], &'a [&'a str]);
        let fixtures: [RedactionFixture<'_>; 5] = [
            (
                "settings.json",
                br#"{"password":"JSON_SECRET_SENTINEL","authentication":{"type":"oauth2"},"public_token":"visible-cursor","script":"SAFE=ok\nTOKEN=MULTILINE_SECRET_SENTINEL"}"#,
                &["JSON_SECRET_SENTINEL", "MULTILINE_SECRET_SENTINEL"],
                &["oauth2", "visible-cursor"],
            ),
            (
                "settings.toml",
                b"api_key = \"TOML_SECRET_SENTINEL\"\nmode = \"visible-mode\"\n",
                &["TOML_SECRET_SENTINEL"],
                &["visible-mode"],
            ),
            (
                ".env",
                b"AWS_SECRET_ACCESS_KEY=ENV_SECRET_SENTINEL\nMODE=visible-env\n",
                &["ENV_SECRET_SENTINEL"],
                &["visible-env"],
            ),
            (
                "service.properties",
                b"db.password=PROPERTY_SECRET_SENTINEL\ndb.mode=visible-property\n",
                &["PROPERTY_SECRET_SENTINEL"],
                &["visible-property"],
            ),
            (
                "accounts.csv",
                b"name,password,public_token\napi,CSV_SECRET_SENTINEL,visible-page-token\n",
                &["CSV_SECRET_SENTINEL"],
                &["api", "visible-page-token"],
            ),
        ];
        for (name, input, secrets, visible) in fixtures {
            let output = extract(name, input);
            let rendered = rendered(&output);
            for secret in secrets {
                assert!(!rendered.contains(secret), "{name} leaked {secret}");
            }
            for expected in visible {
                assert!(
                    rendered.contains(expected),
                    "{name} dropped safe value {expected}"
                );
            }
            assert!(
                output.extraction.nodes.iter().any(|node| {
                    node.extra.get("structured_value_redacted") == Some(&Value::Bool(true))
                }),
                "{name} did not retain an explicit scalar redaction marker"
            );
        }
    }

    #[test]
    fn xml_redacts_direct_attribute_and_order_independent_sibling_values() {
        let output = extract(
            "settings.xml",
            br#"<root>
                <password>XML_DIRECT_SECRET</password>
                <property name="password" value="XML_ATTRIBUTE_SECRET">XML_PROPERTY_SECRET</property>
                <entry><value>XML_BEFORE_SECRET</value><key>password</key></entry>
                <entry><key>password</key><value>XML_AFTER_SECRET</value></entry>
                <property name="theme" value="visible-theme">visible-text</property>
            </root>"#,
        );
        let rendered = rendered(&output);
        for secret in [
            "XML_DIRECT_SECRET",
            "XML_ATTRIBUTE_SECRET",
            "XML_PROPERTY_SECRET",
            "XML_BEFORE_SECRET",
            "XML_AFTER_SECRET",
        ] {
            assert!(!rendered.contains(secret), "XML leaked {secret}");
        }
        assert!(rendered.contains("visible-theme"));
        assert!(rendered.contains("visible-text"));
        assert!(output.extraction.nodes.iter().any(|node| {
            node.extra.get("structured_text_redacted") == Some(&Value::Bool(true))
        }));
    }

    #[test]
    fn secret_like_or_truncated_table_headers_never_become_labels() {
        let secret_header = "ghp_1234567890abcdef";
        let output = extract(
            "headers.csv",
            format!("{secret_header},name\nvalue,alice\n").as_bytes(),
        );
        let serialized = rendered(&output);
        assert!(!serialized.contains(secret_header));
        assert!(output
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "column 0"));

        let limited = extract_structured_bytes_with_limits(
            Path::new("headers.csv"),
            "headers.csv",
            b"password-too-long,name\nTRUNCATED_CELL_SECRET,alice\n",
            StructuredLimits {
                max_scalar_bytes: 16,
                ..StructuredLimits::default()
            },
        )
        .expect("registered CSV");
        let rendered = rendered(&limited);
        assert!(!rendered.contains("password"));
        assert!(!rendered.contains("TRUNCATED_CELL_SECRET"));
        let cell = node_with_path(&limited, "$rows[1][0]");
        assert_eq!(cell.extra["structured_value_truncated"], true);
        assert_eq!(cell.extra["structured_value_redacted"], true);
        assert_eq!(cell.extra["structured_value_type"], "string");
    }

    #[test]
    fn scalar_size_admission_counts_json_escaping_without_an_output_buffer() {
        let scalar = Value::String("line\n\"snow: 雪".into());
        let encoded = serde_json::to_string(&scalar).expect("serialize scalar fixture");
        assert!(serialized_len_at_most(&scalar, encoded.len()));
        assert!(!serialized_len_at_most(&scalar, encoded.len() - 1));
        assert!(serialized_len_at_most("line\n\"snow: 雪", encoded.len()));
    }

    #[test]
    fn toml_and_xml_preserve_parser_values() {
        let toml = extract(
            "Cargo.toml",
            b"[package]\nname = \"graphoxide\"\nversion = 1\n",
        );
        assert!(values(&toml).contains(&Value::String("graphoxide".into())));
        assert!(values(&toml).contains(&Value::from(1)));

        let xml = extract(
            "diagram.xml",
            b"<root enabled=\"true\"><child id=\"7\">ok</child><child id=\"8\" /></root>",
        );
        assert!(values(&xml).contains(&Value::String("true".into())));
        assert!(values(&xml).contains(&Value::String("8".into())));
        assert!(xml
            .extraction
            .nodes
            .iter()
            .any(|node| node.extra.get("structured_text") == Some(&Value::String("ok".into()))));
    }

    #[test]
    fn yaml_ini_and_delimited_inputs_have_bounded_structure() {
        let yaml = extract(
            "service.yaml",
            b"services:\n  - name: api\n    replicas: 3\n",
        );
        assert_eq!(
            yaml.extraction.nodes[0].extra["format_capability"],
            "structural_partial"
        );
        assert!(yaml
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "services" && node.extra["structured_unparsed"] == true));

        let ini = extract("app.env", b"export PORT=3000\n[database]\nhost=localhost\n");
        assert!(values(&ini).contains(&Value::String("3000".into())));
        assert!(values(&ini).contains(&Value::String("localhost".into())));

        let csv = extract("data.csv", b"name,enabled\napi,true\nworker,false\n");
        assert!(values(&csv).contains(&Value::String("api".into())));
        assert!(values(&csv).contains(&Value::String("false".into())));
    }

    #[test]
    fn documentation_formats_emit_headings_and_references() {
        let markdown = extract("guide.md", b"# Guide\nSee [API](api.md).\n## Detail\n");
        assert!(markdown
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "Guide"));
        assert!(markdown
            .extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "references"));

        let rst = extract("guide.rst", b"Guide\n=====\n\n.. note:: Useful\n");
        assert!(rst
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "Guide"));

        let adoc = extract("guide.adoc", b"= Guide\n\n== Detail\n");
        assert!(adoc
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "Detail"));

        let html = extract("guide.html", b"<h1>Guide</h1><a href=\"api.html\">API</a>");
        assert!(html
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "Guide"));
    }

    #[test]
    fn invalid_text_is_diagnostic_not_panic() {
        let output = extract("broken.yaml", &[0xff, 0xfe]);
        assert_eq!(output.extraction.nodes.len(), 1);
        assert!(output
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "invalid_utf8"));
    }

    #[test]
    fn limits_stop_fact_growth_deterministically() {
        let defaults = StructuredLimits::default();
        assert_eq!(
            defaults.max_input_bytes,
            crate::format_registry::STRUCTURED_TEXT_LIMITS.max_input_bytes as usize
        );
        assert_eq!(
            defaults.max_facts,
            crate::format_registry::STRUCTURED_TEXT_LIMITS.max_records
        );
        assert_eq!(
            defaults.max_depth,
            crate::format_registry::STRUCTURED_TEXT_LIMITS.max_nesting
        );

        let limits = StructuredLimits {
            max_facts: 3,
            ..defaults
        };
        let output = extract_structured_bytes_with_limits(
            Path::new("large.json"),
            "large.json",
            br#"{"one":1,"two":2,"three":3}"#,
            limits,
        )
        .expect("registered structured extension");
        assert!(output
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "fact_limit"));
        assert!(output.extraction.nodes.len() + output.extraction.edges.len() <= 3);

        let output = extract_structured_bytes_with_limits(
            Path::new("events.jsonl"),
            "events.jsonl",
            b"{\"event\":1}\n{\"event\":2}\n",
            StructuredLimits {
                max_rows: 1,
                ..defaults
            },
        )
        .expect("registered JSON Lines extension");
        assert!(output
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "row_limit"));
        assert_eq!(
            output
                .extraction
                .nodes
                .iter()
                .filter(|node| node.extra["type"] == "json_record")
                .count(),
            1
        );
    }

    #[test]
    fn parser_fact_budget_truncates_before_retained_facts() {
        let input = br#"{"service":{"replicas":3}}"#;
        let allowance = input.len() * 16 + 16 * 1024 + 2 * 1024;
        let plan = crate::parser_budget::ParserPlan::for_source(allowance, input.len())
            .expect("one-fact parser plan");
        assert_eq!(plan.max_facts(), 1);

        let (output, exhausted) = crate::parser_budget::with_plan(plan, || {
            extract_structured_bytes(Path::new("service.json"), "service.json", input)
                .expect("registered structured extension")
        });
        assert!(exhausted);
        assert_eq!(output.extraction.nodes.len(), 1);
        assert!(output.extraction.edges.is_empty());
        assert_eq!(output.extraction.nodes[0].extra["parse_status"], "partial");
        assert_eq!(
            output.extraction.nodes[0].extra["parser_diagnostic"],
            "parser_arena_fact_limit"
        );
    }

    #[test]
    fn delimited_rows_stream_with_bounded_fields() {
        let input = vec![b','; 1_000_000];
        let limits = StructuredLimits {
            max_facts: 64,
            max_depth: 4,
            ..StructuredLimits::default()
        };
        let run = || {
            extract_structured_bytes_with_limits(Path::new("wide.csv"), "wide.csv", &input, limits)
                .expect("registered delimited extension")
        };

        let first = run();
        let second = run();
        assert_eq!(
            first
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "field_limit")
                .count(),
            1
        );
        assert_eq!(
            first
                .extraction
                .nodes
                .iter()
                .filter(|node| node.extra["type"] == "table_column")
                .count(),
            limits.max_depth
        );
        assert!(first.extraction.nodes.len() + first.extraction.edges.len() <= 11);
        assert_eq!(
            serde_json::to_string(&first.extraction).expect("serialize first extraction"),
            serde_json::to_string(&second.extraction).expect("serialize second extraction")
        );
        assert_eq!(first.diagnostics, second.diagnostics);

        let long_field = vec![b'x'; 1_000_000];
        let scalar_limits = StructuredLimits {
            max_scalar_bytes: 16,
            ..limits
        };
        let scalar = extract_structured_bytes_with_limits(
            Path::new("long.csv"),
            "long.csv",
            &long_field,
            scalar_limits,
        )
        .expect("registered delimited extension");
        assert_eq!(
            scalar
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "scalar_limit")
                .count(),
            1
        );
        assert!(scalar.extraction.nodes.iter().any(|node| {
            node.extra.get("structured_value_truncated") == Some(&Value::Bool(true))
        }));
        assert!(scalar
            .extraction
            .nodes
            .iter()
            .all(|node| node.label.len() <= scalar_limits.max_scalar_bytes));
    }

    #[test]
    fn json5_and_yaml_fallbacks_cap_depth_and_path_growth() {
        let limits = StructuredLimits {
            max_depth: 4,
            ..StructuredLimits::default()
        };
        let mut json5 = String::from("{\n");
        for depth in 0..64 {
            json5.push_str(&format!("{}level_{depth}: {{\n", "  ".repeat(depth + 1)));
        }
        for depth in (0..64).rev() {
            json5.push_str(&format!("{}}},\n", "  ".repeat(depth + 1)));
        }
        json5.push_str("  tail: 1\n}\n");
        let json5 = extract_structured_bytes_with_limits(
            Path::new("deep.json5"),
            "deep.json5",
            json5.as_bytes(),
            limits,
        )
        .expect("registered JSON5 extension");
        assert_eq!(
            json5
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "depth_limit")
                .count(),
            1
        );
        assert!(json5
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "tail"));
        assert!(!json5
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "level_4"));

        let mut yaml = String::new();
        for depth in 0..64 {
            yaml.push_str(&format!("{}level_{depth}:\n", "  ".repeat(depth)));
        }
        yaml.push_str("tail: ok\n");
        let yaml = extract_structured_bytes_with_limits(
            Path::new("deep.yaml"),
            "deep.yaml",
            yaml.as_bytes(),
            limits,
        )
        .expect("registered YAML extension");
        assert_eq!(
            yaml.diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "depth_limit")
                .count(),
            1
        );
        assert!(yaml
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "tail"));
        assert!(!yaml
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "level_4"));

        let path_limits = StructuredLimits {
            max_scalar_bytes: 16,
            ..StructuredLimits::default()
        };
        for (name, input) in [
            (
                "wide.json5",
                format!("{{\n{}: {{\n  nested: 1\n}}\n}}", "x".repeat(64)),
            ),
            (
                "wide.yaml",
                format!("{}:\n  nested: 1\ntail: ok\n", "x".repeat(64)),
            ),
        ] {
            let output = extract_structured_bytes_with_limits(
                Path::new(name),
                name,
                input.as_bytes(),
                path_limits,
            )
            .expect("registered structural fallback extension");
            assert_eq!(
                output
                    .diagnostics
                    .iter()
                    .filter(|diagnostic| diagnostic.code == "path_limit")
                    .count(),
                1,
                "{name}"
            );
            assert!(output.extraction.nodes.iter().all(|node| {
                node.extra
                    .get("structured_path")
                    .and_then(Value::as_str)
                    .is_none_or(|path| path.len() <= path_limits.max_scalar_bytes)
            }));
        }
    }

    #[test]
    fn xml_depth_limit_skips_deep_subtrees_and_recovers_siblings() {
        let limits = StructuredLimits {
            max_depth: 4,
            ..StructuredLimits::default()
        };
        let mut xml = String::from("<root>");
        for depth in 0..64 {
            xml.push_str(&format!("<level_{depth}>"));
        }
        for depth in (0..64).rev() {
            xml.push_str(&format!("</level_{depth}>"));
        }
        xml.push_str("<tail /></root>");
        let output = extract_structured_bytes_with_limits(
            Path::new("deep.xml"),
            "deep.xml",
            xml.as_bytes(),
            limits,
        )
        .expect("registered XML extension");

        assert_eq!(
            output
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "depth_limit")
                .count(),
            1
        );
        assert!(output
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "tail"));
        assert!(!output
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "level_3"));
        assert_eq!(
            output
                .extraction
                .nodes
                .iter()
                .filter(|node| node.extra["type"] == "xml_element")
                .count(),
            limits.max_depth + 1
        );
    }

    #[test]
    fn xml_paths_and_names_are_bounded_before_subtrees_are_retained() {
        let limits = StructuredLimits {
            max_scalar_bytes: 24,
            ..StructuredLimits::default()
        };
        let long_name = "x".repeat(128);
        let xml = format!(
            "<root><abcdefghij><klmnopqrst><inside /></klmnopqrst></abcdefghij><{long_name}><hidden /></{long_name}><tail /></root>"
        );
        let output = extract_structured_bytes_with_limits(
            Path::new("paths.xml"),
            "paths.xml",
            xml.as_bytes(),
            limits,
        )
        .expect("registered XML extension");

        assert_eq!(
            output
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "path_limit")
                .count(),
            1
        );
        assert!(output
            .extraction
            .nodes
            .iter()
            .any(|node| node.label == "tail"));
        assert!(!output.extraction.nodes.iter().any(|node| {
            matches!(node.label.as_str(), "klmnopqrst" | "inside" | "hidden")
                || node.label == long_name
        }));
        assert!(output.extraction.nodes.iter().all(|node| {
            node.extra
                .get("structured_path")
                .and_then(Value::as_str)
                .is_none_or(|path| path.len() <= limits.max_scalar_bytes)
        }));
    }

    #[test]
    fn strict_value_walk_caps_object_and_array_paths() {
        let limits = StructuredLimits {
            max_scalar_bytes: 16,
            ..StructuredLimits::default()
        };
        for (name, input, omitted) in [
            (
                "strict.json",
                br#"{"abcdefgh":{"ijklmnop":{"qrstuv":1}},"arrays":[[[[[1]]]]],"tail":2}"#
                    .as_slice(),
                "ijklmnop",
            ),
            (
                "strict.toml",
                b"[abcdefgh.ijklmnop.qrstuv]\nvalue = 1\n[tail]\nvalue = 2\n".as_slice(),
                "ijklmnop",
            ),
        ] {
            let output = extract_structured_bytes_with_limits(Path::new(name), name, input, limits)
                .expect("registered strict structured extension");
            assert_eq!(
                output
                    .diagnostics
                    .iter()
                    .filter(|diagnostic| diagnostic.code == "path_limit")
                    .count(),
                1,
                "{name}"
            );
            assert!(output
                .extraction
                .nodes
                .iter()
                .any(|node| node.label == "tail"));
            assert!(!output
                .extraction
                .nodes
                .iter()
                .any(|node| node.label == omitted));
            assert!(!values(&output).contains(&Value::from(1)), "{name}");
            assert!(values(&output).contains(&Value::from(2)), "{name}");
            assert!(output.extraction.nodes.iter().all(|node| {
                node.extra
                    .get("structured_path")
                    .and_then(Value::as_str)
                    .is_none_or(|path| path.len() <= limits.max_scalar_bytes)
            }));
        }
    }
}
