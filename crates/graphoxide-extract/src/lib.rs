//! File detection and per-file extraction.
//!
//! Port of upstream `detect.py`, `extract.py`, `extractors/*`, `cache.py`,
//! `manifest.py`. The pipeline stage contract is unchanged:
//!
//! ```text
//! collect_files(root) -> Vec<PathBuf>
//! extract(path)       -> Extraction { nodes, edges }
//! ```
//!
//! Extraction runs in-process on a rayon pool (upstream used a subprocess
//! pool to dodge the GIL — unnecessary here, and one of the main speed wins).

mod bash;
pub mod cache;
pub mod cargo_introspect;
mod compat;
mod csharp;
mod dart;
pub mod detect;
mod dotnet;
pub mod engine;
pub mod extractor_registry;
mod fallback;
mod java;
mod js_resolution;
mod json_config;
pub mod languages;
pub mod llm;
pub mod manifest_ingest;
mod native;
mod pascal;
pub mod pg_introspect;
mod php;
pub mod resolution;
pub mod resolver_registry;
mod ruby;
pub mod scip_ingest;
pub mod semantic_pipeline;
mod sfc;
mod sql;
pub mod stale;
mod swift;
pub mod terraform;
pub mod vision;

pub use detect::collect_files;
pub use engine::extract;
pub use js_resolution::resolve_js_module_path;
pub use sfc::mask_vue_non_script;
pub use terraform::extract_terraform;

/// Collect and extract a project in parallel, storing repo-relative paths.
pub fn extract_project(root: &std::path::Path) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    extract_project_with_options(root, false)
}

/// Extract a project, optionally bypassing the AST cache for a true full scan.
pub fn extract_project_with_options(
    root: &std::path::Path,
    force: bool,
) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    extract_project_with_options_and_output(root, force, &root.join("graphoxide-out"))
}

/// Extract a project while storing the incremental manifest and AST cache in
/// an explicit managed output directory.
///
/// This keeps scans side-effect free inside `root` when callers direct output
/// elsewhere, while the existing wrappers retain `root/graphoxide-out`.
pub fn extract_project_with_options_and_output(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    extract_project_with_options_and_output_filtered(root, force, managed_output_dir, false)
}

/// Extract a project with an explicit code-only boundary. The filtered mode
/// excludes document, paper, image, and video tiers before cache lookup, so a
/// `--code-only` build cannot accidentally retain locally parsed document
/// nodes or create semantic-cache artifacts for skipped inputs.
pub fn extract_project_with_options_and_output_filtered(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    Ok(extract_project_with_scan_options(
        root,
        force,
        managed_output_dir,
        code_only,
        &detect::DetectOptions::default(),
    )?
    .extractions)
}

#[derive(Debug, Clone)]
pub struct ProjectExtractionResult {
    pub extractions: Vec<graphoxide_core::Extraction>,
    pub detection: detect::DetectResult,
}

/// Completion evidence for a project extraction attempt.
///
/// A filesystem walk error represents work that could not be enumerated, so it
/// contributes one unsuccessful unit even though there is no path to extract.
/// Successful returns otherwise contain one completed unit per dispatched file;
/// an extractor error aborts before a result is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectExtractionProgress {
    pub total: usize,
    pub succeeded: usize,
}

impl ProjectExtractionProgress {
    pub const fn is_complete(self) -> bool {
        self.succeeded == self.total
    }
}

/// A scan manifest prepared in memory but not yet made visible on disk.
///
/// Callers that also write a graph should commit the graph first, then consume
/// this value. If the graph is refused or its write fails, dropping this value
/// leaves the previous manifest untouched.
#[derive(Debug)]
#[must_use = "commit this manifest only after the corresponding graph write succeeds"]
pub struct PendingProjectManifest {
    output_directory: std::path::PathBuf,
    entries: cache::Manifest,
}

impl PendingProjectManifest {
    pub fn path(&self) -> std::path::PathBuf {
        self.output_directory.join("manifest.json")
    }

    pub fn commit(self) -> anyhow::Result<()> {
        cache::save_manifest_to_output(&self.output_directory, &self.entries)
    }
}

/// Project extraction whose manifest is intentionally deferred until the graph
/// artifact has been durably accepted.
#[derive(Debug)]
#[must_use = "the pending manifest must be committed or deliberately discarded"]
pub struct DeferredProjectExtractionResult {
    pub extractions: Vec<graphoxide_core::Extraction>,
    pub detection: detect::DetectResult,
    pub progress: ProjectExtractionProgress,
    pub pending_manifest: PendingProjectManifest,
}

