//! Safe graph.json loading and writing.

use crate::{Confidence, Extraction, KnowledgeGraph};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Default graph-load memory cap (512 MiB), matching upstream Graphify.
pub const DEFAULT_MAX_GRAPH_BYTES: u64 = 512 * 1024 * 1024;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn read_graph(path: impl AsRef<Path>) -> anyhow::Result<KnowledgeGraph> {
    read_graph_with_cap(path, max_graph_bytes())
}

/// One graph generation admitted through a single bounded file handle.
#[derive(Debug)]
pub struct CappedGraphRead {
    /// Parsed graph from the admitted bytes.
    pub graph: KnowledgeGraph,
    /// Exact number of source bytes admitted before deserialization.
    pub admitted_bytes: usize,
    /// SHA-256 of exactly the admitted source bytes.
    pub sha256: [u8; 32],
}

/// Read a graph with an explicit byte cap. This makes embedders and tests able
/// to impose a tighter policy without mutating process-global environment.
pub fn read_graph_with_cap(path: impl AsRef<Path>, cap: u64) -> anyhow::Result<KnowledgeGraph> {
    Ok(read_graph_capped(path, cap)?.graph)
}

/// Read one graph generation with an explicit cap, returning accounting and
/// digest evidence from the exact byte slice that was deserialized.
pub fn read_graph_capped(path: impl AsRef<Path>, cap: u64) -> anyhow::Result<CappedGraphRead> {
    let path = path.as_ref();
    let file = fs::File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("graph file not found: {}", path.display())
        } else {
            anyhow::anyhow!("Cannot read graph file {}: {error}", path.display())
        }
    })?;
    read_graph_from_open_file_with_cap(path, file, cap)
}

fn read_graph_from_open_file_with_cap(
    path: &Path,
    mut file: fs::File,
    cap: u64,
) -> anyhow::Result<CappedGraphRead> {
    // Inspect and read the same opened object. In particular, an atomic path
    // replacement cannot swap in an unchecked file between these operations.
    let size = file
        .metadata()
        .map_err(|error| anyhow::anyhow!("Cannot read graph file {}: {error}", path.display()))?
        .len();
    anyhow::ensure!(
        size <= cap,
        "graph file {} is {size} bytes, exceeds {cap}-byte cap",
        path.display()
    );
    let bytes = read_bytes_with_cap(path, &mut file, cap, "Cannot read graph file")?;
    let admitted_bytes = bytes.len();
    let sha256 = Sha256::digest(&bytes).into();
    let graph = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "Cannot read graph file {}: {error}. The file may be corrupted; regenerate or rebuild it",
            path.display()
        )
    })?;
    Ok(CappedGraphRead {
        graph,
        admitted_bytes,
        sha256,
    })
}

fn read_bytes_with_cap(
    path: &Path,
    reader: impl Read,
    cap: u64,
    read_error_prefix: &str,
) -> anyhow::Result<Vec<u8>> {
    // Metadata is only a snapshot: an in-place writer may grow the opened file
    // after the check. Read at most cap + 1 so growth is detected before JSON
    // deserialization and no input bytes beyond that bounded prefix are admitted.
    let read_limit = cap.saturating_add(1);
    let mut bounded = reader.take(read_limit);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .map_err(|error| anyhow::anyhow!("{read_error_prefix} {}: {error}", path.display()))?;
    let bytes_read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    anyhow::ensure!(
        bytes_read <= cap,
        "graph file {} is at least {bytes_read} bytes, exceeds {cap}-byte cap",
        path.display()
    );
    Ok(bytes)
}

/// Read an arbitrary JSON object with the same size cap and actionable corruption
/// diagnostics as graph loading.
pub fn read_json_object(
    path: impl AsRef<Path>,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    read_json_object_with_cap(path, max_graph_bytes())
}

/// Read an arbitrary JSON object through one opened handle with an explicit
/// byte cap.
///
/// Metadata and bytes are taken from the same filesystem object, and the read
/// admits at most `cap + 1` bytes so an in-place growth race is rejected before
/// deserialization.
pub fn read_json_object_with_cap(
    path: impl AsRef<Path>,
    cap: u64,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let path = path.as_ref();
    let file = fs::File::open(path)
        .map_err(|error| anyhow::anyhow!("Cannot parse {}: {error}", path.display()))?;
    read_json_object_from_open_file_with_cap(path, file, cap)
}

