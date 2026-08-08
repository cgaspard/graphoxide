//! Incremental manifest and content-addressed AST cache.

use graphoxide_core::Extraction;
use graphoxide_index_runtime::cache::{
    RuntimeCache, RuntimeCacheHit, RuntimeCacheIoPersistOutcome, RuntimeCacheIoService,
    RuntimeCacheIoServiceError, RuntimeCacheKey,
};
use md5::{Digest as _, Md5};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
};
// Bump whenever a built-in extractor's persisted fact schema changes. Stage 3
// replaces the partial DOT scanner with semantic Graphviz facts, so v25 entries
// must not be replayed into an incremental build.
pub const AST_CACHE_VERSION: u32 = 26;

/// Binary framing for future cache artifacts.
///
/// The current on-disk cache remains JSON for compatibility. Readers accept
/// this frame in addition to legacy JSON so a later append-only cache can
/// reuse the validation boundary without breaking existing entries.
const CACHE_FRAME_MAGIC: [u8; 8] = *b"GOXCACHE";
const CACHE_FRAME_VERSION: u8 = 1;
const CACHE_FRAME_ALGORITHM_BLAKE3: u8 = 1;
const CACHE_FRAME_HEADER_LEN: usize = 56;

/// Largest framed cache payload accepted by the default decoder.
///
/// The legacy JSON cache deliberately has no new size policy. This limit is
/// only for the new framed representation, whose header advertises its size
/// before a decoder is allowed to inspect the payload. I/O owners may use a
/// smaller limit for a particular queue or cache partition.
pub const DEFAULT_CACHE_FRAME_MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Why a framed cache record could not be used. Callers must treat these as a
/// cache miss rather than allowing malformed cache data to reach a decoder.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum CacheFrameError {
    #[error("truncated cache frame header")]
    TruncatedHeader,
    #[error("unsupported cache frame version {0}")]
    UnsupportedVersion(u8),
    #[error("unsupported cache frame digest algorithm {0}")]
    UnsupportedAlgorithm(u8),
    #[error("cache frame reserved bytes are non-zero")]
    ReservedBytes,
    #[error("cache frame payload length does not match the record")]
    LengthMismatch,
    #[error(
        "cache frame payload is {payload_len} bytes, exceeding the {max_payload_bytes}-byte limit"
    )]
    PayloadTooLarge {
        payload_len: u64,
        max_payload_bytes: usize,
    },
    #[error("cache frame CRC32 mismatch")]
    ChecksumMismatch,
    #[error("cache frame BLAKE3 mismatch")]
    DigestMismatch,
}

/// Frame cache bytes with a version, CRC, and BLAKE3 digest.
///
/// The resulting record is intentionally self-contained, has no allocations
/// beyond the returned buffer, and is safe to pass directly between an I/O
/// owner and a cache decoder. The current JSON cache writers do not call this
/// yet; it is an additive compatibility boundary for the runtime cache.
pub fn frame_cache_bytes(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(CACHE_FRAME_HEADER_LEN + payload.len());
    frame.extend_from_slice(&CACHE_FRAME_MAGIC);
    frame.push(CACHE_FRAME_VERSION);
    frame.push(CACHE_FRAME_ALGORITHM_BLAKE3);
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    frame.extend_from_slice(&crate::bytes::crc32(payload).to_le_bytes());
    frame.extend_from_slice(&crate::bytes::blake3_digest(payload));
    frame.extend_from_slice(payload);
    frame
}

/// Frame a cache artifact only when its payload fits the caller's byte budget.
///
/// New runtime cache writers should use this function so an artifact accepted
/// for writing is also accepted by the corresponding bounded reader. The
/// infallible [`frame_cache_bytes`] remains for compatibility with the initial
/// framing API and test fixtures.
pub fn try_frame_cache_bytes(
    payload: &[u8],
    max_payload_bytes: usize,
) -> Result<Vec<u8>, CacheFrameError> {
    if payload.len() > max_payload_bytes {
        return Err(CacheFrameError::PayloadTooLarge {
            payload_len: payload.len() as u64,
            max_payload_bytes,
        });
    }
    Ok(frame_cache_bytes(payload))
}

/// Return a validated framed payload, or `Ok(None)` for a legacy unframed
/// value. The returned slice borrows `bytes`; decoding therefore does not copy
/// cache payloads before JSON deserialization.
pub fn unframe_cache_bytes(bytes: &[u8]) -> Result<Option<&[u8]>, CacheFrameError> {
    unframe_cache_bytes_with_limit(bytes, DEFAULT_CACHE_FRAME_MAX_PAYLOAD_BYTES)
}

/// Return a validated framed payload subject to a caller-owned size limit.
///
/// This function validates the complete header and declared payload size
/// before checksumming or deserializing. It returns `Ok(None)` for legacy JSON
/// bytes so existing cache entries continue to use their historical contract.
/// A malformed frame is an error for observability; cache callers deliberately
/// map that error to a cache miss through [`cache_payload`].
pub fn unframe_cache_bytes_with_limit(
    bytes: &[u8],
    max_payload_bytes: usize,
) -> Result<Option<&[u8]>, CacheFrameError> {
    if !bytes.starts_with(&CACHE_FRAME_MAGIC) {
        return Ok(None);
    }
    if bytes.len() < CACHE_FRAME_HEADER_LEN {
        return Err(CacheFrameError::TruncatedHeader);
    }
    if bytes[8] != CACHE_FRAME_VERSION {
        return Err(CacheFrameError::UnsupportedVersion(bytes[8]));
    }
    if bytes[9] != CACHE_FRAME_ALGORITHM_BLAKE3 {
        return Err(CacheFrameError::UnsupportedAlgorithm(bytes[9]));
    }
    if bytes[10..12] != [0, 0] {
        return Err(CacheFrameError::ReservedBytes);
    }
    let payload_len = u64::from_le_bytes(bytes[12..20].try_into().expect("fixed header"));
    let payload_len = usize::try_from(payload_len).map_err(|_| CacheFrameError::LengthMismatch)?;
    let record_len = CACHE_FRAME_HEADER_LEN
        .checked_add(payload_len)
        .ok_or(CacheFrameError::LengthMismatch)?;
    if bytes.len() != record_len {
        return Err(CacheFrameError::LengthMismatch);
    }
    if payload_len > max_payload_bytes {
        return Err(CacheFrameError::PayloadTooLarge {
            payload_len: payload_len as u64,
            max_payload_bytes,
        });
    }
    let expected_crc = u32::from_le_bytes(bytes[20..24].try_into().expect("fixed header"));
    let expected_digest = &bytes[24..56];
    let payload = &bytes[CACHE_FRAME_HEADER_LEN..];
    if crate::bytes::crc32(payload) != expected_crc {
        return Err(CacheFrameError::ChecksumMismatch);
    }
    if crate::bytes::blake3_digest(payload).as_slice() != expected_digest {
        return Err(CacheFrameError::DigestMismatch);
    }
    Ok(Some(payload))
}

fn cache_payload(bytes: &[u8]) -> Option<&[u8]> {
    match unframe_cache_bytes(bytes) {
        Ok(Some(payload)) => Some(payload),
        Ok(None) => Some(bytes),
        Err(_) => None,
    }
}