fn normalized_project_key(
    path: &std::path::Path,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> String {
    use unicode_normalization::UnicodeNormalization;

    path.strip_prefix(resolved_root)
        .or_else(|_| path.strip_prefix(original_root))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
        .nfc()
        .collect()
}

fn normalized_manifest_key(
    stored: &str,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> String {
    use unicode_normalization::UnicodeNormalization;

    let path = std::path::Path::new(stored);
    if path.is_absolute() {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        normalized_project_key(&resolved, resolved_root, original_root)
    } else {
        stored.replace('\\', "/").nfc().collect()
    }
}

fn normalized_previous_manifest(
    manifest: &cache::Manifest,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> cache::Manifest {
    let mut normalized = cache::Manifest::new();
    // Load legacy absolute spellings first. Portable relative rows are newer
    // and authoritative when both forms address the same source.
    for absolute in [true, false] {
        for (stored, entry) in manifest {
            if std::path::Path::new(stored).is_absolute() == absolute {
                normalized.insert(
                    normalized_manifest_key(stored, resolved_root, original_root),
                    entry.clone(),
                );
            }
        }
    }
    normalized
}

impl DeferredProjectExtractionResult {
    /// Preserve the legacy extract-only behavior by publishing the manifest and
    /// returning the ordinary result shape.
    pub fn commit_manifest(self) -> anyhow::Result<ProjectExtractionResult> {
        let Self {
            extractions,
            detection,
            pending_manifest,
            ..
        } = self;
        pending_manifest.commit()?;
        Ok(ProjectExtractionResult {
            extractions,
            detection,
        })
    }
}

/// Extract a project with caller-controlled ignore policy and retain the full
/// discovery diagnostics. CLI callers use this to persist `--exclude` and
/// `--no-gitignore` without performing a second, potentially divergent scan.
pub fn extract_project_with_scan_options(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
) -> anyhow::Result<ProjectExtractionResult> {
    extract_project_with_scan_options_deferred_manifest(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
    )?
    .commit_manifest()
}

/// Extract a project without publishing its next manifest. This is the safe
/// entry point for graph-building callers: write the graph using `progress`,
/// then call `pending_manifest.commit()` only when that write succeeds.
pub fn extract_project_with_scan_options_deferred_manifest(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
) -> anyhow::Result<DeferredProjectExtractionResult> {
    use md5::Digest as _;
    use rayon::prelude::*;
    let managed_output_dir = if managed_output_dir.is_absolute() {
        managed_output_dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(managed_output_dir)
    };
    let mut detect_options = detect_options.clone();
    // Discovery and cache persistence must agree on which generated directory
    // belongs to this build. A mismatched caller option could otherwise ingest
    // the real managed output back into the corpus.
    detect_options.output_dir = Some(managed_output_dir.clone());
    let detection = detect::detect(root, &detect_options)?;
    let mut files = detection
        .files
        .iter()
        .filter(|(kind, _)| !code_only || kind.as_str() == detect::FileType::Code.as_str())
        .flat_map(|(_, paths)| paths)
        .map(std::path::PathBuf::from)
        .filter(|path| detect::is_supported_path(path))
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    let total_work = files.len().saturating_add(detection.walk_errors.len());
    let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let rows: anyhow::Result<Vec<_>> = files
        .par_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&resolved_root)
                .or_else(|_| path.strip_prefix(root))
                .map_or_else(
                    |_| normalized_project_key(path, &resolved_root, root),
                    |relative| normalized_project_key(relative, &resolved_root, root),
                );
            let bytes = std::fs::read(path)?;
            let extraction = if !force {
                cache::ast_cache_get_from_output(&managed_output_dir, &relative, &bytes)
            } else {
                None
            };
            let extraction = if let Some(cached) = extraction {
                cached
            } else {
                let extracted = engine::extract_as(path, &relative)?;
                cache::ast_cache_put_to_output(&managed_output_dir, &relative, &bytes, &extracted)?;
                extracted
            };
            let metadata = std::fs::metadata(path)?;
            let mtime = metadata
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            let hash = format!("{:x}", md5::Md5::digest(&bytes));
            Ok((relative, extraction, mtime, hash))
        })
        .collect();
    let rows = rows?;
    let succeeded = rows.len();
    let previous = normalized_previous_manifest(
        &cache::load_manifest_from_output(&managed_output_dir),
        &resolved_root,
        root,
    );
    let mut manifest: cache::Manifest = rows
        .iter()
        .map(|(relative, _, mtime, hash)| {
            let semantic_hash = previous
                .get(relative)
                .filter(|entry| entry.ast_hash == *hash)
                .map(|entry| entry.semantic_hash.clone())
                .unwrap_or_default();
            (
                relative.clone(),
                cache::ManifestEntry {
                    mtime: *mtime,
                    ast_hash: hash.clone(),
                    semantic_hash,
                },
            )
        })
        .collect();
    if code_only {
        for paths in detection
            .files
            .iter()
            .filter(|(kind, _)| kind.as_str() != detect::FileType::Code.as_str())
            .map(|(_, paths)| paths)
        {
            for path in paths {
                let path = std::path::Path::new(path);
                let key = normalized_project_key(path, &resolved_root, root);
                if let Some(entry) = previous.get(&key) {
                    manifest.entry(key).or_insert_with(|| entry.clone());
                }
            }
        }
    }
    let mut extractions: Vec<_> = rows
        .into_iter()
        .map(|(_, extraction, _, _)| extraction)
        .collect();
    resolution::resolve_with_root(&mut extractions, &resolved_root);
    Ok(DeferredProjectExtractionResult {
        extractions,
        detection,
        progress: ProjectExtractionProgress {
            total: total_work,
            succeeded,
        },
        pending_manifest: PendingProjectManifest {
            output_directory: managed_output_dir,
            entries: manifest,
        },
    })
}

