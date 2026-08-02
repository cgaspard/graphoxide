//! Safe graph.json loading and writing.

use crate::{Confidence, Extraction, KnowledgeGraph};
use std::{fs, io::Write, path::Path};

const DEFAULT_MAX_GRAPH_BYTES: u64 = 512 * 1024 * 1024;

pub fn read_graph(path: impl AsRef<Path>) -> anyhow::Result<KnowledgeGraph> {
    let path = path.as_ref();
    let size = fs::metadata(path)?.len();
    let cap = max_graph_bytes();
    anyhow::ensure!(
        size <= cap,
        "graph file {} is {size} bytes, exceeds {cap}-byte cap",
        path.display()
    );
    Ok(serde_json::from_slice(&fs::read(path)?)?)
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
    let bytes = serde_json::to_vec_pretty(value)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("graph.json");
    let temp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        replace_file(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
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
                fs::copy(temporary, destination).map_err(|copy_error| {
                    std::io::Error::new(
                        copy_error.kind(),
                        format!(
                            "rename failed ({rename_error}); copy fallback failed: {copy_error}"
                        ),
                    )
                })?;
                fs::OpenOptions::new()
                    .write(true)
                    .open(destination)?
                    .sync_all()?;
                fs::remove_file(temporary)
            }
            #[cfg(not(windows))]
            {
                Err(rename_error)
            }
        }
    }
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

fn max_graph_bytes() -> u64 {
    let Ok(raw) = std::env::var("GRAPHOXIDE_MAX_GRAPH_BYTES") else {
        return DEFAULT_MAX_GRAPH_BYTES;
    };
    let raw = raw.trim();
    if let Some(gb) = raw.strip_suffix("GB").or_else(|| raw.strip_suffix("gb")) {
        return gb
            .trim()
            .parse::<u64>()
            .ok()
            .and_then(|v| v.checked_mul(1024 * 1024 * 1024))
            .unwrap_or(DEFAULT_MAX_GRAPH_BYTES);
    }
    raw.parse().unwrap_or(DEFAULT_MAX_GRAPH_BYTES)
}

#[cfg(test)]
mod tests {
    use super::prepare_for_export;

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
}
