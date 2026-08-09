//! Bounded, byte-only PDF text and page extraction.
//!
//! This module deliberately supports a conservative PDF subset: classic
//! cross-reference tables whose page/content/font objects are direct objects.
//! It never renders pages, follows actions, opens attachments, performs I/O,
//! or delegates to an external program. Unsupported representations fail
//! closed before semantic facts are published.

use flate2::bufread::ZlibDecoder;
use graphoxide_core::{make_id, sanitize_metadata_string, Confidence, Edge, Extraction, Node};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    ops::Range,
    path::Path,
};

const MIB: usize = 1024 * 1024;
const FIXED_ALLOWANCE_BYTES: usize = 64 * 1024;
const SOURCE_SCRATCH_MULTIPLIER: usize = 16;
const RETAINED_BYTES_PER_FACT: usize = 2 * 1024;
const DECODE_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ObjectId {
    number: u32,
    generation: u16,
}

#[derive(Debug, Clone, PartialEq)]
enum PdfValue {
    Null,
    Boolean,
    Integer(i64),
    Real,
    Name(Vec<u8>),
    String(Vec<u8>),
    Array(Vec<PdfValue>),
    Dictionary(BTreeMap<Vec<u8>, PdfValue>),
    Reference(ObjectId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamFilter {
    Raw,
    Flate,
}

#[derive(Debug, Clone)]
struct StreamSpec {
    encoded: Range<usize>,
    filter: StreamFilter,
}

#[derive(Debug, Clone)]
struct PdfObject {
    value: PdfValue,
    stream: Option<StreamSpec>,
}

#[derive(Debug)]
struct ParsedPdf {
    objects: BTreeMap<ObjectId, PdfObject>,
    trailer: BTreeMap<Vec<u8>, PdfValue>,
}

#[derive(Debug, Clone, Copy)]
struct XrefEntry {
    id: ObjectId,
    offset: usize,
}

#[derive(Debug)]
struct XrefTable {
    entries: Vec<XrefEntry>,
    trailer: BTreeMap<Vec<u8>, PdfValue>,
    xref_offset: usize,
    counters: ParseCounters,
}

/// Explicit ceilings for one PDF parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PdfLimits {
    pub(crate) max_input_bytes: usize,
    pub(crate) max_objects: usize,
    pub(crate) max_pages: usize,
    pub(crate) max_page_tree_depth: usize,
    pub(crate) max_reference_depth: usize,
    pub(crate) max_object_nesting: usize,
    pub(crate) max_tokens: usize,
    pub(crate) max_tokens_per_object: usize,
    pub(crate) max_container_entries: usize,
    pub(crate) max_container_entries_per_object: usize,
    pub(crate) max_streams: usize,
    pub(crate) max_stream_input_bytes: usize,
    pub(crate) max_stream_decoded_bytes: usize,
    pub(crate) max_total_decoded_bytes: usize,
    pub(crate) max_expansion_ratio: usize,
    pub(crate) max_content_operations: usize,
    pub(crate) max_content_nesting: usize,
    pub(crate) max_text_bytes_per_page: usize,
    pub(crate) max_total_text_bytes: usize,
    pub(crate) max_metadata_bytes: usize,
    pub(crate) max_facts: usize,
}

impl Default for PdfLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * MIB,
            max_objects: 16 * 1024,
            max_pages: 512,
            max_page_tree_depth: 32,
            max_reference_depth: 32,
            max_object_nesting: 32,
            max_tokens: 262_144,
            max_tokens_per_object: 65_536,
            max_container_entries: 131_072,
            max_container_entries_per_object: 32_768,
            max_streams: 2_048,
            max_stream_input_bytes: 4 * MIB,
            max_stream_decoded_bytes: 4 * MIB,
            max_total_decoded_bytes: 16 * MIB,
            max_expansion_ratio: 64,
            max_content_operations: 100_000,
            max_content_nesting: 32,
            // A page fact remains comfortably below the graph's one-MiB
            // serialized-fact boundary even after JSON escaping/attributes.
            max_text_bytes_per_page: 256 * 1024,
            max_total_text_bytes: 4 * MIB,
            max_metadata_bytes: 64 * 1024,
            max_facts: 1_025,
        }
    }
}

impl PdfLimits {
    /// Tighten PDF-specific retained/decode ceilings to one isolated parser
    /// allowance. The generic parser plan independently performs source x16
    /// admission and installs fact credits; this method keeps the PDF's own
    /// scratch classes within that same exact allowance.
    pub(crate) fn for_parser_allowance(allowance_bytes: usize, source_len: usize) -> Option<Self> {
        let mut limits = Self::default();
        if source_len > limits.max_input_bytes {
            return None;
        }
        let source_scratch = source_len
            .checked_mul(SOURCE_SCRATCH_MULTIPLIER)?
            .checked_add(FIXED_ALLOWANCE_BYTES)?;
        let available = allowance_bytes.checked_sub(source_scratch)?;
        let decoded = limits.max_total_decoded_bytes.min(available / 2);
        let text = limits.max_total_text_bytes.min(available / 4);
        let retained = available.checked_sub(decoded)?.checked_sub(text)?;
        let facts = limits.max_facts.min(retained / RETAINED_BYTES_PER_FACT);
        if decoded < 64 * 1024 || text < 4 * 1024 || facts < 3 {
            return None;
        }
        limits.max_total_decoded_bytes = decoded;
        limits.max_stream_decoded_bytes = limits.max_stream_decoded_bytes.min(decoded);
        limits.max_total_text_bytes = text;
        limits.max_text_bytes_per_page = limits.max_text_bytes_per_page.min(text);
        limits.max_metadata_bytes = limits.max_metadata_bytes.min(text / 4);
        limits.max_facts = facts;
        limits.max_pages = limits.max_pages.min(facts.saturating_sub(1) / 2);
        (limits.max_pages > 0).then_some(limits)
    }
}

/// Stable, non-source-bearing rejection classes for adapter diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PdfError {
    #[error("PDF input exceeds its byte ceiling")]
    InputLimit,
    #[error("PDF parsing was cancelled")]
    Cancelled,
    #[error("PDF header is malformed")]
    InvalidHeader,
    #[error("PDF requires an unsupported cross-reference representation")]
    UnsupportedXref,
    #[error("incrementally updated PDFs are unsupported")]
    UnsupportedIncremental,
    #[error("PDF object streams are unsupported")]
    UnsupportedObjectStream,
    #[error("encrypted PDFs are unsupported")]
    Encrypted,
    #[error("active or externally resolved PDF content is unsupported")]
    ActiveContent,
    #[error("PDF syntax is malformed")]
    Malformed,
    #[error("PDF object ceiling was exceeded")]
    ObjectLimit,
    #[error("PDF token or container ceiling was exceeded")]
    TokenLimit,
    #[error("PDF nesting ceiling was exceeded")]
    NestingLimit,
    #[error("PDF page ceiling was exceeded")]
    PageLimit,
    #[error("PDF reference ceiling or cycle was encountered")]
    ReferenceLimit,
    #[error("PDF stream boundary is invalid")]
    InvalidStream,
    #[error("PDF stream representation is unsupported")]
    UnsupportedFilter,
    #[error("PDF decoded-stream ceiling was exceeded")]
    DecompressionLimit,
    #[error("PDF stream expansion ratio was exceeded")]
    ExpansionRatioLimit,
    #[error("PDF content operation ceiling was exceeded")]
    ContentLimit,
    #[error("PDF inline images are unsupported")]
    InlineImage,
    #[error("PDF font representation is unsupported")]
    UnsupportedFont,
    #[error("PDF text ceiling was exceeded")]
    TextLimit,
    #[error("PDF metadata ceiling was exceeded")]
    MetadataLimit,
    #[error("PDF fact ceiling was exceeded")]
    FactLimit,
}

impl PdfError {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::InputLimit => "pdf_input_limit",
            Self::Cancelled => "cancelled",
            Self::InvalidHeader => "pdf_invalid_header",
            Self::UnsupportedXref => "pdf_unsupported_xref",
            Self::UnsupportedIncremental => "pdf_incremental_unsupported",
            Self::UnsupportedObjectStream => "pdf_object_stream_unsupported",
            Self::Encrypted => "pdf_encrypted",
            Self::ActiveContent => "pdf_active_content_unsupported",
            Self::Malformed => "pdf_malformed",
            Self::ObjectLimit => "pdf_object_limit",
            Self::TokenLimit => "pdf_token_limit",
            Self::NestingLimit => "pdf_nesting_limit",
            Self::PageLimit => "pdf_page_limit",
            Self::ReferenceLimit => "pdf_reference_limit",
            Self::InvalidStream => "pdf_stream_invalid",
            Self::UnsupportedFilter => "pdf_filter_unsupported",
            Self::DecompressionLimit => "pdf_decompression_limit",
            Self::ExpansionRatioLimit => "pdf_expansion_ratio_limit",
            Self::ContentLimit => "pdf_content_limit",
            Self::InlineImage => "pdf_inline_image_unsupported",
            Self::UnsupportedFont => "pdf_font_unsupported",
            Self::TextLimit => "pdf_text_limit",
            Self::MetadataLimit => "pdf_metadata_limit",
            Self::FactLimit => "pdf_fact_limit",
        }
    }
}

