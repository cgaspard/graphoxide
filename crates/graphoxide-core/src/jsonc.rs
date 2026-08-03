//! Strict-first parsing for JSON-with-comments configuration files.
//!
//! VS Code and several developer tools store JSONC in files whose suffix is
//! still `.json`. The normal parser remains the fast path; the fallback removes
//! comments and trailing commas without changing byte offsets or line breaks.

use serde_json::Value;

/// Parse strict JSON, falling back to the JSONC features used by editor
/// configuration files: line comments, block comments, and trailing commas.
pub fn parse_jsonc(text: &str) -> serde_json::Result<Value> {
    match serde_json::from_str(text) {
        Ok(value) => Ok(value),
        Err(strict_error) => normalized_jsonc(text).map_or(Err(strict_error), |normalized| {
            serde_json::from_str(&normalized)
        }),
    }
}

/// Byte-slice variant of [`parse_jsonc`]. Invalid UTF-8 retains the original
/// `serde_json` diagnostic instead of being decoded lossily.
pub fn parse_jsonc_slice(bytes: &[u8]) -> serde_json::Result<Value> {
    match serde_json::from_slice(bytes) {
        Ok(value) => Ok(value),
        Err(strict_error) => {
            let Ok(text) = std::str::from_utf8(bytes) else {
                return Err(strict_error);
            };
            normalized_jsonc(text).map_or(Err(strict_error), |normalized| {
                serde_json::from_str(&normalized)
            })
        }
    }
}

/// Replace JSONC-only syntax with spaces. Preserving byte offsets and newlines
/// keeps parser diagnostics and source-location extraction aligned with the
/// original file.
fn normalized_jsonc(text: &str) -> Option<String> {
    let input = text.as_bytes();
    let mut output = input.to_vec();
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut changed = false;

    while index < input.len() {
        let byte = input[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if byte == b'/' && input.get(index + 1) == Some(&b'/') {
            changed = true;
            while index < input.len() && !matches!(input[index], b'\n' | b'\r') {
                output[index] = b' ';
                index += 1;
            }
            continue;
        }
        if byte == b'/' && input.get(index + 1) == Some(&b'*') {
            changed = true;
            output[index] = b' ';
            output[index + 1] = b' ';
            index += 2;
            let mut closed = false;
            while index < input.len() {
                if input[index] == b'*' && input.get(index + 1) == Some(&b'/') {
                    output[index] = b' ';
                    output[index + 1] = b' ';
                    index += 2;
                    closed = true;
                    break;
                }
                if !matches!(input[index], b'\n' | b'\r') {
                    output[index] = b' ';
                }
                index += 1;
            }
            if !closed {
                return None;
            }
            continue;
        }
        index += 1;
    }

    index = 0;
    in_string = false;
    escaped = false;
    while index < output.len() {
        let byte = output[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if byte == b',' {
            let mut next = index + 1;
            while output
                .get(next)
                .is_some_and(|byte| byte.is_ascii_whitespace())
            {
                next += 1;
            }
            if output
                .get(next)
                .is_some_and(|next| matches!(*next, b'}' | b']'))
            {
                output[index] = b' ';
                changed = true;
            }
        }
        index += 1;
    }

    changed.then(|| String::from_utf8(output).expect("JSONC normalization preserves UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_and_trailing_commas_without_changing_strings() {
        let parsed = parse_jsonc(
            r#"{
                // VS Code permits comments.
                "url": "https://example.test/a//b",
                "pattern": "/* literal */",
                "items": ["one", "two",],
                /* and block comments */
            }"#,
        )
        .expect("parse JSONC");

        assert_eq!(parsed["url"], "https://example.test/a//b");
        assert_eq!(parsed["pattern"], "/* literal */");
        assert_eq!(parsed["items"], serde_json::json!(["one", "two"]));
    }

    #[test]
    fn preserves_unicode_outside_and_inside_comments() {
        let parsed =
            parse_jsonc("{/* 注釈 */\n\"label\":\"café 🚀\",\n}").expect("parse Unicode JSONC");
        assert_eq!(parsed["label"], "café 🚀");
    }

    #[test]
    fn rejects_non_jsonc_syntax_and_unterminated_comments() {
        assert!(parse_jsonc("{unquoted: true}").is_err());
        assert!(parse_jsonc("{\"ok\": true} /* unterminated").is_err());
    }
}