fn decode_cache_json(bytes: &[u8]) -> Option<Value> {
    serde_json::from_slice(cache_payload(bytes)?).ok()
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub mtime: f64,
    /// Persisted extractor fact-schema version.
    ///
    /// Legacy manifests omit this field and deserialize as version zero so
    /// they are treated as cache misses after schema-aware manifests ship.
    #[serde(default)]
    pub ast_version: u32,
    #[serde(default)]
    pub ast_hash: String,
    #[serde(default)]
    pub semantic_hash: String,
}
pub type Manifest = BTreeMap<String, ManifestEntry>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StatIndexEntry {
    size: u64,
    mtime_ns: u64,
    #[serde(default)]
    hashes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    word_count: Option<usize>,
}

#[derive(Debug, Clone)]
struct StatIndex {
    anchor: PathBuf,
    entries: BTreeMap<PathBuf, StatIndexEntry>,
    /// Cache-file writes for a given root must remain serialized after the
    /// registry lock is released; otherwise two snapshots can race and lose
    /// an entry. The lock is per index, not process-global.
    flush_lock: Arc<Mutex<()>>,
}

impl Default for StatIndex {
    fn default() -> Self {
        Self {
            anchor: PathBuf::new(),
            entries: BTreeMap::new(),
            flush_lock: Arc::new(Mutex::new(())),
        }
    }
}

type StatIndexKey = (PathBuf, PathBuf);

static STAT_INDEXES: OnceLock<RwLock<BTreeMap<StatIndexKey, StatIndex>>> = OnceLock::new();

fn stat_indexes() -> &'static RwLock<BTreeMap<StatIndexKey, StatIndex>> {
    STAT_INDEXES.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Strip a leading, well-formed YAML frontmatter block without changing any
/// byte after the closing delimiter. Delimiters must be whole `---` lines;
/// thematic breaks (`----`) and prose (`--- title`) are ordinary content.
pub fn body_content(content: &[u8]) -> Vec<u8> {
    markdown_body_start(content)
        .map(|start| content[start..].to_vec())
        .unwrap_or_else(|| content.to_vec())
}

