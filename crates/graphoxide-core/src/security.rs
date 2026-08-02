//! Input validation helpers, derived from upstream Graphify's `security.py`:
//! - URL validation (http/https only, no file:// redirects)
//! - graph path validation (must resolve inside graphoxide-out/)
//! - label sanitization (strip control chars, cap 256 chars, HTML-escape)

/// Strip ASCII control characters and cap at 256 Unicode scalar values.
///
/// HTML escaping belongs at the point of direct HTML injection, matching
/// upstream's `sanitize_label` contract.
pub fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .filter(|ch| !matches!(*ch as u32, 0x00..=0x1f | 0x7f))
        .take(256)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::sanitize_label;

    #[test]
    fn strips_only_upstream_control_range_and_caps() {
        assert_eq!(sanitize_label("a\n\tb\u{0085}c\u{007f}"), "ab\u{0085}c");
        assert_eq!(sanitize_label(&"界".repeat(300)).chars().count(), 256);
    }
}
