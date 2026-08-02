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

pub mod cache;
pub mod detect;
pub mod engine;
mod fallback;
pub mod languages;
pub mod resolution;

pub use detect::collect_files;
pub use engine::extract;

/// Collect and extract a project in parallel, storing repo-relative paths.
pub fn extract_project(root: &std::path::Path) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    extract_project_with_options(root, false)
}

/// Extract a project, optionally bypassing the AST cache for a true full scan.
pub fn extract_project_with_options(
    root: &std::path::Path,
    force: bool,
) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    use md5::Digest as _;
    use rayon::prelude::*;
    let files = collect_files(root)?;
    let rows: anyhow::Result<Vec<_>> = files
        .par_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(path)?;
            let extraction = if !force {
                cache::ast_cache_get(root, &relative, &bytes)
            } else {
                None
            };
            let extraction = if let Some(cached) = extraction {
                cached
            } else {
                let extracted = engine::extract_as(path, &relative)?;
                cache::ast_cache_put(root, &relative, &bytes, &extracted)?;
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
    let previous = cache::load_manifest(root);
    let manifest = rows
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
    cache::save_manifest(root, &manifest)?;
    let mut extractions: Vec<_> = rows
        .into_iter()
        .map(|(_, extraction, _, _)| extraction)
        .collect();
    resolution::resolve(&mut extractions);
    Ok(extractions)
}
