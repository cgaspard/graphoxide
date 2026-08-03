//! Safe graph.json loading and writing.

use crate::{Confidence, Extraction, KnowledgeGraph};
use serde::Serialize;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Default graph-load memory cap (512 MiB), matching upstream Graphify.
pub const DEFAULT_MAX_GRAPH_BYTES: u64 = 512 * 1024 * 1024;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn read_graph(path: impl AsRef<Path>) -> anyhow::Result<KnowledgeGraph> {
    read_graph_with_cap(path, max_graph_bytes())
}

/// Read a graph with an explicit byte cap. This makes embedders and tests able
/// to impose a tighter policy without mutating process-global environment.
pub fn read_graph_with_cap(path: impl AsRef<Path>, cap: u64) -> anyhow::Result<KnowledgeGraph> {
    let path = path.as_ref();
    let size = fs::metadata(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("graph file not found: {}", path.display())
            } else {
                anyhow::anyhow!("Cannot read graph file {}: {error}", path.display())
            }
        })?
        .len();
    anyhow::ensure!(
        size <= cap,
        "graph file {} is {size} bytes, exceeds {cap}-byte cap",
        path.display()
    );
    let bytes = fs::read(path)
        .map_err(|error| anyhow::anyhow!("Cannot read graph file {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "Cannot read graph file {}: {error}. The file may be corrupted; regenerate or rebuild it",
            path.display()
        )
    })
}

/// Read an arbitrary JSON object with the same size cap and actionable corruption
/// diagnostics as graph loading.
pub fn read_json_object(
    path: impl AsRef<Path>,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let path = path.as_ref();
    check_graph_file_size_cap(path)?;
    let bytes = fs::read(path)
        .map_err(|error| anyhow::anyhow!("Cannot parse {}: {error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "Cannot parse {}: {error}. The file may be corrupted; regenerate or rebuild it",
            path.display()
        )
    })?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("diagnostic input {} must be a JSON object", path.display()))
}

pub fn write_graph_atomic(
    path: impl AsRef<Path>,
    graph: &KnowledgeGraph,
    force: bool,
) -> anyhow::Result<bool> {
    let path = path.as_ref();
    if !force && path.exists() {
        let existing = read_graph(path).map_err(|error| {
            anyhow::anyhow!(
                "refusing to overwrite unreadable existing graph {}: {error}",
                path.display()
            )
        })?;
        if graph.nodes.len() < existing.nodes.len() {
            return Ok(false);
        }
    }

    let mut value = serde_json::to_value(graph)?;
    prepare_for_export(&mut value);
    atomic_value(path, &value)?;
    Ok(true)
}

/// Write the raw pre-build format used by `extract --no-cluster` upstream.
pub fn write_raw_extractions_atomic(
    path: impl AsRef<Path>,
    extractions: &[Extraction],
    force: bool,
) -> anyhow::Result<bool> {
    let path = path.as_ref();
    let node_count: usize = extractions.iter().map(|e| e.nodes.len()).sum();
    if !force && path.exists() {
        let existing = read_graph(path).map_err(|error| {
            anyhow::anyhow!(
                "refusing to overwrite unreadable existing graph {}: {error}",
                path.display()
            )
        })?;
        if node_count < existing.nodes.len() {
            return Ok(false);
        }
    }
    let nodes: Vec<_> = extractions.iter().flat_map(|e| e.nodes.iter()).collect();
    let edges: Vec<_> = extractions.iter().flat_map(|e| e.edges.iter()).collect();
    let hyperedges: Vec<_> = extractions
        .iter()
        .flat_map(|e| e.hyperedges.iter())
        .collect();
    let mut value = serde_json::json!({
        "nodes": nodes,
        "edges": edges,
        "hyperedges": hyperedges,
        "input_tokens": 0,
        "output_tokens": 0
    });
    if let Some(edges) = value.get_mut("edges").and_then(|v| v.as_array_mut()) {
        for edge in edges {
            if let Some(edge) = edge.as_object_mut() {
                if let (Some(source), Some(target)) = (edge.remove("_src"), edge.remove("_tgt")) {
                    edge.insert("source".into(), source);
                    edge.insert("target".into(), target);
                }
            }
        }
    }
    atomic_value(path, &value)?;
    Ok(true)
}