#[derive(Debug, Clone)]
pub struct ExtractFilesResult {
    pub extractions: Vec<graphoxide_core::Extraction>,
    pub warnings: Vec<String>,
    pub key_root: std::path::PathBuf,
    pub managed_output_dir: std::path::PathBuf,
}

/// Explicit-file extraction whose replacement manifest is not yet visible.
///
/// Graph-building callers must accept their graph artifact before committing
/// this manifest. Dropping the pending manifest keeps the prior scan state
/// intact, which makes graph-build failures and shrink refusals retryable.
#[derive(Debug)]
#[must_use = "commit this manifest only after the corresponding graph write succeeds"]
pub struct DeferredExtractFilesResult {
    pub result: ExtractFilesResult,
    pub pending_manifest: PendingProjectManifest,
}

impl DeferredExtractFilesResult {
    /// Publish the prepared manifest and return the legacy result shape.
    pub fn commit_manifest(self) -> anyhow::Result<ExtractFilesResult> {
        self.pending_manifest.commit()?;
        Ok(self.result)
    }

    /// Deliberately retain the previous manifest. Callers that publish a
    /// separately reconstructed full-corpus manifest after graph acceptance
    /// use this to consume the extraction without exposing the target subset.
    pub fn discard_manifest(self) -> ExtractFilesResult {
        self.result
    }
}

fn common_file_parent(files: &[std::path::PathBuf]) -> anyhow::Result<std::path::PathBuf> {
    anyhow::ensure!(!files.is_empty(), "at least one input file is required");
    let resolved = files
        .iter()
        .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect::<Vec<_>>();
    let mut root = resolved[0]
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("input file has no parent: {}", files[0].display()))?;
    while !resolved.iter().all(|path| path.starts_with(&root)) {
        root = root
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("input files have no common parent"))?;
    }
    Ok(root)
}

/// Extract an explicit file set. When `cache_root` contains every input it is
/// also the source-identity anchor, matching upstream's root fallback. An
/// out-of-tree cache root remains storage-only and source identity falls back
/// to the files' common corpus parent.
pub fn extract_files(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    force: bool,
) -> anyhow::Result<ExtractFilesResult> {
    extract_files_deferred_manifest(files, cache_root, force)?.commit_manifest()
}

/// Extract an explicit file set without publishing its replacement manifest.
pub fn extract_files_deferred_manifest(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    force: bool,
) -> anyhow::Result<DeferredExtractFilesResult> {
    extract_files_with_deferred_manifest(files, cache_root, force, |path, relative| {
        engine::extract_as(path, relative)
    })
}

/// Injectable variant used by backend adapters and failure-path tests.
pub fn extract_files_with<F>(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    force: bool,
    extractor: F,
) -> anyhow::Result<ExtractFilesResult>
where
    F: Fn(&std::path::Path, &str) -> anyhow::Result<graphoxide_core::Extraction>,
{
    extract_files_with_deferred_manifest(files, cache_root, force, extractor)?.commit_manifest()
}