fn read_json_object_from_open_file_with_cap(
    path: &Path,
    mut file: fs::File,
    cap: u64,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let size = file
        .metadata()
        .map_err(|error| anyhow::anyhow!("Cannot parse {}: {error}", path.display()))?
        .len();
    read_json_object_from_reader_with_cap(path, &mut file, size, cap)
}

fn read_json_object_from_reader_with_cap(
    path: &Path,
    reader: impl Read,
    observed_size: u64,
    cap: u64,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    anyhow::ensure!(
        observed_size <= cap,
        "graph file {} is {observed_size} bytes, exceeds {cap}-byte cap",
        path.display()
    );
    let bytes = read_bytes_with_cap(path, reader, cap, "Cannot parse")?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "Cannot parse {}: {error}. The file may be corrupted; re-run 'graphoxide extract'",
            path.display()
        )
    })?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("diagnostic input must be a JSON object"))
}

pub fn write_graph_atomic(
    path: impl AsRef<Path>,
    graph: &KnowledgeGraph,
    force: bool,
) -> anyhow::Result<bool> {
    let path = path.as_ref();
    write_graph_atomic_with(path, graph, force, atomic_value)
}

/// Atomically write a graph without following a destination link or falling
/// back to an in-place copy when replacement is unavailable.
pub fn write_graph_atomic_strict(
    path: impl AsRef<Path>,
    graph: &KnowledgeGraph,
    force: bool,
) -> anyhow::Result<bool> {
    let path = path.as_ref();
    validate_strict_destination(path)?;
    write_graph_atomic_with(path, graph, force, atomic_value_strict)
}

/// Test/embedding hook for strict graph publication with an injected replace.
#[doc(hidden)]
pub fn write_graph_atomic_strict_with_replacer<R>(
    path: impl AsRef<Path>,
    graph: &KnowledgeGraph,
    force: bool,
    replace: R,
) -> anyhow::Result<bool>
where
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let path = path.as_ref();
    validate_strict_destination(path)?;
    write_graph_atomic_with(path, graph, force, |path, value| {
        write_json_atomic_strict_with_replacer(path, value, true, replace)
    })
}

fn write_graph_atomic_with<W>(
    path: &Path,
    graph: &KnowledgeGraph,
    force: bool,
    write: W,
) -> anyhow::Result<bool>
where
    W: FnOnce(&Path, &serde_json::Value) -> anyhow::Result<()>,
{
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
    write(path, &value)?;
    Ok(true)
}

/// Write the raw pre-build format used by `extract --no-cluster` upstream.
pub fn write_raw_extractions_atomic(
    path: impl AsRef<Path>,
    extractions: &[Extraction],
    force: bool,
) -> anyhow::Result<bool> {
    let path = path.as_ref();
    write_raw_extractions_atomic_with(path, extractions, force, atomic_value)
}

/// Strict raw-graph writer for transactional publication workflows.
pub fn write_raw_extractions_atomic_strict(
    path: impl AsRef<Path>,
    extractions: &[Extraction],
    force: bool,
) -> anyhow::Result<bool> {
    let path = path.as_ref();
    validate_strict_destination(path)?;
    write_raw_extractions_atomic_with(path, extractions, force, atomic_value_strict)
}

/// Test/embedding hook for strict raw-graph publication with an injected
/// replace operation.
#[doc(hidden)]
pub fn write_raw_extractions_atomic_strict_with_replacer<R>(
    path: impl AsRef<Path>,
    extractions: &[Extraction],
    force: bool,
    replace: R,
) -> anyhow::Result<bool>
where
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let path = path.as_ref();
    validate_strict_destination(path)?;
    write_raw_extractions_atomic_with(path, extractions, force, |path, value| {
        write_json_atomic_strict_with_replacer(path, value, true, replace)
    })
}

