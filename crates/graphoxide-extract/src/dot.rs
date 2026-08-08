//! Bounded, grammar-aware Graphviz DOT semantic extraction.
//!
//! The parser consumes only the caller-owned byte slice. It never resolves DOT
//! references, invokes Graphviz, opens files, starts processes, or performs
//! network I/O. UTF-8 (with an optional BOM) is preferred. Invalid UTF-8 is
//! accepted as ISO-8859-1 only when the parsed root graph explicitly declares
//! one of Graphviz's Latin-1 charset aliases.

use anyhow::{bail, ensure};
use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    path::Path,
};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_NESTING: usize = 64;
const MAX_NODES: usize = 100_000;
const MAX_FACTS: usize = 350_000;
const MAX_EDGE_OCCURRENCES: usize = 250_000;
const MAX_TOKENS: usize = 1_000_000;
const MAX_TOKEN_BYTES: usize = 4 * 1024;
const MAX_ATTRS: usize = 256;
const MAX_DIAGNOSTICS: usize = 64;
const MAX_RETAINED_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const MAX_RETAINED_METADATA_BYTES: usize = 64 * 1024 * 1024;
const MAX_CHAIN_GROUPS: usize = 4_096;
const MAX_CHAIN_ENDPOINTS: usize = 100_000;
const MAX_ENDPOINT_MATERIALIZATIONS: usize = 250_000;
const MAX_SOURCE_FILE_BYTES: usize = 16 * 1024;
const EXACT_ID_MARKER: &str = "graphoxide_exact";
/// The graph staging layer rejects any serialized fact at 1 MiB. Keeping DOT
/// facts below half that ceiling leaves room for future common metadata.
const MAX_EMITTED_FACT_BYTES: usize = 512 * 1024;
/// Leave ample headroom beneath the graph runtime's 1 MiB serialized-fact
/// admission limit for serde field names and batch framing.
const MAX_EDGE_JSON_ESTIMATE: usize = MAX_EMITTED_FACT_BYTES;

/// Parse one admitted DOT allocation without consulting the filesystem.
pub(crate) fn extract_dot_bytes(source_file: &str, source: &[u8]) -> anyhow::Result<Extraction> {
    ensure!(
        source_file.len() <= MAX_SOURCE_FILE_BYTES,
        "DOT source path exceeds {MAX_SOURCE_FILE_BYTES} byte limit"
    );
    ensure!(
        source.len() <= MAX_BYTES,
        "DOT source exceeds {MAX_BYTES} byte limit"
    );
    let utf8 =
        std::str::from_utf8(source.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(source)).is_ok();
    let probe_encoding = if utf8 {
        Encoding::Utf8
    } else {
        Encoding::Latin1
    };
    let declared_charset = preflight_root_charset(source, probe_encoding);
    let declared_latin1 = declared_charset.as_deref().is_some_and(is_latin1_alias);
    if !utf8 && !declared_latin1 {
        bail!("DOT source is not UTF-8 and does not declare charset=latin1")
    }
    // Preserve valid UTF-8 bytes in semantic IDs and inert attributes. A
    // Latin-1 declaration affects display-label interpretation only. Raw-byte
    // Latin-1 decoding is the fallback for otherwise-invalid UTF-8.
    let encoding = if utf8 {
        Encoding::Utf8
    } else {
        Encoding::Latin1
    };
    let mut parser = Parser::new(source_file, source, encoding);
    parser.parse_graph();
    if encoding == Encoding::Latin1 && !parser.state.declares_latin1() {
        bail!("DOT source is not UTF-8 and does not declare charset=latin1")
    }
    Ok(parser.finish())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Utf8,
    Latin1,
}

fn is_latin1_alias(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "latin1" | "latin-1" | "iso-8859-1" | "iso_8859-1" | "iso8859-1" | "iso-ir-100" | "l1"
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

impl Span {
    fn join(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdStyle {
    Plain,
    Quoted,
    Html,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Id(String, IdStyle),
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Equal,
    Semi,
    Comma,
    Colon,
    Plus,
    Arrow,
    DashDash,
    Invalid(&'static str, &'static str, bool),
    Eof,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    span: Span,
    truncated: bool,
}

struct Lexer<'a> {
    bytes: &'a [u8],
    encoding: Encoding,
    pos: usize,
    tokens: usize,
    token_limit_reported: bool,
}

impl<'a> Lexer<'a> {
    fn new(bytes: &'a [u8], encoding: Encoding) -> Self {
        let pos = usize::from(bytes.starts_with(&[0xef, 0xbb, 0xbf])) * 3;
        Self {
            bytes,
            encoding,
            pos,
            tokens: 0,
            token_limit_reported: false,
        }
    }

    fn next(&mut self) -> Token {
        if self.tokens >= MAX_TOKENS {
            let span = Span {
                start: self.pos,
                end: self.pos,
            };
            self.pos = self.bytes.len();
            if !self.token_limit_reported {
                self.token_limit_reported = true;
                return Token {
                    kind: TokenKind::Invalid("dot_token_limit", "DOT token limit reached", true),
                    span,
                    truncated: false,
                };
            }
        }
        if let Some(token) = self.skip_space_and_comments() {
            return token;
        }
        let start = self.pos;
        let Some(&byte) = self.bytes.get(self.pos) else {
            return Token {
                kind: TokenKind::Eof,
                span: Span { start, end: start },
                truncated: false,
            };
        };
        self.tokens += 1;
        let single = |kind, end| Token {
            kind,
            span: Span { start, end },
            truncated: false,
        };
        match byte {
            b'{' => {
                self.pos += 1;
                single(TokenKind::LBrace, self.pos)
            }
            b'}' => {
                self.pos += 1;
                single(TokenKind::RBrace, self.pos)
            }
            b'[' => {
                self.pos += 1;
                single(TokenKind::LBracket, self.pos)
            }
            b']' => {
                self.pos += 1;
                single(TokenKind::RBracket, self.pos)
            }
            b'=' => {
                self.pos += 1;
                single(TokenKind::Equal, self.pos)
            }
            b';' => {
                self.pos += 1;
                single(TokenKind::Semi, self.pos)
            }
            b',' => {
                self.pos += 1;
                single(TokenKind::Comma, self.pos)
            }
            b':' => {
                self.pos += 1;
                single(TokenKind::Colon, self.pos)
            }
            b'+' => {
                self.pos += 1;
                single(TokenKind::Plus, self.pos)
            }
            b'-' if self.bytes.get(self.pos + 1) == Some(&b'>') => {
                self.pos += 2;
                single(TokenKind::Arrow, self.pos)
            }
            b'-' if self.bytes.get(self.pos + 1) == Some(&b'-') => {
                self.pos += 2;
                single(TokenKind::DashDash, self.pos)
            }
            b'"' => self.quoted(),
            b'<' => self.html(),
            b'-' | b'.' | b'0'..=b'9' if self.is_number_start() => self.number(),
            _ if is_id_start(byte) => self.plain_id(),
            _ => {
                self.pos += 1;
                Token {
                    kind: TokenKind::Invalid(
                        "dot_invalid_character",
                        "invalid character in DOT source",
                        false,
                    ),
                    span: Span {
                        start,
                        end: self.pos,
                    },
                    truncated: false,
                }
            }
        }
    }

    fn skip_space_and_comments(&mut self) -> Option<Token> {
        loop {
            while let Some(&byte) = self.bytes.get(self.pos) {
                if !byte.is_ascii_whitespace() {
                    break;
                }
                self.pos += 1;
            }
            if self.bytes.get(self.pos..self.pos + 2) == Some(b"//") {
                self.pos += 2;
                while let Some(&byte) = self.bytes.get(self.pos) {
                    self.pos += 1;
                    if byte == b'\n' || byte == b'\r' {
                        break;
                    }
                }
                continue;
            }
            // CGraph's scanner discards hash comments wherever they begin,
            // while only its optional line-number interpretation is anchored.
            if self.bytes.get(self.pos) == Some(&b'#') {
                self.pos += 1;
                while let Some(&byte) = self.bytes.get(self.pos) {
                    self.pos += 1;
                    if byte == b'\n' || byte == b'\r' {
                        break;
                    }
                }
                continue;
            }
            if self.bytes.get(self.pos..self.pos + 2) == Some(b"/*") {
                let start = self.pos;
                self.pos += 2;
                if let Some(relative) = self.bytes[self.pos..]
                    .windows(2)
                    .position(|window| window == b"*/")
                {
                    let end = self.pos + relative + 2;
                    self.pos = end;
                    continue;
                }
                self.pos = self.bytes.len();
                return Some(Token {
                    kind: TokenKind::Invalid(
                        "dot_unterminated_comment",
                        "unterminated block comment",
                        false,
                    ),
                    span: Span {
                        start,
                        end: self.pos,
                    },
                    truncated: false,
                });
            }
            return None;
        }
    }

    fn is_number_start(&self) -> bool {
        match self.bytes.get(self.pos).copied() {
            Some(b'-') => match self.bytes.get(self.pos + 1).copied() {
                Some(b'0'..=b'9') => true,
                Some(b'.') => self.bytes.get(self.pos + 2).is_some_and(u8::is_ascii_digit),
                _ => false,
            },
            Some(b'.') => self.bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit),
            Some(b'0'..=b'9') => true,
            _ => false,
        }
    }

    fn number(&mut self) -> Token {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        if self.bytes.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                self.pos += 1;
            }
        }
        self.id_token(start, self.pos, IdStyle::Plain)
    }

    fn plain_id(&mut self) -> Token {
        let start = self.pos;
        self.pos += 1;
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|byte| is_id_continue(*byte))
        {
            self.pos += 1;
        }
        self.id_token(start, self.pos, IdStyle::Plain)
    }

    fn quoted(&mut self) -> Token {
        let start = self.pos;
        self.pos += 1;
        let content_start = self.pos;
        let mut escaped = false;
        while let Some(&byte) = self.bytes.get(self.pos) {
            if escaped {
                escaped = false;
                self.pos += 1;
                continue;
            }
            if byte == b'\\' {
                escaped = true;
                self.pos += 1;
                continue;
            }
            if byte == b'"' {
                let content_end = self.pos;
                self.pos += 1;
                let (value, truncated) = self.decode_quoted(content_start, content_end);
                return Token {
                    kind: TokenKind::Id(value, IdStyle::Quoted),
                    span: Span {
                        start,
                        end: self.pos,
                    },
                    truncated,
                };
            }
            self.pos += 1;
        }
        Token {
            kind: TokenKind::Invalid("dot_unterminated_string", "unterminated quoted ID", false),
            span: Span {
                start,
                end: self.pos,
            },
            truncated: false,
        }
    }

    fn html(&mut self) -> Token {
        let start = self.pos;
        let mut depth = 0usize;
        let mut quote = None;
        let mut in_tag = false;
        while let Some(&byte) = self.bytes.get(self.pos) {
            if let Some(delimiter) = quote {
                self.pos += 1;
                if byte == delimiter {
                    quote = None;
                }
                continue;
            }
            if in_tag && (byte == b'\'' || byte == b'"') {
                quote = Some(byte);
                self.pos += 1;
                continue;
            }
            if byte == b'<' {
                if self.bytes.get(self.pos..self.pos + 4) == Some(b"<!--") {
                    depth += 1;
                    self.pos += 4;
                    if let Some(relative) = self.bytes[self.pos..]
                        .windows(3)
                        .position(|window| window == b"-->")
                    {
                        self.pos += relative + 3;
                        depth -= 1;
                        if depth == 0 {
                            return self.id_token(start, self.pos, IdStyle::Html);
                        }
                        in_tag = false;
                        continue;
                    }
                    self.pos = self.bytes.len();
                    break;
                }
                depth += 1;
                // The first pair is DOT's HTML-token wrapper. Quotes in its
                // text are ordinary; quote mode starts only in nested tags.
                in_tag = depth > 1;
            } else if byte == b'>' {
                depth = depth.saturating_sub(1);
                self.pos += 1;
                in_tag = false;
                if depth == 0 {
                    return self.id_token(start, self.pos, IdStyle::Html);
                }
                continue;
            }
            self.pos += 1;
        }
        Token {
            kind: TokenKind::Invalid(
                "dot_unterminated_html_id",
                "unterminated HTML-like ID",
                false,
            ),
            span: Span {
                start,
                end: self.pos,
            },
            truncated: false,
        }
    }

    fn id_token(&self, start: usize, end: usize, style: IdStyle) -> Token {
        let (value, truncated) = self.decode_bounded(start, end);
        Token {
            kind: TokenKind::Id(value, style),
            span: Span { start, end },
            truncated,
        }
    }

    fn decode_bounded(&self, start: usize, end: usize) -> (String, bool) {
        let raw = &self.bytes[start..end];
        match self.encoding {
            Encoding::Utf8 => {
                truncate_utf8(std::str::from_utf8(raw).unwrap_or(""), MAX_TOKEN_BYTES)
            }
            Encoding::Latin1 => {
                let truncated = raw.len() > MAX_TOKEN_BYTES;
                let mut value = String::with_capacity(raw.len().min(MAX_TOKEN_BYTES));
                for byte in raw.iter().take(MAX_TOKEN_BYTES) {
                    value.push(char::from(*byte));
                }
                (value, truncated)
            }
        }
    }

    fn decode_quoted(&self, start: usize, end: usize) -> (String, bool) {
        let mut value = String::new();
        let mut cursor = start;
        let mut truncated = false;
        while cursor < end {
            if value.len() >= MAX_TOKEN_BYTES {
                truncated = true;
                break;
            }
            if self.bytes[cursor] == b'\\' && cursor + 1 < end {
                let next = self.bytes[cursor + 1];
                if next == b'\n' {
                    cursor += 2;
                    continue;
                }
                if next == b'\r' {
                    cursor += 2;
                    if self.bytes.get(cursor) == Some(&b'\n') {
                        cursor += 1;
                    }
                    continue;
                }
                if next == b'"' {
                    push_bounded(&mut value, "\"", &mut truncated);
                    cursor += 2;
                    continue;
                }
                push_bounded(&mut value, "\\", &mut truncated);
                cursor += 1;
                continue;
            }
            let width = if self.encoding == Encoding::Utf8 {
                utf8_width(self.bytes[cursor])
            } else {
                1
            }
            .min(end - cursor);
            let (piece, piece_truncated) = self.decode_bounded(cursor, cursor + width);
            push_bounded(&mut value, &piece, &mut truncated);
            truncated |= piece_truncated;
            cursor += width;
        }
        (value, truncated)
    }
}