/// Extract a bounded semantic page graph from ready PDF bytes.
pub(crate) fn extract_pdf_bytes(
    path: &Path,
    source_file: &str,
    source: &[u8],
    limits: PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Extraction, PdfError> {
    if source.len() > limits.max_input_bytes {
        return Err(PdfError::InputLimit);
    }
    check_cancelled(cancelled)?;
    validate_pdf_header(source)?;
    let xref = parse_classic_xref(source, &limits, cancelled)?;
    let parsed = parse_indirect_objects(source, xref, &limits, cancelled)?;
    let page_ids = collect_page_ids(&parsed, &limits, cancelled)?;
    let required_facts = page_ids
        .len()
        .checked_mul(2)
        .and_then(|facts| facts.checked_add(1))
        .ok_or(PdfError::FactLimit)?;
    if required_facts > limits.max_facts {
        return Err(PdfError::FactLimit);
    }

    let mut decode = DecodeBudget::new(limits);
    let mut content_budget = ContentBudget::default();
    let mut page_text = Vec::new();
    page_text
        .try_reserve_exact(page_ids.len())
        .map_err(|_| PdfError::PageLimit)?;
    for (page_index, page_id) in page_ids.iter().copied().enumerate() {
        check_cancelled(cancelled)?;
        let content_ids = page_content_ids(&parsed, page_id, &limits)?;
        let fonts = page_font_encodings(&parsed, page_id, &limits)?;
        let text = extract_page_text(
            PageTextRequest {
                source,
                parsed: &parsed,
                content_ids: &content_ids,
                fonts: &fonts,
                limits: &limits,
                cancelled,
            },
            &mut decode,
            &mut content_budget,
        )?;
        page_text.push((page_index + 1, text));
    }

    let metadata = extract_metadata(&parsed, &limits, decode.total_text_bytes)?;
    let final_text_bytes = decode
        .total_text_bytes
        .checked_add(metadata.total_bytes)
        .ok_or(PdfError::TextLimit)?;
    if final_text_bytes > limits.max_total_text_bytes {
        return Err(PdfError::TextLimit);
    }
    check_cancelled(cancelled)?;

    // This is intentionally the last fallible admission check. Once credits
    // are consumed, materialization below contains no parser/cancellation path
    // that can fail and strand the adapter's rejection-root credit.
    if !crate::parser_budget::try_reserve_facts(required_facts) {
        return Err(PdfError::FactLimit);
    }
    Ok(materialize_extraction(
        path,
        source_file,
        page_text,
        metadata,
        decode.decoded_streams,
        decode.total_decoded_bytes,
        final_text_bytes,
    ))
}

/// Deterministically join bounded page text for compatibility callers.
pub(crate) fn extraction_text(extraction: &Extraction) -> String {
    let mut pages = extraction
        .nodes
        .iter()
        .filter_map(|node| {
            let page = node.extra.get("page_number")?.as_u64()?;
            let text = node.extra.get("text")?.as_str()?;
            Some((page, node.id.as_str(), text))
        })
        .collect::<Vec<_>>();
    pages.sort_unstable_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
    pages
        .into_iter()
        .map(|(_, _, text)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_cancelled(cancelled: Option<&dyn Fn() -> bool>) -> Result<(), PdfError> {
    if cancelled.is_some_and(|check| check()) {
        Err(PdfError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Debug, Default)]
struct ParseCounters {
    tokens: usize,
    container_entries: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Integer(i64),
    Real,
    Name(Vec<u8>),
    String(Vec<u8>),
    ArrayStart,
    ArrayEnd,
    DictionaryStart,
    DictionaryEnd,
    Keyword(Vec<u8>),
}

struct ValueParser<'a, 'b> {
    source: &'a [u8],
    position: usize,
    end: usize,
    limits: &'b PdfLimits,
    global: &'b mut ParseCounters,
    cancelled: Option<&'b dyn Fn() -> bool>,
    local_tokens: usize,
    local_entries: usize,
}

impl<'a, 'b> ValueParser<'a, 'b> {
    fn new(
        source: &'a [u8],
        position: usize,
        end: usize,
        limits: &'b PdfLimits,
        global: &'b mut ParseCounters,
        cancelled: Option<&'b dyn Fn() -> bool>,
    ) -> Self {
        Self {
            source,
            position,
            end,
            limits,
            global,
            cancelled,
            local_tokens: 0,
            local_entries: 0,
        }
    }

    fn next(&mut self) -> Result<Token, PdfError> {
        if self.global.tokens.is_multiple_of(1_024) {
            check_cancelled(self.cancelled)?;
        }
        let (token, end) = lex_token_at(self.source, self.position, self.end, self.limits)?;
        self.position = end;
        self.local_tokens = self
            .local_tokens
            .checked_add(1)
            .ok_or(PdfError::TokenLimit)?;
        self.global.tokens = self
            .global
            .tokens
            .checked_add(1)
            .ok_or(PdfError::TokenLimit)?;
        if self.local_tokens > self.limits.max_tokens_per_object
            || self.global.tokens > self.limits.max_tokens
        {
            return Err(PdfError::TokenLimit);
        }
        Ok(token)
    }

    fn peek(&self) -> Result<Token, PdfError> {
        lex_token_at(self.source, self.position, self.end, self.limits).map(|value| value.0)
    }

    fn add_entry(&mut self) -> Result<(), PdfError> {
        self.local_entries = self
            .local_entries
            .checked_add(1)
            .ok_or(PdfError::TokenLimit)?;
        self.global.container_entries = self
            .global
            .container_entries
            .checked_add(1)
            .ok_or(PdfError::TokenLimit)?;
        if self.local_entries > self.limits.max_container_entries_per_object
            || self.global.container_entries > self.limits.max_container_entries
        {
            return Err(PdfError::TokenLimit);
        }
        Ok(())
    }

    fn parse_value(&mut self, depth: usize) -> Result<PdfValue, PdfError> {
        if depth > self.limits.max_object_nesting {
            return Err(PdfError::NestingLimit);
        }
        match self.next()? {
            Token::Integer(number) => {
                let after_number = self.position;
                if let Ok((Token::Integer(generation), after_generation)) =
                    lex_token_at(self.source, after_number, self.end, self.limits)
                    && let Ok((Token::Keyword(reference), _)) =
                        lex_token_at(self.source, after_generation, self.end, self.limits)
                    && reference == b"R"
                {
                    let _ = self.next()?;
                    let _ = self.next()?;
                    let number = u32::try_from(number).map_err(|_| PdfError::Malformed)?;
                    let generation = u16::try_from(generation).map_err(|_| PdfError::Malformed)?;
                    return Ok(PdfValue::Reference(ObjectId { number, generation }));
                }
                Ok(PdfValue::Integer(number))
            }
            Token::Real => Ok(PdfValue::Real),
            Token::Name(name) => Ok(PdfValue::Name(name)),
            Token::String(value) => Ok(PdfValue::String(value)),
            Token::Keyword(keyword) if keyword == b"null" => Ok(PdfValue::Null),
            Token::Keyword(keyword) if matches!(keyword.as_slice(), b"true" | b"false") => {
                Ok(PdfValue::Boolean)
            }
            Token::ArrayStart => {
                let mut values = Vec::new();
                loop {
                    if self.peek()? == Token::ArrayEnd {
                        let _ = self.next()?;
                        break;
                    }
                    self.add_entry()?;
                    values.try_reserve(1).map_err(|_| PdfError::TokenLimit)?;
                    values.push(self.parse_value(depth + 1)?);
                }
                Ok(PdfValue::Array(values))
            }
            Token::DictionaryStart => {
                let mut values = BTreeMap::new();
                loop {
                    if self.peek()? == Token::DictionaryEnd {
                        let _ = self.next()?;
                        break;
                    }
                    let Token::Name(key) = self.next()? else {
                        return Err(PdfError::Malformed);
                    };
                    self.add_entry()?;
                    let value = self.parse_value(depth + 1)?;
                    if values.insert(key, value).is_some() {
                        return Err(PdfError::Malformed);
                    }
                }
                Ok(PdfValue::Dictionary(values))
            }
            _ => Err(PdfError::Malformed),
        }
    }
}

fn lex_token_at(
    source: &[u8],
    position: usize,
    end: usize,
    limits: &PdfLimits,
) -> Result<(Token, usize), PdfError> {
    lex_token_at_with_byte_limit(source, position, end, limits, limits.max_input_bytes)
}

fn lex_token_at_with_byte_limit(
    source: &[u8],
    position: usize,
    end: usize,
    limits: &PdfLimits,
    max_token_bytes: usize,
) -> Result<(Token, usize), PdfError> {
    let position = skip_space_and_comments(source, position, end);
    if position >= end {
        return Err(PdfError::Malformed);
    }
    let byte = source[position];
    match byte {
        b'[' => Ok((Token::ArrayStart, position + 1)),
        b']' => Ok((Token::ArrayEnd, position + 1)),
        b'<' if position + 1 < end && source[position + 1] == b'<' => {
            Ok((Token::DictionaryStart, position + 2))
        }
        b'>' if position + 1 < end && source[position + 1] == b'>' => {
            Ok((Token::DictionaryEnd, position + 2))
        }
        b'/' => lex_name(source, position, end, max_token_bytes),
        b'(' => lex_literal_string(source, position, end, limits, max_token_bytes),
        b'<' => lex_hex_string(source, position, end, max_token_bytes),
        b')' | b'>' | b'{' | b'}' => Err(PdfError::Malformed),
        _ => lex_bare_token(source, position, end, max_token_bytes),
    }
}

fn lex_name(
    source: &[u8],
    start: usize,
    end: usize,
    max_token_bytes: usize,
) -> Result<(Token, usize), PdfError> {
    let mut position = start + 1;
    let mut name = Vec::new();
    while position < end && !is_pdf_delimiter(source[position]) {
        let value = if source[position] == b'#' {
            if position.checked_add(2).is_none_or(|last| last >= end) {
                return Err(PdfError::Malformed);
            }
            let high = source[position + 1];
            let low = source[position + 2];
            position += 3;
            (hex_digit(high).ok_or(PdfError::Malformed)? << 4)
                | hex_digit(low).ok_or(PdfError::Malformed)?
        } else {
            let value = source[position];
            position += 1;
            value
        };
        name.try_reserve(1).map_err(|_| PdfError::TokenLimit)?;
        name.push(value);
        if name.len() > max_token_bytes {
            return Err(PdfError::ContentLimit);
        }
    }
    reject_unsafe_name(&name)?;
    Ok((Token::Name(name), position))
}

fn reject_unsafe_name(name: &[u8]) -> Result<(), PdfError> {
    match name {
        b"Encrypt" => Err(PdfError::Encrypted),
        b"ObjStm" => Err(PdfError::UnsupportedObjectStream),
        b"XRef" | b"XRefStm" => Err(PdfError::UnsupportedXref),
        b"Prev" => Err(PdfError::UnsupportedIncremental),
        b"ToUnicode" => Err(PdfError::UnsupportedFont),
        b"JavaScript" | b"JS" | b"Launch" | b"EmbeddedFile" | b"EmbeddedFiles" | b"Filespec"
        | b"GoToR" | b"SubmitForm" | b"ImportData" | b"RichMedia" | b"XFA" | b"OpenAction"
        | b"AA" | b"URI" => Err(PdfError::ActiveContent),
        _ => Ok(()),
    }
}

fn lex_literal_string(
    source: &[u8],
    start: usize,
    end: usize,
    limits: &PdfLimits,
    max_token_bytes: usize,
) -> Result<(Token, usize), PdfError> {
    let mut position = start + 1;
    let mut depth = 1_usize;
    let mut value = Vec::new();
    while position < end {
        let byte = source[position];
        position += 1;
        match byte {
            b'(' => {
                depth = depth.checked_add(1).ok_or(PdfError::NestingLimit)?;
                if depth > limits.max_object_nesting {
                    return Err(PdfError::NestingLimit);
                }
                value.push(byte);
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((Token::String(value), position));
                }
                value.push(byte);
            }
            b'\\' => {
                if position >= end {
                    return Err(PdfError::Malformed);
                }
                let escaped = source[position];
                position += 1;
                match escaped {
                    b'n' => value.push(b'\n'),
                    b'r' => value.push(b'\r'),
                    b't' => value.push(b'\t'),
                    b'b' => value.push(8),
                    b'f' => value.push(12),
                    b'(' | b')' | b'\\' => value.push(escaped),
                    b'\r' => {
                        if position < end && source[position] == b'\n' {
                            position += 1;
                        }
                    }
                    b'\n' => {}
                    b'0'..=b'7' => {
                        let mut octal = u16::from(escaped - b'0');
                        for _ in 0..2 {
                            if position >= end {
                                break;
                            }
                            let next = source[position];
                            if !matches!(next, b'0'..=b'7') {
                                break;
                            }
                            octal = octal * 8 + u16::from(next - b'0');
                            position += 1;
                        }
                        value.push((octal & 0xff) as u8);
                    }
                    _ => value.push(escaped),
                }
            }
            _ => value.push(byte),
        }
        if value.len() > max_token_bytes {
            return Err(PdfError::ContentLimit);
        }
    }
    Err(PdfError::Malformed)
}

fn lex_hex_string(
    source: &[u8],
    start: usize,
    end: usize,
    max_token_bytes: usize,
) -> Result<(Token, usize), PdfError> {
    let mut position = start + 1;
    let mut value = Vec::new();
    let mut pending = None;
    while position < end {
        let byte = source[position];
        position += 1;
        if byte == b'>' {
            if let Some(high) = pending {
                value.push(high << 4);
            }
            return Ok((Token::String(value), position));
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        let digit = hex_digit(byte).ok_or(PdfError::Malformed)?;
        if let Some(high) = pending.take() {
            value.push((high << 4) | digit);
        } else {
            pending = Some(digit);
        }
        if value.len() > max_token_bytes {
            return Err(PdfError::ContentLimit);
        }
    }
    Err(PdfError::Malformed)
}

fn lex_bare_token(
    source: &[u8],
    start: usize,
    end: usize,
    max_token_bytes: usize,
) -> Result<(Token, usize), PdfError> {
    let mut position = start;
    while position < end && !is_pdf_delimiter(source[position]) {
        position += 1;
    }
    if position == start {
        return Err(PdfError::Malformed);
    }
    let bytes = &source[start..position];
    if bytes.len() > max_token_bytes {
        return Err(PdfError::ContentLimit);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        if let Ok(integer) = text.parse::<i64>() {
            return Ok((Token::Integer(integer), position));
        }
        if (text.contains('.') || text.starts_with('+') || text.starts_with('-'))
            && text.parse::<f64>().is_ok_and(f64::is_finite)
        {
            return Ok((Token::Real, position));
        }
    }
    Ok((Token::Keyword(bytes.to_vec()), position))
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_pdf_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            0 | b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

fn skip_space_and_comments(source: &[u8], mut position: usize, end: usize) -> usize {
    loop {
        while position < end && (source[position].is_ascii_whitespace() || source[position] == 0) {
            position += 1;
        }
        if position >= end || source[position] != b'%' {
            return position;
        }
        while position < end && !matches!(source[position], b'\r' | b'\n') {
            position += 1;
        }
    }
}

fn skip_pdf_whitespace(source: &[u8], mut position: usize, end: usize) -> usize {
    while position < end && (source[position].is_ascii_whitespace() || source[position] == 0) {
        position += 1;
    }
    position
}

fn validate_pdf_header(source: &[u8]) -> Result<(), PdfError> {
    if source.len() < 9 || !source.starts_with(b"%PDF-") {
        return Err(PdfError::InvalidHeader);
    }
    let major = source[5];
    let minor = source[7];
    if !major.is_ascii_digit() || source[6] != b'.' || !minor.is_ascii_digit() {
        return Err(PdfError::InvalidHeader);
    }
    if !matches!((major, minor), (b'1', b'0'..=b'7') | (b'2', b'0')) {
        return Err(PdfError::InvalidHeader);
    }
    if !matches!(source[8], b'\r' | b'\n') {
        return Err(PdfError::InvalidHeader);
    }
    let after_header = consume_line_ending(source, 8, source.len());
    if after_header >= source.len() {
        return Err(PdfError::InvalidHeader);
    }
    // A binary marker, when present, must be a complete comment on the line
    // immediately following the header. It is never interpreted as syntax.
    if source[after_header] == b'%' {
        let marker_end = source[after_header..]
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
            .map(|offset| after_header + offset)
            .ok_or(PdfError::InvalidHeader)?;
        if marker_end == after_header + 1 {
            return Err(PdfError::InvalidHeader);
        }
    }
    Ok(())
}

fn parse_classic_xref(
    source: &[u8],
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<XrefTable, PdfError> {
    let startxref_position =
        unique_line_keyword_offset(source, b"startxref", cancelled)?.ok_or(PdfError::Malformed)?;
    unique_line_keyword_offset(source, b"%%EOF", cancelled)?.ok_or(PdfError::Malformed)?;
    let mut position = startxref_position + b"startxref".len();
    position = skip_space_and_comments(source, position, source.len());
    let (xref_offset_u64, after_offset) = parse_ascii_u64(source, position, source.len())?;
    let xref_offset = usize::try_from(xref_offset_u64).map_err(|_| PdfError::UnsupportedXref)?;
    if xref_offset >= startxref_position
        || !source
            .get(xref_offset..)
            .is_some_and(|tail| tail.starts_with(b"xref") && token_ends_at(tail, 4))
    {
        return Err(PdfError::UnsupportedXref);
    }
    let eof_position = skip_pdf_whitespace(source, after_offset, source.len());
    if !source
        .get(eof_position..)
        .is_some_and(|tail| tail.starts_with(b"%%EOF"))
    {
        return Err(PdfError::Malformed);
    }
    let after_eof = eof_position + b"%%EOF".len();
    if !source[after_eof..]
        .iter()
        .all(|byte| byte.is_ascii_whitespace())
    {
        return Err(PdfError::Malformed);
    }

    let mut entries = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut normal_offsets = BTreeSet::new();
    let mut max_seen_id = 0_u32;
    position = xref_offset + 4;
    loop {
        check_cancelled(cancelled)?;
        position = skip_blank_lines(source, position, startxref_position);
        if source
            .get(position..)
            .is_some_and(|tail| tail.starts_with(b"trailer") && token_ends_at(tail, 7))
        {
            position += 7;
            break;
        }
        let (header, next) = read_line(source, position, startxref_position)?;
        position = next;
        let mut fields = header
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty());
        let (Some(first), Some(count), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err(PdfError::Malformed);
        };
        let first = parse_decimal_field(first)?;
        let count = parse_decimal_field(count)?;
        let end_id = first.checked_add(count).ok_or(PdfError::ObjectLimit)?;
        if count == 0 || end_id > limits.max_objects as u64 {
            return Err(PdfError::ObjectLimit);
        }
        for index in 0..count {
            if index % 1_024 == 0 {
                check_cancelled(cancelled)?;
            }
            let (row, next) = read_line(source, position, startxref_position)?;
            position = next;
            let mut fields = row
                .split(u8::is_ascii_whitespace)
                .filter(|field| !field.is_empty());
            let (Some(offset), Some(generation), Some(state), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(PdfError::Malformed);
            };
            if offset.len() != 10 || generation.len() != 5 || state.len() != 1 {
                return Err(PdfError::Malformed);
            }
            let offset = parse_decimal_field(offset)?;
            let generation = parse_decimal_field(generation)?;
            let number = first.checked_add(index).ok_or(PdfError::ObjectLimit)?;
            let id = ObjectId {
                number: u32::try_from(number).map_err(|_| PdfError::ObjectLimit)?,
                generation: u16::try_from(generation).map_err(|_| PdfError::Malformed)?,
            };
            if !seen_ids.insert(id.number) {
                return Err(PdfError::Malformed);
            }
            max_seen_id = max_seen_id.max(id.number);
            match state[0] {
                b'f' => {}
                b'n' => {
                    let offset = usize::try_from(offset).map_err(|_| PdfError::Malformed)?;
                    if offset == 0 || offset >= xref_offset || !normal_offsets.insert(offset) {
                        return Err(PdfError::Malformed);
                    }
                    validate_indirect_header(source, offset, xref_offset, id)?;
                    entries.try_reserve(1).map_err(|_| PdfError::ObjectLimit)?;
                    entries.push(XrefEntry { id, offset });
                    if entries.len() > limits.max_objects {
                        return Err(PdfError::ObjectLimit);
                    }
                }
                _ => return Err(PdfError::Malformed),
            }
        }
    }
    if entries.is_empty() {
        return Err(PdfError::Malformed);
    }

    let mut counters = ParseCounters::default();
    let mut trailer_parser = ValueParser::new(
        source,
        position,
        startxref_position,
        limits,
        &mut counters,
        cancelled,
    );
    let PdfValue::Dictionary(trailer) = trailer_parser.parse_value(0)? else {
        return Err(PdfError::Malformed);
    };
    if skip_space_and_comments(source, trailer_parser.position, startxref_position)
        != startxref_position
    {
        return Err(PdfError::Malformed);
    }
    let size = dictionary_integer(&trailer, b"Size")?;
    if size <= 0 {
        return Err(PdfError::Malformed);
    }
    let size = u32::try_from(size).map_err(|_| PdfError::ObjectLimit)?;
    if usize::try_from(size).unwrap_or(usize::MAX) > limits.max_objects {
        return Err(PdfError::ObjectLimit);
    }
    if size != max_seen_id.checked_add(1).ok_or(PdfError::ObjectLimit)? {
        return Err(PdfError::Malformed);
    }
    if !matches!(
        trailer.get(b"Root".as_slice()),
        Some(PdfValue::Reference(_))
    ) {
        return Err(PdfError::Malformed);
    }
    entries.sort_unstable_by_key(|entry| entry.offset);
    Ok(XrefTable {
        entries,
        trailer,
        xref_offset,
        counters,
    })
}

fn unique_line_keyword_offset(
    source: &[u8],
    keyword: &[u8],
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Option<usize>, PdfError> {
    let mut found = None;
    let mut line_start = 0;
    while line_start < source.len() {
        check_cancelled(cancelled)?;
        let mut position = line_start;
        while position < source.len() && matches!(source[position], 0 | b'\t' | 0x0c | b' ') {
            position += 1;
            if position.is_multiple_of(DECODE_CHUNK_BYTES) {
                check_cancelled(cancelled)?;
            }
        }
        if source
            .get(position..)
            .is_some_and(|tail| tail.starts_with(keyword) && token_ends_at(tail, keyword.len()))
            && found.replace(position).is_some()
        {
            return Err(PdfError::UnsupportedIncremental);
        }
        let mut line_end = position;
        while line_end < source.len() && !matches!(source[line_end], b'\r' | b'\n') {
            line_end += 1;
            if line_end.is_multiple_of(DECODE_CHUNK_BYTES) {
                check_cancelled(cancelled)?;
            }
        }
        if line_end == source.len() {
            break;
        }
        line_start = consume_line_ending(source, line_end, source.len());
    }
    Ok(found)
}

fn token_ends_at(source: &[u8], length: usize) -> bool {
    source
        .get(length)
        .is_none_or(|byte| is_pdf_delimiter(*byte))
}

fn skip_blank_lines(source: &[u8], mut position: usize, end: usize) -> usize {
    while position < end {
        let line_end = source[position..end]
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
            .map_or(end, |offset| position + offset);
        let line = &source[position..line_end];
        if !line.iter().all(u8::is_ascii_whitespace) {
            return position;
        }
        position = consume_line_ending(source, line_end, end);
    }
    position
}

fn read_line(source: &[u8], position: usize, end: usize) -> Result<(&[u8], usize), PdfError> {
    if position >= end {
        return Err(PdfError::Malformed);
    }
    let line_end = source[position..end]
        .iter()
        .position(|byte| matches!(byte, b'\r' | b'\n'))
        .map_or(end, |offset| position + offset);
    if line_end == end {
        return Err(PdfError::Malformed);
    }
    Ok((
        &source[position..line_end],
        consume_line_ending(source, line_end, end),
    ))
}

fn consume_line_ending(source: &[u8], mut position: usize, end: usize) -> usize {
    if position < end && source[position] == b'\r' {
        position += 1;
        if position < end && source[position] == b'\n' {
            position += 1;
        }
    } else if position < end && source[position] == b'\n' {
        position += 1;
    }
    position
}

fn parse_decimal_field(field: &[u8]) -> Result<u64, PdfError> {
    if field.is_empty() || !field.iter().all(u8::is_ascii_digit) {
        return Err(PdfError::Malformed);
    }
    field.iter().try_fold(0_u64, |value, byte| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*byte - b'0')))
            .ok_or(PdfError::ObjectLimit)
    })
}

fn parse_ascii_u64(source: &[u8], start: usize, end: usize) -> Result<(u64, usize), PdfError> {
    let mut position = start;
    while position < end && source[position].is_ascii_digit() {
        position += 1;
    }
    if position == start {
        return Err(PdfError::Malformed);
    }
    Ok((parse_decimal_field(&source[start..position])?, position))
}

fn validate_indirect_header(
    source: &[u8],
    offset: usize,
    end: usize,
    expected: ObjectId,
) -> Result<(), PdfError> {
    let (number, position) = parse_ascii_u64(source, offset, end)?;
    let position = skip_required_space(source, position, end)?;
    let (generation, position) = parse_ascii_u64(source, position, end)?;
    let position = skip_required_space(source, position, end)?;
    if !source
        .get(position..end)
        .is_some_and(|tail| tail.starts_with(b"obj") && token_ends_at(tail, 3))
        || number != u64::from(expected.number)
        || generation != u64::from(expected.generation)
    {
        return Err(PdfError::Malformed);
    }
    Ok(())
}

fn skip_required_space(source: &[u8], mut position: usize, end: usize) -> Result<usize, PdfError> {
    let start = position;
    while position < end && source[position].is_ascii_whitespace() {
        position += 1;
    }
    (position > start)
        .then_some(position)
        .ok_or(PdfError::Malformed)
}

fn dictionary_integer(
    dictionary: &BTreeMap<Vec<u8>, PdfValue>,
    key: &[u8],
) -> Result<i64, PdfError> {
    match dictionary.get(key) {
        Some(PdfValue::Integer(value)) => Ok(*value),
        _ => Err(PdfError::Malformed),
    }
}

fn parse_indirect_objects(
    source: &[u8],
    xref: XrefTable,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<ParsedPdf, PdfError> {
    let XrefTable {
        entries,
        trailer,
        xref_offset,
        mut counters,
    } = xref;
    let mut objects = BTreeMap::new();
    let mut total_stream_input = 0_usize;
    let mut stream_count = 0_usize;

    for (index, entry) in entries.iter().enumerate() {
        check_cancelled(cancelled)?;
        let span_end = entries
            .get(index + 1)
            .map_or(xref_offset, |next| next.offset);
        if entry.offset >= span_end {
            return Err(PdfError::Malformed);
        }
        let value_start = indirect_value_start(source, entry.offset, span_end, entry.id)?;
        let mut parser = ValueParser::new(
            source,
            value_start,
            span_end,
            limits,
            &mut counters,
            cancelled,
        );
        let value = parser.parse_value(0)?;
        let mut stream = None;
        let after_value = skip_space_and_comments(source, parser.position, span_end);
        if source
            .get(after_value..span_end)
            .is_some_and(|tail| tail.starts_with(b"stream") && token_ends_at(tail, 6))
        {
            let PdfValue::Dictionary(dictionary) = &value else {
                return Err(PdfError::InvalidStream);
            };
            parser.position = after_value;
            if parser.next()? != Token::Keyword(b"stream".to_vec()) {
                return Err(PdfError::InvalidStream);
            }
            let data_start = consume_stream_eol(source, parser.position, span_end)?;
            let length = dictionary
                .get(b"Length".as_slice())
                .ok_or(PdfError::InvalidStream)?;
            let PdfValue::Integer(length) = length else {
                // Indirect and cyclic lengths are deliberately unsupported;
                // no dependency gets a chance to cast or follow them.
                return Err(PdfError::InvalidStream);
            };
            let length = usize::try_from(*length).map_err(|_| PdfError::InvalidStream)?;
            if length > limits.max_stream_input_bytes {
                return Err(PdfError::DecompressionLimit);
            }
            total_stream_input = total_stream_input
                .checked_add(length)
                .ok_or(PdfError::DecompressionLimit)?;
            if total_stream_input > limits.max_input_bytes {
                return Err(PdfError::DecompressionLimit);
            }
            let data_end = data_start
                .checked_add(length)
                .filter(|end| *end <= span_end)
                .ok_or(PdfError::InvalidStream)?;
            let filter = stream_filter(dictionary)?;
            let after_endstream = consume_endstream(source, data_end, span_end)?;
            parser.position = after_endstream;
            let Token::Keyword(endobj) = parser.next()? else {
                return Err(PdfError::Malformed);
            };
            if endobj != b"endobj" {
                return Err(PdfError::Malformed);
            }
            if skip_space_and_comments(source, parser.position, span_end) != span_end {
                return Err(PdfError::Malformed);
            }
            stream_count = stream_count
                .checked_add(1)
                .ok_or(PdfError::DecompressionLimit)?;
            if stream_count > limits.max_streams {
                return Err(PdfError::DecompressionLimit);
            }
            stream = Some(StreamSpec {
                encoded: data_start..data_end,
                filter,
            });
        } else {
            parser.position = after_value;
            let Token::Keyword(endobj) = parser.next()? else {
                return Err(PdfError::Malformed);
            };
            if endobj != b"endobj"
                || skip_space_and_comments(source, parser.position, span_end) != span_end
            {
                return Err(PdfError::Malformed);
            }
        }
        if objects
            .insert(entry.id, PdfObject { value, stream })
            .is_some()
        {
            return Err(PdfError::Malformed);
        }
    }
    if objects.len() > limits.max_objects {
        return Err(PdfError::ObjectLimit);
    }
    Ok(ParsedPdf { objects, trailer })
}

fn indirect_value_start(
    source: &[u8],
    offset: usize,
    end: usize,
    expected: ObjectId,
) -> Result<usize, PdfError> {
    let (number, position) = parse_ascii_u64(source, offset, end)?;
    let position = skip_required_space(source, position, end)?;
    let (generation, position) = parse_ascii_u64(source, position, end)?;
    let position = skip_required_space(source, position, end)?;
    if number != u64::from(expected.number)
        || generation != u64::from(expected.generation)
        || !source
            .get(position..end)
            .is_some_and(|tail| tail.starts_with(b"obj") && token_ends_at(tail, 3))
    {
        return Err(PdfError::Malformed);
    }
    Ok(position + 3)
}

fn consume_stream_eol(source: &[u8], mut position: usize, end: usize) -> Result<usize, PdfError> {
    while position < end && matches!(source[position], b' ' | b'\t') {
        position += 1;
    }
    if position >= end || !matches!(source[position], b'\r' | b'\n') {
        return Err(PdfError::InvalidStream);
    }
    Ok(consume_line_ending(source, position, end))
}

fn consume_endstream(source: &[u8], data_end: usize, span_end: usize) -> Result<usize, PdfError> {
    for position in
        std::iter::once(data_end).chain(consume_one_line_ending(source, data_end, span_end))
    {
        if source
            .get(position..span_end)
            .is_some_and(|tail| tail.starts_with(b"endstream") && token_ends_at(tail, 9))
        {
            return Ok(position + 9);
        }
    }
    Err(PdfError::InvalidStream)
}

fn consume_one_line_ending(source: &[u8], position: usize, end: usize) -> Option<usize> {
    match source.get(position..end)? {
        [b'\r', b'\n', ..] => Some(position + 2),
        [b'\r' | b'\n', ..] => Some(position + 1),
        _ => None,
    }
}

fn stream_filter(dictionary: &BTreeMap<Vec<u8>, PdfValue>) -> Result<StreamFilter, PdfError> {
    if [b"F".as_slice(), b"FFilter", b"FDecodeParms"]
        .iter()
        .any(|key| dictionary.contains_key(*key))
    {
        // External-file streams are deliberately unsupported. Ignoring these
        // keys and indexing the inline bytes would publish text that a PDF
        // consumer does not treat as the authoritative stream contents.
        return Err(PdfError::ActiveContent);
    }
    if let Some(parameters) = dictionary.get(b"DecodeParms".as_slice())
        && !matches!(parameters, PdfValue::Null)
    {
        // Predictors and parameterized decoding are excluded. This check
        // occurs before any decoder observes attacker-controlled dimensions.
        return Err(PdfError::UnsupportedFilter);
    }
    match dictionary.get(b"Filter".as_slice()) {
        None | Some(PdfValue::Null) => Ok(StreamFilter::Raw),
        Some(PdfValue::Name(filter)) if matches!(filter.as_slice(), b"FlateDecode" | b"Fl") => {
            Ok(StreamFilter::Flate)
        }
        _ => Err(PdfError::UnsupportedFilter),
    }
}

fn resolve_reference_id(
    parsed: &ParsedPdf,
    start: ObjectId,
    limits: &PdfLimits,
) -> Result<ObjectId, PdfError> {
    let mut current = start;
    let mut seen = BTreeSet::new();
    for _ in 0..=limits.max_reference_depth {
        if !seen.insert(current) {
            return Err(PdfError::ReferenceLimit);
        }
        let object = parsed.objects.get(&current).ok_or(PdfError::Malformed)?;
        match object.value {
            PdfValue::Reference(next) => current = next,
            _ => return Ok(current),
        }
    }
    Err(PdfError::ReferenceLimit)
}

fn resolve_value<'a>(
    parsed: &'a ParsedPdf,
    value: &'a PdfValue,
    limits: &PdfLimits,
) -> Result<(Option<ObjectId>, &'a PdfValue), PdfError> {
    let PdfValue::Reference(id) = value else {
        return Ok((None, value));
    };
    let id = resolve_reference_id(parsed, *id, limits)?;
    let value = &parsed.objects.get(&id).ok_or(PdfError::Malformed)?.value;
    Ok((Some(id), value))
}

fn object_dictionary(
    parsed: &ParsedPdf,
    id: ObjectId,
) -> Result<&BTreeMap<Vec<u8>, PdfValue>, PdfError> {
    match &parsed.objects.get(&id).ok_or(PdfError::Malformed)?.value {
        PdfValue::Dictionary(dictionary) => Ok(dictionary),
        _ => Err(PdfError::Malformed),
    }
}

fn dictionary_name_is(dictionary: &BTreeMap<Vec<u8>, PdfValue>, key: &[u8], name: &[u8]) -> bool {
    matches!(dictionary.get(key), Some(PdfValue::Name(value)) if value == name)
}

fn collect_page_ids(
    parsed: &ParsedPdf,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Vec<ObjectId>, PdfError> {
    let root = parsed
        .trailer
        .get(b"Root".as_slice())
        .ok_or(PdfError::Malformed)?;
    let (Some(_catalog_id), PdfValue::Dictionary(catalog)) = resolve_value(parsed, root, limits)?
    else {
        return Err(PdfError::Malformed);
    };
    if !dictionary_name_is(catalog, b"Type", b"Catalog") {
        return Err(PdfError::Malformed);
    }
    let pages = catalog
        .get(b"Pages".as_slice())
        .ok_or(PdfError::Malformed)?;
    let (Some(pages_id), PdfValue::Dictionary(_)) = resolve_value(parsed, pages, limits)? else {
        return Err(PdfError::Malformed);
    };

    let mut stack = vec![(pages_id, 0_usize)];
    let mut seen = BTreeSet::new();
    let mut pages = Vec::new();
    while let Some((id, depth)) = stack.pop() {
        check_cancelled(cancelled)?;
        if depth > limits.max_page_tree_depth || !seen.insert(id) {
            return Err(PdfError::ReferenceLimit);
        }
        let dictionary = object_dictionary(parsed, id)?;
        if dictionary_name_is(dictionary, b"Type", b"Page") {
            pages.try_reserve(1).map_err(|_| PdfError::PageLimit)?;
            pages.push(id);
            if pages.len() > limits.max_pages {
                return Err(PdfError::PageLimit);
            }
            validate_page_parent_chain(parsed, id, pages_id, limits)?;
            continue;
        }
        if !dictionary_name_is(dictionary, b"Type", b"Pages") {
            return Err(PdfError::Malformed);
        }
        if let Some(PdfValue::Integer(count)) = dictionary.get(b"Count".as_slice())
            && (*count < 0 || usize::try_from(*count).unwrap_or(usize::MAX) > limits.max_pages)
        {
            return Err(PdfError::PageLimit);
        }
        let kids = dictionary
            .get(b"Kids".as_slice())
            .ok_or(PdfError::Malformed)?;
        let (_, PdfValue::Array(kids)) = resolve_value(parsed, kids, limits)? else {
            return Err(PdfError::Malformed);
        };
        if kids.len() > limits.max_pages {
            return Err(PdfError::PageLimit);
        }
        for kid in kids.iter().rev() {
            let (Some(kid_id), PdfValue::Dictionary(_)) = resolve_value(parsed, kid, limits)?
            else {
                return Err(PdfError::Malformed);
            };
            let kid_dictionary = object_dictionary(parsed, kid_id)?;
            let Some(PdfValue::Reference(parent)) = kid_dictionary.get(b"Parent".as_slice()) else {
                return Err(PdfError::Malformed);
            };
            if resolve_reference_id(parsed, *parent, limits)? != id {
                return Err(PdfError::Malformed);
            }
            stack.push((kid_id, depth + 1));
        }
    }
    let declared_count = dictionary_integer(object_dictionary(parsed, pages_id)?, b"Count")?;
    if declared_count < 0 || usize::try_from(declared_count).ok() != Some(pages.len()) {
        return Err(PdfError::Malformed);
    }
    Ok(pages)
}

fn validate_page_parent_chain(
    parsed: &ParsedPdf,
    page_id: ObjectId,
    pages_root_id: ObjectId,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    let mut current = page_id;
    let mut seen = BTreeSet::new();
    for _ in 0..=limits.max_page_tree_depth {
        if current == pages_root_id {
            return Ok(());
        }
        if !seen.insert(current) {
            return Err(PdfError::ReferenceLimit);
        }
        let dictionary = object_dictionary(parsed, current)?;
        let Some(PdfValue::Reference(parent)) = dictionary.get(b"Parent".as_slice()) else {
            return Err(PdfError::Malformed);
        };
        let parent = resolve_reference_id(parsed, *parent, limits)?;
        if !dictionary_name_is(object_dictionary(parsed, parent)?, b"Type", b"Pages") {
            return Err(PdfError::Malformed);
        }
        current = parent;
    }
    Err(PdfError::ReferenceLimit)
}

fn page_content_ids(
    parsed: &ParsedPdf,
    page_id: ObjectId,
    limits: &PdfLimits,
) -> Result<Vec<ObjectId>, PdfError> {
    let page = object_dictionary(parsed, page_id)?;
    let Some(contents) = page.get(b"Contents".as_slice()) else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    match contents {
        PdfValue::Reference(id) => ids.push(resolve_reference_id(parsed, *id, limits)?),
        PdfValue::Array(values) => {
            if values.len() > limits.max_streams {
                return Err(PdfError::DecompressionLimit);
            }
            ids.try_reserve_exact(values.len())
                .map_err(|_| PdfError::DecompressionLimit)?;
            for value in values {
                let PdfValue::Reference(id) = value else {
                    return Err(PdfError::InvalidStream);
                };
                ids.push(resolve_reference_id(parsed, *id, limits)?);
            }
        }
        _ => return Err(PdfError::InvalidStream),
    }
    let mut seen = BTreeSet::new();
    for id in &ids {
        if !seen.insert(*id)
            || parsed
                .objects
                .get(id)
                .and_then(|object| object.stream.as_ref())
                .is_none()
        {
            return Err(PdfError::InvalidStream);
        }
    }
    Ok(ids)
}

struct DecodeBudget {
    limits: PdfLimits,
    decoded: BTreeMap<ObjectId, Vec<u8>>,
    decoded_streams: usize,
    total_decoded_bytes: usize,
    total_text_bytes: usize,
}

#[derive(Debug, Default)]
struct ContentBudget {
    tokens: usize,
    operations: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FontEncoding {
    Standard,
    WinAnsi,
}

fn page_font_encodings(
    parsed: &ParsedPdf,
    page_id: ObjectId,
    limits: &PdfLimits,
) -> Result<BTreeMap<Vec<u8>, FontEncoding>, PdfError> {
    let mut current = page_id;
    let mut seen = BTreeSet::new();
    for _ in 0..=limits.max_reference_depth {
        if !seen.insert(current) {
            return Err(PdfError::ReferenceLimit);
        }
        let dictionary = object_dictionary(parsed, current)?;
        if let Some(resources) = dictionary.get(b"Resources".as_slice()) {
            let (_, PdfValue::Dictionary(resources)) = resolve_value(parsed, resources, limits)?
            else {
                return Err(PdfError::Malformed);
            };
            return parse_font_resources(parsed, resources, limits);
        }
        let Some(PdfValue::Reference(parent)) = dictionary.get(b"Parent".as_slice()) else {
            return Ok(BTreeMap::new());
        };
        current = resolve_reference_id(parsed, *parent, limits)?;
    }
    Err(PdfError::ReferenceLimit)
}

fn parse_font_resources(
    parsed: &ParsedPdf,
    resources: &BTreeMap<Vec<u8>, PdfValue>,
    limits: &PdfLimits,
) -> Result<BTreeMap<Vec<u8>, FontEncoding>, PdfError> {
    let Some(fonts) = resources.get(b"Font".as_slice()) else {
        return Ok(BTreeMap::new());
    };
    let (_, PdfValue::Dictionary(fonts)) = resolve_value(parsed, fonts, limits)? else {
        return Err(PdfError::UnsupportedFont);
    };
    if fonts.len() > limits.max_container_entries_per_object {
        return Err(PdfError::TokenLimit);
    }
    let mut encodings = BTreeMap::new();
    for (resource_name, font) in fonts {
        let (_, PdfValue::Dictionary(font)) = resolve_value(parsed, font, limits)? else {
            return Err(PdfError::UnsupportedFont);
        };
        if !dictionary_name_is(font, b"Type", b"Font")
            || !dictionary_name_is(font, b"Subtype", b"Type1")
            || font.contains_key(b"ToUnicode".as_slice())
        {
            return Err(PdfError::UnsupportedFont);
        }
        let Some(PdfValue::Name(base_font)) = font.get(b"BaseFont".as_slice()) else {
            return Err(PdfError::UnsupportedFont);
        };
        let encoding = match font.get(b"Encoding".as_slice()) {
            None if is_unmodified_standard_font(font, base_font) => FontEncoding::Standard,
            Some(PdfValue::Name(name))
                if name == b"StandardEncoding" && is_unmodified_standard_font(font, base_font) =>
            {
                FontEncoding::Standard
            }
            Some(PdfValue::Name(name))
                if name == b"WinAnsiEncoding"
                    && !matches!(base_font.as_slice(), b"Symbol" | b"ZapfDingbats") =>
            {
                FontEncoding::WinAnsi
            }
            _ => return Err(PdfError::UnsupportedFont),
        };
        if encodings.insert(resource_name.clone(), encoding).is_some() {
            return Err(PdfError::Malformed);
        }
    }
    Ok(encodings)
}

fn is_unmodified_standard_font(font: &BTreeMap<Vec<u8>, PdfValue>, base_font: &[u8]) -> bool {
    const LATIN_STANDARD_14: [&[u8]; 12] = [
        b"Times-Roman",
        b"Times-Bold",
        b"Times-Italic",
        b"Times-BoldItalic",
        b"Helvetica",
        b"Helvetica-Bold",
        b"Helvetica-Oblique",
        b"Helvetica-BoldOblique",
        b"Courier",
        b"Courier-Bold",
        b"Courier-Oblique",
        b"Courier-BoldOblique",
    ];
    LATIN_STANDARD_14.contains(&base_font)
        && [
            b"FirstChar".as_slice(),
            b"LastChar",
            b"Widths",
            b"FontDescriptor",
        ]
        .iter()
        .all(|key| !font.contains_key(*key))
}

impl DecodeBudget {
    fn new(limits: PdfLimits) -> Self {
        Self {
            limits,
            decoded: BTreeMap::new(),
            decoded_streams: 0,
            total_decoded_bytes: 0,
            total_text_bytes: 0,
        }
    }

    fn stream<'a>(
        &'a mut self,
        source: &[u8],
        parsed: &ParsedPdf,
        id: ObjectId,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<&'a [u8], PdfError> {
        if !self.decoded.contains_key(&id) {
            check_cancelled(cancelled)?;
            let stream = parsed
                .objects
                .get(&id)
                .and_then(|object| object.stream.as_ref())
                .ok_or(PdfError::InvalidStream)?;
            let encoded = source
                .get(stream.encoded.clone())
                .ok_or(PdfError::InvalidStream)?;
            let remaining = self
                .limits
                .max_total_decoded_bytes
                .checked_sub(self.total_decoded_bytes)
                .ok_or(PdfError::DecompressionLimit)?;
            let decoded =
                decode_stream_bytes(encoded, stream.filter, remaining, &self.limits, cancelled)?;
            self.total_decoded_bytes = self
                .total_decoded_bytes
                .checked_add(decoded.len())
                .ok_or(PdfError::DecompressionLimit)?;
            if self.total_decoded_bytes > self.limits.max_total_decoded_bytes {
                return Err(PdfError::DecompressionLimit);
            }
            self.decoded_streams = self
                .decoded_streams
                .checked_add(1)
                .ok_or(PdfError::DecompressionLimit)?;
            if self.decoded_streams > self.limits.max_streams {
                return Err(PdfError::DecompressionLimit);
            }
            self.decoded.insert(id, decoded);
        }
        self.decoded
            .get(&id)
            .map(Vec::as_slice)
            .ok_or(PdfError::InvalidStream)
    }
}

fn decode_stream_bytes(
    encoded: &[u8],
    filter: StreamFilter,
    remaining: usize,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Vec<u8>, PdfError> {
    let per_stream = limits.max_stream_decoded_bytes.min(remaining);
    match filter {
        StreamFilter::Raw => {
            if encoded.len() > per_stream {
                return Err(PdfError::DecompressionLimit);
            }
            let mut output = Vec::new();
            output
                .try_reserve_exact(encoded.len())
                .map_err(|_| PdfError::DecompressionLimit)?;
            for chunk in encoded.chunks(DECODE_CHUNK_BYTES) {
                check_cancelled(cancelled)?;
                output.extend_from_slice(chunk);
            }
            Ok(output)
        }
        StreamFilter::Flate => {
            if encoded.is_empty() {
                return Err(PdfError::InvalidStream);
            }
            let ratio_limit = encoded
                .len()
                .checked_mul(limits.max_expansion_ratio)
                .ok_or(PdfError::ExpansionRatioLimit)?;
            let allowed = per_stream.min(ratio_limit);
            let initial = encoded.len().saturating_mul(4).min(allowed);
            let mut output = Vec::new();
            output
                .try_reserve_exact(initial)
                .map_err(|_| PdfError::DecompressionLimit)?;
            let mut decoder = ZlibDecoder::new(encoded);
            let mut chunk = [0_u8; DECODE_CHUNK_BYTES];
            loop {
                check_cancelled(cancelled)?;
                let read = decoder
                    .read(&mut chunk)
                    .map_err(|_| PdfError::InvalidStream)?;
                if read == 0 {
                    break;
                }
                let next = output
                    .len()
                    .checked_add(read)
                    .ok_or(PdfError::DecompressionLimit)?;
                if next > allowed {
                    return if next > ratio_limit {
                        Err(PdfError::ExpansionRatioLimit)
                    } else {
                        Err(PdfError::DecompressionLimit)
                    };
                }
                output.extend_from_slice(&chunk[..read]);
            }
            if usize::try_from(decoder.total_in()).ok() != Some(encoded.len()) {
                return Err(PdfError::InvalidStream);
            }
            Ok(output)
        }
    }
}

#[derive(Debug, Clone)]
enum ContentOperand {
    String(Vec<u8>),
    Number,
    Name(Vec<u8>),
    Array(Vec<ContentArrayItem>),
}

#[derive(Debug, Clone)]
enum ContentArrayItem {
    String(Vec<u8>),
    Number(Option<i64>),
}

struct PageTextRequest<'a> {
    source: &'a [u8],
    parsed: &'a ParsedPdf,
    content_ids: &'a [ObjectId],
    fonts: &'a BTreeMap<Vec<u8>, FontEncoding>,
    limits: &'a PdfLimits,
    cancelled: Option<&'a dyn Fn() -> bool>,
}

fn extract_page_text(
    request: PageTextRequest<'_>,
    decode: &mut DecodeBudget,
    content_budget: &mut ContentBudget,
) -> Result<String, PdfError> {
    let PageTextRequest {
        source,
        parsed,
        content_ids,
        fonts,
        limits,
        cancelled,
    } = request;
    let mut page = String::new();
    let remaining_total = limits
        .max_total_text_bytes
        .checked_sub(decode.total_text_bytes)
        .ok_or(PdfError::TextLimit)?;
    let mut page_limits = *limits;
    page_limits.max_text_bytes_per_page = limits.max_text_bytes_per_page.min(remaining_total);
    page_limits.max_total_text_bytes = page_limits.max_text_bytes_per_page;
    let mut current_font = None;
    for id in content_ids {
        let content = decode.stream(source, parsed, *id, cancelled)?;
        parse_content_text(
            content,
            &mut page,
            fonts,
            &mut current_font,
            content_budget,
            &page_limits,
            cancelled,
        )?;
    }
    trim_text_in_place(&mut page);
    if page.len() > limits.max_text_bytes_per_page {
        return Err(PdfError::TextLimit);
    }
    let total = decode
        .total_text_bytes
        .checked_add(page.len())
        .ok_or(PdfError::TextLimit)?;
    if total > limits.max_total_text_bytes {
        return Err(PdfError::TextLimit);
    }
    decode.total_text_bytes = total;
    Ok(page)
}

fn parse_content_text(
    content: &[u8],
    output: &mut String,
    fonts: &BTreeMap<Vec<u8>, FontEncoding>,
    current_font: &mut Option<FontEncoding>,
    budget: &mut ContentBudget,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<(), PdfError> {
    let mut position = 0_usize;
    let mut array = None::<Vec<ContentArrayItem>>;
    let mut in_text = false;
    let mut operands = Vec::new();
    while skip_space_and_comments(content, position, content.len()) < content.len() {
        if budget.tokens.is_multiple_of(1_024) {
            check_cancelled(cancelled)?;
        }
        let (token, end) = lex_token_at_with_byte_limit(
            content,
            position,
            content.len(),
            limits,
            limits.max_text_bytes_per_page.saturating_add(1).max(4_096),
        )?;
        position = end;
        budget.tokens = budget.tokens.checked_add(1).ok_or(PdfError::ContentLimit)?;
        if budget.tokens > limits.max_tokens {
            return Err(PdfError::ContentLimit);
        }
        match token {
            Token::ArrayStart => {
                if array.is_some() {
                    return Err(PdfError::Malformed);
                }
                array = Some(Vec::new());
            }
            Token::ArrayEnd => {
                let values = array.take().ok_or(PdfError::Malformed)?;
                push_content_operand(&mut operands, ContentOperand::Array(values), limits)?;
            }
            Token::DictionaryStart | Token::DictionaryEnd => return Err(PdfError::ContentLimit),
            Token::String(bytes) => {
                if let Some(values) = &mut array {
                    push_content_array_item(values, ContentArrayItem::String(bytes), limits)?;
                } else {
                    push_content_operand(&mut operands, ContentOperand::String(bytes), limits)?;
                }
            }
            Token::Integer(value) => {
                if let Some(values) = &mut array {
                    push_content_array_item(values, ContentArrayItem::Number(Some(value)), limits)?;
                } else {
                    push_content_operand(&mut operands, ContentOperand::Number, limits)?;
                }
            }
            Token::Real => {
                if let Some(values) = &mut array {
                    push_content_array_item(values, ContentArrayItem::Number(None), limits)?;
                } else {
                    push_content_operand(&mut operands, ContentOperand::Number, limits)?;
                }
            }
            Token::Name(name) => {
                if array.is_some() {
                    return Err(PdfError::Malformed);
                }
                push_content_operand(&mut operands, ContentOperand::Name(name), limits)?;
            }
            Token::Keyword(operator) => {
                if array.is_some() {
                    return Err(PdfError::Malformed);
                }
                budget.operations = budget
                    .operations
                    .checked_add(1)
                    .ok_or(PdfError::ContentLimit)?;
                if budget.operations > limits.max_content_operations {
                    return Err(PdfError::ContentLimit);
                }
                match operator.as_slice() {
                    b"BI" | b"ID" | b"EI" => return Err(PdfError::InlineImage),
                    b"BT" => {
                        require_no_operands(&operands)?;
                        if in_text {
                            return Err(PdfError::Malformed);
                        }
                        in_text = true;
                        *current_font = None;
                    }
                    b"ET" => {
                        require_no_operands(&operands)?;
                        if !in_text {
                            return Err(PdfError::Malformed);
                        }
                        append_line_break(output, limits)?;
                        in_text = false;
                    }
                    b"Tf" if in_text => {
                        let [ContentOperand::Name(name), ContentOperand::Number] =
                            operands.as_slice()
                        else {
                            return Err(PdfError::Malformed);
                        };
                        *current_font = Some(
                            *fonts
                                .get(name.as_slice())
                                .ok_or(PdfError::UnsupportedFont)?,
                        );
                    }
                    b"Tj" if in_text => {
                        let [ContentOperand::String(bytes)] = operands.as_slice() else {
                            return Err(PdfError::Malformed);
                        };
                        append_pdf_string(
                            output,
                            bytes,
                            current_font.ok_or(PdfError::UnsupportedFont)?,
                            limits,
                        )?;
                    }
                    b"TJ" if in_text => {
                        let [ContentOperand::Array(values)] = operands.as_slice() else {
                            return Err(PdfError::Malformed);
                        };
                        append_text_array(
                            output,
                            values,
                            current_font.ok_or(PdfError::UnsupportedFont)?,
                            limits,
                        )?;
                    }
                    b"'" if in_text => {
                        let [ContentOperand::String(bytes)] = operands.as_slice() else {
                            return Err(PdfError::Malformed);
                        };
                        append_line_break(output, limits)?;
                        append_pdf_string(
                            output,
                            bytes,
                            current_font.ok_or(PdfError::UnsupportedFont)?,
                            limits,
                        )?;
                    }
                    b"\"" if in_text => {
                        let [ContentOperand::Number, ContentOperand::Number, ContentOperand::String(bytes)] =
                            operands.as_slice()
                        else {
                            return Err(PdfError::Malformed);
                        };
                        append_line_break(output, limits)?;
                        append_pdf_string(
                            output,
                            bytes,
                            current_font.ok_or(PdfError::UnsupportedFont)?,
                            limits,
                        )?;
                    }
                    b"T*" if in_text => {
                        require_no_operands(&operands)?;
                        append_line_break(output, limits)?;
                    }
                    b"TD" | b"Td" if in_text => {
                        let [ContentOperand::Number, ContentOperand::Number] = operands.as_slice()
                        else {
                            return Err(PdfError::Malformed);
                        };
                        if operator == b"TD" || !output.is_empty() {
                            append_line_break(output, limits)?;
                        }
                    }
                    _ => {}
                }
                operands.clear();
            }
        }
    }
    if array.is_some() || in_text || !operands.is_empty() {
        return Err(PdfError::Malformed);
    }
    Ok(())
}

fn require_no_operands(operands: &[ContentOperand]) -> Result<(), PdfError> {
    operands.is_empty().then_some(()).ok_or(PdfError::Malformed)
}

fn push_content_operand(
    operands: &mut Vec<ContentOperand>,
    value: ContentOperand,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    // Operand retention is per operation, not per page. This prevents a long
    // prefix with no operator from materializing a second operation graph.
    let max_operands = limits.max_content_nesting.saturating_mul(8).max(8);
    if operands.len() >= max_operands {
        return Err(PdfError::ContentLimit);
    }
    operands
        .try_reserve(1)
        .map_err(|_| PdfError::ContentLimit)?;
    operands.push(value);
    Ok(())
}

fn push_content_array_item(
    values: &mut Vec<ContentArrayItem>,
    value: ContentArrayItem,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    if values.len()
        >= limits
            .max_tokens_per_object
            .min(limits.max_content_operations)
    {
        return Err(PdfError::ContentLimit);
    }
    values.try_reserve(1).map_err(|_| PdfError::ContentLimit)?;
    values.push(value);
    Ok(())
}

fn append_text_array(
    output: &mut String,
    values: &[ContentArrayItem],
    encoding: FontEncoding,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    for value in values {
        match value {
            ContentArrayItem::String(bytes) => append_pdf_string(output, bytes, encoding, limits)?,
            ContentArrayItem::Number(Some(value)) if *value < -100 => append_space(output, limits)?,
            ContentArrayItem::Number(_) => {}
        }
    }
    Ok(())
}

fn append_pdf_string(
    output: &mut String,
    bytes: &[u8],
    encoding: FontEncoding,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    for byte in bytes {
        let character = match encoding {
            FontEncoding::Standard => standard_encoding_character(*byte)?,
            FontEncoding::WinAnsi => win_ansi_character(*byte),
        };
        append_source_char(output, character, limits)?;
    }
    Ok(())
}

fn standard_encoding_character(byte: u8) -> Result<char, PdfError> {
    match byte {
        0x27 => Ok('\u{2019}'),
        0x60 => Ok('\u{2018}'),
        0x20..=0x7e => Ok(char::from(byte)),
        _ => Err(PdfError::UnsupportedFont),
    }
}

fn append_source_char(
    output: &mut String,
    character: char,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    if character.is_control() {
        return Ok(());
    }
    if character.is_whitespace() {
        append_space(output, limits)
    } else {
        append_char(output, character, limits)
    }
}

fn win_ansi_character(byte: u8) -> char {
    const WINDOWS_1252: [char; 32] = [
        '\u{20ac}', '\u{0081}', '\u{201a}', '\u{0192}', '\u{201e}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02c6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008d}',
        '\u{017d}', '\u{008f}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02dc}', '\u{2122}', '\u{0161}', '\u{203a}',
        '\u{0153}', '\u{009d}', '\u{017e}', '\u{0178}',
    ];
    if matches!(byte, 0x7f | 0x81 | 0x8d | 0x8f | 0x90 | 0x9d) {
        '\u{2022}'
    } else if byte == 0xa0 {
        ' '
    } else if byte == 0xad {
        '-'
    } else if (0x80..=0x9f).contains(&byte) {
        WINDOWS_1252[usize::from(byte - 0x80)]
    } else {
        char::from(byte)
    }
}

fn append_char(output: &mut String, character: char, limits: &PdfLimits) -> Result<(), PdfError> {
    let next = output
        .len()
        .checked_add(character.len_utf8())
        .ok_or(PdfError::TextLimit)?;
    if next > limits.max_text_bytes_per_page || next > limits.max_total_text_bytes {
        return Err(PdfError::TextLimit);
    }
    output.push(character);
    Ok(())
}

fn append_space(output: &mut String, limits: &PdfLimits) -> Result<(), PdfError> {
    if !output.ends_with(|character: char| character.is_whitespace()) {
        append_char(output, ' ', limits)?;
    }
    Ok(())
}

fn append_line_break(output: &mut String, limits: &PdfLimits) -> Result<(), PdfError> {
    while output.ends_with(' ') {
        output.pop();
    }
    if !output.is_empty() && !output.ends_with('\n') {
        if output.len() == limits.max_text_bytes_per_page
            || output.len() == limits.max_total_text_bytes
        {
            return Ok(());
        }
        append_char(output, '\n', limits)?;
    }
    Ok(())
}

fn trim_text_in_place(text: &mut String) {
    let leading = text
        .char_indices()
        .find_map(|(index, character)| (!character.is_whitespace()).then_some(index));
    let Some(leading) = leading else {
        text.clear();
        return;
    };
    let trailing = text
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            (!character.is_whitespace()).then_some(index + character.len_utf8())
        })
        .unwrap_or(leading);
    text.truncate(trailing);
    if leading != 0 {
        text.drain(..leading);
    }
}

#[derive(Debug, Default)]
struct PdfMetadata {
    values: BTreeMap<String, String>,
    total_bytes: usize,
    decoded_bytes: usize,
}

fn extract_metadata(
    parsed: &ParsedPdf,
    limits: &PdfLimits,
    existing_text_bytes: usize,
) -> Result<PdfMetadata, PdfError> {
    let Some(info) = parsed.trailer.get(b"Info".as_slice()) else {
        return Ok(PdfMetadata::default());
    };
    let (_, PdfValue::Dictionary(info)) = resolve_value(parsed, info, limits)? else {
        return Err(PdfError::Malformed);
    };
    let fields: [(&[u8], &str); 8] = [
        (b"Title", "title"),
        (b"Author", "author"),
        (b"Subject", "subject"),
        (b"Keywords", "keywords"),
        (b"Creator", "creator"),
        (b"Producer", "producer"),
        (b"CreationDate", "creation_date"),
        (b"ModDate", "modification_date"),
    ];
    let mut metadata = PdfMetadata::default();
    for (pdf_key, graph_key) in fields {
        let Some(value) = info.get(pdf_key) else {
            continue;
        };
        let (_, PdfValue::String(bytes)) = resolve_value(parsed, value, limits)? else {
            return Err(PdfError::Malformed);
        };
        let remaining = limits
            .max_metadata_bytes
            .checked_sub(metadata.decoded_bytes)
            .ok_or(PdfError::MetadataLimit)?;
        let decoded = decode_info_text_string(bytes, remaining)?;
        metadata.decoded_bytes = metadata
            .decoded_bytes
            .checked_add(decoded.len())
            .ok_or(PdfError::MetadataLimit)?;
        if metadata.decoded_bytes > limits.max_metadata_bytes {
            return Err(PdfError::MetadataLimit);
        }
        let sanitized = sanitize_metadata_string(decoded);
        metadata.total_bytes = metadata
            .total_bytes
            .checked_add(sanitized.len())
            .ok_or(PdfError::MetadataLimit)?;
        if metadata.total_bytes > limits.max_metadata_bytes
            || existing_text_bytes
                .checked_add(metadata.total_bytes)
                .is_none_or(|total| total > limits.max_total_text_bytes)
        {
            return Err(PdfError::MetadataLimit);
        }
        if !sanitized.is_empty() {
            metadata.values.insert(graph_key.into(), sanitized);
        }
    }
    Ok(metadata)
}

fn decode_info_text_string(bytes: &[u8], max_bytes: usize) -> Result<String, PdfError> {
    let mut output = String::new();
    if bytes.starts_with(&[0xfe, 0xff]) {
        let mut units = bytes[2..].chunks_exact(2);
        for character in char::decode_utf16(
            units
                .by_ref()
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]])),
        ) {
            push_bounded_metadata_char(
                &mut output,
                character.map_err(|_| PdfError::Malformed)?,
                max_bytes,
            )?;
        }
        if !units.remainder().is_empty() {
            return Err(PdfError::Malformed);
        }
        return Ok(output);
    }

    // The supported non-BOM subset is the ASCII intersection of
    // PDFDocEncoding. Reject high bytes rather than silently treating them as
    // WinAnsi glyphs.
    for byte in bytes {
        if *byte > 0x7f {
            return Err(PdfError::Malformed);
        }
        push_bounded_metadata_char(&mut output, char::from(*byte), max_bytes)?;
    }
    Ok(output)
}

