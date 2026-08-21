//! Bounded RTF (Rich Text Format) extraction.
//!
//! RTF is a plaintext markup format with a `{\rtfN ...}` header, nested
//! brace groups, control words (`\par`, `\line`, `\uN`, hex escapes), and
//! plain-text content. This module extracts a bounded document graph:
//! a root `document` node and one `document_section` node per paragraph,
//! with `contains` edges from root to each section.
//!
//! No RTF macros, fields, OLE objects, or external references are executed
//! or fetched. The parser is purely structural and text-extraction oriented.

use std::collections::BTreeMap;
use std::path::Path;

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};

/// Bounded ceilings for RTF extraction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RtfLimits {
    pub max_input_bytes: usize,
    pub max_nesting: usize,
    pub max_sections: usize,
    pub max_text_bytes: usize,
    pub max_section_bytes: usize,
}

impl Default for RtfLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_nesting: 64,
            max_sections: 4096,
            max_text_bytes: 4 * 1024 * 1024,
            max_section_bytes: 512 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum RtfError {
    InputLimit,
    NestingLimit,
    SectionLimit,
    TextLimit,
    NotRtf,
    Malformed,
}

impl RtfError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InputLimit => "rtf_input_limit",
            Self::NestingLimit => "rtf_nesting_limit",
            Self::SectionLimit => "rtf_section_limit",
            Self::TextLimit => "rtf_text_limit",
            Self::NotRtf => "rtf_not_rtf",
            Self::Malformed => "rtf_malformed",
        }
    }
}

fn validate_rtf_header(source: &[u8]) -> Result<(), RtfError> {
    let trimmed = skip_leading_whitespace(source);
    if !trimmed.starts_with(b"{\\rtf") {
        return Err(RtfError::NotRtf);
    }
    Ok(())
}

fn skip_leading_whitespace(source: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < source.len()
        && (source[start] == b' '
            || source[start] == b'\t'
            || source[start] == b'\n'
            || source[start] == b'\r')
    {
        start += 1;
    }
    if start + 3 <= source.len()
        && source[start] == 0xEF
        && source[start + 1] == 0xBB
        && source[start + 2] == 0xBF
    {
        start += 3;
    }
    &source[start..]
}