fn is_id_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic() || byte >= 0x80
}

fn is_id_continue(byte: u8) -> bool {
    is_id_start(byte) || byte.is_ascii_digit()
}

fn utf8_width(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn truncate_utf8(value: &str, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value.to_owned(), false);
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

fn push_bounded(target: &mut String, piece: &str, truncated: &mut bool) {
    if target.len() >= MAX_TOKEN_BYTES {
        *truncated = true;
        return;
    }
    let available = MAX_TOKEN_BYTES - target.len();
    if piece.len() <= available {
        target.push_str(piece);
        return;
    }
    let mut end = available;
    while !piece.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&piece[..end]);
    *truncated = true;
}

/// Select the byte decoder before semantic fact admission. This deliberately
/// recognizes only effective root-scope assignments and `graph [...]`
/// attributes; node/subgraph-local `charset` text cannot select Latin-1.
fn preflight_root_charset(source: &[u8], encoding: Encoding) -> Option<String> {
    let mut cursor = PreflightCursor::new(source, encoding);
    while !matches!(cursor.current.kind, TokenKind::LBrace | TokenKind::Eof) {
        cursor.bump();
    }
    if !matches!(cursor.current.kind, TokenKind::LBrace) {
        return None;
    }
    cursor.bump();
    let mut depth = 1usize;
    let mut charset = None;
    while depth > 0 && !matches!(cursor.current.kind, TokenKind::Eof) {
        match cursor.current.kind {
            TokenKind::LBrace => {
                depth += 1;
                cursor.bump();
            }
            TokenKind::RBrace => {
                depth -= 1;
                cursor.bump();
            }
            _ if depth != 1 => cursor.bump(),
            TokenKind::Id(ref value, IdStyle::Plain) if value.eq_ignore_ascii_case("graph") => {
                cursor.bump();
                while matches!(cursor.current.kind, TokenKind::LBracket) {
                    preflight_attr_list(&mut cursor, &mut charset);
                }
            }
            TokenKind::LBracket => skip_preflight_attr_list(&mut cursor),
            TokenKind::Id(..) => {
                let Some(key) = cursor.take_id() else {
                    cursor.bump();
                    continue;
                };
                if key == "charset" && matches!(cursor.current.kind, TokenKind::Equal) {
                    cursor.bump();
                    if let Some(value) = cursor.take_id() {
                        charset = Some(value);
                    }
                }
            }
            _ => cursor.bump(),
        }
    }
    charset
}

fn skip_preflight_attr_list(cursor: &mut PreflightCursor<'_>) {
    let mut depth = 0usize;
    while !matches!(cursor.current.kind, TokenKind::Eof) {
        match cursor.current.kind {
            TokenKind::LBracket => depth += 1,
            TokenKind::RBracket => {
                depth = depth.saturating_sub(1);
                cursor.bump();
                if depth == 0 {
                    break;
                }
                continue;
            }
            _ => {}
        }
        cursor.bump();
    }
}

fn preflight_attr_list(cursor: &mut PreflightCursor<'_>, charset: &mut Option<String>) {
    cursor.bump();
    while !matches!(cursor.current.kind, TokenKind::RBracket | TokenKind::Eof) {
        if matches!(cursor.current.kind, TokenKind::Comma | TokenKind::Semi) {
            cursor.bump();
            continue;
        }
        let Some(key) = cursor.take_id() else {
            cursor.bump();
            continue;
        };
        if !matches!(cursor.current.kind, TokenKind::Equal) {
            continue;
        }
        cursor.bump();
        if let Some(value) = cursor.take_id()
            && key == "charset"
        {
            *charset = Some(value);
        }
    }
    if matches!(cursor.current.kind, TokenKind::RBracket) {
        cursor.bump();
    }
}

struct PreflightCursor<'a> {
    lexer: Lexer<'a>,
    current: Token,
}

impl<'a> PreflightCursor<'a> {
    fn new(source: &'a [u8], encoding: Encoding) -> Self {
        let mut lexer = Lexer::new(source, encoding);
        let current = lexer.next();
        Self { lexer, current }
    }

    fn bump(&mut self) {
        self.current = self.lexer.next();
    }

    fn take_id(&mut self) -> Option<String> {
        let TokenKind::Id(mut value, style) = self.current.kind.clone() else {
            return None;
        };
        self.bump();
        while style == IdStyle::Quoted && matches!(self.current.kind, TokenKind::Plus) {
            self.bump();
            let TokenKind::Id(piece, IdStyle::Quoted) = self.current.kind.clone() else {
                break;
            };
            let mut truncated = false;
            push_bounded(&mut value, &piece, &mut truncated);
            self.bump();
        }
        Some(value)
    }
}

// Parser and semantic state are implemented below. Keeping the lexer isolated
// makes its byte-only and bounded contract independently testable.

type Attrs = BTreeMap<String, String>;

#[derive(Debug, Clone)]
struct ParsedId {
    value: String,
    style: IdStyle,
    span: Span,
}

#[derive(Debug, Clone, Default)]
struct Port {
    port: Option<String>,
    compass: Option<String>,
}

#[derive(Debug, Clone)]
struct Endpoint {
    node: usize,
    port: Port,
    span: Span,
}

#[derive(Debug, Clone)]
struct EndpointGroup {
    endpoints: Vec<Endpoint>,
    subgraph: Option<usize>,
    span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphKind {
    Graph,
    Digraph,
}

impl GraphKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Graph => "graph",
            Self::Digraph => "digraph",
        }
    }

    const fn expected_edge(self) -> TokenKind {
        match self {
            Self::Graph => TokenKind::DashDash,
            Self::Digraph => TokenKind::Arrow,
        }
    }
}

#[derive(Debug, Clone)]
struct NodeRecord {
    id: String,
    dot_id: String,
    identity: String,
    legacy_eligible: bool,
    label: String,
    kind: &'static str,
    span: Span,
    attrs: Attrs,
    subgraphs: BTreeSet<String>,
    node_defaults: Attrs,
    edge_defaults: Attrs,
}

#[derive(Debug, Clone)]
struct SubgraphRecord {
    node: usize,
    members: BTreeSet<usize>,
}

#[derive(Debug, Clone)]
struct Containment {
    source: String,
    target: String,
    span: Span,
}

#[derive(Debug, Clone)]
struct EdgeOccurrence {
    attrs: Attrs,
    span: Span,
    source_dot_id: String,
    target_dot_id: String,
    operator: &'static str,
    source_port: Port,
    target_port: Port,
    statement: usize,
    key: Option<String>,
}

#[derive(Debug, Clone)]
struct EdgeAggregate {
    source: usize,
    target: usize,
    relation: &'static str,
    span: Span,
    attrs: Attrs,
    occurrences: Vec<EdgeOccurrence>,
    statements: BTreeSet<usize>,
    parallel_edges: Vec<ParallelEdge>,
    keyed_parallel: BTreeMap<String, usize>,
    json_bytes_estimate: usize,
}

#[derive(Debug, Clone)]
struct ParallelEdge {
    key: Option<String>,
    attrs: Attrs,
    span: Span,
    occurrence_count: usize,
}

struct EdgeInput<'a> {
    source: &'a Endpoint,
    target: &'a Endpoint,
    relation: &'static str,
    effective_attrs: &'a Attrs,
    explicit_attrs: &'a Attrs,
    dot_key: Option<String>,
    span: Span,
    statement: usize,
}

#[derive(Debug, Clone)]
struct ScopeFrame {
    subgraph: Option<usize>,
    graph_attrs: Attrs,
    node_defaults: Attrs,
    edge_defaults: Attrs,
}

#[derive(Debug, Clone, Copy)]
enum ScopeAttrs {
    Graph,
    Node,
    Edge,
}

#[derive(Debug, Clone)]
struct Diagnostic {
    code: &'static str,
    message: String,
    span: Span,
}

#[derive(Debug)]
struct LineMap {
    starts: Vec<u32>,
}

impl LineMap {
    fn new(source: &[u8]) -> Self {
        let mut newline_count = 0usize;
        let mut cursor = 0usize;
        while cursor < source.len() {
            if source[cursor] == b'\r' {
                newline_count += 1;
                cursor += usize::from(source.get(cursor + 1) == Some(&b'\n')) + 1;
            } else {
                newline_count += usize::from(source[cursor] == b'\n');
                cursor += 1;
            }
        }
        let mut starts = Vec::with_capacity(newline_count + 1);
        starts.push(0);
        cursor = 0;
        while cursor < source.len() {
            if source[cursor] == b'\r' {
                cursor += usize::from(source.get(cursor + 1) == Some(&b'\n')) + 1;
                starts.push(cursor as u32);
            } else {
                cursor += 1;
                if source[cursor - 1] == b'\n' {
                    starts.push(cursor as u32);
                }
            }
        }
        Self { starts }
    }

    fn point(&self, byte: usize) -> (usize, usize) {
        let byte = byte as u32;
        let line_index = self.starts.partition_point(|start| *start <= byte) - 1;
        (
            line_index + 1,
            byte.saturating_sub(self.starts[line_index]) as usize + 1,
        )
    }

    fn retained_bytes(&self) -> usize {
        self.starts.len().saturating_mul(std::mem::size_of::<u32>())
    }

    fn range(&self, span: Span) -> Value {
        let (start_line, start_column) = self.point(span.start);
        let (end_line, end_column) = self.point(span.end);
        json!({
            "start": {"byte": span.start, "line": start_line, "column": start_column},
            "end": {"byte": span.end, "line": end_line, "column": end_column},
            "display": format!("L{start_line}:C{start_column}-L{end_line}:C{end_column}"),
        })
    }

    fn location(&self, span: Span) -> String {
        let (start_line, _) = self.point(span.start);
        format!("L{start_line}")
    }
}

struct DotState<'a> {
    source_file: &'a str,
    stem: String,
    root_id: String,
    root_admitted: bool,
    lines: LineMap,
    graph_kind: Option<GraphKind>,
    graph_id: Option<String>,
    strict: bool,
    root_span: Span,
    root_attrs: Attrs,
    root_node_defaults: Attrs,
    root_edge_defaults: Attrs,
    nodes: Vec<NodeRecord>,
    node_by_dot_id: BTreeMap<String, usize>,
    subgraphs: Vec<SubgraphRecord>,
    subgraph_by_key: BTreeMap<String, usize>,
    containments: Vec<Containment>,
    containment_set: BTreeSet<(String, String)>,
    edges: Vec<EdgeAggregate>,
    edge_by_key: BTreeMap<(String, String, String), usize>,
    diagnostics: Vec<Diagnostic>,
    diagnostic_bytes: usize,
    metadata_bytes: usize,
    diagnostics_omitted: usize,
    syntax_recovered: bool,
    resource_truncated: bool,
    retention_exhausted: bool,
    display_latin1: bool,
    facts: usize,
    edge_occurrences: usize,
    statement_sequence: usize,
}