/// Injectable deferred-manifest variant used by graph-building adapters and
/// failure-path tests.
pub fn extract_files_with_deferred_manifest<F>(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    force: bool,
    extractor: F,
) -> anyhow::Result<DeferredExtractFilesResult>
where
    F: Fn(&std::path::Path, &str) -> anyhow::Result<graphoxide_core::Extraction>,
{
    use md5::Digest as _;
    let common_root = common_file_parent(files)?;
    let key_root = cache_root
        .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()))
        .filter(|root| {
            files.iter().all(|path| {
                std::fs::canonicalize(path)
                    .unwrap_or_else(|_| path.clone())
                    .starts_with(root)
            })
        })
        .unwrap_or(common_root);
    let cache_base = cache_root.map(std::path::Path::to_path_buf).unwrap_or(
        std::env::current_dir()
            .map_err(|error| anyhow::anyhow!("resolve current directory: {error}"))?,
    );
    let managed_output_dir = cache_base.join("graphoxide-out");
    let previous = cache::load_manifest_from_output(&managed_output_dir);
    let mut rows = Vec::with_capacity(files.len());
    let mut warnings = Vec::new();
    let mut missing_extractors = std::collections::BTreeMap::<String, usize>::new();
    for original in files {
        let path = std::fs::canonicalize(original).unwrap_or_else(|_| original.clone());
        let relative = path
            .strip_prefix(&key_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(&path)?;
        let cached = (!force)
            .then(|| cache::ast_cache_get_from_output(&managed_output_dir, &relative, &bytes))
            .flatten();
        let extraction = if let Some(cached) = cached {
            cached
        } else {
            let extracted = extractor(&path, &relative)?;
            if extracted.nodes.is_empty() {
                if detect::classify_file(&path) == Some(detect::FileType::Code)
                    && !engine::has_ast_extractor(&path)
                {
                    let suffix = path
                        .extension()
                        .and_then(|value| value.to_str())
                        .map(|value| format!(".{}", value.to_ascii_lowercase()))
                        .unwrap_or_else(|| "<extensionless>".into());
                    *missing_extractors.entry(suffix).or_default() += 1;
                } else {
                    warnings.push(format!(
                        "{} produced zero nodes; the anomalous result was not cached and will be retried",
                        relative
                    ));
                }
            } else {
                cache::ast_cache_put_to_output(&managed_output_dir, &relative, &bytes, &extracted)?;
            }
            extracted
        };
        let metadata = std::fs::metadata(&path)?;
        let mtime = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let hash = if extraction.nodes.is_empty() {
            String::new()
        } else {
            format!("{:x}", md5::Md5::digest(&bytes))
        };
        rows.push((relative, extraction, mtime, hash));
    }
    if !missing_extractors.is_empty() {
        let summary = missing_extractors
            .into_iter()
            .map(|(suffix, count)| format!("{suffix} ({count})"))
            .collect::<Vec<_>>()
            .join(", ");
        warnings.push(format!(
            "code files have no AST extractor (#1689): {summary}"
        ));
    }
    let manifest = rows
        .iter()
        .map(|(relative, _, mtime, hash)| {
            let semantic_hash = previous
                .get(relative)
                .filter(|entry| entry.ast_hash == *hash && !hash.is_empty())
                .map(|entry| entry.semantic_hash.clone())
                .unwrap_or_default();
            (
                relative.clone(),
                cache::ManifestEntry {
                    mtime: *mtime,
                    ast_hash: hash.clone(),
                    semantic_hash,
                },
            )
        })
        .collect();
    let mut extractions = rows
        .into_iter()
        .map(|(_, extraction, _, _)| extraction)
        .collect::<Vec<_>>();
    resolution::resolve_with_root(&mut extractions, &key_root);
    Ok(DeferredExtractFilesResult {
        result: ExtractFilesResult {
            extractions,
            warnings,
            key_root,
            managed_output_dir: managed_output_dir.clone(),
        },
        pending_manifest: PendingProjectManifest {
            output_directory: managed_output_dir,
            entries: manifest,
        },
    })
}