fn push_bounded_metadata_char(
    output: &mut String,
    character: char,
    max_bytes: usize,
) -> Result<(), PdfError> {
    let next = output
        .len()
        .checked_add(character.len_utf8())
        .ok_or(PdfError::MetadataLimit)?;
    if next > max_bytes {
        return Err(PdfError::MetadataLimit);
    }
    output.push(character);
    Ok(())
}

fn materialize_extraction(
    path: &Path,
    source_file: &str,
    page_text: Vec<(usize, String)>,
    metadata: PdfMetadata,
    decoded_streams: usize,
    decoded_bytes: usize,
    text_bytes: usize,
) -> Extraction {
    let mut root = document_node(path, source_file);
    root.extra
        .insert("page_count".into(), page_text.len().into());
    root.extra
        .insert("extracted_page_count".into(), page_text.len().into());
    root.extra
        .insert("decoded_stream_count".into(), decoded_streams.into());
    root.extra
        .insert("decompressed_bytes".into(), decoded_bytes.into());
    root.extra.insert("text_bytes".into(), text_bytes.into());
    for (key, value) in metadata.values {
        root.extra.insert(key, value.into());
    }

    let root_id = root.id.clone();
    let mut nodes = Vec::with_capacity(page_text.len() + 1);
    let mut edges = Vec::with_capacity(page_text.len());
    nodes.push(root);
    for (page_number, text) in page_text {
        let ordinal = format!("{page_number:06}");
        let page_id = make_id(&[&root_id, "page", &ordinal]);
        let page_label = format!("Page {page_number}");
        nodes.push(Node {
            id: page_id.clone(),
            label: page_label.clone(),
            file_type: "paper".into(),
            source_file: source_file.into(),
            source_location: None,
            community: None,
            extra: BTreeMap::from([
                ("_origin".into(), "pdf".into()),
                ("page_label".into(), page_label.into()),
                ("page_number".into(), page_number.into()),
                ("text_bytes".into(), text.len().into()),
                ("text".into(), text.into()),
                ("type".into(), "pdf_page".into()),
            ]),
        });
        edges.push(contains_edge(&root_id, &page_id, source_file));
    }
    Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    }
}