impl<'a> DotState<'a> {
    fn new(source_file: &'a str, source: &[u8]) -> Self {
        let stem = Path::new(source_file)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let root_id = make_id(&[&stem, "diagram"]);
        let lines = LineMap::new(source);
        let initial_metadata = lines
            .retained_bytes()
            .saturating_add(stem.len())
            .saturating_add(root_id.len().saturating_mul(2))
            .saturating_add(source_file.len())
            .saturating_add(512);
        let root_admitted = crate::parser_budget::try_reserve_facts(1);
        Self {
            source_file,
            stem,
            root_id,
            root_admitted,
            lines,
            graph_kind: None,
            graph_id: None,
            strict: false,
            root_span: Span {
                start: 0,
                end: source.len(),
            },
            root_attrs: Attrs::new(),
            root_node_defaults: Attrs::new(),
            root_edge_defaults: Attrs::new(),
            nodes: Vec::new(),
            node_by_dot_id: BTreeMap::new(),
            subgraphs: Vec::new(),
            subgraph_by_key: BTreeMap::new(),
            containments: Vec::new(),
            containment_set: BTreeSet::new(),
            edges: Vec::new(),
            edge_by_key: BTreeMap::new(),
            diagnostics: Vec::new(),
            diagnostic_bytes: 0,
            metadata_bytes: initial_metadata,
            diagnostics_omitted: 0,
            syntax_recovered: false,
            resource_truncated: !root_admitted,
            retention_exhausted: !root_admitted,
            display_latin1: false,
            facts: usize::from(root_admitted),
            edge_occurrences: 0,
            statement_sequence: 0,
        }
    }

    fn declares_latin1(&self) -> bool {
        self.root_attrs
            .get("charset")
            .is_some_and(|value| is_latin1_alias(value))
    }

    fn status(&self) -> &'static str {
        if self.resource_truncated {
            "partial"
        } else if self.syntax_recovered {
            "recovered"
        } else {
            "complete"
        }
    }

    fn reserve_fact(&mut self) -> bool {
        if self.facts >= MAX_FACTS || !crate::parser_budget::try_reserve_facts(1) {
            self.retention_exhausted = true;
            self.resource(
                "dot_fact_limit",
                "DOT semantic fact limit reached",
                self.root_span,
            );
            return false;
        }
        self.facts += 1;
        true
    }

    fn charge_metadata(&mut self, bytes: usize, span: Span) -> bool {
        let admitted = self
            .metadata_bytes
            .checked_add(bytes)
            .is_some_and(|total| total <= MAX_RETAINED_METADATA_BYTES);
        if !admitted {
            self.resource_truncated = true;
            self.retention_exhausted = true;
            self.retain_diagnostic(
                "dot_metadata_limit",
                "DOT retained metadata limit reached".into(),
                span,
            );
            return false;
        }
        self.metadata_bytes += bytes;
        true
    }

    fn syntax(&mut self, code: &'static str, message: impl Into<String>, span: Span) {
        self.syntax_recovered = true;
        self.retain_diagnostic(code, message.into(), span);
    }

    fn resource(&mut self, code: &'static str, message: impl Into<String>, span: Span) {
        self.resource_truncated = true;
        self.retain_diagnostic(code, message.into(), span);
    }

    fn retain_diagnostic(&mut self, code: &'static str, mut message: String, span: Span) {
        if message.len() > 512 {
            let (bounded, _) = truncate_utf8(&message, 512);
            message = bounded;
        }
        let bytes = code.len().saturating_add(message.len()).saturating_add(64);
        if self.diagnostics.len() >= MAX_DIAGNOSTICS
            || self.diagnostic_bytes.saturating_add(bytes) > MAX_RETAINED_DIAGNOSTIC_BYTES
            || self.metadata_bytes.saturating_add(bytes.saturating_mul(2))
                > MAX_RETAINED_METADATA_BYTES
        {
            self.diagnostics_omitted = self.diagnostics_omitted.saturating_add(1);
            return;
        }
        self.diagnostic_bytes += bytes;
        self.metadata_bytes = self.metadata_bytes.saturating_add(bytes.saturating_mul(2));
        self.diagnostics.push(Diagnostic {
            code,
            message,
            span,
        });
    }

    fn collision_safe_id(&self, dot_id: &str, namespace: &str, identity: &str) -> String {
        // Preserve established IDs for canonical ordinary node names. Lossy
        // normalized IDs and internal subgraphs use a reserved, domain-
        // separated full hash. Excluding that marker from the legacy set means
        // a declared ID cannot copy a generated suffix.
        if namespace == "node"
            && !dot_id.is_empty()
            && make_id(&[dot_id]) == dot_id
            && !dot_id.contains(EXACT_ID_MARKER)
        {
            return make_id(&[&self.stem, "diagram", "graphviz", dot_id]);
        }
        let base = make_id(&[&self.stem, "diagram", "graphviz", namespace, dot_id]);
        let owner = format!("{namespace}\0{identity}");
        let digest = blake3::hash(owner.as_bytes()).to_hex();
        format!("{base}_{EXACT_ID_MARKER}_{digest}")
    }

    fn ensure_node(
        &mut self,
        dot_id: &str,
        span: Span,
        defaults: Attrs,
        subgraph_indices: &[usize],
    ) -> Option<usize> {
        if let Some(index) = self.node_by_dot_id.get(dot_id).copied() {
            self.add_memberships(index, span, subgraph_indices);
            return Some(index);
        }
        if self
            .nodes
            .len()
            .saturating_add(usize::from(self.root_admitted))
            >= MAX_NODES
        {
            self.retention_exhausted = true;
            self.resource("dot_node_limit", "DOT node limit reached", span);
            return None;
        }
        let id = self.collision_safe_id(dot_id, "node", dot_id);
        let identity = format!("node\0{dot_id}");
        let label = defaults
            .get("label")
            .cloned()
            .unwrap_or_else(|| dot_id.to_owned());
        let retained = dot_id
            .len()
            .saturating_mul(3)
            .saturating_add(id.len().saturating_mul(2))
            .saturating_add(identity.len())
            .saturating_add(label.len().saturating_mul(3))
            .saturating_add(self.source_file.len())
            .saturating_add(attrs_cost(&defaults))
            .saturating_add(512);
        if !self.charge_metadata(retained, span) || !self.reserve_fact() {
            return None;
        }
        let index = self.nodes.len();
        self.nodes.push(NodeRecord {
            id,
            dot_id: dot_id.to_owned(),
            identity,
            legacy_eligible: true,
            label,
            kind: "node",
            span,
            attrs: defaults,
            subgraphs: BTreeSet::new(),
            node_defaults: Attrs::new(),
            edge_defaults: Attrs::new(),
        });
        self.node_by_dot_id.insert(dot_id.to_owned(), index);
        self.add_memberships(index, span, subgraph_indices);
        Some(index)
    }

    fn add_memberships(&mut self, node: usize, span: Span, subgraph_indices: &[usize]) {
        let source = self.root_id.clone();
        let target = self.nodes[node].id.clone();
        self.contain(source, target, span);
        for &subgraph in subgraph_indices {
            let dot_id = self.nodes[self.subgraphs[subgraph].node].dot_id.clone();
            if !self.nodes[node].subgraphs.contains(&dot_id)
                && self.charge_metadata(dot_id.len().saturating_mul(2).saturating_add(256), span)
            {
                self.nodes[node].subgraphs.insert(dot_id);
                self.subgraphs[subgraph].members.insert(node);
            }
        }
        if let Some(&direct) = subgraph_indices.last() {
            let source = self.nodes[self.subgraphs[direct].node].id.clone();
            let target = self.nodes[node].id.clone();
            self.contain(source, target, span);
        }
    }

    fn ensure_subgraph(
        &mut self,
        key: String,
        dot_id: String,
        span: Span,
        parent_subgraph: Option<usize>,
    ) -> Option<usize> {
        if let Some(index) = self.subgraph_by_key.get(&key).copied() {
            let source = parent_subgraph
                .map(|parent| self.nodes[self.subgraphs[parent].node].id.clone())
                .unwrap_or_else(|| self.root_id.clone());
            let target = self.nodes[self.subgraphs[index].node].id.clone();
            self.contain(source, target, span);
            return Some(index);
        }
        let id = self.collision_safe_id(&dot_id, "subgraph", &key);
        let identity = format!("subgraph\0{key}");
        let legacy_eligible = key.starts_with("named:");
        let label = dot_id
            .strip_prefix("cluster_")
            .unwrap_or(&dot_id)
            .to_owned();
        if self
            .nodes
            .len()
            .saturating_add(usize::from(self.root_admitted))
            >= MAX_NODES
            || !self.charge_metadata(
                key.len()
                    .saturating_mul(2)
                    .saturating_add(dot_id.len().saturating_mul(3))
                    .saturating_add(id.len().saturating_mul(2))
                    .saturating_add(identity.len())
                    .saturating_add(label.len().saturating_mul(3))
                    .saturating_add(self.source_file.len())
                    .saturating_add(512),
                span,
            )
            || !self.reserve_fact()
        {
            self.retention_exhausted = true;
            self.resource("dot_node_limit", "DOT subgraph limit reached", span);
            return None;
        }
        let node = self.nodes.len();
        self.nodes.push(NodeRecord {
            id: id.clone(),
            dot_id,
            identity,
            legacy_eligible,
            label,
            kind: "subgraph",
            span,
            attrs: Attrs::new(),
            subgraphs: BTreeSet::new(),
            node_defaults: Attrs::new(),
            edge_defaults: Attrs::new(),
        });
        let index = self.subgraphs.len();
        self.subgraphs.push(SubgraphRecord {
            node,
            members: BTreeSet::new(),
        });
        self.subgraph_by_key.insert(key, index);
        let source = parent_subgraph
            .map(|parent| self.nodes[self.subgraphs[parent].node].id.clone())
            .unwrap_or_else(|| self.root_id.clone());
        self.contain(source, id, span);
        Some(index)
    }

    fn contain(&mut self, source: String, target: String, span: Span) {
        if self
            .containment_set
            .contains(&(source.clone(), target.clone()))
        {
            return;
        }
        if !self.charge_metadata(
            source
                .len()
                .saturating_mul(5)
                .saturating_add(target.len().saturating_mul(5))
                .saturating_add(self.source_file.len())
                .saturating_add(320),
            span,
        ) {
            return;
        }
        self.containment_set
            .insert((source.clone(), target.clone()));
        if self.reserve_fact() {
            self.containments.push(Containment {
                source,
                target,
                span,
            });
        }
    }

    fn add_edge(&mut self, input: EdgeInput<'_>) -> bool {
        let EdgeInput {
            source,
            target,
            relation,
            effective_attrs,
            explicit_attrs,
            dot_key,
            span,
            statement,
        } = input;
        if self.edge_occurrences >= MAX_EDGE_OCCURRENCES {
            self.retention_exhausted = true;
            self.resource("dot_edge_limit", "DOT edge occurrence limit reached", span);
            return false;
        }
        let source_id = self.nodes[source.node].id.clone();
        let target_id = self.nodes[target.node].id.clone();
        let (key_source, key_target) = if relation == "connected_to" && source_id > target_id {
            (target_id.clone(), source_id.clone())
        } else {
            (source_id.clone(), target_id.clone())
        };
        let aggregate_key = (key_source, key_target, relation.to_owned());
        let existing = self.edge_by_key.get(&aggregate_key).copied();
        let effective_json = attrs_json_worst(effective_attrs);
        let detail_add = if let Some(index) = existing {
            let edge = &self.edges[index];
            let reused_identity = if self.strict {
                Some(0)
            } else {
                dot_key
                    .as_ref()
                    .and_then(|key| edge.keyed_parallel.get(key).copied())
            };
            if let Some(identity) = reused_identity {
                let prospective_attrs = attrs_json_worst(&edge.parallel_edges[identity].attrs)
                    .saturating_add(attrs_json_worst(explicit_attrs));
                occurrence_json_worst(
                    prospective_attrs,
                    &self.nodes[source.node].dot_id,
                    &self.nodes[target.node].dot_id,
                    &source.port,
                    &target.port,
                    dot_key.as_deref(),
                )
                .saturating_add(parallel_json_worst(prospective_attrs, dot_key.as_deref()))
                .saturating_add(if identity == 0 { prospective_attrs } else { 0 })
            } else {
                occurrence_json_worst(
                    effective_json,
                    &self.nodes[source.node].dot_id,
                    &self.nodes[target.node].dot_id,
                    &source.port,
                    &target.port,
                    dot_key.as_deref(),
                )
                .saturating_add(parallel_json_worst(effective_json, dot_key.as_deref()))
            }
        } else {
            4_096usize
                .saturating_add(json_string_worst(&source_id).saturating_mul(2))
                .saturating_add(json_string_worst(&target_id).saturating_mul(2))
                .saturating_add(json_string_worst(self.source_file))
                .saturating_add(effective_json)
                .saturating_add(occurrence_json_worst(
                    effective_json,
                    &self.nodes[source.node].dot_id,
                    &self.nodes[target.node].dot_id,
                    &source.port,
                    &target.port,
                    dot_key.as_deref(),
                ))
                .saturating_add(parallel_json_worst(effective_json, dot_key.as_deref()))
        };
        let projected_detail = existing
            .map_or(0, |index| self.edges[index].json_bytes_estimate)
            .saturating_add(detail_add);
        if projected_detail > MAX_EDGE_JSON_ESTIMATE {
            self.retention_exhausted = true;
            self.resource(
                "dot_edge_metadata_limit",
                "DOT edge metadata would exceed downstream fact limits",
                span,
            );
            return false;
        }
        let mut retained = attrs_cost(effective_attrs)
            .saturating_add(attrs_cost(explicit_attrs))
            .saturating_add(
                effective_attrs
                    .get("label")
                    .map_or(0, String::len)
                    .saturating_mul(2),
            )
            .saturating_add(source_id.len().saturating_mul(3))
            .saturating_add(target_id.len().saturating_mul(3))
            .saturating_add(self.nodes[source.node].dot_id.len().saturating_mul(2))
            .saturating_add(self.nodes[target.node].dot_id.len().saturating_mul(2))
            .saturating_add(port_cost(&source.port))
            .saturating_add(port_cost(&target.port))
            .saturating_add(dot_key.as_ref().map_or(0, String::len).saturating_mul(5))
            .saturating_add(512);
        if existing.is_none() {
            retained = retained
                .saturating_add(attrs_cost(effective_attrs).saturating_mul(2))
                .saturating_add(self.source_file.len())
                .saturating_add(768);
        } else if let Some(edge) = existing.map(|index| &self.edges[index]) {
            let reused_identity = if self.strict {
                Some(0)
            } else {
                dot_key
                    .as_ref()
                    .and_then(|key| edge.keyed_parallel.get(key).copied())
            };
            if let Some(identity) = reused_identity {
                // The final identity map is retained and cloned into the new
                // occurrence (and aggregate representative for identity 0).
                let multiplier = if identity == 0 { 2 } else { 1 };
                retained = retained.saturating_add(
                    attrs_cost(&edge.parallel_edges[identity].attrs).saturating_mul(multiplier),
                );
            } else {
                // A new parallel identity and its occurrence each own a map.
                retained = retained.saturating_add(attrs_cost(effective_attrs));
            }
        }
        if !self.charge_metadata(retained, span) || !self.reserve_fact() {
            return false;
        }
        self.edge_occurrences += 1;
        if let Some(index) = existing {
            let edge = &mut self.edges[index];
            edge.json_bytes_estimate = edge.json_bytes_estimate.saturating_add(detail_add);
            edge.span = edge.span.join(span);
            edge.statements.insert(statement);
            let identity = if self.strict {
                0
            } else if let Some(key) = dot_key.as_ref() {
                if let Some(identity) = edge.keyed_parallel.get(key).copied() {
                    identity
                } else {
                    let identity = edge.parallel_edges.len();
                    edge.parallel_edges.push(ParallelEdge {
                        key: Some(key.clone()),
                        attrs: effective_attrs.clone(),
                        span,
                        occurrence_count: 0,
                    });
                    edge.keyed_parallel.insert(key.clone(), identity);
                    identity
                }
            } else {
                let identity = edge.parallel_edges.len();
                edge.parallel_edges.push(ParallelEdge {
                    key: None,
                    attrs: effective_attrs.clone(),
                    span,
                    occurrence_count: 0,
                });
                identity
            };
            let parallel = &mut edge.parallel_edges[identity];
            if (parallel.occurrence_count > 0 || self.strict)
                && merge_attrs_bounded(&mut parallel.attrs, explicit_attrs)
            {
                self.resource_truncated = true;
            }
            parallel.span = parallel.span.join(span);
            parallel.occurrence_count = parallel.occurrence_count.saturating_add(1);
            let occurrence_attrs = parallel.attrs.clone();
            if identity == 0 {
                edge.attrs.clone_from(&occurrence_attrs);
            }
            edge.occurrences.push(EdgeOccurrence {
                attrs: occurrence_attrs,
                span,
                source_dot_id: self.nodes[source.node].dot_id.clone(),
                target_dot_id: self.nodes[target.node].dot_id.clone(),
                operator: if relation == "connected_to" {
                    "--"
                } else {
                    "->"
                },
                source_port: source.port.clone(),
                target_port: target.port.clone(),
                statement,
                key: dot_key,
            });
            return true;
        }
        let mut keyed_parallel = BTreeMap::new();
        if let Some(key) = dot_key.as_ref() {
            keyed_parallel.insert(key.clone(), 0);
        }
        let occurrence = EdgeOccurrence {
            attrs: effective_attrs.clone(),
            span,
            source_dot_id: self.nodes[source.node].dot_id.clone(),
            target_dot_id: self.nodes[target.node].dot_id.clone(),
            operator: if relation == "connected_to" {
                "--"
            } else {
                "->"
            },
            source_port: source.port.clone(),
            target_port: target.port.clone(),
            statement,
            key: dot_key.clone(),
        };
        let index = self.edges.len();
        self.edges.push(EdgeAggregate {
            source: source.node,
            target: target.node,
            relation,
            span,
            attrs: effective_attrs.clone(),
            occurrences: vec![occurrence],
            statements: BTreeSet::from([statement]),
            parallel_edges: vec![ParallelEdge {
                key: dot_key,
                attrs: effective_attrs.clone(),
                span,
                occurrence_count: 1,
            }],
            keyed_parallel,
            json_bytes_estimate: detail_add,
        });
        self.edge_by_key.insert(aggregate_key, index);
        true
    }

    fn finalize_compatible_ids(&mut self) {
        let mut groups = BTreeMap::<String, Vec<usize>>::new();
        let mut pending = Vec::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if node.legacy_eligible {
                groups
                    .entry(make_id(&[&self.stem, "diagram", "graphviz", &node.dot_id]))
                    .or_default()
                    .push(index);
            } else {
                pending.push(index);
            }
        }

        let mut final_ids = vec![String::new(); self.nodes.len()];
        let mut used = BTreeSet::from([self.root_id.clone()]);
        for (candidate, mut owners) in groups {
            owners.sort_by(|left, right| {
                let left = &self.nodes[*left];
                let right = &self.nodes[*right];
                (usize::from(left.kind != "node"), left.identity.as_str())
                    .cmp(&(usize::from(right.kind != "node"), right.identity.as_str()))
            });
            if !used.contains(&candidate) {
                let winner = owners.remove(0);
                final_ids[winner] = candidate.clone();
                used.insert(candidate);
            }
            pending.extend(owners);
        }
        pending.sort_by(|left, right| self.nodes[*left].identity.cmp(&self.nodes[*right].identity));
        for index in pending {
            let node = &self.nodes[index];
            let base = make_id(&[&self.stem, "diagram", "graphviz", node.kind, &node.dot_id]);
            let mut counter = 0u64;
            let candidate = loop {
                let owner = if counter == 0 {
                    node.identity.clone()
                } else {
                    format!("{}\0{counter}", node.identity)
                };
                let digest = blake3::hash(owner.as_bytes()).to_hex();
                let candidate = format!("{base}_{EXACT_ID_MARKER}_{digest}");
                if !used.contains(&candidate) {
                    break candidate;
                }
                counter = counter.saturating_add(1);
            };
            used.insert(candidate.clone());
            final_ids[index] = candidate;
        }

        self.node_by_dot_id.clear();
        self.subgraph_by_key.clear();
        self.edge_by_key.clear();
        self.containment_set.clear();
        let nodes = &self.nodes;
        let lookup = nodes
            .iter()
            .zip(&final_ids)
            .map(|(node, final_id)| (node.id.as_str(), final_id.as_str()))
            .collect::<BTreeMap<_, _>>();
        for containment in &mut self.containments {
            if let Some(final_id) = lookup.get(containment.source.as_str()) {
                containment.source = (*final_id).to_owned();
            }
            if let Some(final_id) = lookup.get(containment.target.as_str()) {
                containment.target = (*final_id).to_owned();
            }
        }
        drop(lookup);
        for (node, final_id) in self.nodes.iter_mut().zip(final_ids) {
            node.id = final_id;
        }
    }
}