/// Return the first byte after the closing YAML frontmatter delimiter.
/// Keeping this as a slice offset lets content hashing avoid allocating a
/// second copy of a Markdown file merely to skip metadata.
fn markdown_body_start(content: &[u8]) -> Option<usize> {
    fn delimiter(line: &[u8]) -> bool {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let end = line
            .iter()
            .rposition(|byte| !matches!(*byte, b' ' | b'\t'))
            .map_or(0, |index| index + 1);
        &line[..end] == b"---"
    }

    let first_end = content
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(content.len(), |index| index + 1);
    if !delimiter(&content[..first_end]) {
        return None;
    }

    let mut start = first_end;
    while start < content.len() {
        let end = content[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(content.len(), |index| start + index + 1);
        if delimiter(&content[start..end]) {
            return Some(start + 3);
        }
        start = end;
    }
    None
}

/// SHA-256 over effective file contents plus a lower-cased, root-relative path.
/// Markdown frontmatter is intentionally excluded from the content portion so
/// metadata-only edits do not invalidate expensive extraction results.
pub fn file_hash(path: &Path, root: &Path) -> anyhow::Result<String> {
    file_hash_at(path, root, root)
}

/// SHA-256 content key using bytes already read by an I/O owner.
///
/// This is equivalent to [`file_hash`] for a current regular file while
/// avoiding a second source read at cache boundaries. The size check prevents
/// callers from accidentally keying an entry with an incomplete read.
pub fn file_hash_from_bytes(path: &Path, root: &Path, bytes: &[u8]) -> anyhow::Result<String> {
    file_hash_from_bytes_at(path, root, root, bytes)
}

struct FileHashContext {
    resolved: PathBuf,
    root_resolved: PathBuf,
    index_root: PathBuf,
    index_key: StatIndexKey,
    salt: String,
    size: u64,
    mtime_ns: u64,
}

fn file_hash_context(
    path: &Path,
    root: &Path,
    index_root: &Path,
) -> anyhow::Result<FileHashContext> {
    anyhow::ensure!(
        path.is_file(),
        "file_hash requires a regular file: {}",
        path.display()
    );
    let resolved = fs::canonicalize(path)?;
    let root_resolved = fs::canonicalize(root).unwrap_or_else(|_| absolute_lexical(root));
    let index_root = absolute_lexical(index_root);
    let index_key = (root_resolved.clone(), index_root.clone());
    let salt_path = resolved.strip_prefix(&root_resolved).unwrap_or(&resolved);
    let salt = salt_path
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    let (size, mtime_ns) = stat_signature(&resolved)?;
    ensure_stat_index_loaded(&root_resolved, &index_root)?;
    Ok(FileHashContext {
        resolved,
        root_resolved,
        index_root,
        index_key,
        salt,
        size,
        mtime_ns,
    })
}

fn indexed_file_hash(context: &FileHashContext) -> Option<String> {
    stat_indexes()
        .read()
        .expect("stat index rwlock poisoned")
        .get(&context.index_key)
        .and_then(|index| index.entries.get(&context.resolved))
        .filter(|entry| entry.size == context.size && entry.mtime_ns == context.mtime_ns)
        .and_then(|entry| entry.hashes.get(&context.salt))
        .cloned()
}

fn hash_bytes_with_salt(path: &Path, bytes: &[u8], salt: &str) -> String {
    let content = if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
    {
        markdown_body_start(bytes)
            .map(|start| &bytes[start..])
            .unwrap_or(bytes)
    } else {
        bytes
    };
    let mut hash = Sha256::new();
    hash.update(content);
    hash.update(b"\0");
    hash.update(salt.as_bytes());
    hex::encode(hash.finalize())
}

fn record_file_hash(context: &FileHashContext, digest: String) -> anyhow::Result<String> {
    {
        let mut indexes = stat_indexes().write().expect("stat index rwlock poisoned");
        let index = indexes
            .get_mut(&context.index_key)
            .expect("stat index was loaded");
        let entry = index.entries.entry(context.resolved.clone()).or_default();
        if entry.size != context.size || entry.mtime_ns != context.mtime_ns {
            *entry = StatIndexEntry {
                size: context.size,
                mtime_ns: context.mtime_ns,
                ..StatIndexEntry::default()
            };
        }
        entry.hashes.insert(context.salt.clone(), digest.clone());
    }
    flush_stat_index_at(&context.root_resolved, &context.index_root)?;
    Ok(digest)
}

fn file_hash_at(path: &Path, root: &Path, index_root: &Path) -> anyhow::Result<String> {
    let context = file_hash_context(path, root, index_root)?;
    if let Some(hash) = indexed_file_hash(&context) {
        return Ok(hash);
    }
    let raw = fs::read(path)?;
    record_file_hash(&context, hash_bytes_with_salt(path, &raw, &context.salt))
}

fn file_hash_from_bytes_at(
    path: &Path,
    root: &Path,
    index_root: &Path,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let context = file_hash_context(path, root, index_root)?;
    anyhow::ensure!(
        bytes.len() as u64 == context.size,
        "provided bytes do not match file size for {}",
        path.display()
    );
    if let Some(hash) = indexed_file_hash(&context) {
        return Ok(hash);
    }
    record_file_hash(&context, hash_bytes_with_salt(path, bytes, &context.salt))
}

/// Return a cached word count while the file's size and nanosecond mtime are
/// unchanged. The count shares the portable stat-index entry used by hashes.
pub fn cached_word_count<F>(path: &Path, root: &Path, compute: F) -> anyhow::Result<usize>
where
    F: FnOnce(&Path) -> anyhow::Result<usize>,
{
    anyhow::ensure!(
        path.is_file(),
        "word-count input is not a file: {}",
        path.display()
    );
    let resolved = fs::canonicalize(path)?;
    let root_resolved = fs::canonicalize(root).unwrap_or_else(|_| absolute_lexical(root));
    let index_root = absolute_lexical(root);
    let index_key = (root_resolved.clone(), index_root.clone());
    let (size, mtime_ns) = stat_signature(&resolved)?;
    ensure_stat_index_loaded(&root_resolved, &index_root)?;
    if let Some(word_count) = stat_indexes()
        .read()
        .expect("stat index rwlock poisoned")
        .get(&index_key)
        .and_then(|index| index.entries.get(&resolved))
        .filter(|entry| entry.size == size && entry.mtime_ns == mtime_ns)
        .and_then(|entry| entry.word_count)
    {
        return Ok(word_count);
    }
    let word_count = compute(path)?;
    {
        let mut indexes = stat_indexes().write().expect("stat index rwlock poisoned");
        let index = indexes.get_mut(&index_key).expect("stat index was loaded");
        let entry = index.entries.entry(resolved).or_default();
        if entry.size != size || entry.mtime_ns != mtime_ns {
            *entry = StatIndexEntry {
                size,
                mtime_ns,
                ..StatIndexEntry::default()
            };
        }
        entry.word_count = Some(word_count);
    }
    flush_stat_index_at(&root_resolved, &index_root)?;
    Ok(word_count)
}

/// Persist the in-memory stat index with root-relative POSIX keys and prune
/// entries for files that no longer exist.
pub fn flush_stat_index(root: &Path) -> anyhow::Result<()> {
    let anchor = fs::canonicalize(root).unwrap_or_else(|_| absolute_lexical(root));
    let index_root = absolute_lexical(root);
    flush_stat_index_at(&anchor, &index_root)
}

fn flush_stat_index_at(anchor: &Path, index_root: &Path) -> anyhow::Result<()> {
    let key = (anchor.to_path_buf(), index_root.to_path_buf());
    ensure_stat_index_loaded(anchor, index_root)?;
    let flush_lock = stat_indexes()
        .read()
        .expect("stat index rwlock poisoned")
        .get(&key)
        .expect("stat index was loaded")
        .flush_lock
        .clone();
    let _flush = flush_lock.lock().expect("stat index flush mutex poisoned");
    // Metadata probes may block on a remote or contended filesystem. Gather
    // stale paths under a shared lock, probe them without the registry lock,
    // then apply only those removals while taking the short write lock.
    let paths = stat_indexes()
        .read()
        .expect("stat index rwlock poisoned")
        .get(&key)
        .expect("stat index was loaded")
        .entries
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let stale = paths
        .into_iter()
        .filter(|path| !path.is_file())
        .collect::<BTreeSet<_>>();
    let stored = {
        let mut indexes = stat_indexes().write().expect("stat index rwlock poisoned");
        let index = indexes.get_mut(&key).expect("stat index was loaded");
        index.entries.retain(|path, _| !stale.contains(path));
        index
            .entries
            .iter()
            .map(|(path, entry)| (stat_storage_key(path, &index.anchor), entry.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    graphoxide_core::write_json_atomic(stat_index_path(index_root), &stored, true)
}

fn stat_signature(path: &Path) -> anyhow::Result<(u64, u64)> {
    let metadata = fs::metadata(path)?;
    let nanos = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    Ok((metadata.len(), nanos))
}

fn stat_index_path(root: &Path) -> PathBuf {
    root.join("graphoxide-out/cache/stat-index.json")
}

fn stat_storage_key(path: &Path, anchor: &Path) -> String {
    path.strip_prefix(anchor)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn ensure_stat_index_loaded(anchor: &Path, index_root: &Path) -> anyhow::Result<()> {
    let key = (anchor.to_path_buf(), index_root.to_path_buf());
    if stat_indexes()
        .read()
        .expect("stat index rwlock poisoned")
        .contains_key(&key)
    {
        return Ok(());
    }
    // Do not hold the process-wide registry lock during cache I/O or JSON
    // decoding. Concurrent first readers may duplicate this cheap load, but
    // the first writer wins and all callers observe one stable index.
    let raw: BTreeMap<String, StatIndexEntry> = fs::read(stat_index_path(index_root))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let mut entries = BTreeMap::new();
    // Legacy absolute keys load first; a portable relative entry for the same
    // file is authoritative and overwrites it in the second pass.
    for relative_pass in [false, true] {
        for (stored, entry) in &raw {
            let normalized = stored.replace('\\', "/");
            let path = PathBuf::from(&normalized);
            if path.is_absolute() == relative_pass {
                continue;
            }
            let absolute = if path.is_absolute() {
                path
            } else {
                anchor.join(path)
            };
            entries.insert(absolute, entry.clone());
        }
    }
    let mut indexes = stat_indexes().write().expect("stat index rwlock poisoned");
    indexes.entry(key).or_insert_with(|| StatIndex {
        anchor: anchor.to_owned(),
        entries,
        flush_lock: Arc::new(Mutex::new(())),
    });
    Ok(())
}

/// Resolve and create a portable extraction-cache namespace.
pub fn cache_dir(
    root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
) -> anyhow::Result<PathBuf> {
    cache_dir_with_ast_version(root, kind, prompt_fingerprint, AST_CACHE_VERSION)
}

/// Variant used by upgrade tests and callers that need to stage a cache for a
/// specific extractor schema version.
pub fn cache_dir_with_ast_version(
    root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
    ast_version: u32,
) -> anyhow::Result<PathBuf> {
    let mut directory = root.join("graphoxide-out/cache");
    if kind == "ast" {
        directory.push("ast");
        fs::create_dir_all(&directory)?;
        let current_name = format!("v{ast_version}");
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let name = entry.file_name();
            if file_type.is_dir()
                && name.to_string_lossy().starts_with('v')
                && name != current_name.as_str()
            {
                fs::remove_dir_all(entry.path())?;
            } else if file_type.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
            {
                fs::remove_file(entry.path())?;
            }
        }
        directory.push(current_name);
    } else {
        directory.push(kind);
        if let Some(fingerprint) = prompt_fingerprint {
            directory.push(format!("p{fingerprint}"));
        }
    }
    fs::create_dir_all(&directory)?;
    Ok(directory)
}

const ROOT_MARKER: &str = "$graphoxide-root$";
const CANONICAL_SOURCE_ROOT_MARKER: &str = "_graphoxide_cache_canonical_source_root";

fn absolute_lexical(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn root_path_spellings(root: &Path) -> Vec<PathBuf> {
    let lexical = absolute_lexical(root);
    let mut spellings = vec![lexical.clone()];
    if let Ok(canonical) = fs::canonicalize(&lexical)
        && canonical != lexical
    {
        spellings.push(canonical);
    }
    spellings
}

fn root_string_spellings(root: &Path) -> Vec<String> {
    let mut spellings = Vec::new();
    for path in root_path_spellings(root) {
        let raw = path.to_string_lossy().into_owned();
        for spelling in [raw.clone(), raw.replace('\\', "/")] {
            if !spellings.contains(&spelling) {
                spellings.push(spelling);
            }
        }
    }
    spellings
}

fn rewrite_strings(value: &mut Value, transform: &impl Fn(&str) -> String) {
    match value {
        Value::String(string) => *string = transform(string),
        Value::Array(values) => {
            for value in values {
                rewrite_strings(value, transform);
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                rewrite_strings(value, transform);
            }
        }
        _ => {}
    }
}

fn for_source_items(value: &mut Value, mut visit: impl FnMut(&mut serde_json::Map<String, Value>)) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    for bucket in ["nodes", "edges", "hyperedges", "raw_calls"] {
        let Some(items) = object.get_mut(bucket).and_then(Value::as_array_mut) else {
            continue;
        };
        for item in items {
            if let Some(item) = item.as_object_mut() {
                visit(item);
            }
        }
    }
}

fn relativize_source_files(value: &mut Value, root: &Path) {
    let roots = root_path_spellings(root);
    for_source_items(value, |item| {
        let Some(source) = item.get("source_file").and_then(Value::as_str) else {
            return;
        };
        let normalized = source.replace('\\', "/");
        let source_path = Path::new(&normalized);
        if !source_path.is_absolute() {
            if normalized != source {
                item.insert("source_file".into(), Value::String(normalized));
            }
            return;
        }
        let Some((root_index, relative)) = roots.iter().enumerate().find_map(|(index, root)| {
            source_path
                .strip_prefix(root)
                .ok()
                .map(|path| (index, path))
        }) else {
            return;
        };
        if root_index > 0 {
            item.insert(CANONICAL_SOURCE_ROOT_MARKER.into(), Value::Bool(true));
        }
        item.insert(
            "source_file".into(),
            Value::String(relative.to_string_lossy().replace('\\', "/")),
        );
    });
}

fn absolutize_source_files(value: &mut Value, root: &Path) {
    let lexical_root = absolute_lexical(root);
    let canonical_root = fs::canonicalize(&lexical_root).unwrap_or_else(|_| lexical_root.clone());
    for_source_items(value, |item| {
        let canonical = item
            .remove(CANONICAL_SOURCE_ROOT_MARKER)
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let Some(source) = item.get("source_file").and_then(Value::as_str) else {
            return;
        };
        if Path::new(source).is_absolute() {
            return;
        }
        item.insert(
            "source_file".into(),
            Value::String(
                if canonical {
                    &canonical_root
                } else {
                    &lexical_root
                }
                .join(source)
                .to_string_lossy()
                .into_owned(),
            ),
        );
    });
}

fn anchor_portable_strings(value: &mut Value, root: &Path) {
    let root_paths = root_string_spellings(root);
    let root_ids = root_paths
        .iter()
        .map(|path| graphoxide_core::normalize_id(path))
        .filter(|id| !id.is_empty())
        .collect::<BTreeSet<_>>();
    rewrite_strings(value, &|string| {
        for root_path in &root_paths {
            if string == root_path {
                return ROOT_MARKER.into();
            }
            for separator in ['/', '\\'] {
                let prefix = format!("{root_path}{separator}");
                if let Some(rest) = string.strip_prefix(&prefix) {
                    return format!("{ROOT_MARKER}/{}", rest.replace('\\', "/"));
                }
            }
        }
        for root_id in &root_ids {
            let prefix = format!("{root_id}_");
            if let Some(rest) = string.strip_prefix(&prefix) {
                return format!("{ROOT_MARKER}_{rest}");
            }
        }
        string.into()
    });
}

fn restore_portable_strings(value: &mut Value, root: &Path) {
    let root = absolute_lexical(root);
    let root_path = root.to_string_lossy().into_owned();
    let root_id = graphoxide_core::normalize_id(&root_path);
    rewrite_strings(value, &|string| {
        let Some(rest) = string.strip_prefix(ROOT_MARKER) else {
            return string.into();
        };
        if rest.is_empty() {
            root_path.clone()
        } else if let Some(path_tail) = rest.strip_prefix('/') {
            root.join(path_tail).to_string_lossy().into_owned()
        } else if rest.starts_with('_') {
            format!("{root_id}{rest}")
        } else {
            string.into()
        }
    });
}

/// Save arbitrary extraction JSON under its current portable content key.
pub fn save_cached_value(
    path: &Path,
    value: &Value,
    root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    save_cached_value_at(path, value, root, root, kind, prompt_fingerprint)
}

/// Save a cache value keyed from source bytes that were already read by an I/O
/// owner. This avoids a second file read while preserving the legacy JSON
/// cache layout and SHA-256 key contract.
pub fn save_cached_value_from_bytes(
    path: &Path,
    source_bytes: &[u8],
    value: &Value,
    root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    save_cached_value_from_bytes_at(
        path,
        source_bytes,
        value,
        root,
        root,
        kind,
        prompt_fingerprint,
    )
}

/// Byte-aware variant of [`save_cached_value_at`] for a storage root that is
/// distinct from the portable content-key root.
pub fn save_cached_value_from_bytes_at(
    path: &Path,
    source_bytes: &[u8],
    value: &Value,
    root: &Path,
    cache_root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let hash = file_hash_from_bytes_at(path, root, cache_root, source_bytes)?;
    let entry =
        cache_dir_with_ast_version(cache_root, kind, prompt_fingerprint, AST_CACHE_VERSION)?
            .join(format!("{hash}.json"));
    let mut portable = value.clone();
    relativize_source_files(&mut portable, root);
    anchor_portable_strings(&mut portable, root);
    atomic_json(&entry, &portable)
}

/// Save an entry while keeping its portable content-key anchor (`root`)
/// independent from the directory that owns the cache (`cache_root`).
pub fn save_cached_value_at(
    path: &Path,
    value: &Value,
    root: &Path,
    cache_root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    save_cached_value_at_with_version(
        path,
        value,
        root,
        cache_root,
        kind,
        prompt_fingerprint,
        AST_CACHE_VERSION,
    )
}

fn save_cached_value_at_with_version(
    path: &Path,
    value: &Value,
    root: &Path,
    cache_root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
    ast_version: u32,
) -> anyhow::Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let hash = file_hash_at(path, root, cache_root)?;
    let entry = cache_dir_with_ast_version(cache_root, kind, prompt_fingerprint, ast_version)?
        .join(format!("{hash}.json"));
    let mut portable = value.clone();
    relativize_source_files(&mut portable, root);
    anchor_portable_strings(&mut portable, root);
    atomic_json(&entry, &portable)
}

pub fn save_cached_value_with_version(
    path: &Path,
    value: &Value,
    root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
    ast_version: u32,
) -> anyhow::Result<()> {
    save_cached_value_at_with_version(
        path,
        value,
        root,
        root,
        kind,
        prompt_fingerprint,
        ast_version,
    )
}

/// Load arbitrary extraction JSON only when the current content key still
/// matches the saved entry. Corrupt entries degrade to cache misses.
pub fn load_cached_value(
    path: &Path,
    root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
) -> Option<Value> {
    load_cached_value_at(path, root, root, kind, prompt_fingerprint)
}

/// Load a cache value with a key calculated from caller-owned source bytes.
/// The helper reads only the cache artifact; it never rereads `path` to derive
/// the content key.
pub fn load_cached_value_from_bytes(
    path: &Path,
    source_bytes: &[u8],
    root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
) -> Option<Value> {
    load_cached_value_from_bytes_at(path, source_bytes, root, root, kind, prompt_fingerprint)
}

/// Byte-aware variant of [`load_cached_value_at`] for a storage root that is
/// distinct from the portable content-key root.
pub fn load_cached_value_from_bytes_at(
    path: &Path,
    source_bytes: &[u8],
    root: &Path,
    cache_root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
) -> Option<Value> {
    let hash = file_hash_from_bytes_at(path, root, cache_root, source_bytes).ok()?;
    load_cached_value_for_hash(
        &hash,
        root,
        cache_root,
        kind,
        prompt_fingerprint,
        AST_CACHE_VERSION,
        false,
    )
}

/// Load an entry whose hash/path identity is anchored at `root` but whose
/// bytes are stored below `cache_root`.
pub fn load_cached_value_at(
    path: &Path,
    root: &Path,
    cache_root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
) -> Option<Value> {
    load_cached_value_internal(
        path,
        root,
        cache_root,
        kind,
        prompt_fingerprint,
        AST_CACHE_VERSION,
        false,
    )
}

/// Diagnostic read that exposes a partial semantic entry without promoting it
/// to a normal cache hit.
pub fn load_cached_value_allow_partial(
    path: &Path,
    root: &Path,
    cache_root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
) -> Option<Value> {
    load_cached_value_internal(
        path,
        root,
        cache_root,
        kind,
        prompt_fingerprint,
        AST_CACHE_VERSION,
        true,
    )
}

pub fn load_cached_value_with_version(
    path: &Path,
    root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
    ast_version: u32,
) -> Option<Value> {
    load_cached_value_internal(
        path,
        root,
        root,
        kind,
        prompt_fingerprint,
        ast_version,
        false,
    )
}

fn load_cached_value_internal(
    path: &Path,
    root: &Path,
    cache_root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
    ast_version: u32,
    allow_partial: bool,
) -> Option<Value> {
    let hash = file_hash_at(path, root, cache_root).ok()?;
    load_cached_value_for_hash(
        &hash,
        root,
        cache_root,
        kind,
        prompt_fingerprint,
        ast_version,
        allow_partial,
    )
}

fn load_cached_value_for_hash(
    hash: &str,
    root: &Path,
    cache_root: &Path,
    kind: &str,
    prompt_fingerprint: Option<&str>,
    ast_version: u32,
    allow_partial: bool,
) -> Option<Value> {
    let entry = cache_dir_with_ast_version(cache_root, kind, prompt_fingerprint, ast_version)
        .ok()?
        .join(format!("{hash}.json"));
    let mut value = fs::read(entry)
        .ok()
        .and_then(|bytes| decode_cache_json(&bytes))?;
    if !allow_partial
        && kind.starts_with("semantic")
        && value.get("partial").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    restore_portable_strings(&mut value, root);
    absolutize_source_files(&mut value, root);
    Some(value)
}

fn visit_json_files(directory: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            visit_json_files(&path, output);
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            output.push(path);
        }
    }
}

/// Return every content hash represented by a cache entry in any namespace.
pub fn cached_files(root: &Path) -> BTreeSet<String> {
    let mut entries = Vec::new();
    visit_json_files(&root.join("graphoxide-out/cache"), &mut entries);
    entries
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) != Some("stat-index.json"))
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_owned))
        .collect()
}