pub(crate) fn extract_rtf_bytes(
    path: &Path,
    source_file: &str,
    source: &[u8],
    limits: RtfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Extraction, RtfError> {
    if source.len() > limits.max_input_bytes {
        return Err(RtfError::InputLimit);
    }
    validate_rtf_header(source)?;

    let text = decode_rtf_text(source, limits, cancelled)?;

    let stem = source_file
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(source_file);
    let file_id = make_id(&[stem]);

    let mut root_extra: BTreeMap<String, serde_json::Value> = BTreeMap::from([
        ("type".into(), "rtf_document".into()),
        ("format".into(), "rtf".into()),
        ("_origin".into(), "rtf".into()),
        ("format_capability".into(), "structural_partial".into()),
        ("parse_status".into(), "complete".into()),
    ]);

    let sections: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .take(limits.max_sections + 1)
        .collect();

    let truncated = sections.len() > limits.max_sections;
    if truncated {
        root_extra.insert("parse_status".into(), "partial".into());
    }
    let sections: Vec<&str> = sections.into_iter().take(limits.max_sections).collect();

    let mut nodes = vec![Node {
        id: file_id.clone(),
        label: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(source_file)
            .to_owned(),
        file_type: "document".into(),
        source_file: source_file.into(),
        source_location: None,
        community: None,
        extra: root_extra,
    }];
    let mut edges = Vec::new();

    for (index, section) in sections.iter().enumerate() {
        let section_text: String = section
            .trim()
            .chars()
            .take(limits.max_section_bytes)
            .collect();
        if section_text.is_empty() {
            continue;
        }
        let section_id = format!("{}__rtf_section_{}", file_id, index + 1);
        nodes.push(Node {
            id: section_id.clone(),
            label: format!("Paragraph {}", index + 1),
            file_type: "document".into(),
            source_file: source_file.into(),
            source_location: None,
            community: None,
            extra: BTreeMap::from([
                ("type".into(), "document_section".into()),
                ("text".into(), section_text.into()),
                ("index".into(), (index + 1).into()),
            ]),
        });
        edges.push(Edge {
            source: file_id.clone(),
            target: section_id,
            relation: "contains".into(),
            confidence: Confidence::Extracted,
            source_file: source_file.into(),
            extra: BTreeMap::new(),
        });
    }

    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

fn decode_rtf_text(
    source: &[u8],
    limits: RtfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<String, RtfError> {
    let mut output = String::new();
    let mut depth: usize = 0;
    let mut i = 0_usize;
    let len = source.len();
    let mut total_text_bytes = 0_usize;

    while i < len {
        if i.is_multiple_of(4096) && cancelled.is_some_and(|c| c()) {
            return Err(RtfError::Malformed);
        }
        match source[i] {
            b'{' => {
                depth += 1;
                if depth > limits.max_nesting {
                    return Err(RtfError::NestingLimit);
                }
                i += 1;
            }
            b'}' => {
                if depth == 0 {
                    return Err(RtfError::Malformed);
                }
                depth -= 1;
                i += 1;
            }
            b'\\' if i + 1 < len => {
                let next = source[i + 1];
                if next.is_ascii_alphabetic() {
                    // Control word: \word or \wordN
                    let word_start = i + 1;
                    let mut word_end = word_start;
                    while word_end < len && source[word_end].is_ascii_alphabetic() {
                        word_end += 1;
                    }
                    let word = &source[word_start..word_end];
                    let param_start = word_end;

                    // Optional numeric parameter.
                    let mut param: Option<i64> = None;
                    let mut pos = param_start;
                    if pos < len && (source[pos] == b'-' || source[pos].is_ascii_digit()) {
                        let neg = source[pos] == b'-';
                        let mut num_pos = if neg { pos + 1 } else { pos };
                        let mut num = 0_i64;
                        while num_pos < len && source[num_pos].is_ascii_digit() {
                            num = num
                                .checked_mul(10)
                                .and_then(|n| n.checked_add((source[num_pos] - b'0') as i64))
                                .ok_or(RtfError::Malformed)?;
                            num_pos += 1;
                        }
                        param = Some(if neg { -num } else { num });
                        pos = num_pos;
                    }
                    // Optional trailing space.
                    if pos < len && source[pos] == b' ' {
                        pos += 1;
                    }

                    if depth == 1 {
                        total_text_bytes =
                            emit_control_word(&mut output, word, param, total_text_bytes)?;
                    }
                    i = pos;
                } else if next == b'\'' && i + 4 <= len {
                    let hi = hex_nibble(source[i + 2]).ok_or(RtfError::Malformed)?;
                    let lo = hex_nibble(source[i + 3]).ok_or(RtfError::Malformed)?;
                    let byte = (hi << 4) | lo;
                    if depth == 1 && byte < 0x80 {
                        output.push(byte as char);
                        total_text_bytes =
                            total_text_bytes.checked_add(1).ok_or(RtfError::TextLimit)?;
                    }
                    i += 4;
                } else if next == b'{' || next == b'}' {
                    if depth == 1 {
                        output.push(next as char);
                        total_text_bytes =
                            total_text_bytes.checked_add(1).ok_or(RtfError::TextLimit)?;
                    }
                    i += 2;
                } else {
                    i += 2;
                }
            }
            _ => {
                if depth == 1 {
                    let run_start = i;
                    while i < len
                        && source[i] != b'{'
                        && source[i] != b'}'
                        && source[i] != b'\\'
                        && (source[i] >= 0x20 || source[i] == b'\n' || source[i] == b'\t')
                    {
                        i += 1;
                    }
                    let text = String::from_utf8_lossy(&source[run_start..i]);
                    output.push_str(&text);
                    total_text_bytes = total_text_bytes
                        .checked_add(text.len())
                        .ok_or(RtfError::TextLimit)?;
                } else {
                    i += 1;
                }
            }
        }
        if total_text_bytes > limits.max_text_bytes {
            return Err(RtfError::TextLimit);
        }
    }

    if depth != 0 {
        return Err(RtfError::Malformed);
    }
    Ok(output)
}

fn emit_control_word(
    output: &mut String,
    word: &[u8],
    param: Option<i64>,
    mut total: usize,
) -> Result<usize, RtfError> {
    match word {
        b"par" | b"line" => {
            output.push('\n');
            Ok(total.checked_add(1).ok_or(RtfError::TextLimit)?)
        }
        b"tab" => {
            output.push('\t');
            Ok(total.checked_add(1).ok_or(RtfError::TextLimit)?)
        }
        b"u" => {
            if let Some(code) = param {
                let cp = if code < 0 {
                    0xFFFF_i64 + code + 1
                } else {
                    code
                };
                if let Some(ch) = char::from_u32(cp as u32) {
                    output.push(ch);
                    total = total
                        .checked_add(ch.len_utf8())
                        .ok_or(RtfError::TextLimit)?;
                }
            }
            Ok(total)
        }
        _ => Ok(total),
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(name: &str, bytes: &[u8]) -> Result<Extraction, RtfError> {
        extract_rtf_bytes(Path::new(name), name, bytes, RtfLimits::default(), None)
    }

    #[test]
    fn simple_rtf_extracts_document_and_sections() {
        let rtf = br"{\rtf1\ansi Hello World\par\par Second paragraph}";
        let extraction = extract("test.rtf", rtf).expect("should parse");
        assert_eq!(extraction.nodes.len(), 3); // root + 2 sections
        assert_eq!(extraction.nodes[0].file_type, "document");
        assert_eq!(
            extraction.nodes[0].extra.get("type").unwrap(),
            &serde_json::Value::from("rtf_document")
        );
        assert_eq!(extraction.edges.len(), 2);
        assert_eq!(extraction.edges[0].relation, "contains");
        assert_eq!(
            extraction.nodes[1].extra.get("text").unwrap(),
            &serde_json::Value::from("Hello World")
        );
        assert_eq!(
            extraction.nodes[2].extra.get("text").unwrap(),
            &serde_json::Value::from("Second paragraph")
        );
    }

    #[test]
    fn rtf_with_unicode_escapes_decodes_text() {
        // \u233 is the copyright symbol ©
        let rtf = br"{\rtf1\ansi Copyright \u169  2024}";
        let extraction = extract("test.rtf", rtf).expect("should parse");
        let text = extraction.nodes[1].extra.get("text").unwrap();
        assert!(text.as_str().unwrap().contains("2024"));
    }

    #[test]
    fn rtf_with_hex_escapes_decodes_text() {
        // \'41 = 'A', \'42 = 'B'
        let rtf = br"{\rtf1\ansi \'41\'42}";
        let extraction = extract("test.rtf", rtf).expect("should parse");
        let text = extraction.nodes[1].extra.get("text").unwrap();
        assert_eq!(text, &serde_json::Value::from("AB"));
    }

    #[test]
    fn rtf_rejects_non_rtf_input() {
        let result = extract("test.rtf", b"not an rtf file");
        assert_eq!(result.unwrap_err(), RtfError::NotRtf);
    }

    #[test]
    fn rtf_rejects_oversized_input() {
        let mut big = vec![b'{', b'\\', b'r', b't', b'f', b'1'];
        big.extend(std::iter::repeat_n(b'x', 20 * 1024 * 1024));
        big.push(b'}');
        let result = extract("big.rtf", &big);
        assert_eq!(result.unwrap_err(), RtfError::InputLimit);
    }

    #[test]
    fn rtf_rejects_unbalanced_braces() {
        let rtf = br"{\rtf1\ansi Hello {unclosed}";
        let result = extract("test.rtf", rtf);
        assert_eq!(result.unwrap_err(), RtfError::Malformed);
    }

    #[test]
    fn rtf_rejects_deep_nesting() {
        let mut deep = vec![b'{', b'\\', b'r', b't', b'f', b'1'];
        deep.extend(std::iter::repeat_n(b'{', 100));
        deep.extend(std::iter::repeat_n(b'}', 100));
        deep.push(b'}');
        let result = extract("deep.rtf", &deep);
        assert_eq!(result.unwrap_err(), RtfError::NestingLimit);
    }

    #[test]
    fn rtf_empty_body_produces_root_only() {
        let rtf = br"{\rtf1\ansi}";
        let extraction = extract("test.rtf", rtf).expect("should parse");
        assert_eq!(extraction.nodes.len(), 1);
        assert_eq!(extraction.edges.len(), 0);
    }

    #[test]
    fn rtf_respects_section_limit() {
        let mut rtf = vec![b'{', b'\\', b'r', b't', b'f', b'1'];
        for i in 0..5000 {
            rtf.extend(format!(" P{i}\\par\\par").as_bytes());
        }
        rtf.push(b'}');
        let limits = RtfLimits {
            max_sections: 100,
            ..Default::default()
        };
        let extraction = extract_rtf_bytes(Path::new("many.rtf"), "many.rtf", &rtf, limits, None)
            .expect("should parse");
        // Root + 100 sections
        assert_eq!(extraction.nodes.len(), 101);
        assert_eq!(
            extraction.nodes[0].extra.get("parse_status").unwrap(),
            &serde_json::Value::from("partial")
        );
    }
}