struct Parser<'a> {
    lexer: Lexer<'a>,
    current: Token,
    state: DotState<'a>,
    scopes: Vec<ScopeFrame>,
    last_span: Span,
    endpoint_materializations: usize,
}

impl<'a> Parser<'a> {
    fn new(source_file: &'a str, source: &'a [u8], encoding: Encoding) -> Self {
        let mut lexer = Lexer::new(source, encoding);
        let current = lexer.next();
        let mut parser = Self {
            lexer,
            current,
            state: DotState::new(source_file, source),
            scopes: Vec::new(),
            last_span: Span::default(),
            endpoint_materializations: 0,
        };
        parser.observe_current();
        parser
    }

    fn parse_graph(&mut self) {
        let header_start = self.current.span.start;
        if self.current_keyword("strict") {
            self.state.strict = true;
            self.bump();
        }
        let kind_span = self.current.span;
        self.state.graph_kind = if self.current_keyword("graph") {
            self.bump();
            Some(GraphKind::Graph)
        } else if self.current_keyword("digraph") {
            self.bump();
            Some(GraphKind::Digraph)
        } else {
            self.state.syntax(
                "dot_expected_graph_header",
                "expected graph or digraph header",
                kind_span,
            );
            None
        };
        if matches!(self.current.kind, TokenKind::Id(..))
            && let Some(id) = self.parse_id()
        {
            self.reject_reserved_id(&id);
            self.state.graph_id = Some(id.value);
        }
        if !matches!(self.current.kind, TokenKind::LBrace) {
            self.state.syntax(
                "dot_expected_graph_body",
                "expected '{' after DOT graph header",
                self.current.span,
            );
            while !matches!(self.current.kind, TokenKind::LBrace | TokenKind::Eof) {
                self.bump();
            }
        }
        if matches!(self.current.kind, TokenKind::LBrace) {
            self.bump();
        } else {
            self.state.root_span = Span {
                start: header_start,
                end: self.current.span.end,
            };
            return;
        }
        self.scopes.push(ScopeFrame {
            subgraph: None,
            graph_attrs: Attrs::new(),
            node_defaults: Attrs::new(),
            edge_defaults: Attrs::new(),
        });
        self.parse_stmt_list(0);
        let end = if matches!(self.current.kind, TokenKind::RBrace) {
            let end = self.current.span.end;
            self.bump();
            end
        } else {
            self.state.syntax(
                "dot_unclosed_graph",
                "expected '}' to close DOT graph",
                self.current.span,
            );
            self.current.span.end
        };
        if let Some(root) = self.scopes.pop() {
            self.state.root_attrs = root.graph_attrs;
            self.state.root_node_defaults = root.node_defaults;
            self.state.root_edge_defaults = root.edge_defaults;
        }
        self.state.root_span = Span {
            start: header_start,
            end,
        };
        if !matches!(self.current.kind, TokenKind::Eof) {
            let code = if self.current_keyword("strict")
                || self.current_keyword("graph")
                || self.current_keyword("digraph")
            {
                "dot_multiple_graphs_unsupported"
            } else {
                "dot_trailing_tokens"
            };
            self.state.syntax(
                code,
                "tokens after the single DOT graph were ignored",
                self.current.span,
            );
            while !matches!(self.current.kind, TokenKind::Eof) {
                self.bump();
            }
        }
    }

    fn parse_stmt_list(&mut self, depth: usize) {
        while !matches!(self.current.kind, TokenKind::RBrace | TokenKind::Eof) {
            if self.state.retention_exhausted {
                self.stop_semantic_parsing();
                return;
            }
            if matches!(
                self.current.kind,
                TokenKind::Semi | TokenKind::Comma | TokenKind::Invalid(..)
            ) {
                self.bump();
                continue;
            }
            let before = self.current.span.start;
            self.parse_stmt(depth);
            while matches!(self.current.kind, TokenKind::Semi) {
                self.bump();
            }
            if self.current.span.start == before
                && !matches!(self.current.kind, TokenKind::RBrace | TokenKind::Eof)
            {
                self.state.syntax(
                    "dot_parser_stalled",
                    "ignored a token while recovering from a DOT statement",
                    self.current.span,
                );
                self.bump();
            }
        }
    }