#[cfg(test)]
mod tests {
    use graphoxide_core::make_id;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "graphoxide-injected-calls-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).expect("create extraction fixture");
            Self { root }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, contents).expect("write extraction fixture");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).expect("remove extraction fixture");
        }
    }

    fn extract(path: &Path, source_file: &str) -> graphoxide_core::Extraction {
        super::engine::extract_as(path, source_file).expect("extract fixture file")
    }

    fn definition_labels(extraction: &graphoxide_core::Extraction) -> Vec<&str> {
        let mut labels: Vec<_> = extraction
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.extra.get("type").and_then(|value| value.as_str()),
                    Some("class" | "function")
                )
            })
            .map(|node| node.label.as_str())
            .collect();
        labels.sort_unstable();
        labels
    }

    fn assert_definition(extraction: &graphoxide_core::Extraction, id: &str, kind: &str) {
        let node = extraction
            .nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("missing {kind} node {id}"));
        assert_eq!(
            node.extra.get("type").and_then(|value| value.as_str()),
            Some(kind),
            "node {id} should be a {kind}"
        );
    }

    fn assert_export_status(extraction: &graphoxide_core::Extraction, id: &str, exported: bool) {
        let node = extraction
            .nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("missing node {id}"));
        assert_eq!(
            node.extra
                .get("exported")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            exported,
            "unexpected export status for node {id}"
        );
    }

    fn assert_single_edge(
        extraction: &graphoxide_core::Extraction,
        source: &str,
        target: &str,
        relation: &str,
    ) {
        let count = extraction
            .edges
            .iter()
            .filter(|edge| {
                edge.relation == relation
                    && edge.true_source() == source
                    && edge.true_target() == target
            })
            .count();
        assert_eq!(
            count, 1,
            "expected one {relation} edge from {source} to {target}"
        );
    }

    fn resolved_call_targets<'a>(
        extraction: &'a graphoxide_core::Extraction,
        source: &str,
    ) -> Vec<&'a str> {
        extraction
            .edges
            .iter()
            .filter(|edge| {
                edge.relation == "calls"
                    && edge.true_source() == source
                    && !edge
                        .extra
                        .get("unresolved_call")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false)
            })
            .map(|edge| edge.true_target())
            .collect()
    }

    #[test]
    fn detected_markdown_suffixes_extract_links() {
        let fixture = Fixture::new();
        for (filename, target) in [
            ("guide.md", "reference.md"),
            ("handbook.markdown", "reference.markdown"),
        ] {
            let markdown = fixture.write(filename, &format!("[Reference]({target})\n"));
            assert!(super::detect::is_supported_path(&markdown));

            let extraction = extract(&markdown, filename);
            let file_id = make_id(&[Path::new(filename)
                .with_extension("")
                .to_string_lossy()
                .as_ref()]);
            let target_id = make_id(&[Path::new(target)
                .with_extension("")
                .to_string_lossy()
                .as_ref()]);
            assert!(
                extraction.nodes.iter().all(|node| node.id != target_id),
                "a raw local link must not fabricate a target node for {target}"
            );
            assert_single_edge(&extraction, &file_id, &target_id, "references");
        }
    }

    #[test]
    fn javascript_extracts_exported_and_variable_bound_declarations() {
        let fixture = Fixture::new();
        let javascript = fixture.write(
            "demo.js",
            r#"
function bareFn() {}
async function bareAsyncFn() {}
const bareArrow = () => {};
const bareAsyncArrow = async () => {};
const bareFnExpr = function () {};
class BareClass { bareMethod() {} }

export function expFn() {}
export async function expAsyncFn() {}
export const expArrow = () => {};
export class ExpClass { expMethod() {} }
export default function defFn() {}
"#,
        );
        let extraction = extract(&javascript, "demo.js");

        let definitions = definition_labels(&extraction);
        assert_eq!(definitions.len(), 13);
        for label in [
            "BareClass",
            "ExpClass",
            "bareArrow()",
            "bareAsyncArrow()",
            "bareAsyncFn()",
            "bareFn()",
            "bareFnExpr()",
            "defFn()",
            "expArrow()",
            "expAsyncFn()",
            "expFn()",
        ] {
            assert!(definitions.contains(&label), "missing definition {label}");
        }

        let file = make_id(&["demo"]);
        for (name, kind) in [
            ("bareFn", "function"),
            ("bareAsyncFn", "function"),
            ("bareArrow", "function"),
            ("bareAsyncArrow", "function"),
            ("bareFnExpr", "function"),
            ("BareClass", "class"),
            ("expFn", "function"),
            ("expAsyncFn", "function"),
            ("expArrow", "function"),
            ("ExpClass", "class"),
            ("defFn", "function"),
        ] {
            let id = make_id(&["demo", name]);
            assert_definition(&extraction, &id, kind);
            assert_single_edge(&extraction, &file, &id, "contains");
        }

        for id in [
            make_id(&["demo", "expFn"]),
            make_id(&["demo", "expAsyncFn"]),
            make_id(&["demo", "expArrow"]),
            make_id(&["demo", "ExpClass"]),
            make_id(&["demo", "defFn"]),
        ] {
            assert_export_status(&extraction, &id, true);
        }
        assert_export_status(&extraction, &make_id(&["demo", "bareFn"]), false);
        assert_export_status(&extraction, &make_id(&["demo", "bareArrow"]), false);

        for (class, method) in [("BareClass", "bareMethod"), ("ExpClass", "expMethod")] {
            let class = make_id(&["demo", class]);
            let method = make_id(&[&class, method]);
            assert_definition(&extraction, &method, "function");
            assert_single_edge(&extraction, &class, &method, "method");
        }
    }

    #[test]
    fn javascript_variable_binding_names_own_their_calls() {
        let fixture = Fixture::new();
        let javascript = fixture.write(
            "calls.js",
            r#"
function helper() {}
export const publicName = function internalName() { helper(); };
"#,
        );
        let extraction = extract(&javascript, "calls.js");

        assert_eq!(
            definition_labels(&extraction),
            vec!["helper()", "publicName()"]
        );
        let public_name = make_id(&["calls", "publicName"]);
        assert_export_status(&extraction, &public_name, true);
        assert!(extraction
            .nodes
            .iter()
            .all(|node| node.id != make_id(&["calls", "internalName"])));
        assert_single_edge(
            &extraction,
            &public_name,
            &make_id(&["calls", "helper"]),
            "calls",
        );
    }

    #[test]
    fn typescript_extracts_exported_variable_bound_functions() {
        let fixture = Fixture::new();
        let typescript = fixture.write(
            "demo.ts",
            r#"
function helper(): void {}
export const typedArrow = async (): Promise<void> => { helper(); };
export const typedFnExpr = function (): void { helper(); };
export class Service {}
"#,
        );
        let extraction = extract(&typescript, "demo.ts");

        assert_eq!(
            definition_labels(&extraction),
            vec!["Service", "helper()", "typedArrow()", "typedFnExpr()"]
        );
        for id in [
            make_id(&["demo", "typedArrow"]),
            make_id(&["demo", "typedFnExpr"]),
            make_id(&["demo", "Service"]),
        ] {
            assert_export_status(&extraction, &id, true);
        }
        assert_export_status(&extraction, &make_id(&["demo", "helper"]), false);

        let helper = make_id(&["demo", "helper"]);
        for caller in ["typedArrow", "typedFnExpr"] {
            assert_single_edge(&extraction, &make_id(&["demo", caller]), &helper, "calls");
        }
    }

    #[test]
    fn javascript_cross_file_direct_calls_resolve_through_imports() {
        let fixture = Fixture::new();
        let library = fixture.write(
            "library.js",
            "export function helper() {}\nexport function open() {}\n",
        );
        let caller = fixture.write(
            "caller.js",
            r#"
import { helper, open } from "./library";
export function run() { helper(); open(); }
"#,
        );
        let mut extractions = vec![
            extract(&library, "library.js"),
            extract(&caller, "caller.js"),
        ];

        super::resolution::resolve(&mut extractions);

        assert_single_edge(
            &extractions[1],
            &make_id(&["caller", "run"]),
            &make_id(&["library", "helper"]),
            "calls",
        );
        assert_single_edge(
            &extractions[1],
            &make_id(&["caller", "run"]),
            &make_id(&["library", "open"]),
            "calls",
        );
    }

    #[test]
    fn go_cross_file_direct_calls_resolve_within_the_package() {
        let fixture = Fixture::new();
        let library = fixture.write("library.go", "package demo\nfunc helper() {}\n");
        let caller = fixture.write("caller.go", "package demo\nfunc run() { helper() }\n");
        let mut extractions = vec![
            extract(&library, "demo/library.go"),
            extract(&caller, "demo/caller.go"),
        ];

        super::resolution::resolve(&mut extractions);

        assert_single_edge(
            &extractions[1],
            &make_id(&["demo/caller", "run"]),
            &make_id(&["demo/library", "helper"]),
            "calls",
        );
    }

    #[test]
    fn compiled_languages_retain_unresolved_direct_call_facts() {
        let fixture = Fixture::new();
        for (filename, source) in [
            ("direct.py", "def caller():\n    missing()\n"),
            ("direct.js", "function caller() { missing(); }\n"),
            ("direct.ts", "function caller(): void { missing(); }\n"),
            ("direct.tsx", "function caller(): void { missing(); }\n"),
            ("direct.go", "package demo\nfunc caller() { missing() }\n"),
            ("direct.rs", "fn caller() { missing(); }\n"),
            (
                "Direct.java",
                "class Direct { void caller() { missing(); } }\n",
            ),
            ("direct.c", "void caller(void) { missing(); }\n"),
            ("direct.cpp", "void caller() { missing(); }\n"),
            ("direct.rb", "def caller\n  missing()\nend\n"),
            (
                "Direct.cs",
                "class Direct { void Caller() { Missing(); } }\n",
            ),
        ] {
            let path = fixture.write(filename, source);
            let extraction = extract(&path, filename);
            let fact = extraction
                .edges
                .iter()
                .find(|edge| {
                    edge.extra
                        .get("unresolved_call")
                        .and_then(|value| value.as_bool())
                        == Some(true)
                })
                .unwrap_or_else(|| panic!("{filename} dropped its unresolved direct call"));
            assert_eq!(
                fact.extra
                    .get("callee")
                    .and_then(|value| value.as_str())
                    .map(str::to_lowercase)
                    .as_deref(),
                Some("missing"),
                "{filename} retained the wrong callee"
            );
            assert_eq!(
                fact.extra
                    .get("member_call")
                    .and_then(|value| value.as_bool()),
                Some(false),
                "{filename} misclassified a direct call as a member call"
            );
        }
    }

    #[test]
    fn ast_parse_recovery_is_visible_on_the_file_anchor() {
        let fixture = Fixture::new();
        let path = fixture.write("broken.js", "function broken( {\n");

        let extraction = extract(&path, "broken.js");
        let file = extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("type").and_then(|value| value.as_str()) == Some("file"))
            .expect("file anchor");

        assert_eq!(
            file.extra
                .get("parser_has_error")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        let diagnostic_nodes = ["parse_error_count", "missing_node_count"]
            .iter()
            .filter_map(|key| file.extra.get(*key).and_then(|value| value.as_u64()))
            .sum::<u64>();
        assert!(diagnostic_nodes > 0, "parser recovery must be quantified");
        assert!(file
            .extra
            .get("parse_error_spans")
            .and_then(|value| value.as_array())
            .is_some_and(|spans| !spans.is_empty()));
    }

    #[test]
    fn rust_2021_raw_references_are_grammar_warnings_not_parse_errors() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "raw_reference.rs",
            "fn consume(_: &str) {}\nfn demo(raw: &str) { consume(&raw); }\n",
        );

        let extraction = extract(&path, "raw_reference.rs");
        let file = extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("type").and_then(|value| value.as_str()) == Some("file"))
            .expect("file anchor");

        assert_eq!(
            file.extra
                .get("parse_error_count")
                .and_then(|value| value.as_u64()),
            Some(0)
        );
        assert_eq!(
            file.extra
                .get("parser_compatibility_count")
                .and_then(|value| value.as_u64()),
            Some(1)
        );
    }

    #[test]
    fn javascript_member_calls_do_not_change_with_same_name_decoys() {
        for (case, extra_decoy) in [("one", ""), ("two", "class Backup { save() {} }")] {
            let fixture = Fixture::new();
            let library = fixture.write("library.js", "export function save() {}\n");
            let path = fixture.write(
                "member.js",
                &format!(
                    "import {{ save }} from './library';\nclass Repo {{ save() {{}} }}\n{extra_decoy}\nfunction caller(other) {{ other.save(); save(); }}\n"
                ),
            );
            let mut extractions =
                vec![extract(&library, "library.js"), extract(&path, "member.js")];

            super::resolution::resolve(&mut extractions);

            let caller = make_id(&["member", "caller"]);
            assert_eq!(
                resolved_call_targets(&extractions[1], &caller),
                vec![make_id(&["library", "save"])],
                "member-call decoys changed direct-call resolution in {case} case"
            );
            let unresolved: Vec<_> = extractions[1]
                .edges
                .iter()
                .filter(|edge| {
                    edge.true_source() == caller
                        && edge
                            .extra
                            .get("unresolved_call")
                            .and_then(|value| value.as_bool())
                            == Some(true)
                })
                .collect();
            assert_eq!(
                unresolved.len(),
                1,
                "expected the unsafe member call to remain auditable in {case} case"
            );
            assert_eq!(
                unresolved[0]
                    .extra
                    .get("member_call")
                    .and_then(|value| value.as_bool()),
                Some(true)
            );
        }
    }

    #[test]
    fn go_member_calls_do_not_change_with_same_name_decoys() {
        for extra_decoy in ["", "type Backup struct{}\nfunc (Backup) Save() {}\n"] {
            let fixture = Fixture::new();
            let library = fixture.write("library.go", "package demo\nfunc Save() {}\n");
            let path = fixture.write(
                "member.go",
                &format!(
                    "package demo\ntype Repo struct{{}}\nfunc (Repo) Save() {{}}\n{extra_decoy}func caller(other any) {{ other.Save(); Save() }}\n"
                ),
            );
            let mut extractions = vec![
                extract(&library, "demo/library.go"),
                extract(&path, "demo/member.go"),
            ];

            super::resolution::resolve(&mut extractions);

            let caller = make_id(&["demo/member", "caller"]);
            assert_eq!(
                resolved_call_targets(&extractions[1], &caller),
                vec![make_id(&["demo/library", "Save"])],
                "member-call decoys changed direct-call resolution"
            );
            assert_eq!(
                extractions[1]
                    .edges
                    .iter()
                    .filter(|edge| {
                        edge.true_source() == caller
                            && edge
                                .extra
                                .get("unresolved_call")
                                .and_then(|value| value.as_bool())
                                == Some(true)
                            && edge
                                .extra
                                .get("member_call")
                                .and_then(|value| value.as_bool())
                                == Some(true)
                    })
                    .count(),
                1,
                "unsafe Go member call should remain an unresolved audit fact"
            );
        }
    }

    #[test]
    fn python_injected_fields_resolve_to_their_typed_methods() {
        let fixture = Fixture::new();
        let ports = fixture.write(
            "ports.py",
            r#"
class InventoryRepository:
    def reserve(self, items): ...
    def release(self, items): ...

class PaymentGateway:
    def charge(self, order_id): ...

class DemoPaymentGateway:
    def charge(self, order_id): ...

class OrderRepository:
    def save(self, order): ...

class InMemoryOrderRepository:
    def save(self, order): ...

class NotificationService:
    def send_confirmation(self, order): ...
"#,
        );
        let checkout_file = fixture.write(
            "checkout.py",
            r#"
from ports import InventoryRepository, NotificationService, OrderRepository, PaymentGateway

class CheckoutService:
    def __init__(
        self,
        inventory: InventoryRepository,
        payments: PaymentGateway,
        orders: OrderRepository,
        notifications: NotificationService,
    ):
        self.inventory = inventory
        self.payments = payments
        self.orders = orders
        self.notifications = notifications

    def checkout(self, order):
        self.inventory.reserve(order.items)
        self.payments.charge(order.order_id)
        self.inventory.release(order.items)
        self.orders.save(order)
        self.notifications.send_confirmation(order)
"#,
        );
        let mut extractions = vec![
            extract(&ports, "ports.py"),
            extract(&checkout_file, "checkout.py"),
        ];
        super::resolution::resolve(&mut extractions);

        let checkout = make_id(&["checkout", "CheckoutService", "checkout"]);
        let expected = [
            (
                make_id(&["ports", "InventoryRepository", "reserve"]),
                "InventoryRepository",
            ),
            (
                make_id(&["ports", "InventoryRepository", "release"]),
                "InventoryRepository",
            ),
            (
                make_id(&["ports", "PaymentGateway", "charge"]),
                "PaymentGateway",
            ),
            (
                make_id(&["ports", "OrderRepository", "save"]),
                "OrderRepository",
            ),
            (
                make_id(&["ports", "NotificationService", "send_confirmation"]),
                "NotificationService",
            ),
        ];

        for (target, receiver_type) in expected {
            let edge = extractions
                .iter()
                .flat_map(|extraction| &extraction.edges)
                .find(|edge| {
                    edge.relation == "calls"
                        && edge.true_source() == checkout
                        && edge.true_target() == target
                })
                .unwrap_or_else(|| panic!("missing injected call from {checkout} to {target}"));
            assert_eq!(
                edge.extra
                    .get("receiver_type")
                    .and_then(|value| value.as_str()),
                Some(receiver_type)
            );
        }
    }
}