/// Remove cache JSON entries recursively while leaving unrelated files and the
/// namespace directories themselves intact.
pub fn clear_cache(root: &Path) -> anyhow::Result<usize> {
    let mut entries = Vec::new();
    visit_json_files(&root.join("graphoxide-out/cache"), &mut entries);
    let mut removed = 0;
    for path in entries {
        fs::remove_file(&path)?;
        if path.file_name().and_then(|name| name.to_str()) != Some("stat-index.json") {
            removed += 1;
        }
    }
    let index_root = absolute_lexical(root);
    stat_indexes()
        .write()
        .expect("stat index rwlock poisoned")
        .retain(|(_, storage_root), _| storage_root != &index_root);
    Ok(removed)
}

/// Stable twelve-hex-character identity for a semantic extraction prompt.
/// Line endings and trailing whitespace are normalized so checkouts on
/// different platforms share the same semantic-cache vintage.
pub fn prompt_fingerprint(prompt: &str) -> String {
    let normalized = prompt
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    let digest = Sha256::digest(normalized.as_bytes());
    hex::encode(digest)[..12].to_owned()
}

pub fn prompt_file_fingerprint(path: &Path) -> anyhow::Result<String> {
    Ok(prompt_fingerprint(&fs::read_to_string(path)?))
}