    fn parse_stmt(&mut self, depth: usize) {
        if self.current_keyword("subgraph") || matches!(self.current.kind, TokenKind::LBrace) {
            let left = self.parse_subgraph(depth);
            if self.is_edge_op() {
                self.parse_edge_rhs(left, depth);
            }
            return;
        }
        let Some(id) = self.parse_id() else {
            self.state.syntax(
                "dot_expected_statement",
                "expected a DOT statement",
                self.current.span,
            );
            self.recover_statement();
            return;
        };
        if matches!(self.current.kind, TokenKind::Equal) {
            self.reject_reserved_id(&id);
            self.bump();
            if let Some(value) = self.parse_id() {
                self.reject_reserved_id(&value);
                self.insert_graph_attr(id.value, value.value, id.span.join(value.span));
            } else {
                self.state.syntax(
                    "dot_expected_assignment_value",
                    "expected an ID after '='",
                    self.current.span,
                );
            }
            return;
        }
        if id.style == IdStyle::Plain
            && matches!(
                id.value.to_ascii_lowercase().as_str(),
                "graph" | "node" | "edge"
            )
            && matches!(self.current.kind, TokenKind::LBracket)
        {
            let mut attrs = self.parse_attr_lists();
            let attribute_kind = id.value.to_ascii_lowercase();
            if attribute_kind == "edge" {
                attrs.remove("key");
            }
            match attribute_kind.as_str() {
                "graph" => self.merge_scope_attrs(ScopeAttrs::Graph, &attrs, id.span),
                "node" => self.merge_scope_attrs(ScopeAttrs::Node, &attrs, id.span),
                "edge" => self.merge_scope_attrs(ScopeAttrs::Edge, &attrs, id.span),
                _ => unreachable!(),
            }
            return;
        }
        self.reject_reserved_id(&id);
        let first = self.endpoint_from_id(id);
        let mut left = EndpointGroup {
            span: first.as_ref().map_or(self.current.span, |value| value.span),
            endpoints: first.into_iter().collect(),
            subgraph: None,
        };
        while matches!(self.current.kind, TokenKind::Comma) {
            if left.endpoints.len() >= MAX_CHAIN_ENDPOINTS {
                self.state.resource(
                    "dot_edge_endpoint_limit",
                    "DOT endpoint list limit reached",
                    self.current.span,
                );
                self.recover_statement();
                break;
            }
            self.bump();
            let Some(next_id) = self.parse_id() else {
                self.state.syntax(
                    "dot_expected_node_after_comma",
                    "expected a node ID after ','",
                    self.current.span,
                );
                break;
            };
            self.reject_reserved_id(&next_id);
            if let Some(endpoint) = self.endpoint_from_id(next_id) {
                left.span = left.span.join(endpoint.span);
                left.endpoints.push(endpoint);
            }
        }
        if self.is_edge_op() {
            self.parse_edge_rhs(left, depth);
            return;
        }
        let attrs = self.parse_attr_lists();
        let statement_span = Span {
            start: left.span.start,
            end: self.last_span.end.max(left.span.end),
        };
        for endpoint in left.endpoints {
            self.apply_node_attrs(endpoint.node, &attrs, statement_span);
        }
    }