fn write_raw_extractions_atomic_with<W>(
    path: &Path,
    extractions: &[Extraction],
    force: bool,
    write: W,
) -> anyhow::Result<bool>
where
    W: FnOnce(&Path, &serde_json::Value) -> anyhow::Result<()>,
{
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
            if let Some(edge) = edge.as_object_mut()
                && let (Some(source), Some(target)) = (edge.remove("_src"), edge.remove("_tgt"))
            {
                edge.insert("source".into(), source);
                edge.insert("target".into(), target);
            }
        }
    }
    write(path, &value)?;
    Ok(true)
}

fn atomic_value(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    write_json_atomic(path, value, true)
}

fn atomic_value_strict(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    write_json_atomic_strict(path, value, true)
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

/// Atomically serialize JSON without link following or an in-place copy
/// fallback when the final filesystem replacement cannot be performed.
pub fn write_json_atomic_strict(
    path: impl AsRef<Path>,
    value: &impl Serialize,
    pretty: bool,
) -> anyhow::Result<()> {
    write_json_atomic_strict_with_replacer(path, value, pretty, replace_file_strict)
}

/// Test/embedding hook for strict JSON publication with an injected replace.
#[doc(hidden)]
pub fn write_json_atomic_strict_with_replacer<R>(
    path: impl AsRef<Path>,
    value: &impl Serialize,
    pretty: bool,
    replace: R,
) -> anyhow::Result<()>
where
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    atomic_write_strict(
        path.as_ref(),
        |file| {
            if pretty {
                serde_json::to_writer_pretty(file, value).map_err(std::io::Error::other)
            } else {
                serde_json::to_writer(file, value).map_err(std::io::Error::other)
            }
        },
        replace,
    )
}

fn atomic_write<W, R>(path: &Path, write: W, replace: R) -> anyhow::Result<()>
where
    W: FnOnce(&mut fs::File) -> std::io::Result<()>,
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let destination = resolve_destination(path)?;
    atomic_write_destination(&destination, write, replace)
}

fn atomic_write_strict<W, R>(path: &Path, write: W, replace: R) -> anyhow::Result<()>
where
    W: FnOnce(&mut fs::File) -> std::io::Result<()>,
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    validate_strict_destination(path)?;
    atomic_write_destination(path, write, replace)
}

fn atomic_write_destination<W, R>(destination: &Path, write: W, replace: R) -> anyhow::Result<()>
where
    W: FnOnce(&mut fs::File) -> std::io::Result<()>,
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
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
        if let Ok(metadata) = fs::metadata(destination) {
            fs::set_permissions(&temporary, metadata.permissions())?;
        }
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        replace(&temporary, destination)?;
        // Sync the containing directory so the rename itself is durable.
        // Without this, a crash after the rename but before the OS caches
        // the directory entry can lose the publication entirely.
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Fsync a directory to make metadata changes (rename, create, unlink) durable.
///
/// On platforms where directory fsync is unsupported or fails with
/// `ENOTSUP`/`EINVAL` (some FUSE/overlay filesystems), the call is a no-op
/// so that callers are never blocked by an unsupported operation.
fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let file = fs::OpenOptions::new().read(true).open(path)?;
        // `sync_all` on a directory fd issues `fsync(2)`.
        if let Err(error) = file.sync_all() {
            // Some filesystems (e.g. 9p, certain FUSE mounts) return
            // ENOTSUP or EINVAL for directory fsync. Treat those as
            // non-fatal so the write is not rejected for an unsupported op.
            if !matches!(
                error.raw_os_error(),
                Some(code) if code == libc::ENOTSUP || code == libc::EINVAL
            ) {
                return Err(error);
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Windows: directory metadata is journaled by the NTFS log;
        // no explicit directory sync is needed.
        let _ = path;
    }
    Ok(())
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

fn validate_strict_destination(path: &Path) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing symlinked publication destination {}",
                path.display()
            ),
        )),
        Ok(metadata) if !metadata.file_type().is_file() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing non-file publication destination {}",
                path.display()
            ),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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

/// Replace a destination in one filesystem operation, with no copy fallback.
///
/// Both paths must name entries in the same directory. On Windows,
/// MOVEFILE_WRITE_THROUGH ensures the move reaches storage before the
/// publication sequence proceeds.
#[cfg(not(windows))]
pub fn replace_file_strict(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(temporary, destination)
}

