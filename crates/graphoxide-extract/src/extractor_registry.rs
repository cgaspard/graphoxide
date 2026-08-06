//! Registry for dedicated, non-tree-sitter language extractors.
//!
//! Entries are function pointers rather than boxed facade wrappers, so the
//! registry and public facade cannot silently diverge.

use graphoxide_core::Extraction;
use std::path::Path;

pub type ExtractorFn = fn(&Path, &str) -> anyhow::Result<Extraction>;
/// A dedicated extractor that consumes source bytes prepared by the I/O
/// service instead of reading the path itself.
pub type ByteExtractorFn = fn(&Path, &str, &[u8]) -> anyhow::Result<Extraction>;

#[derive(Clone, Copy)]
pub struct LanguageExtractor {
    pub name: &'static str,
    pub suffixes: &'static [&'static str],
    pub extract: ExtractorFn,
    pub extract_bytes: Option<ByteExtractorFn>,
}

impl LanguageExtractor {
    pub const fn new(
        name: &'static str,
        suffixes: &'static [&'static str],
        extract: ExtractorFn,
    ) -> Self {
        Self {
            name,
            suffixes,
            extract,
            extract_bytes: None,
        }
    }

    /// Construct an extractor that supports both the legacy path facade and
    /// the I/O-isolated byte execution path.
    pub const fn with_bytes(
        name: &'static str,
        suffixes: &'static [&'static str],
        extract: ExtractorFn,
        extract_bytes: ByteExtractorFn,
    ) -> Self {
        Self {
            name,
            suffixes,
            extract,
            extract_bytes: Some(extract_bytes),
        }
    }

    pub fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                self.suffixes
                    .iter()
                    .any(|suffix| extension.eq_ignore_ascii_case(suffix))
            })
    }
}

static LANGUAGE_EXTRACTORS: &[LanguageExtractor] = &[LanguageExtractor::with_bytes(
    "terraform",
    &["tf", "tfvars", "hcl"],
    crate::terraform::extract_terraform,
    crate::terraform::extract_terraform_bytes,
)];

pub fn registered_extractors() -> &'static [LanguageExtractor] {
    LANGUAGE_EXTRACTORS
}

pub(crate) fn extractor_for_path(path: &Path) -> Option<&'static LanguageExtractor> {
    LANGUAGE_EXTRACTORS
        .iter()
        .find(|extractor| extractor.supports(path))
}

#[derive(Default)]
pub struct ExtractorRegistry {
    entries: Vec<LanguageExtractor>,
}

impl ExtractorRegistry {
    pub fn register(&mut self, extractor: LanguageExtractor) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self
                .entries
                .iter()
                .any(|registered| registered.name == extractor.name),
            "extractor {:?} is already registered",
            extractor.name
        );
        self.entries.push(extractor);
        Ok(())
    }

    pub fn entries(&self) -> &[LanguageExtractor] {
        &self.entries
    }
}