    fn parse_edge_rhs(&mut self, first: EndpointGroup, depth: usize) {
        let start = first.span;
        let mut retained_endpoint_count = first.endpoints.len();
        let mut groups = vec![first];
        let mut operators = Vec::new();
        while self.is_edge_op() {
            if groups.len() >= MAX_CHAIN_GROUPS {
                self.state.resource(
                    "dot_edge_chain_limit",
                    "DOT edge chain limit reached",
                    self.current.span,
                );
                self.recover_statement();
                return;
            }
            let operator = self.current.kind.clone();
            let operator_span = self.current.span;
            if let Some(kind) = self.state.graph_kind {
                let mismatch = !matches!(
                    (kind.expected_edge(), &operator),
                    (TokenKind::Arrow, TokenKind::Arrow)
                        | (TokenKind::DashDash, TokenKind::DashDash)
                );
                if mismatch {
                    self.state.syntax(
                        "dot_edge_operator_mismatch",
                        format!("edge operator does not match {}", kind.name()),
                        operator_span,
                    );
                }
            }
            self.bump();
            let Some(group) = self.parse_endpoint_group(depth) else {
                self.state.syntax(
                    "dot_expected_edge_endpoint",
                    "expected a node or subgraph after edge operator",
                    self.current.span,
                );
                self.recover_statement();
                return;
            };
            let Some(next_retained) = retained_endpoint_count.checked_add(group.endpoints.len())
            else {
                self.state.resource(
                    "dot_edge_endpoint_limit",
                    "DOT edge endpoint retention limit reached",
                    group.span,
                );
                return;
            };
            if next_retained > MAX_CHAIN_ENDPOINTS {
                self.state.resource(
                    "dot_edge_endpoint_limit",
                    "DOT edge endpoint retention limit reached",
                    group.span,
                );
                return;
            }
            retained_endpoint_count = next_retained;
            operators.push(operator);
            groups.push(group);
        }
        let mut explicit = self.parse_attr_lists();
        let dot_key = explicit.remove("key");
        // CGraph holds live subgraph objects through the complete RHS and only
        // expands their members in `endedge`. Materializing after every group
        // is parsed preserves that behavior when a later RHS occurrence adds
        // members to a named subgraph referenced earlier in the chain.
        let mut endpoint_count = 0usize;
        for group in &mut groups {
            if !self.materialize_subgraph_endpoints(group) {
                self.recover_statement();
                return;
            }
            let Some(next_endpoint_count) = endpoint_count.checked_add(group.endpoints.len())
            else {
                self.state.resource(
                    "dot_edge_endpoint_limit",
                    "DOT edge endpoint expansion limit reached",
                    group.span,
                );
                return;
            };
            if next_endpoint_count > MAX_CHAIN_ENDPOINTS {
                self.state.resource(
                    "dot_edge_endpoint_limit",
                    "DOT edge endpoint expansion limit reached",
                    group.span,
                );
                return;
            }
            endpoint_count = next_endpoint_count;
        }
        let mut attrs = self.current_scope().edge_defaults.clone();
        // Lowercase `key` is consumed by CGraph as edge identity. A default
        // key never names an edge; uppercase `Key` remains an ordinary attr.
        attrs.remove("key");
        if merge_attrs_bounded(&mut attrs, &explicit) {
            self.state.resource(
                "dot_attribute_limit",
                "DOT effective edge attribute limit reached",
                start,
            );
        }
        self.state.statement_sequence = self.state.statement_sequence.saturating_add(1);
        let statement = self.state.statement_sequence;
        let endpoint_end = groups.last().map_or(start.end, |group| group.span.end);
        let span = Span {
            start: start.start,
            end: self.last_span.end.max(endpoint_end),
        };
        for (segment, operator) in operators.into_iter().enumerate() {
            if self.state.retention_exhausted {
                break;
            }
            let relation = if matches!(operator, TokenKind::DashDash) {
                "connected_to"
            } else {
                "flows_to"
            };
            'pairs: for source in &groups[segment].endpoints {
                for target in &groups[segment + 1].endpoints {
                    if !self.state.add_edge(EdgeInput {
                        source,
                        target,
                        relation,
                        effective_attrs: &attrs,
                        explicit_attrs: &explicit,
                        dot_key: dot_key.clone(),
                        span,
                        statement,
                    }) {
                        break 'pairs;
                    }
                }
            }
        }
    }

    fn parse_endpoint_group(&mut self, depth: usize) -> Option<EndpointGroup> {
        if self.current_keyword("subgraph") || matches!(self.current.kind, TokenKind::LBrace) {
            return Some(self.parse_subgraph(depth));
        }
        let id = self.parse_id()?;
        self.reject_reserved_id(&id);
        let endpoint = self.endpoint_from_id(id)?;
        let mut group = EndpointGroup {
            endpoints: vec![endpoint.clone()],
            subgraph: None,
            span: endpoint.span,
        };
        while matches!(self.current.kind, TokenKind::Comma) {
            if group.endpoints.len() >= MAX_CHAIN_ENDPOINTS {
                self.state.resource(
                    "dot_edge_endpoint_limit",
                    "DOT endpoint list limit reached",
                    self.current.span,
                );
                self.recover_statement();
                break;
            }
            self.bump();
            let Some(id) = self.parse_id() else {
                self.state.syntax(
                    "dot_expected_node_after_comma",
                    "expected a node ID after ','",
                    self.current.span,
                );
                break;
            };
            self.reject_reserved_id(&id);
            if let Some(endpoint) = self.endpoint_from_id(id) {
                group.span = group.span.join(endpoint.span);
                group.endpoints.push(endpoint);
            }
        }
        Some(group)
    }

    fn endpoint_from_id(&mut self, id: ParsedId) -> Option<Endpoint> {
        let (port, end) = self.parse_port(id.span.end);
        let span = Span {
            start: id.span.start,
            end,
        };
        let subgraphs = self.active_subgraphs();
        let node = if let Some(node) = self.state.node_by_dot_id.get(&id.value).copied() {
            self.state.add_memberships(node, span, &subgraphs);
            node
        } else {
            let defaults = self.current_scope().node_defaults.clone();
            self.state
                .ensure_node(&id.value, span, defaults, &subgraphs)?
        };
        Some(Endpoint { node, port, span })
    }

    fn parse_port(&mut self, initial_end: usize) -> (Port, usize) {
        if !matches!(self.current.kind, TokenKind::Colon) {
            return (Port::default(), initial_end);
        }
        self.bump();
        let Some(first) = self.parse_id() else {
            self.state.syntax(
                "dot_expected_port_id",
                "expected port or compass ID after ':'",
                self.current.span,
            );
            return (Port::default(), initial_end);
        };
        self.reject_reserved_id(&first);
        let mut result = Port::default();
        let mut end = first.span.end;
        if matches!(self.current.kind, TokenKind::Colon) {
            self.bump();
            result.port = Some(first.value);
            if let Some(compass) = self.parse_id() {
                self.reject_reserved_id(&compass);
                end = compass.span.end;
                result.compass = Some(compass.value);
            } else {
                self.state.syntax(
                    "dot_expected_compass",
                    "expected compass point after second ':'",
                    self.current.span,
                );
            }
        } else if is_compass(&first.value) {
            result.compass = Some(first.value);
        } else {
            result.port = Some(first.value);
        }
        (result, end)
    }

    fn parse_subgraph(&mut self, depth: usize) -> EndpointGroup {
        let start = self.current.span.start;
        let named_prefix = self.current_keyword("subgraph");
        if named_prefix {
            self.bump();
        }
        let name = if named_prefix && matches!(self.current.kind, TokenKind::Id(..)) {
            let parsed = self.parse_id();
            if let Some(id) = &parsed {
                self.reject_reserved_id(id);
            }
            parsed
        } else {
            None
        };
        if !matches!(self.current.kind, TokenKind::LBrace) {
            self.state.syntax(
                "dot_expected_subgraph_body",
                "expected '{' to open subgraph",
                self.current.span,
            );
            return EndpointGroup {
                endpoints: Vec::new(),
                subgraph: None,
                span: Span {
                    start,
                    end: self.current.span.end,
                },
            };
        }
        let brace = self.current.span;
        self.bump();
        let dot_id = name
            .as_ref()
            .map(|id| id.value.clone())
            .unwrap_or_else(|| format!("@anonymous:{start}"));
        let key = name
            .as_ref()
            .map(|id| format!("named:{}", id.value))
            .unwrap_or_else(|| format!("anonymous:{start}"));
        let existing_subgraph = self.state.subgraph_by_key.get(&key).copied();
        let parent = self.active_subgraphs().last().copied();
        let subgraph = self.state.ensure_subgraph(key, dot_id, brace, parent);
        let inherited_cost = if let Some(index) = existing_subgraph {
            let node = &self.state.nodes[self.state.subgraphs[index].node];
            attrs_cost(&node.attrs)
                .saturating_add(attrs_cost(&node.node_defaults))
                .saturating_add(attrs_cost(&node.edge_defaults))
        } else {
            attrs_cost(&self.current_scope().graph_attrs)
                .saturating_add(attrs_cost(&self.current_scope().node_defaults))
                .saturating_add(attrs_cost(&self.current_scope().edge_defaults))
        }
        .saturating_add(256);
        let inherited = if self.state.charge_metadata(inherited_cost, brace) {
            if let Some(index) = existing_subgraph {
                let node = &self.state.nodes[self.state.subgraphs[index].node];
                ScopeFrame {
                    subgraph,
                    graph_attrs: node.attrs.clone(),
                    node_defaults: node.node_defaults.clone(),
                    edge_defaults: node.edge_defaults.clone(),
                }
            } else {
                self.current_scope().clone()
            }
        } else {
            ScopeFrame {
                subgraph: None,
                graph_attrs: Attrs::new(),
                node_defaults: Attrs::new(),
                edge_defaults: Attrs::new(),
            }
        };
        self.scopes.push(ScopeFrame {
            subgraph,
            graph_attrs: inherited.graph_attrs,
            node_defaults: inherited.node_defaults,
            edge_defaults: inherited.edge_defaults,
        });
        if depth >= MAX_NESTING {
            self.state.resource(
                "dot_nesting_limit",
                "DOT subgraph nesting limit reached",
                brace,
            );
            self.skip_balanced_body();
        } else {
            self.parse_stmt_list(depth + 1);
        }
        let end = if matches!(self.current.kind, TokenKind::RBrace) {
            let end = self.current.span.end;
            self.bump();
            end
        } else {
            self.state.syntax(
                "dot_unclosed_subgraph",
                "expected '}' to close subgraph",
                self.current.span,
            );
            self.current.span.end
        };
        let frame = self.scopes.pop().expect("subgraph scope");
        if let Some(index) = subgraph {
            let node_index = self.state.subgraphs[index].node;
            let node = &mut self.state.nodes[node_index];
            node.span = node.span.join(Span { start, end });
            node.attrs = frame.graph_attrs;
            node.node_defaults = frame.node_defaults;
            node.edge_defaults = frame.edge_defaults;
            if let Some(label) = node.attrs.get("label") {
                node.label.clone_from(label);
            }
        }
        EndpointGroup {
            endpoints: Vec::new(),
            subgraph,
            span: Span { start, end },
        }
    }

    fn materialize_subgraph_endpoints(&mut self, group: &mut EndpointGroup) -> bool {
        let Some(subgraph) = group.subgraph.take() else {
            return true;
        };
        let members = &self.state.subgraphs[subgraph].members;
        let Some(total) = self.endpoint_materializations.checked_add(members.len()) else {
            self.state.resource(
                "dot_endpoint_work_limit",
                "DOT subgraph endpoint materialization limit reached",
                group.span,
            );
            self.state.retention_exhausted = true;
            return false;
        };
        if total > MAX_ENDPOINT_MATERIALIZATIONS {
            self.state.resource(
                "dot_endpoint_work_limit",
                "DOT subgraph endpoint materialization limit reached",
                group.span,
            );
            self.state.retention_exhausted = true;
            return false;
        }
        self.endpoint_materializations = total;
        group.endpoints.reserve(members.len());
        group
            .endpoints
            .extend(members.iter().copied().map(|node| Endpoint {
                node,
                port: Port::default(),
                span: group.span,
            }));
        true
    }

    fn skip_balanced_body(&mut self) {
        let mut nesting = 1usize;
        while nesting > 0 && !matches!(self.current.kind, TokenKind::Eof) {
            match self.current.kind {
                TokenKind::LBrace => nesting = nesting.saturating_add(1),
                TokenKind::RBrace => {
                    nesting -= 1;
                    if nesting == 0 {
                        break;
                    }
                }
                _ => {}
            }
            self.bump();
        }
    }

    fn parse_attr_lists(&mut self) -> Attrs {
        let mut attrs = Attrs::new();
        while matches!(self.current.kind, TokenKind::LBracket) {
            self.bump();
            while !matches!(self.current.kind, TokenKind::RBracket | TokenKind::Eof) {
                if matches!(self.current.kind, TokenKind::Semi | TokenKind::Comma) {
                    self.bump();
                    continue;
                }
                if matches!(self.current.kind, TokenKind::Invalid(..)) {
                    self.bump();
                    continue;
                }
                let Some(key) = self.parse_id() else {
                    self.state.syntax(
                        "dot_expected_attribute_name",
                        "expected an attribute name",
                        self.current.span,
                    );
                    self.bump();
                    continue;
                };
                self.reject_reserved_id(&key);
                if !matches!(self.current.kind, TokenKind::Equal) {
                    self.state.syntax(
                        "dot_expected_attribute_equal",
                        "expected '=' after attribute name",
                        self.current.span,
                    );
                    continue;
                }
                self.bump();
                let Some(value) = self.parse_id() else {
                    self.state.syntax(
                        "dot_expected_attribute_value",
                        "expected an attribute value",
                        self.current.span,
                    );
                    continue;
                };
                self.reject_reserved_id(&value);
                if attrs.len() < MAX_ATTRS || attrs.contains_key(&key.value) {
                    attrs.insert(key.value, value.value);
                } else {
                    self.state.resource(
                        "dot_attribute_limit",
                        "DOT attribute retention limit reached",
                        key.span.join(value.span),
                    );
                }
            }
            if matches!(self.current.kind, TokenKind::RBracket) {
                self.bump();
            } else {
                self.state.syntax(
                    "dot_unclosed_attribute_list",
                    "expected ']' to close attribute list",
                    self.current.span,
                );
                break;
            }
        }
        attrs
    }

    fn parse_id(&mut self) -> Option<ParsedId> {
        let TokenKind::Id(value, style) = self.current.kind.clone() else {
            return None;
        };
        let mut id = ParsedId {
            value,
            style,
            span: self.current.span,
        };
        self.bump();
        while id.style == IdStyle::Quoted && matches!(self.current.kind, TokenKind::Plus) {
            let plus = self.current.span;
            self.bump();
            let TokenKind::Id(piece, IdStyle::Quoted) = self.current.kind.clone() else {
                self.state.syntax(
                    "dot_invalid_string_concatenation",
                    "'+' in an ID must be followed by a quoted string",
                    plus,
                );
                break;
            };
            let mut truncated = false;
            push_bounded(&mut id.value, &piece, &mut truncated);
            if truncated {
                self.state.resource(
                    "dot_token_text_limit",
                    "concatenated DOT ID exceeded retained token limit",
                    id.span.join(self.current.span),
                );
            }
            id.span = id.span.join(self.current.span);
            self.bump();
        }
        Some(id)
    }

    fn apply_node_attrs(&mut self, node: usize, attrs: &Attrs, span: Span) {
        if !self
            .state
            .charge_metadata(attrs_cost(attrs).saturating_add(64), span)
        {
            return;
        }
        let record = &mut self.state.nodes[node];
        let truncated = merge_attrs_bounded(&mut record.attrs, attrs);
        if let Some(label) = attrs.get("label") {
            record.label.clone_from(label);
        }
        record.span = record.span.join(span);
        if truncated {
            self.state.resource(
                "dot_attribute_limit",
                "DOT node attribute retention limit reached",
                span,
            );
        }
    }

    fn insert_graph_attr(&mut self, key: String, value: String, span: Span) {
        if !self.state.charge_metadata(
            key.len().saturating_add(value.len()).saturating_add(96),
            span,
        ) {
            return;
        }
        let graph_attrs = &mut self.current_scope_mut().graph_attrs;
        if graph_attrs.len() < MAX_ATTRS || graph_attrs.contains_key(&key) {
            graph_attrs.insert(key, value);
        } else {
            self.state.resource(
                "dot_attribute_limit",
                "DOT graph attribute retention limit reached",
                span,
            );
        }
    }

    fn merge_scope_attrs(&mut self, target: ScopeAttrs, attrs: &Attrs, span: Span) {
        if !self
            .state
            .charge_metadata(attrs_cost(attrs).saturating_add(64), span)
        {
            return;
        }
        let scope = self.current_scope_mut();
        let destination = match target {
            ScopeAttrs::Graph => &mut scope.graph_attrs,
            ScopeAttrs::Node => &mut scope.node_defaults,
            ScopeAttrs::Edge => &mut scope.edge_defaults,
        };
        if merge_attrs_bounded(destination, attrs) {
            self.state.resource(
                "dot_attribute_limit",
                "DOT scoped attribute retention limit reached",
                span,
            );
        }
    }

    fn active_subgraphs(&self) -> Vec<usize> {
        self.scopes
            .iter()
            .filter_map(|scope| scope.subgraph)
            .collect()
    }

    fn current_scope(&self) -> &ScopeFrame {
        self.scopes.last().expect("DOT root scope")
    }

    fn current_scope_mut(&mut self) -> &mut ScopeFrame {
        self.scopes.last_mut().expect("DOT root scope")
    }

    fn current_keyword(&self, expected: &str) -> bool {
        matches!(
            &self.current.kind,
            TokenKind::Id(value, IdStyle::Plain) if value.eq_ignore_ascii_case(expected)
        )
    }

    fn reject_reserved_id(&mut self, id: &ParsedId) {
        if id.style == IdStyle::Plain && is_reserved_keyword(&id.value) {
            self.state.syntax(
                "dot_reserved_keyword_as_id",
                "reserved DOT keyword must be quoted when used as an ID",
                id.span,
            );
        }
    }

    fn is_edge_op(&self) -> bool {
        matches!(self.current.kind, TokenKind::Arrow | TokenKind::DashDash)
    }

    fn recover_statement(&mut self) {
        while !matches!(
            self.current.kind,
            TokenKind::Semi | TokenKind::RBrace | TokenKind::Eof
        ) {
            self.bump();
        }
    }

    fn stop_semantic_parsing(&mut self) {
        let end = self.lexer.bytes.len();
        self.lexer.pos = end;
        self.current = Token {
            kind: TokenKind::Eof,
            span: Span { start: end, end },
            truncated: false,
        };
    }

    fn observe_current(&mut self) {
        if self.current.truncated {
            self.state.resource(
                "dot_token_text_limit",
                "DOT token exceeded retained text limit",
                self.current.span,
            );
        }
        if let TokenKind::Invalid(code, message, resource) = self.current.kind {
            if resource {
                self.state.resource(code, message, self.current.span);
            } else {
                self.state.syntax(code, message, self.current.span);
            }
        }
    }

    fn bump(&mut self) {
        self.last_span = self.current.span;
        self.current = self.lexer.next();
        self.observe_current();
    }

    fn finish(mut self) -> Extraction {
        self.state.display_latin1 =
            self.lexer.encoding == Encoding::Utf8 && self.state.declares_latin1();
        self.state.into_extraction()
    }
}

fn is_compass(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "n" | "ne" | "e" | "se" | "s" | "sw" | "w" | "nw" | "c" | "_"
    )
}

fn is_reserved_keyword(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "strict" | "graph" | "digraph" | "node" | "edge" | "subgraph"
    )
}