fn atomic_value(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    write_json_atomic(path, value, true)
}

/// Atomically write UTF-8 text, preserving an existing destination's mode and
/// writing through a destination symlink instead of replacing the link itself.
pub fn write_text_atomic(path: impl AsRef<Path>, text: &str) -> anyhow::Result<()> {
    atomic_write(
        path.as_ref(),
        |file| file.write_all(text.as_bytes()),
        replace_file,
    )
}

/// Test/embedding hook for simulating a replace failure without weakening the
/// production writer's atomicity guarantees.
#[doc(hidden)]
pub fn write_text_atomic_with_replacer<R>(
    path: impl AsRef<Path>,
    text: &str,
    replace: R,
) -> anyhow::Result<()>
where
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    atomic_write(
        path.as_ref(),
        |file| file.write_all(text.as_bytes()),
        replace,
    )
}

/// Atomically serialize JSON. `pretty` controls indentation; serde_json emits
/// non-ASCII text as UTF-8 rather than replacing it with `\\uXXXX` escapes.
pub fn write_json_atomic(
    path: impl AsRef<Path>,
    value: &impl Serialize,
    pretty: bool,
) -> anyhow::Result<()> {
    atomic_write(
        path.as_ref(),
        |file| {
            if pretty {
                serde_json::to_writer_pretty(file, value).map_err(std::io::Error::other)
            } else {
                serde_json::to_writer(file, value).map_err(std::io::Error::other)
            }
        },
        replace_file,
    )
}

fn atomic_write<W, R>(path: &Path, write: W, replace: R) -> anyhow::Result<()>
where
    W: FnOnce(&mut fs::File) -> std::io::Result<()>,
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let destination = resolve_destination(path)?;
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("graph.json");
    let mut temporary = None;
    let mut file = None;
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(opened) => {
                temporary = Some(candidate);
                file = Some(opened);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let temporary = temporary.ok_or_else(|| {
        anyhow::anyhow!(
            "could not allocate a unique temporary file beside {}",
            destination.display()
        )
    })?;
    let mut file = file.expect("temporary path and file are created together");
    let result = (|| -> anyhow::Result<()> {
        if let Ok(metadata) = fs::metadata(&destination) {
            fs::set_permissions(&temporary, metadata.permissions())?;
        }
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        replace(&temporary, &destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn resolve_destination(path: &Path) -> std::io::Result<PathBuf> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return fs::canonicalize(path);
    }
    Ok(path.to_path_buf())
}

/// Replace `destination` with a fully-written temporary file.
///
/// Windows does not consistently permit `rename` over an existing file. In
/// that case, retain upstream compatibility with a copy-and-sync fallback.
pub fn replace_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    match fs::rename(temporary, destination) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            #[cfg(windows)]
            {
                permission_fallback(temporary, destination).map_err(|copy_error| {
                    std::io::Error::new(
                        copy_error.kind(),
                        format!(
                            "rename failed ({rename_error}); copy fallback failed: {copy_error}"
                        ),
                    )
                })
            }
            #[cfg(not(windows))]
            {
                Err(rename_error)
            }
        }
    }
}

/// Copy, sync, and delete a completed temporary file. This is the Windows
/// fallback when atomic replace is blocked by a transient destination lock.
#[doc(hidden)]
pub fn permission_fallback(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    fs::copy(temporary, destination)?;
    fs::OpenOptions::new()
        .write(true)
        .open(destination)?
        .sync_all()?;
    fs::remove_file(temporary)
}

fn prepare_for_export(value: &mut serde_json::Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    root.entry("graph").or_insert_with(|| serde_json::json!({}));
    if let Some(nodes) = root.get_mut("nodes").and_then(|v| v.as_array_mut()) {
        for node in nodes {
            let Some(node) = node.as_object_mut() else {
                continue;
            };
            let label = node
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            node.entry("norm_label")
                .or_insert_with(|| strip_diacritics(&label).to_lowercase().into());
        }
    }
    if let Some(edges) = root.get_mut("links").and_then(|v| v.as_array_mut()) {
        for edge in edges {
            let Some(edge) = edge.as_object_mut() else {
                continue;
            };
            let score = edge
                .get("confidence")
                .and_then(|v| v.as_str())
                .map(|v| match v {
                    "INFERRED" => Confidence::Inferred.default_score(),
                    "AMBIGUOUS" => Confidence::Ambiguous.default_score(),
                    _ => Confidence::Extracted.default_score(),
                })
                .unwrap_or(1.0);
            edge.entry("confidence_score")
                .or_insert_with(|| score.into());
            if let (Some(source), Some(target)) = (edge.remove("_src"), edge.remove("_tgt")) {
                edge.insert("source".into(), source);
                edge.insert("target".into(), target);
            }
            edge.remove("target_file");
            edge.remove("local_alias");
        }
    }
}

