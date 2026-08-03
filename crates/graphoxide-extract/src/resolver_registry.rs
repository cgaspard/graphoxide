//! Ordered, suffix-gated corpus resolver registry.
//!
//! Resolver errors and panics are isolated per entry. A broken optional
//! language pass must not abort graph construction or prevent later passes
//! from repairing their own language facts.

use graphoxide_core::Extraction;
use std::{
    collections::BTreeSet,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
};

pub type ResolverFn = fn(&mut [Extraction]) -> anyhow::Result<()>;

#[derive(Clone, Copy)]
pub struct LanguageResolver {
    pub name: &'static str,
    pub suffixes: &'static [&'static str],
    pub resolve: ResolverFn,
}

impl LanguageResolver {
    pub const fn new(
        name: &'static str,
        suffixes: &'static [&'static str],
        resolve: ResolverFn,
    ) -> Self {
        Self {
            name,
            suffixes,
            resolve,
        }
    }

    fn enabled(&self, suffixes: &BTreeSet<String>) -> bool {
        self.suffixes
            .iter()
            .any(|suffix| suffixes.contains(&suffix.trim_start_matches('.').to_ascii_lowercase()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverFailure {
    pub name: String,
    pub message: String,
}

fn swift_member_calls(extractions: &mut [Extraction]) -> anyhow::Result<()> {
    crate::swift::resolve(extractions);
    Ok(())
}

fn python_member_calls(extractions: &mut [Extraction]) -> anyhow::Result<()> {
    crate::resolution::resolve_python_imports(extractions);
    Ok(())
}

static LANGUAGE_RESOLVERS: &[LanguageResolver] = &[
    LanguageResolver::new("swift_member_calls", &["swift"], swift_member_calls),
    LanguageResolver::new("python_member_calls", &["py", "pyi"], python_member_calls),
];

pub fn registered_resolvers() -> &'static [LanguageResolver] {
    LANGUAGE_RESOLVERS
}

#[derive(Default)]
pub struct ResolverRegistry {
    entries: Vec<LanguageResolver>,
}

impl ResolverRegistry {
    pub fn register(&mut self, resolver: LanguageResolver) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self
                .entries
                .iter()
                .any(|registered| registered.name == resolver.name),
            "resolver {:?} is already registered",
            resolver.name
        );
        self.entries.push(resolver);
        Ok(())
    }

    pub fn entries(&self) -> &[LanguageResolver] {
        &self.entries
    }
}

fn suffixes(paths: &[PathBuf]) -> BTreeSet<String> {
    paths
        .iter()
        .filter_map(|path| Path::new(path).extension())
        .filter_map(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|value| (*value).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "resolver panicked".into())
}

pub fn run_language_resolvers(
    paths: &[PathBuf],
    extractions: &mut [Extraction],
    resolvers: &[LanguageResolver],
) -> Vec<ResolverFailure> {
    let present = suffixes(paths);
    let mut failures = Vec::new();
    for resolver in resolvers {
        if !resolver.enabled(&present) {
            continue;
        }
        match catch_unwind(AssertUnwindSafe(|| (resolver.resolve)(extractions))) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(ResolverFailure {
                name: resolver.name.into(),
                message: error.to_string(),
            }),
            Err(payload) => failures.push(ResolverFailure {
                name: resolver.name.into(),
                message: panic_message(payload),
            }),
        }
    }
    failures
}

fn extraction_paths(extractions: &[Extraction]) -> Vec<PathBuf> {
    extractions
        .iter()
        .flat_map(|extraction| {
            extraction
                .nodes
                .iter()
                .map(|node| node.source_file.as_str())
                .chain(
                    extraction
                        .edges
                        .iter()
                        .map(|edge| edge.source_file.as_str()),
                )
        })
        .filter(|source| !source.is_empty())
        .map(PathBuf::from)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn run_registered_language_resolvers(extractions: &mut [Extraction]) {
    let paths = extraction_paths(extractions);
    for failure in run_language_resolvers(&paths, extractions, LANGUAGE_RESOLVERS) {
        tracing::warn!(
            resolver = failure.name,
            error = failure.message,
            "language resolver failed; continuing with later passes"
        );
    }
}