impl DotState<'_> {
    fn into_extraction(mut self) -> Extraction {
        if !self.root_admitted {
            return Extraction::default();
        }
        self.finalize_compatible_ids();
        let status = self.status();
        let diagnostics = self
            .diagnostics
            .iter()
            .map(|diagnostic| {
                json!({
                    "code": diagnostic.code,
                    "message": diagnostic.message,
                    "source_range": self.lines.range(diagnostic.span),
                })
            })
            .collect::<Vec<_>>();
        let mut root_extra = BTreeMap::from([
            ("_origin".into(), Value::String("diagram".into())),
            ("diagram_format".into(), Value::String("graphviz".into())),
            ("type".into(), Value::String("diagram".into())),
            ("parse_status".into(), Value::String(status.into())),
            (
                "format_capability".into(),
                Value::String("semantic_full".into()),
            ),
            (
                "dot_graph_kind".into(),
                self.graph_kind
                    .map(|kind| Value::String(kind.name().into()))
                    .unwrap_or(Value::Null),
            ),
            (
                "dot_graph_id".into(),
                self.graph_id
                    .as_ref()
                    .map(|id| Value::String(id.clone()))
                    .unwrap_or(Value::Null),
            ),
            ("dot_strict".into(), Value::Bool(self.strict)),
            ("dot_attributes".into(), attrs_value(&self.root_attrs)),
            (
                "dot_node_defaults".into(),
                attrs_value(&self.root_node_defaults),
            ),
            (
                "dot_edge_defaults".into(),
                attrs_value(&self.root_edge_defaults),
            ),
            ("dot_diagnostics".into(), Value::Array(diagnostics)),
            ("source_range".into(), self.lines.range(self.root_span)),
        ]);
        if self.resource_truncated {
            root_extra.insert("truncated".into(), Value::Bool(true));
        }
        if self.diagnostics_omitted > 0 {
            root_extra.insert(
                "dot_diagnostics_omitted".into(),
                Value::from(self.diagnostics_omitted as u64),
            );
        }
        let root = Node {
            id: self.root_id.clone(),
            label: Path::new(self.source_file)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(self.source_file)
                .to_owned(),
            file_type: "document".into(),
            source_file: self.source_file.into(),
            source_location: Some("L1".into()),
            community: None,
            extra: root_extra,
        };

        let mut nodes = Vec::with_capacity(self.nodes.len() + 1);
        nodes.push(root);
        for record in &self.nodes {
            let mut extra = BTreeMap::from([
                ("_origin".into(), Value::String("diagram".into())),
                ("diagram_format".into(), Value::String("graphviz".into())),
                ("diagram_kind".into(), Value::String(record.kind.into())),
                ("type".into(), Value::String(record.kind.into())),
                ("dot_id".into(), Value::String(record.dot_id.clone())),
                ("dot_attributes".into(), attrs_value(&record.attrs)),
                ("source_range".into(), self.lines.range(record.span)),
            ]);
            extra.insert(
                "dot_subgraphs".into(),
                Value::Array(
                    record
                        .subgraphs
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
            if record.kind == "subgraph" {
                extra.insert(
                    "dot_node_defaults".into(),
                    attrs_value(&record.node_defaults),
                );
                extra.insert(
                    "dot_edge_defaults".into(),
                    attrs_value(&record.edge_defaults),
                );
            }
            nodes.push(Node {
                id: record.id.clone(),
                label: self.display_label(&record.label),
                file_type: "document".into(),
                source_file: self.source_file.into(),
                source_location: Some(self.lines.location(record.span)),
                community: None,
                extra,
            });
        }

        let mut edges = Vec::with_capacity(self.containments.len() + self.edges.len());
        for containment in &self.containments {
            edges.push(Edge {
                source: containment.source.clone(),
                target: containment.target.clone(),
                relation: "contains".into(),
                confidence: Confidence::Extracted,
                source_file: self.source_file.into(),
                extra: BTreeMap::from([
                    ("_src".into(), Value::String(containment.source.clone())),
                    ("_tgt".into(), Value::String(containment.target.clone())),
                    (
                        "source_location".into(),
                        Value::String(self.lines.location(containment.span)),
                    ),
                    ("source_range".into(), self.lines.range(containment.span)),
                    ("diagram_format".into(), Value::String("graphviz".into())),
                    ("weight".into(), Value::from(1.0)),
                ]),
            });
        }
        for aggregate in &self.edges {
            let source = self.nodes[aggregate.source].id.clone();
            let target = self.nodes[aggregate.target].id.clone();
            let mut extra = BTreeMap::from([
                ("_src".into(), Value::String(source.clone())),
                ("_tgt".into(), Value::String(target.clone())),
                (
                    "source_location".into(),
                    Value::String(self.lines.location(aggregate.span)),
                ),
                ("source_range".into(), self.lines.range(aggregate.span)),
                ("diagram_format".into(), Value::String("graphviz".into())),
                ("weight".into(), Value::from(1.0)),
            ]);
            if let Value::Object(object) = self.edge_semantics_value(aggregate) {
                extra.extend(object);
            }
            if let Some(label) = aggregate.attrs.get("label") {
                extra.insert("label".into(), Value::String(self.display_label(label)));
            }
            edges.push(Edge {
                source,
                target,
                relation: aggregate.relation.into(),
                confidence: Confidence::Extracted,
                source_file: self.source_file.into(),
                extra,
            });
        }
        let mut extraction = Extraction {
            nodes,
            edges,
            hyperedges: Vec::new(),
        };
        enforce_emitted_fact_limits(&mut extraction);
        extraction
    }

    fn edge_semantics_value(&self, aggregate: &EdgeAggregate) -> Value {
        let occurrences = aggregate
            .occurrences
            .iter()
            .map(|occurrence| {
                json!({
                    "source_id": occurrence.source_dot_id,
                    "target_id": occurrence.target_dot_id,
                    "operator": occurrence.operator,
                    "source_port": occurrence.source_port.port,
                    "source_compass": occurrence.source_port.compass,
                    "target_port": occurrence.target_port.port,
                    "target_compass": occurrence.target_port.compass,
                    "dot_attributes": attrs_value(&occurrence.attrs),
                    "dot_key": occurrence.key,
                    "dot_statement": occurrence.statement,
                    "source_range": self.lines.range(occurrence.span),
                })
            })
            .collect::<Vec<_>>();
        let parallel_edges = aggregate
            .parallel_edges
            .iter()
            .map(|parallel| {
                json!({
                    "dot_key": parallel.key,
                    "dot_attributes": attrs_value(&parallel.attrs),
                    "dot_occurrence_count": parallel.occurrence_count,
                    "source_range": self.lines.range(parallel.span),
                })
            })
            .collect::<Vec<_>>();
        json!({
            "source_id": self.nodes[aggregate.source].dot_id,
            "target_id": self.nodes[aggregate.target].dot_id,
            "dot_occurrence_count": aggregate.occurrences.len(),
            "dot_parallel_count": aggregate.parallel_edges.len(),
            "dot_parallel_edges": parallel_edges,
            "dot_statement_count": aggregate.statements.len(),
            "dot_occurrences": occurrences,
            "dot_attributes": attrs_value(&aggregate.attrs),
            "source_range": self.lines.range(aggregate.span),
        })
    }

    fn display_label(&self, value: &str) -> String {
        if !self.display_latin1 {
            return value.to_owned();
        }
        value
            .as_bytes()
            .iter()
            .map(|byte| char::from(*byte))
            .collect()
    }
}

fn attrs_value(attrs: &Attrs) -> Value {
    let mut object = Map::new();
    for (key, value) in attrs {
        object.insert(key.clone(), Value::String(value.clone()));
    }
    Value::Object(object)
}

fn attrs_cost(attrs: &Attrs) -> usize {
    attrs.iter().fold(0usize, |total, (key, value)| {
        total
            .saturating_add(key.len().saturating_mul(2))
            .saturating_add(value.len().saturating_mul(2))
            .saturating_add(192)
    })
}

fn json_string_worst(value: &str) -> usize {
    // JSON may encode a control byte as six ASCII bytes (`\u00XX`).
    value.len().saturating_mul(6).saturating_add(2)
}

fn attrs_json_worst(attrs: &Attrs) -> usize {
    attrs.iter().fold(2usize, |total, (key, value)| {
        total
            .saturating_add(json_string_worst(key))
            .saturating_add(json_string_worst(value))
            .saturating_add(2)
    })
}

fn occurrence_json_worst(
    attrs_bytes: usize,
    source_dot_id: &str,
    target_dot_id: &str,
    source_port: &Port,
    target_port: &Port,
    dot_key: Option<&str>,
) -> usize {
    1_024usize
        .saturating_add(attrs_bytes)
        .saturating_add(json_string_worst(source_dot_id))
        .saturating_add(json_string_worst(target_dot_id))
        .saturating_add(source_port.port.as_deref().map_or(4, json_string_worst))
        .saturating_add(source_port.compass.as_deref().map_or(4, json_string_worst))
        .saturating_add(target_port.port.as_deref().map_or(4, json_string_worst))
        .saturating_add(target_port.compass.as_deref().map_or(4, json_string_worst))
        .saturating_add(dot_key.map_or(4, json_string_worst))
}

fn parallel_json_worst(attrs_bytes: usize, dot_key: Option<&str>) -> usize {
    512usize
        .saturating_add(attrs_bytes)
        .saturating_add(dot_key.map_or(4, json_string_worst))
}

#[derive(Debug)]
struct LimitedWriter {
    remaining: usize,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "serialized DOT fact exceeds its byte limit",
            ));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_fact_fits(value: &impl Serialize) -> bool {
    serde_json::to_writer(
        &mut LimitedWriter {
            remaining: MAX_EMITTED_FACT_BYTES,
        },
        value,
    )
    .is_ok()
}

fn retained_detail_len(extra: &BTreeMap<String, Value>, key: &str) -> usize {
    extra.get(key).map_or(0, |value| match value {
        Value::Array(values) => values.len(),
        Value::Object(values) => values.len(),
        _ => 0,
    })
}

fn compact_node_fact(node: &mut Node, root: bool) {
    let mut omitted = Map::new();
    for key in ["dot_attributes", "dot_node_defaults", "dot_edge_defaults"] {
        let count = retained_detail_len(&node.extra, key);
        if count > 0 {
            omitted.insert(key.into(), Value::from(count as u64));
        }
        if node.extra.contains_key(key) {
            node.extra.insert(key.into(), Value::Object(Map::new()));
        }
    }
    if !root {
        let count = retained_detail_len(&node.extra, "dot_subgraphs");
        if count > 0 {
            omitted.insert("dot_subgraphs".into(), Value::from(count as u64));
        }
        node.extra
            .insert("dot_subgraphs".into(), Value::Array(Vec::new()));
    }
    node.extra
        .insert("dot_metadata_truncated".into(), Value::Bool(true));
    node.extra
        .insert("dot_metadata_omitted".into(), Value::Object(omitted));
}

fn compact_edge_fact(edge: &mut Edge) {
    let mut omitted = Map::new();
    for key in ["dot_occurrences", "dot_parallel_edges", "dot_attributes"] {
        let count = retained_detail_len(&edge.extra, key);
        if count > 0 {
            omitted.insert(key.into(), Value::from(count as u64));
        }
    }
    edge.extra
        .insert("dot_occurrences".into(), Value::Array(Vec::new()));
    edge.extra
        .insert("dot_parallel_edges".into(), Value::Array(Vec::new()));
    edge.extra
        .insert("dot_attributes".into(), Value::Object(Map::new()));
    edge.extra
        .insert("dot_metadata_truncated".into(), Value::Bool(true));
    edge.extra
        .insert("dot_metadata_omitted".into(), Value::Object(omitted));
}

fn mark_fact_size_truncation(root: &mut Node) {
    root.extra
        .insert("parse_status".into(), Value::String("partial".into()));
    root.extra.insert("truncated".into(), Value::Bool(true));
    let source_range = root
        .extra
        .get("source_range")
        .cloned()
        .unwrap_or(Value::Null);
    let diagnostic = json!({
        "code": "dot_fact_size_limit",
        "message": "DOT semantic detail was omitted to keep every emitted fact below its byte limit",
        "source_range": source_range,
    });
    let diagnostics = root
        .extra
        .entry("dot_diagnostics".into())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(diagnostics) = diagnostics
        && !diagnostics
            .iter()
            .any(|entry| entry["code"] == "dot_fact_size_limit")
    {
        diagnostics.push(diagnostic);
    }
}

/// Enforce the downstream graph staging layer's single-fact ceiling against
/// the exact serialized representation. Counts and identities remain intact;
/// only bulky replay/detail collections are omitted, with a root diagnostic
/// making the resulting semantic extraction explicitly partial.
fn enforce_emitted_fact_limits(extraction: &mut Extraction) {
    let mut truncated = false;
    for node in extraction.nodes.iter_mut().skip(1) {
        if !serialized_fact_fits(node) {
            compact_node_fact(node, false);
            truncated = true;
        }
    }
    for edge in &mut extraction.edges {
        if !serialized_fact_fits(edge) {
            compact_edge_fact(edge);
            truncated = true;
        }
    }
    if let Some(root) = extraction.nodes.first_mut() {
        if !serialized_fact_fits(root) {
            compact_node_fact(root, true);
            truncated = true;
        }
        if truncated {
            mark_fact_size_truncation(root);
        }
        if !serialized_fact_fits(root) {
            // Existing bounded diagnostics can collectively occupy the final
            // headroom. Retain the size-limit diagnostic as the truthful one.
            root.extra
                .insert("dot_diagnostics".into(), Value::Array(Vec::new()));
            compact_node_fact(root, true);
            mark_fact_size_truncation(root);
        }
    }
    // Mandatory fields are bounded by MAX_SOURCE_FILE_BYTES/MAX_TOKEN_BYTES;
    // after compaction, every fact therefore has a small fixed upper bound.
    debug_assert!(extraction.nodes.iter().all(serialized_fact_fits));
    debug_assert!(extraction.edges.iter().all(serialized_fact_fits));
}

fn port_cost(port: &Port) -> usize {
    port.port
        .as_ref()
        .map_or(0, String::len)
        .saturating_add(port.compass.as_ref().map_or(0, String::len))
        .saturating_mul(2)
        .saturating_add(128)
}