fn strip_diacritics(value: &str) -> String {
    use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
    value.nfd().filter(|ch| !is_combining_mark(*ch)).collect()
}

/// Return the effective graph size cap. Graphoxide's variable takes precedence;
/// the upstream variable remains accepted for compatibility with existing setups.
pub fn max_graph_bytes() -> u64 {
    let raw = std::env::var("GRAPHOXIDE_MAX_GRAPH_BYTES")
        .ok()
        .or_else(|| std::env::var("GRAPHIFY_MAX_GRAPH_BYTES").ok());
    parse_max_graph_bytes(raw.as_deref())
}

/// Parse the graph cap's plain-byte, MB, or GB form using binary multipliers.
pub fn parse_max_graph_bytes(raw: Option<&str>) -> u64 {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return DEFAULT_MAX_GRAPH_BYTES;
    };
    let text = raw.to_ascii_uppercase();
    let (number, multiplier) = if let Some(value) = text.strip_suffix("GB") {
        (value.trim(), 1024_u64 * 1024 * 1024)
    } else if let Some(value) = text.strip_suffix("MB") {
        (value.trim(), 1024_u64 * 1024)
    } else {
        (text.trim(), 1)
    };
    number
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .and_then(|value| value.checked_mul(multiplier))
        .unwrap_or(DEFAULT_MAX_GRAPH_BYTES)
}

/// Reject a readable graph file strictly larger than the configured cap. A
/// missing or unreadable file is left to the caller's existence/read error.
pub fn check_graph_file_size_cap(path: impl AsRef<Path>) -> anyhow::Result<()> {
    check_graph_file_size_cap_with(path.as_ref(), max_graph_bytes())
}

/// Explicit-cap form used by callers that impose a tighter policy.
pub fn check_graph_file_size_cap_with(path: &Path, cap: u64) -> anyhow::Result<()> {
    let Ok(size) = fs::metadata(path).map(|metadata| metadata.len()) else {
        return Ok(());
    };
    anyhow::ensure!(
        size <= cap,
        "graph file {} is {size} bytes, exceeds {cap}-byte cap",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{check_graph_file_size_cap_with, prepare_for_export};

    #[test]
    fn export_restores_direction_and_backfills_fields() {
        let mut value = serde_json::json!({
            "nodes": [{"id": "n", "label": "Crème", "file_type": "code", "source_file": "a.py"}],
            "links": [{
                "source": "wrong", "target": "order", "relation": "calls",
                "confidence": "INFERRED", "_src": "a", "_tgt": "b",
                "target_file": "transient"
            }],
            "hyperedges": []
        });
        prepare_for_export(&mut value);
        assert_eq!(value["nodes"][0]["norm_label"], "creme");
        assert_eq!(value["links"][0]["source"], "a");
        assert_eq!(value["links"][0]["target"], "b");
        assert_eq!(value["links"][0]["confidence_score"], 0.5);
        assert!(value["links"][0].get("_src").is_none());
        assert!(value["links"][0].get("target_file").is_none());
        assert!(value.get("graph").is_some());
    }

    #[test]
    fn test_query_cli_rejects_oversized_graph() {
        let path = std::env::temp_dir().join(format!(
            "graphoxide-oversized-graph-{}-{}.json",
            std::process::id(),
            super::TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, br#"{\"nodes\":[],\"links\":[]}"#)
            .expect("write oversized graph fixture");
        let error = check_graph_file_size_cap_with(&path, 16).expect_err("must enforce byte cap");
        std::fs::remove_file(&path).expect("remove oversized graph fixture");
        let message = error.to_string();
        assert!(message.contains("exceeds"));
        assert!(message.contains("byte cap"));
    }
}