fn document_node(path: &Path, source_file: &str) -> Node {
    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let id = make_id(&[&stem]);
    Node {
        id,
        label: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(source_file)
            .into(),
        file_type: "paper".into(),
        source_file: source_file.into(),
        source_location: None,
        community: None,
        extra: BTreeMap::from([
            ("_origin".into(), "pdf".into()),
            ("format".into(), "pdf".into()),
            ("format_capability".into(), "structural_partial".into()),
            ("parse_status".into(), "complete".into()),
            ("type".into(), "pdf_document".into()),
        ]),
    }
}

fn contains_edge(source: &str, target: &str, source_file: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "contains".into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: BTreeMap::from([
            ("_origin".into(), "pdf".into()),
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn allowance_tightens_page_text_and_fact_ceilings() {
        let limits =
            PdfLimits::for_parser_allowance(16 * MIB, 128 * 1024).expect("bounded allowance");
        assert!(limits.max_total_decoded_bytes <= PdfLimits::default().max_total_decoded_bytes);
        assert!(limits.max_total_text_bytes <= PdfLimits::default().max_total_text_bytes);
        assert!(limits.max_pages.saturating_mul(2).saturating_add(1) <= limits.max_facts);
    }

    #[test]
    fn page_text_join_is_numeric_and_deterministic() {
        let mut extraction = Extraction::default();
        for (id, page, text) in [("p2", 2, "two"), ("p1", 1, "one")] {
            extraction.nodes.push(Node {
                id: id.into(),
                label: id.into(),
                file_type: "paper".into(),
                source_file: "paper.pdf".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("page_number".into(), page.into()),
                    ("text".into(), text.into()),
                ]),
            });
        }
        assert_eq!(extraction_text(&extraction), "one\ntwo");
    }

    #[test]
    fn immediate_cancellation_precedes_all_pdf_parsing_and_publication() {
        let cancelled = || true;
        let result = extract_pdf_bytes(
            Path::new("cancelled.pdf"),
            "cancelled.pdf",
            b"%PDF-1.7\n",
            PdfLimits::default(),
            Some(&cancelled),
        );
        assert_eq!(
            result.expect_err("cancel before parse"),
            PdfError::Cancelled
        );
        assert_eq!(PdfError::Cancelled.code(), "cancelled");
    }

    #[test]
    fn cancellation_interrupts_a_wide_direct_container() {
        let mut source = Vec::from(b"[".as_slice());
        for _ in 0..2_048 {
            source.extend_from_slice(b" 0");
        }
        source.extend_from_slice(b" ]");
        let calls = Cell::new(0_usize);
        let cancelled = || {
            let next = calls.get() + 1;
            calls.set(next);
            next >= 2
        };
        let limits = PdfLimits::default();
        let mut counters = ParseCounters::default();
        let mut parser = ValueParser::new(
            &source,
            0,
            source.len(),
            &limits,
            &mut counters,
            Some(&cancelled),
        );
        assert_eq!(
            parser.parse_value(0).expect_err("cancel wide array"),
            PdfError::Cancelled
        );
        assert!(counters.tokens <= 1_024);
    }

    #[test]
    fn cancellation_interrupts_chunked_stream_decode() {
        let encoded = vec![b'x'; DECODE_CHUNK_BYTES * 3];
        let calls = Cell::new(0_usize);
        let cancelled = || {
            let next = calls.get() + 1;
            calls.set(next);
            next >= 2
        };
        assert_eq!(
            decode_stream_bytes(
                &encoded,
                StreamFilter::Raw,
                encoded.len(),
                &PdfLimits::default(),
                Some(&cancelled),
            )
            .expect_err("cancel bounded decode"),
            PdfError::Cancelled
        );
    }

    #[test]
    fn cancellation_interrupts_a_long_marker_scan() {
        let source = vec![b' '; DECODE_CHUNK_BYTES * 3];
        let calls = Cell::new(0_usize);
        let cancelled = || {
            let next = calls.get() + 1;
            calls.set(next);
            next >= 2
        };
        assert_eq!(
            unique_line_keyword_offset(&source, b"startxref", Some(&cancelled))
                .expect_err("cancel marker scan"),
            PdfError::Cancelled
        );
    }

    #[test]
    fn external_file_stream_representations_fail_closed() {
        for key in [b"F".as_slice(), b"FFilter", b"FDecodeParms"] {
            let dictionary =
                BTreeMap::from([(key.to_vec(), PdfValue::String(b"SENTINEL".to_vec()))]);
            assert_eq!(
                stream_filter(&dictionary).expect_err("external stream representation"),
                PdfError::ActiveContent
            );
        }
    }

    #[test]
    fn supported_font_encodings_use_pdf_specific_character_maps() {
        let limits = PdfLimits::default();
        let mut standard = String::new();
        append_pdf_string(
            &mut standard,
            &[0x27, b' ', 0x60],
            FontEncoding::Standard,
            &limits,
        )
        .expect("bounded StandardEncoding text");
        assert_eq!(standard, "’ ‘");

        let mut win_ansi = String::new();
        append_pdf_string(
            &mut win_ansi,
            &[0x7f, 0x81, 0x8d, 0x8f, 0x90, 0x9d, 0xa0, 0xad],
            FontEncoding::WinAnsi,
            &limits,
        )
        .expect("bounded WinAnsi text");
        assert_eq!(win_ansi, "•••••• -");
    }
}
