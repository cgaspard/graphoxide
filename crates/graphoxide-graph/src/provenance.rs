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
        "ast" | "fallback" | "terraform" | "sql" | "dotnet" | "scip" => Some(true),
        _ => None,
    }
}