/// Merge in source order while keeping every retained attribute map bounded.
/// Returns true when a new key had to be omitted.
fn merge_attrs_bounded(target: &mut Attrs, incoming: &Attrs) -> bool {
    let mut truncated = false;
    for (key, value) in incoming {
        if target.contains_key(key) || target.len() < MAX_ATTRS {
            target.insert(key.clone(), value.clone());
        } else {
            truncated = true;
        }
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &[u8]) -> Extraction {
        extract_dot_bytes("limits.dot", source).expect("bounded DOT extraction")
    }

    fn root(extraction: &Extraction) -> &Node {
        extraction
            .nodes
            .first()
            .expect("DOT extraction root is admitted outside a managed budget")
    }

    fn status(extraction: &Extraction) -> Option<&str> {
        root(extraction)
            .extra
            .get("parse_status")
            .and_then(Value::as_str)
    }

    #[test]
    fn source_larger_than_the_hard_byte_ceiling_is_rejected_before_parsing() {
        let source = vec![b' '; MAX_BYTES + 1];
        let error = extract_dot_bytes("oversized.dot", &source).expect_err("oversized source");
        assert!(error.to_string().contains("byte limit"));
    }

    #[test]
    fn token_and_nesting_limits_produce_truthful_partial_results() {
        let long_id = "a".repeat(MAX_TOKEN_BYTES + 1);
        let token_limited = parse(format!("digraph {{ \"{long_id}\" }}").as_bytes());
        assert_eq!(status(&token_limited), Some("partial"));
        assert_eq!(root(&token_limited).extra["truncated"], true);

        let mut nested = String::from("digraph {");
        for depth in 0..=MAX_NESTING {
            nested.push_str(&format!("subgraph s{depth} {{"));
        }
        nested.push('a');
        for _ in 0..=MAX_NESTING {
            nested.push('}');
        }
        nested.push('}');
        let nesting_limited = parse(nested.as_bytes());
        assert_eq!(status(&nesting_limited), Some("partial"));
        assert!(root(&nesting_limited).extra["dot_diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics
                .iter()
                .any(|diagnostic| { diagnostic["code"] == "dot_nesting_limit" })));
    }

    #[test]
    fn accumulated_attributes_never_bypass_the_per_map_limit() {
        let mut source = String::from("digraph {");
        for index in 0..=MAX_ATTRS {
            source.push_str(&format!("node [k{index}=v] "));
        }
        source.push_str("a }");
        let extraction = parse(source.as_bytes());
        assert_eq!(status(&extraction), Some("partial"));
        let node = extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("dot_id") == Some(&Value::String("a".into())))
            .expect("bounded node");
        assert_eq!(
            node.extra["dot_attributes"].as_object().map(Map::len),
            Some(MAX_ATTRS)
        );
    }

    #[test]
    fn global_metadata_and_endpoint_expansion_ceilings_are_enforced() {
        let mut state = DotState::new("metadata.dot", b"");
        assert!(!state.charge_metadata(MAX_RETAINED_METADATA_BYTES, Span::default()));
        assert!(state.resource_truncated);

        let mut source = String::from("digraph {");
        for _ in 0..=MAX_CHAIN_ENDPOINTS {
            source.push_str("a,");
        }
        source.push_str("a -> z }");
        let extraction = parse(source.as_bytes());
        assert_eq!(status(&extraction), Some("partial"));
        assert!(root(&extraction).extra["dot_diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics
                .iter()
                .any(|diagnostic| { diagnostic["code"] == "dot_edge_endpoint_limit" })));
    }

    #[test]
    fn bounded_recovery_is_byte_deterministic() {
        let source = b"digraph { a -> ; bad [x=]; good -> retained }";
        let first = parse(source);
        let second = parse(source);
        assert_eq!(
            serde_json::to_value(first).expect("serialize first extraction"),
            serde_json::to_value(second).expect("serialize second extraction")
        );
    }

    #[test]
    fn final_ids_preserve_legacy_compatibility_and_separate_exact_owners() {
        let extraction = parse(
            br#"digraph {
                API; "Service-A"; "quoted name";
                x; subgraph x {}
            }"#,
        );
        for dot_id in ["API", "Service-A", "quoted name", "x"] {
            let node = extraction
                .nodes
                .iter()
                .find(|node| {
                    node.extra.get("diagram_kind") == Some(&Value::String("node".into()))
                        && node.extra.get("dot_id") == Some(&Value::String(dot_id.into()))
                })
                .expect("ordinary DOT node");
            assert_eq!(
                node.id,
                make_id(&["limits", "diagram", "graphviz", dot_id]),
                "collision-free ordinary IDs retain the established Graphoxide ID"
            );
        }
        let subgraph = extraction
            .nodes
            .iter()
            .find(|node| {
                node.extra.get("diagram_kind") == Some(&Value::String("subgraph".into()))
                    && node.extra.get("dot_id") == Some(&Value::String("x".into()))
            })
            .expect("same-named subgraph");
        assert_ne!(
            subgraph.id,
            make_id(&["limits", "diagram", "graphviz", "x"])
        );

        let left = parse(br#"digraph { subgraph "Same-ID" {} "Same-ID" }"#);
        let right = parse(br#"digraph { "Same-ID" subgraph "Same-ID" {} }"#);
        let ids = |extraction: &Extraction| {
            extraction
                .nodes
                .iter()
                .filter_map(|node| {
                    Some((
                        node.extra.get("diagram_kind")?.as_str()?.to_owned(),
                        node.extra.get("dot_id")?.as_str()?.to_owned(),
                        node.id.clone(),
                    ))
                })
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(ids(&left), ids(&right));
    }

    #[test]
    fn generated_id_candidates_cannot_be_copied_by_declared_ids() {
        let generated = format!(
            "{}_{}_{}",
            make_id(&["limits", "diagram", "graphviz", "node", "a"]),
            EXACT_ID_MARKER,
            blake3::hash(b"node\0a").to_hex()
        );
        let copied_dot_id = generated
            .strip_prefix("limits_diagram_graphviz_")
            .expect("generated candidate shares the legacy prefix");
        let source = format!("digraph {{ a; A!; \"{copied_dot_id}\" }}");
        let extraction = parse(source.as_bytes());
        let declared = extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("dot_id") == Some(&Value::String(copied_dot_id.into())))
            .expect("declared suffix-shaped ID");
        let lowercase = extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("dot_id") == Some(&Value::String("a".into())))
            .expect("normalization-colliding node");
        assert_eq!(declared.id, generated);
        assert_ne!(lowercase.id, generated);
        assert_eq!(
            extraction
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            extraction.nodes.len()
        );
    }

    #[test]
    fn standalone_subgraph_reopens_stay_lazy_and_default_key_is_not_retained() {
        let mut source = String::from("digraph { edge [key=ignored color=blue] subgraph s {");
        for index in 0..256 {
            source.push_str(&format!("n{index};"));
        }
        source.push('}');
        for _ in 0..256 {
            source.push_str("subgraph s {}");
        }
        source.push('}');
        let mut parser = Parser::new("lazy.dot", source.as_bytes(), Encoding::Utf8);
        parser.parse_graph();
        assert_eq!(parser.endpoint_materializations, 0);
        assert!(!parser.state.root_edge_defaults.contains_key("key"));
        assert_eq!(
            parser
                .state
                .root_edge_defaults
                .get("color")
                .map(String::as_str),
            Some("blue")
        );
        assert_eq!(parser.state.status(), "complete");
    }

    #[test]
    fn node_range_covers_later_effective_attribute_updates() {
        let source = b"digraph { a; a [label=updated] }";
        let extraction = parse(source);
        let node = extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("dot_id") == Some(&Value::String("a".into())))
            .expect("updated DOT node");
        assert_eq!(node.label, "updated");
        assert_eq!(
            node.extra["source_range"]["end"]["byte"],
            Value::from(source.iter().position(|byte| *byte == b']').unwrap() + 1)
        );
    }

    #[test]
    fn extraction_round_trips_without_flattened_reserved_edge_fields() {
        let extraction = parse(b"digraph { a -> b [label=flow] }");
        let encoded = serde_json::to_vec(&extraction).expect("serialize DOT extraction");
        let decoded: Extraction =
            serde_json::from_slice(&encoded).expect("deserialize DOT extraction");
        assert_eq!(decoded.edges.len(), extraction.edges.len());
        assert_eq!(
            decoded.edges.last().map(|edge| edge.relation.as_str()),
            Some("flows_to")
        );
    }

    #[test]
    fn valid_utf8_latin1_declaration_changes_only_display_labels() {
        let extraction = parse(
            "digraph { graph [charset=latin1] \"λ\" [label=\"café\" tooltip=\"café\"] }".as_bytes(),
        );
        let node = extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("dot_id") == Some(&Value::String("λ".into())))
            .expect("raw UTF-8 DOT identity");
        assert_eq!(node.label, "cafÃ©");
        assert_eq!(node.extra["dot_id"], "λ");
        assert_eq!(node.extra["dot_attributes"]["label"], "café");
        assert_eq!(node.extra["dot_attributes"]["tooltip"], "café");
    }

    #[test]
    fn repeated_edge_detail_stays_below_downstream_fact_limit() {
        let mut source = String::from("digraph {");
        for _ in 0..10_000 {
            source.push_str("a->b;");
        }
        source.push('}');
        let extraction = parse(source.as_bytes());
        assert_eq!(status(&extraction), Some("partial"));
        assert!(root(&extraction).extra["dot_diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics
                .iter()
                .any(|diagnostic| { diagnostic["code"] == "dot_edge_metadata_limit" })));
        for edge in &extraction.edges {
            assert!(
                serde_json::to_vec(edge)
                    .expect("serialize bounded DOT edge")
                    .len()
                    < 1024 * 1024
            );
        }
    }

    #[test]
    fn final_named_subgraph_membership_is_used_for_every_rhs_reference() {
        let extraction = parse(b"digraph { subgraph s {a} -> subgraph s {b} }");
        assert_eq!(status(&extraction), Some("complete"));
        let pairs = extraction
            .edges
            .iter()
            .filter(|edge| edge.relation == "flows_to")
            .map(|edge| {
                (
                    edge.extra["source_id"].as_str().unwrap().to_owned(),
                    edge.extra["target_id"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            pairs,
            BTreeSet::from([
                ("a".into(), "a".into()),
                ("a".into(), "b".into()),
                ("b".into(), "a".into()),
                ("b".into(), "b".into()),
            ])
        );
    }

    #[test]
    fn hash_comments_are_ignored_after_non_whitespace_tokens() {
        let extraction = parse(b"digraph { a # discard the rest of this line\n -> b }");
        assert_eq!(status(&extraction), Some("complete"));
        assert!(extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "flows_to"));
    }

    #[test]
    fn root_node_and_membership_detail_never_exceed_single_fact_ceiling() {
        let value = "v".repeat(2_200);
        let mut node_source = String::from("digraph { a [");
        for index in 0..MAX_ATTRS {
            node_source.push_str(&format!("k{index}=\"{value}\" "));
        }
        node_source.push_str("] }");

        let root_value = "v".repeat(750);
        let mut root_source = String::from("digraph {");
        for kind in ["graph", "node", "edge"] {
            root_source.push_str(kind);
            root_source.push('[');
            for index in 0..MAX_ATTRS {
                root_source.push_str(&format!("{kind}{index}=\"{root_value}\" "));
            }
            root_source.push(']');
        }
        root_source.push('}');

        let long_prefix = "s".repeat(2_950);
        let mut membership_source = String::from("digraph {");
        for index in 0..180 {
            membership_source.push_str(&format!("subgraph \"{long_prefix}_{index}\" {{ shared }}"));
        }
        membership_source.push('}');

        for extraction in [
            parse(node_source.as_bytes()),
            parse(root_source.as_bytes()),
            parse(membership_source.as_bytes()),
        ] {
            assert_eq!(status(&extraction), Some("partial"));
            assert!(root(&extraction).extra["dot_diagnostics"]
                .as_array()
                .is_some_and(|diagnostics| diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic["code"] == "dot_fact_size_limit")));
            assert!(extraction.nodes.iter().all(serialized_fact_fits));
            assert!(extraction.edges.iter().all(serialized_fact_fits));
            assert!(extraction.nodes.iter().all(|node| {
                serde_json::to_vec(node).is_ok_and(|bytes| bytes.len() < 1024 * 1024)
            }));
            assert!(extraction.edges.iter().all(|edge| {
                serde_json::to_vec(edge).is_ok_and(|bytes| bytes.len() < 1024 * 1024)
            }));
        }
    }
}