#[derive(Debug, Clone, Default)]
pub struct SemanticCacheOptions {
    /// Optional storage root, independent from the corpus/key anchor passed to
    /// `save_semantic_cache` and `check_semantic_cache`.
    pub cache_root: Option<PathBuf>,
    pub mode: Option<String>,
    pub prompt: Option<String>,
    pub prompt_file: Option<PathBuf>,
    pub merge_existing: bool,
    pub allowed_source_files: Option<BTreeSet<PathBuf>>,
    pub partial_source_files: Option<BTreeSet<PathBuf>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SemanticSaveReport {
    pub saved: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SemanticCacheCheck {
    pub nodes: Vec<Value>,
    pub edges: Vec<Value>,
    pub hyperedges: Vec<Value>,
    pub uncached: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Whether any semantic item in a grouped cache payload carries the internal
/// truncation marker.
pub fn group_has_partial_marker(group: &Value) -> bool {
    ["nodes", "edges", "hyperedges"].into_iter().any(|bucket| {
        group
            .get(bucket)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|item| item.get("_partial").and_then(Value::as_bool) == Some(true))
    })
}

fn semantic_kind(mode: Option<&str>) -> String {
    mode.map_or_else(|| "semantic".into(), |mode| format!("semantic-{mode}"))
}

fn semantic_prompt_fingerprint(options: &SemanticCacheOptions) -> (Option<String>, Option<String>) {
    if let Some(path) = options.prompt_file.as_deref() {
        return match prompt_file_fingerprint(path) {
            Ok(fingerprint) => (Some(fingerprint), None),
            Err(error) => (
                None,
                Some(format!(
                    "could not read extraction prompt {:?} ({error}); falling back to the unattributed semantic-cache layout",
                    path
                )),
            ),
        };
    }
    (options.prompt.as_deref().map(prompt_fingerprint), None)
}

fn resolved_source_path(root: &Path, source: &Path) -> PathBuf {
    let path = if source.is_absolute() {
        source.to_path_buf()
    } else {
        absolute_lexical(root).join(source)
    };
    fs::canonicalize(&path).unwrap_or(path)
}

fn normalized_source_file(root: &Path, source: &str) -> String {
    let normalized = source.replace('\\', "/");
    let path = Path::new(&normalized);
    if !path.is_absolute() {
        return normalized;
    }
    root_path_spellings(root)
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .map_or_else(
            || normalized.clone(),
            |relative| relative.to_string_lossy().replace('\\', "/"),
        )
}

fn normalized_semantic_item(item: &Value, root: &Path) -> Value {
    let mut item = item.clone();
    let Some(object) = item.as_object_mut() else {
        return item;
    };
    let Some(source) = object.get("source_file").and_then(Value::as_str) else {
        return item;
    };
    object.insert(
        "source_file".into(),
        Value::String(normalized_source_file(root, source)),
    );
    item
}

fn item_source(item: &Value) -> Option<&str> {
    item.get("source_file").and_then(Value::as_str)
}

fn item_id(item: &Value) -> Option<&str> {
    item.get("id").and_then(Value::as_str)
}

fn append_unique(destination: &mut Vec<Value>, source: impl IntoIterator<Item = Value>) {
    for value in source {
        if !destination.contains(&value) {
            destination.push(value);
        }
    }
}

/// Persist semantic results one source file at a time, with optional mode and
/// prompt namespaces. Scoped writes reject model-attributed files outside the
/// dispatched set and remove references to skipped node definitions.
pub fn save_semantic_cache(
    nodes: &[Value],
    edges: &[Value],
    hyperedges: &[Value],
    root: &Path,
    options: &SemanticCacheOptions,
) -> anyhow::Result<SemanticSaveReport> {
    let mut report = SemanticSaveReport::default();
    let cache_root = options.cache_root.as_deref().unwrap_or(root);
    let kind = semantic_kind(options.mode.as_deref());
    let (fingerprint, prompt_warning) = semantic_prompt_fingerprint(options);
    if let Some(warning) = prompt_warning {
        report.warnings.push(warning);
    }

    let mut groups: BTreeMap<String, [Vec<Value>; 3]> = BTreeMap::new();
    for (bucket, values) in [nodes, edges, hyperedges].into_iter().enumerate() {
        for item in values {
            let item = normalized_semantic_item(item, root);
            let Some(source) = item_source(&item).map(str::to_owned) else {
                continue;
            };
            groups.entry(source).or_default()[bucket].push(item);
        }
    }

    let allowed = options.allowed_source_files.as_ref().map(|paths| {
        paths
            .iter()
            .map(|path| resolved_source_path(root, path))
            .collect::<BTreeSet<_>>()
    });
    let partial = options.partial_source_files.as_ref().map(|paths| {
        paths
            .iter()
            .map(|path| resolved_source_path(root, path))
            .collect::<BTreeSet<_>>()
    });
    if let Some(partial) = &partial {
        let present = groups
            .keys()
            .map(|source| resolved_source_path(root, Path::new(source)))
            .collect::<BTreeSet<_>>();
        for path in partial.difference(&present) {
            groups
                .entry(path.to_string_lossy().into_owned())
                .or_default();
        }
    }
    let group_is_skipped = |source: &str| {
        let path = resolved_source_path(root, Path::new(source));
        !path.is_file()
            || allowed
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(&path))
    };

    let mut skipped_ids = BTreeSet::new();
    let mut written_ids = BTreeSet::new();
    if allowed.is_some() {
        for (source, buckets) in &groups {
            let destination = if group_is_skipped(source) {
                &mut skipped_ids
            } else {
                &mut written_ids
            };
            destination.extend(buckets[0].iter().filter_map(item_id).map(str::to_owned));
        }
        skipped_ids.retain(|id| !written_ids.contains(id));
    }

    let candidate_groups = groups.len();
    for (source, mut buckets) in groups {
        if group_is_skipped(&source) {
            if allowed.is_some() && resolved_source_path(root, Path::new(&source)).is_file() {
                report.warnings.push(format!(
                    "rejected out-of-scope source_file '{source}' from semantic cache write"
                ));
            }
            continue;
        }

        if allowed.is_some() && !skipped_ids.is_empty() {
            buckets[1].retain(|edge| {
                let source = edge.get("source").and_then(Value::as_str);
                let target = edge.get("target").and_then(Value::as_str);
                !source.is_some_and(|id| skipped_ids.contains(id))
                    && !target.is_some_and(|id| skipped_ids.contains(id))
            });
            buckets[2].retain(|hyperedge| {
                let members = hyperedge
                    .get("nodes")
                    .or_else(|| hyperedge.get("members"))
                    .or_else(|| hyperedge.get("node_ids"))
                    .and_then(Value::as_array);
                !members.is_some_and(|members| {
                    members
                        .iter()
                        .any(|member| member.as_str().is_some_and(|id| skipped_ids.contains(id)))
                })
            });
        }

        let path = resolved_source_path(root, Path::new(&source));
        let mut previous_partial = false;
        if options.merge_existing
            && let Some(existing) = load_cached_value_internal(
                &path,
                root,
                cache_root,
                &kind,
                fingerprint.as_deref(),
                AST_CACHE_VERSION,
                true,
            )
        {
            previous_partial = existing.get("partial").and_then(Value::as_bool) == Some(true);
            for (index, key) in ["nodes", "edges", "hyperedges"].into_iter().enumerate() {
                let prior = existing
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .cloned();
                let incoming = std::mem::take(&mut buckets[index]);
                let mut merged = Vec::new();
                append_unique(&mut merged, prior);
                append_unique(&mut merged, incoming);
                buckets[index] = merged;
            }
        }
        let is_partial = previous_partial
            || partial.as_ref().is_some_and(|paths| paths.contains(&path))
            || buckets
                .iter()
                .flatten()
                .any(|item| item.get("_partial").and_then(Value::as_bool) == Some(true));
        let mut payload = serde_json::json!({
            "nodes": buckets[0],
            "edges": buckets[1],
            "hyperedges": buckets[2],
        });
        if is_partial {
            payload["partial"] = Value::Bool(true);
        }
        save_cached_value_at(
            &path,
            &payload,
            root,
            cache_root,
            &kind,
            fingerprint.as_deref(),
        )?;
        report.saved += 1;
    }
    if candidate_groups > 0 && report.saved == 0 {
        report.warnings.push(
            "#1991: every semantic cache group was skipped; verify that source_file values are anchored to the corpus root, not the output root"
                .into(),
        );
    }
    Ok(report)
}

/// Read and merge semantic cache entries, preserving input order for misses.
/// With a known prompt, an exact fingerprinted entry wins; a historical flat
/// entry is accepted only as a visibly warned compatibility fallback.
pub fn check_semantic_cache(
    files: &[PathBuf],
    root: &Path,
    options: &SemanticCacheOptions,
) -> SemanticCacheCheck {
    let mut result = SemanticCacheCheck::default();
    let cache_root = options.cache_root.as_deref().unwrap_or(root);
    let kind = semantic_kind(options.mode.as_deref());
    let (fingerprint, prompt_warning) = semantic_prompt_fingerprint(options);
    if let Some(warning) = prompt_warning {
        result.warnings.push(warning);
    }
    let mut legacy_hits = 0;
    for original in files {
        let path = if original.is_absolute() {
            original.clone()
        } else {
            absolute_lexical(root).join(original)
        };
        let mut cached =
            load_cached_value_at(&path, root, cache_root, &kind, fingerprint.as_deref());
        if cached.is_none() && fingerprint.is_some() {
            cached = load_cached_value_at(&path, root, cache_root, &kind, None);
            legacy_hits += usize::from(cached.is_some());
        }
        let Some(cached) = cached else {
            result.uncached.push(original.clone());
            continue;
        };
        result.nodes.extend(
            cached
                .get("nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        result.edges.extend(
            cached
                .get("edges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        result.hyperedges.extend(
            cached
                .get("hyperedges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    if legacy_hits > 0 {
        result.warnings.push(format!(
            "{legacy_hits} semantic cache entr{} predate extraction-prompt fingerprinting and were replayed from an unknown prompt version",
            if legacy_hits == 1 { "y" } else { "ies" }
        ));
    }
    result
}

/// Prune orphaned semantic entries from both standard and deep namespaces,
/// including prompt-fingerprint subdirectories. AST entries and temporaries
/// are outside the traversal by construction.
pub fn prune_semantic_cache(root: &Path, live_hashes: &BTreeSet<String>) -> usize {
    let base = root.join("graphoxide-out/cache");
    let mut removed = 0;
    for kind in ["semantic", "semantic-deep"] {
        let mut entries = Vec::new();
        visit_json_files(&base.join(kind), &mut entries);
        for entry in entries {
            let live = entry
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| live_hashes.contains(stem));
            if !live && fs::remove_file(&entry).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

pub fn load_manifest(root: &Path) -> Manifest {
    load_manifest_from_output(&root.join("graphoxide-out"))
}
pub fn load_manifest_from_output(output_dir: &Path) -> Manifest {
    let path = output_dir.join("manifest.json");
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
        if let Some(old) = manifest.get(relative)
            && old.ast_version == AST_CACHE_VERSION
            && old.mtime == mtime
            && !old.ast_hash.is_empty()
        {
            continue;
        }
        let bytes = fs::read(path)?;
        let hash = format!("{:x}", Md5::digest(&bytes));
        if manifest
            .get(relative)
            .is_some_and(|old| old.ast_version == AST_CACHE_VERSION && old.ast_hash == hash)
        {
            continue;
        }
        changed.push((relative.clone(), path.clone(), hash, mtime));
    }
    Ok(changed)
}
pub fn save_manifest(root: &Path, entries: &Manifest) -> anyhow::Result<()> {
    save_manifest_to_output(&root.join("graphoxide-out"), entries)
}
pub fn save_manifest_to_output(output_dir: &Path, entries: &Manifest) -> anyhow::Result<()> {
    atomic_json(&output_dir.join("manifest.json"), entries)
}
pub fn ast_cache_get(root: &Path, relative: &str, bytes: &[u8]) -> Option<Extraction> {
    ast_cache_get_from_output(&root.join("graphoxide-out"), relative, bytes)
}
pub fn ast_cache_get_from_output(
    output_dir: &Path,
    relative: &str,
    bytes: &[u8],
) -> Option<Extraction> {
    if bypass(relative) {
        return None;
    }
    let path = cache_path(output_dir, relative, bytes);
    let bytes = fs::read(path).ok()?;
    let payload = cache_payload(&bytes)?;
    serde_json::from_slice(payload).ok()
}
pub fn ast_cache_put(
    root: &Path,
    relative: &str,
    bytes: &[u8],
    value: &Extraction,
) -> anyhow::Result<()> {
    ast_cache_put_to_output(&root.join("graphoxide-out"), relative, bytes, value)
}
pub fn ast_cache_put_to_output(
    output_dir: &Path,
    relative: &str,
    bytes: &[u8],
    value: &Extraction,
) -> anyhow::Result<()> {
    if bypass(relative) || value.nodes.is_empty() {
        return Ok(());
    }
    atomic_json(&cache_path(output_dir, relative, bytes), value)
}

/// Derive the runtime-v1 AST artifact key from bytes already materialized by
/// an I/O owner. This is a pure operation and can be performed before a CPU
/// extractor consumes the source lease.
#[must_use]
pub fn runtime_ast_cache_key(relative: &str, bytes: &[u8]) -> RuntimeCacheKey {
    RuntimeCacheKey::for_versioned_bytes("ast", AST_CACHE_VERSION, relative, bytes)
}

/// Read an AST payload from runtime-v1, falling back to the legacy AST cache
/// without modifying it.
///
/// This is intentionally an I/O-plane helper. It returns raw JSON bytes so a
/// CPU consumer can deserialize an extraction without receiving a filesystem
/// path or cache capability. Existing JS/TS/SFC cache exclusions remain in
/// force for both tiers.
#[must_use]
pub fn runtime_ast_cache_payload_from_output(
    runtime_cache: &RuntimeCache,
    output_dir: &Path,
    relative: &str,
    source_bytes: &[u8],
) -> Option<RuntimeCacheHit> {
    if bypass(relative) {
        return None;
    }
    let key = runtime_ast_cache_key(relative, source_bytes);
    runtime_cache.get_or_legacy(key, || {
        let bytes = fs::read(cache_path(output_dir, relative, source_bytes)).ok()?;
        cache_payload(&bytes).map(ToOwned::to_owned)
    })
}

/// Serialize and append an AST artifact to runtime-v1. Call only from an I/O
/// owner after a CPU extractor has returned its result; it performs no source
/// read and does not touch legacy cache files.
pub fn runtime_ast_cache_put(
    runtime_cache: &mut RuntimeCache,
    key: RuntimeCacheKey,
    relative: &str,
    extraction: &Extraction,
) -> Result<(), graphoxide_index_runtime::cache::RuntimeCacheError> {
    if bypass(relative) || extraction.nodes.is_empty() {
        return Ok(());
    }
    let payload = serde_json::to_vec(extraction).map_err(|error| {
        graphoxide_index_runtime::cache::RuntimeCacheError::Encode(error.to_string())
    })?;
    runtime_cache.put(key, &payload)
}

/// Serialize an AST extraction and submit it to the dedicated runtime-cache
/// I/O owner.
///
/// The service performs both the existing-artifact probe and any append on its
/// own thread. This helper is intentionally for the isolated extraction
/// control plane after CPU extraction has completed; `RuntimeCacheIoService`
/// is `!Sync`, so it cannot be captured by a `read_files_concurrently` CPU
/// callback.
pub fn runtime_ast_cache_persist_on_io_owner(
    service: &RuntimeCacheIoService,
    key: RuntimeCacheKey,
    relative: &str,
    extraction: &Extraction,
) -> Result<Option<RuntimeCacheIoPersistOutcome>, RuntimeCacheIoServiceError> {
    if bypass(relative) || extraction.nodes.is_empty() {
        return Ok(None);
    }
    let payload = serde_json::to_vec(extraction).map_err(|error| {
        RuntimeCacheIoServiceError::Cache(
            graphoxide_index_runtime::cache::RuntimeCacheError::Encode(error.to_string()),
        )
    })?;
    service.persist_if_absent(key, payload).map(Some)
}

fn cache_path(output_dir: &Path, relative: &str, bytes: &[u8]) -> std::path::PathBuf {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.update(b"\0");
    hash.update(relative.to_lowercase().as_bytes());
    output_dir.join(format!(
        "cache/ast/v{AST_CACHE_VERSION}/{}.json",
        hex::encode(hash.finalize())
    ))
}
fn bypass(relative: &str) -> bool {
    [
        ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".vue", ".svelte", ".astro",
    ]
    .iter()
    .any(|suffix| relative.to_lowercase().ends_with(suffix))
}
fn atomic_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    graphoxide_core::write_json_atomic(path, value, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_manifest_entries_default_to_schema_zero_and_are_requeued() {
        let temp = tempfile::tempdir().expect("temporary manifest root");
        let source = temp.path().join("design.dot");
        let source_bytes = b"digraph { api -> database; }\n";
        fs::write(&source, source_bytes).expect("write DOT source");
        let mtime = fs::metadata(&source)
            .expect("DOT metadata")
            .modified()
            .expect("DOT mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let hash = format!("{:x}", Md5::digest(source_bytes));
        let mut entry: ManifestEntry = serde_json::from_value(serde_json::json!({
            "mtime": mtime,
            "ast_hash": hash,
            "semantic_hash": "legacy-semantic"
        }))
        .expect("deserialize legacy manifest entry");
        assert_eq!(entry.ast_version, 0);

        let files = vec![("design.dot".to_owned(), source)];
        let mut manifest = Manifest::from([("design.dot".to_owned(), entry.clone())]);
        assert_eq!(
            changed_files(temp.path(), &files, &manifest)
                .expect("compare legacy manifest")
                .len(),
            1,
            "a content-identical legacy entry must be re-extracted"
        );

        entry.ast_version = AST_CACHE_VERSION;
        manifest.insert("design.dot".to_owned(), entry);
        assert!(
            changed_files(temp.path(), &files, &manifest)
                .expect("compare current manifest")
                .is_empty(),
            "a current content-identical entry remains unchanged"
        );
    }

    #[test]
    fn cache_frame_round_trips_without_copying_payload() {
        let payload = br#"{"nodes":[]}"#;
        let framed = frame_cache_bytes(payload);

        assert_eq!(unframe_cache_bytes(&framed), Ok(Some(payload.as_slice())));
        assert_eq!(unframe_cache_bytes(payload), Ok(None));
        assert_eq!(
            decode_cache_json(&framed),
            Some(serde_json::json!({ "nodes": [] }))
        );
    }

    #[test]
    fn corrupted_cache_frames_are_rejected_before_json_decode() {
        let payload = br#"{"nodes":[]}"#;
        let mut crc_corrupt = frame_cache_bytes(payload);
        *crc_corrupt.last_mut().expect("payload byte") ^= 1;
        assert_eq!(
            unframe_cache_bytes(&crc_corrupt),
            Err(CacheFrameError::ChecksumMismatch)
        );
        assert_eq!(decode_cache_json(&crc_corrupt), None);

        let mut digest_corrupt = frame_cache_bytes(payload);
        digest_corrupt[24] ^= 1;
        assert_eq!(
            unframe_cache_bytes(&digest_corrupt),
            Err(CacheFrameError::DigestMismatch)
        );

        let mut length_corrupt = frame_cache_bytes(payload);
        length_corrupt[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            unframe_cache_bytes(&length_corrupt),
            Err(CacheFrameError::LengthMismatch)
        );
    }

    #[test]
    fn framed_cache_payloads_have_explicit_byte_limits() {
        let payload = b"1234";
        assert_eq!(
            try_frame_cache_bytes(payload, 3),
            Err(CacheFrameError::PayloadTooLarge {
                payload_len: 4,
                max_payload_bytes: 3,
            })
        );

        let framed = frame_cache_bytes(payload);
        assert_eq!(
            unframe_cache_bytes_with_limit(&framed, 3),
            Err(CacheFrameError::PayloadTooLarge {
                payload_len: 4,
                max_payload_bytes: 3,
            })
        );
        assert_eq!(unframe_cache_bytes(payload), Ok(None));
    }

    #[test]
    fn byte_aware_cache_helpers_preserve_legacy_json_contract() {
        let temp = tempfile::tempdir().expect("temporary cache root");
        let source = temp.path().join("example.py");
        let source_bytes = b"def example():\n    return 1\n";
        fs::write(&source, source_bytes).expect("source file");
        let value = serde_json::json!({"nodes": [], "edges": []});

        let byte_hash = file_hash_from_bytes(&source, temp.path(), source_bytes)
            .expect("hash caller-owned bytes");
        assert_eq!(
            byte_hash,
            file_hash(&source, temp.path()).expect("legacy hash")
        );
        save_cached_value_from_bytes(&source, source_bytes, &value, temp.path(), "semantic", None)
            .expect("save caller-owned bytes");
        assert_eq!(
            load_cached_value_from_bytes(&source, source_bytes, temp.path(), "semantic", None),
            Some(value.clone())
        );

        let entry = cache_dir(temp.path(), "semantic", None)
            .expect("cache dir")
            .join(format!("{byte_hash}.json"));
        let json = serde_json::to_vec(&value).expect("cache json");
        fs::write(&entry, frame_cache_bytes(&json)).expect("framed cache entry");
        assert_eq!(
            load_cached_value_from_bytes(&source, source_bytes, temp.path(), "semantic", None),
            Some(value)
        );

        let mut corrupt = frame_cache_bytes(&json);
        corrupt[20] ^= 1;
        fs::write(&entry, corrupt).expect("corrupt framed cache entry");
        assert_eq!(
            load_cached_value_from_bytes(&source, source_bytes, temp.path(), "semantic", None),
            None,
            "corrupt framed artifacts must fail open as cache misses"
        );
    }

    #[test]
    fn sibling_dependent_sources_bypass_cache() {
        for path in [
            "src/a.js",
            "src/a.ts",
            "src/App.vue",
            "src/Card.svelte",
            "src/Page.astro",
            "src/PAGE.ASTRO",
        ] {
            assert!(bypass(path), "cache should be bypassed for {path}");
        }
        assert!(!bypass("src/a.py"));
    }

    #[test]
    fn runtime_ast_payload_uses_legacy_as_read_only_fallback_then_runtime_v1() {
        let temp = tempfile::tempdir().expect("temporary cache root");
        let output = temp.path().join("graphoxide-out");
        let relative = "src/example.py";
        let source = b"def example():\n    return 1\n";
        let extraction = Extraction {
            nodes: vec![graphoxide_core::Node {
                id: "example".into(),
                label: "example()".into(),
                file_type: "code".into(),
                source_file: relative.into(),
                source_location: Some("L1".into()),
                community: None,
                extra: BTreeMap::new(),
            }],
            ..Extraction::default()
        };
        ast_cache_put_to_output(&output, relative, source, &extraction).expect("legacy cache");
        let mut runtime = RuntimeCache::open(&output).expect("runtime cache");
        let legacy = runtime_ast_cache_payload_from_output(&runtime, &output, relative, source)
            .expect("legacy fallback");
        assert_eq!(
            legacy.source,
            graphoxide_index_runtime::cache::RuntimeCacheSource::Legacy
        );
        assert_eq!(
            serde_json::from_slice::<Extraction>(&legacy.payload)
                .expect("legacy payload")
                .nodes[0]
                .id,
            "example"
        );

        let key = runtime_ast_cache_key(relative, source);
        runtime_ast_cache_put(&mut runtime, key, relative, &extraction)
            .expect("runtime cache write");
        let runtime_hit =
            runtime_ast_cache_payload_from_output(&runtime, &output, relative, source)
                .expect("runtime hit");
        assert_eq!(
            runtime_hit.source,
            graphoxide_index_runtime::cache::RuntimeCacheSource::RuntimeV1
        );
        assert_eq!(
            serde_json::from_slice::<Extraction>(&runtime_hit.payload)
                .expect("runtime payload")
                .nodes[0]
                .id,
            "example"
        );
    }
}