/// Windows strict replacement using the platform's replace-existing move.
#[cfg(windows)]
pub fn replace_file_strict(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both buffers remain live and are NUL-terminated UTF-16 paths.
    let replaced = unsafe {
        MoveFileExW(
            temporary.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
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
    use super::{
        check_graph_file_size_cap_with, prepare_for_export, read_bytes_with_cap,
        read_graph_from_open_file_with_cap, read_graph_with_cap, read_json_object,
        read_json_object_from_open_file_with_cap, read_json_object_from_reader_with_cap,
    };
    use sha2::{Digest as _, Sha256};
    use std::{
        fs::{self, OpenOptions},
        io::{Seek, Write},
    };
    use tempfile::tempdir;

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

    #[test]
    fn read_graph_with_cap_rejects_oversize_before_deserializing() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join("graph.json");
        let contents = b"not valid graph json";
        fs::write(&path, contents).expect("write oversized invalid graph");

        let cap = 4;
        let error = read_graph_with_cap(&path, cap).expect_err("oversized graph must fail");
        assert_eq!(
            error.to_string(),
            format!(
                "graph file {} is {} bytes, exceeds {cap}-byte cap",
                path.display(),
                contents.len()
            )
        );
    }

    #[test]
    fn opened_graph_handle_is_used_for_metadata_and_reading() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join("graph.json");
        let contents = br#"{"nodes":[],"links":[]}"#;
        fs::write(&path, contents).expect("write graph");
        let file = fs::File::open(&path).expect("open graph");

        // This path intentionally does not exist. It is diagnostic context only;
        // metadata and content must both come from the already-opened handle.
        let diagnostic_path = temp.path().join("replacement.json");
        let admitted = read_graph_from_open_file_with_cap(
            &diagnostic_path,
            file,
            u64::try_from(contents.len()).expect("fixture length fits in u64"),
        )
        .expect("opened graph remains readable");
        assert!(admitted.graph.nodes.is_empty());
        assert!(admitted.graph.links.is_empty());
        assert_eq!(admitted.admitted_bytes, contents.len());
        assert_eq!(
            admitted.sha256.as_slice(),
            Sha256::digest(contents).as_slice()
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_graph_reports_bytes_from_its_generation_after_path_replacement() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join("graph.json");
        let replacement = temp.path().join("replacement.json");
        let mut admitted_contents = br#"{"nodes":[],"links":[]}"#.to_vec();
        admitted_contents.extend(std::iter::repeat_n(b' ', 4096));
        let replacement_contents = br#"{"nodes":[],"links":[]}"#;
        fs::write(&path, &admitted_contents).expect("write admitted graph generation");
        fs::write(&replacement, replacement_contents).expect("write small replacement graph");
        let file = fs::File::open(&path).expect("open admitted graph generation");
        fs::rename(&replacement, &path).expect("atomically replace graph path");
        assert_eq!(
            fs::metadata(&path).expect("replacement metadata").len(),
            u64::try_from(replacement_contents.len()).expect("fixture length fits in u64")
        );

        let admitted = read_graph_from_open_file_with_cap(
            &path,
            file,
            u64::try_from(admitted_contents.len()).expect("fixture length fits in u64"),
        )
        .expect("opened graph generation remains readable");
        assert_eq!(admitted.admitted_bytes, admitted_contents.len());
        assert_eq!(
            admitted.sha256.as_slice(),
            Sha256::digest(&admitted_contents).as_slice()
        );
        assert!(admitted.admitted_bytes > replacement_contents.len());
    }

    #[test]
    fn bounded_reader_rejects_growth_after_metadata_check() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join("graph.json");
        let contents = br#"{"nodes":[],"links":[]}"#;
        fs::write(&path, contents).expect("write graph");
        let mut reader = fs::File::open(&path).expect("open graph");
        let cap = reader.metadata().expect("read checked metadata").len();
        assert_eq!(
            cap,
            u64::try_from(contents.len()).expect("fixture length fits in u64")
        );

        // Simulate an in-place writer growing the file after the metadata check.
        // Whitespace keeps the complete document valid JSON, so the byte cap is
        // the reason this read fails.
        let mut writer = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open graph for append");
        writer
            .write_all(&[b' '; 8 * 1024])
            .expect("grow graph after metadata check");
        writer.flush().expect("flush appended graph bytes");

        let error = read_bytes_with_cap(&path, &mut reader, cap, "Cannot read graph file")
            .expect_err("growth beyond the checked cap must fail");
        assert_eq!(
            error.to_string(),
            format!(
                "graph file {} is at least {} bytes, exceeds {cap}-byte cap",
                path.display(),
                cap + 1
            )
        );
        assert_eq!(
            reader.stream_position().expect("inspect bounded read"),
            cap + 1,
            "the bounded reader must not consume the rest of the grown file"
        );
    }

    #[test]
    fn opened_json_object_handle_is_used_for_metadata_and_reading() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join("diagnostic.json");
        let contents = br#"{"status":"ready"}"#;
        fs::write(&path, contents).expect("write diagnostic object");
        let file = fs::File::open(&path).expect("open diagnostic object");

        let diagnostic_path = temp.path().join("replacement.json");
        let object = read_json_object_from_open_file_with_cap(
            &diagnostic_path,
            file,
            u64::try_from(contents.len()).expect("fixture length fits in u64"),
        )
        .expect("opened diagnostic object remains readable");
        assert_eq!(object.get("status"), Some(&serde_json::json!("ready")));
    }

    #[cfg(unix)]
    #[test]
    fn opened_json_object_handle_ignores_a_path_replacement() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join("diagnostic.json");
        let replacement = temp.path().join("replacement.json");
        let approved = br#"{"status":"approved"}"#;
        fs::write(&path, approved).expect("write approved diagnostic object");
        fs::write(&replacement, br#"{"status":"replacement"}"#)
            .expect("write replacement diagnostic object");
        let file = fs::File::open(&path).expect("open approved diagnostic object");
        fs::rename(&replacement, &path).expect("replace diagnostic path");

        let object = read_json_object_from_open_file_with_cap(
            &path,
            file,
            u64::try_from(approved.len()).expect("fixture length fits in u64"),
        )
        .expect("the already-opened object remains the diagnostic input");
        assert_eq!(object.get("status"), Some(&serde_json::json!("approved")));
    }

    #[test]
    fn json_object_reader_rejects_growth_after_metadata_snapshot() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join("diagnostic.json");
        let contents = br#"{"status":"ready"}"#;
        fs::write(&path, contents).expect("write diagnostic object");
        let mut reader = fs::File::open(&path).expect("open diagnostic object");
        let observed_size = reader.metadata().expect("read checked metadata").len();
        let mut writer = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open diagnostic object for append");
        writer
            .write_all(&[b' '; 8 * 1024])
            .expect("grow diagnostic object after metadata snapshot");
        writer.flush().expect("flush appended diagnostic bytes");

        let error =
            read_json_object_from_reader_with_cap(&path, &mut reader, observed_size, observed_size)
                .expect_err("growth beyond the checked cap must fail before JSON parsing");
        assert_eq!(
            error.to_string(),
            format!(
                "graph file {} is at least {} bytes, exceeds {observed_size}-byte cap",
                path.display(),
                observed_size + 1
            )
        );
    }

    #[test]
    fn read_json_object_rejects_oversize_before_deserializing() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join("diagnostic.json");
        let contents = b"not valid diagnostic json";
        fs::write(&path, contents).expect("write oversized diagnostic object");
        let file = fs::File::open(&path).expect("open diagnostic object");

        let cap = 4;
        let error = read_json_object_from_open_file_with_cap(&path, file, cap)
            .expect_err("oversized diagnostic object must fail");
        assert_eq!(
            error.to_string(),
            format!(
                "graph file {} is {} bytes, exceeds {cap}-byte cap",
                path.display(),
                contents.len()
            )
        );
    }

    #[test]
    fn read_json_object_preserves_parse_specific_corruption_diagnostic() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join("diagnostic.json");
        fs::write(&path, br#"{"unfinished":"#).expect("write corrupt diagnostic object");

        let message = read_json_object(&path)
            .expect_err("corrupt diagnostic object must fail")
            .to_string();
        assert!(message.starts_with(&format!("Cannot parse {}:", path.display())));
        assert!(message.contains("corrupted"));
        assert!(!message.contains("Cannot read graph file"));
    }
}
