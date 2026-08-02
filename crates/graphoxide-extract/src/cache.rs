//! Incremental manifest and content-addressed AST cache.

use graphoxide_core::Extraction;
use md5::{Digest as _, Md5};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{collections::BTreeMap, fs, io::Write, path::Path};
pub const AST_CACHE_VERSION: u32 = 24;
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub mtime: f64,
    #[serde(default)]
    pub ast_hash: String,
    #[serde(default)]
    pub semantic_hash: String,
}
pub type Manifest = BTreeMap<String, ManifestEntry>;

pub fn load_manifest(root: &Path) -> Manifest {
    let path = root.join("graphoxide-out/manifest.json");
    fs::read(path)
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_default()
}
pub fn changed_files(
    _root: &Path,
    files: &[(String, std::path::PathBuf)],
    manifest: &Manifest,
) -> anyhow::Result<Vec<(String, std::path::PathBuf, String, f64)>> {
    let mut changed = Vec::new();
    for (relative, path) in files {
        let metadata = fs::metadata(path)?;
        let mtime = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        if let Some(old) = manifest.get(relative) {
            if old.mtime == mtime && !old.ast_hash.is_empty() {
                continue;
            }
        }
        let bytes = fs::read(path)?;
        let hash = format!("{:x}", Md5::digest(&bytes));
        if manifest
            .get(relative)
            .is_some_and(|old| old.ast_hash == hash)
        {
            continue;
        }
        changed.push((relative.clone(), path.clone(), hash, mtime));
    }
    Ok(changed)
}
pub fn save_manifest(root: &Path, entries: &Manifest) -> anyhow::Result<()> {
    atomic_json(&root.join("graphoxide-out/manifest.json"), entries)
}
pub fn ast_cache_get(root: &Path, relative: &str, bytes: &[u8]) -> Option<Extraction> {
    if bypass(relative) {
        return None;
    }
    let path = cache_path(root, relative, bytes);
    fs::read(path)
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
}
pub fn ast_cache_put(
    root: &Path,
    relative: &str,
    bytes: &[u8],
    value: &Extraction,
) -> anyhow::Result<()> {
    if bypass(relative) || value.nodes.is_empty() {
        return Ok(());
    }
    atomic_json(&cache_path(root, relative, bytes), value)
}
fn cache_path(root: &Path, relative: &str, bytes: &[u8]) -> std::path::PathBuf {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.update(b"\0");
    hash.update(relative.to_lowercase().as_bytes());
    root.join(format!(
        "graphoxide-out/cache/ast/v{AST_CACHE_VERSION}/{}.json",
        hex::encode(hash.finalize())
    ))
}
fn bypass(relative: &str) -> bool {
    [
        ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".vue", ".svelte",
    ]
    .iter()
    .any(|suffix| relative.to_lowercase().ends_with(suffix))
}
fn atomic_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        graphoxide_core::replace_file(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn javascript_bypasses_cache() {
        assert!(bypass("src/a.ts"));
        assert!(!bypass("src/a.py"));
    }
}
