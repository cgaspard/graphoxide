//! Shared extraction-provenance classification.

/// Classify a producer origin into the deterministic structural tier or the
/// model-authored semantic tier. Unknown future origins deliberately return
/// `None` so callers can fall back to source-location evidence.
pub fn origin_is_structural(origin: &str) -> Option<bool> {
    match origin {
        "semantic" => Some(false),
        // Some deterministic extractors are tree-sitter based; others are
        // specialized parsers or conservative fallback scanners. They retain
        // distinct origins for diagnostics but share build/replacement policy.
        "ast" | "fallback" | "terraform" | "sql" | "dotnet" | "scip" | "diagram" | "pdf" => {
            Some(true)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::origin_is_structural;

    #[test]
    fn pdf_is_deterministic_structural_provenance() {
        assert_eq!(origin_is_structural("pdf"), Some(true));
        assert_eq!(origin_is_structural("semantic"), Some(false));
        assert_eq!(origin_is_structural("future-origin"), None);
    }
}
