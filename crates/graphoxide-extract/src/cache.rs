//! Incremental manifest and content-addressed AST cache.

use anyhow::Context as _;
use graphoxide_core::Extraction;
use graphoxide_index_runtime::cache::{
    RuntimeCache, RuntimeCacheHit, RuntimeCacheIoPersistOutcome, RuntimeCacheIoService,
    RuntimeCacheIoServiceError, RuntimeCacheKey, RuntimeCacheSource,
};
use md5::{Digest as _, Md5};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
};
// Bump whenever a built-in extractor's persisted fact schema changes. Version
// 30 redacts secret-bearing scalar values in generic structured facts and must
// not replay version 29 rows that may retain raw credentials. Version 29
// replaced OOXML, ODF, and EPUB inventory entries with bounded package-part,
// document-structure, and internal-relationship facts.
pub const AST_CACHE_VERSION: u32 = 30;

const LAST_PRE_REDACTION_AST_CACHE_VERSION: u32 = 29;
const MAX_AST_CACHE_ROOT_ENTRIES_FOR_PURGE: usize = 1_000_000;
const MAX_PRE_REDACTION_AST_VERSION_ENTRIES_FOR_PURGE: usize = 1_000_000;
const MAX_PRE_REDACTION_AST_ARTIFACTS_FOR_PURGE: usize = 2_000_000;

/// Wire version for the extraction-owned payload stored inside runtime-v1.
///
/// Runtime-v1 validates its outer append-only frame. This independent version
/// validates the meaning of the decoded payload so a future envelope change
/// cannot accidentally replay bytes under a compatible outer frame.
pub const RUNTIME_AST_CACHE_ENVELOPE_VERSION: u32 = 2;

/// Stable version for the isolated byte-extraction policy encoded in runtime
/// AST cache keys. Bump this independently of [`AST_CACHE_VERSION`] when a
/// fact-affecting execution option changes without changing the fact schema.
pub const RUNTIME_AST_CACHE_OPTIONS_VERSION: u32 = 1;

const RUNTIME_AST_CACHE_KEY_DOMAIN: &[u8] = b"graphoxide-runtime-ast-cache-key-v1\0";
const RUNTIME_AST_CACHE_EXTRACTOR_PREFIX: &str = "graphoxide-extract/";
const RUNTIME_AST_CACHE_PREAMBLE_MAGIC: [u8; 8] = *b"GOXAST02";
const RUNTIME_AST_CACHE_PREAMBLE_LEN: usize = 16;

/// Canonical fact-affecting options for one runtime AST extraction.
///
/// The production isolated executor currently has one fact-affecting switch:
/// parser code may not probe sibling paths. Keeping that choice explicit in
/// the key and envelope prevents a legacy path-aware extraction from becoming
/// an isolated hit merely because its source bytes happen to match. New
/// fact-affecting switches must be appended to `update_key` in field order and
/// require an options-version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAstCacheOptions {
    pub version: u32,
    pub allow_path_probes: bool,
    /// Per-worker parser arena allowance. A hit under a different allowance is
    /// deliberately a miss: a result admitted under a larger parser policy
    /// must not silently bypass a tighter run's extraction boundary.
    pub parser_allowance_bytes: u64,
}

impl RuntimeAstCacheOptions {
    /// Options used by the dedicated I/O/CPU runtime.
    #[must_use]
    pub const fn isolated(parser_allowance_bytes: u64) -> Self {
        Self {
            version: RUNTIME_AST_CACHE_OPTIONS_VERSION,
            allow_path_probes: false,
            parser_allowance_bytes,
        }
    }

    fn update_key(self, hasher: &mut blake3::Hasher) {
        hasher.update(&self.version.to_le_bytes());
        hasher.update(&[u8::from(self.allow_path_probes)]);
        hasher.update(&self.parser_allowance_bytes.to_le_bytes());
    }
}

impl Default for RuntimeAstCacheOptions {
    fn default() -> Self {
        Self::isolated(0)
    }
}

/// Immutable evidence used to derive and validate one runtime AST artifact.
///
/// The source digest is computed from bytes already admitted by an I/O owner.
/// It is retained in the envelope as independent evidence rather than relying
/// on the opaque outer cache key alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAstCacheEvidence {
    pub normalized_path: String,
    pub content_digest: [u8; 32],
    pub extractor_id: String,
    pub extractor_version: u32,
    pub options: RuntimeAstCacheOptions,
    pub key: RuntimeCacheKey,
}

/// Deterministic runtime-cache counters exposed to CLI telemetry.
///
/// Counters describe cache decisions, not incidental worker scheduling. The
/// project scan aggregates them only after rows have been restored to stable
/// normalized-path order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeCacheTelemetry {
    pub enabled: bool,
    pub metadata_hits: u64,
    pub runtime_hits: u64,
    pub legacy_hits: u64,
    pub misses: u64,
    pub bypasses: u64,
    pub stale_or_corrupt: u64,
    pub probe_failures: u64,
    pub payload_reads_avoided: u64,
    pub parses_avoided: u64,
    pub stores: u64,
    pub already_present: u64,
    pub store_failures: u64,
}

impl RuntimeCacheTelemetry {
    pub const fn enabled() -> Self {
        Self {
            enabled: true,
            metadata_hits: 0,
            runtime_hits: 0,
            legacy_hits: 0,
            misses: 0,
            bypasses: 0,
            stale_or_corrupt: 0,
            probe_failures: 0,
            payload_reads_avoided: 0,
            parses_avoided: 0,
            stores: 0,
            already_present: 0,
            store_failures: 0,
        }
    }

    /// Record one valid parser-bypassing artifact hit.
    pub fn record_hit(&mut self, source: RuntimeCacheSource) {
        match source {
            RuntimeCacheSource::RuntimeV1 => {
                self.runtime_hits = self.runtime_hits.saturating_add(1)
            }
            RuntimeCacheSource::Legacy => self.legacy_hits = self.legacy_hits.saturating_add(1),
        }
        self.parses_avoided = self.parses_avoided.saturating_add(1);
    }

    pub fn merge(&mut self, other: Self) {
        self.enabled |= other.enabled;
        macro_rules! merge_counter {
            ($field:ident) => {
                self.$field = self.$field.saturating_add(other.$field);
            };
        }
        merge_counter!(metadata_hits);
        merge_counter!(runtime_hits);
        merge_counter!(legacy_hits);
        merge_counter!(misses);
        merge_counter!(bypasses);
        merge_counter!(stale_or_corrupt);
        merge_counter!(probe_failures);
        merge_counter!(payload_reads_avoided);
        merge_counter!(parses_avoided);
        merge_counter!(stores);
        merge_counter!(already_present);
        merge_counter!(store_failures);
    }

    pub fn record_persist(&mut self, outcome: RuntimeCacheIoPersistOutcome) {
        match outcome {
            RuntimeCacheIoPersistOutcome::AlreadyPresent { .. } => {
                self.already_present = self.already_present.saturating_add(1);
            }
            RuntimeCacheIoPersistOutcome::Stored { .. }
            | RuntimeCacheIoPersistOutcome::RepairedRejected { .. }
            | RuntimeCacheIoPersistOutcome::ReplacedExisting { .. } => {
                self.stores = self.stores.saturating_add(1);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeAstCacheEnvelope {
    envelope_version: u32,
    extractor_id: String,
    extractor_version: u32,
    normalized_path: String,
    content_digest: [u8; 32],
    options: RuntimeAstCacheOptions,
    complete: bool,
    extraction: Extraction,
}

#[derive(Serialize)]
struct RuntimeAstCacheEnvelopeRef<'a> {
    envelope_version: u32,
    extractor_id: &'a str,
    extractor_version: u32,
    normalized_path: &'a str,
    content_digest: [u8; 32],
    options: RuntimeAstCacheOptions,
    complete: bool,
    extraction: &'a Extraction,
}

#[derive(Deserialize)]
struct RuntimeAstCacheEnvelopeHeader {
    envelope_version: u32,
    extractor_id: String,
    extractor_version: u32,
    normalized_path: String,
    content_digest: [u8; 32],
    options: RuntimeAstCacheOptions,
    complete: bool,
    #[serde(rename = "extraction")]
    _extraction: serde::de::IgnoredAny,
}

/// A cache payload reached the extraction validation boundary but could not be
/// trusted. Every variant is a safe cache miss; none is a scan failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAstCacheRejection {
    Preamble,
    Decode,
    EnvelopeVersion,
    Extractor,
    ExtractorVersion,
    Path,
    ContentDigest,
    Options,
    Incomplete,
    Empty,
    Provenance,
}

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
    /// Strong generation evidence plus the raw BLAKE3 digest observed during
    /// the committed scan. This authorizes a metadata-only runtime-cache probe
    /// only while a no-follow runtime guard proves the same source generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_cache: Option<RuntimeAstManifestEvidence>,
}
pub type Manifest = BTreeMap<String, ManifestEntry>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAstManifestEvidence {
    pub content_digest: [u8; 32],
    pub source_identity_digest: [u8; 32],
    /// Exact extraction key, including extractor/schema/options/path/content.
    /// A changed parser allowance therefore invalidates baseline reuse even
    /// when the source bytes are unchanged.
    pub artifact_key: [u8; 32],
}

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
        directory.push(format!("v{ast_version}"));
    } else {
        directory.push(kind);
        if let Some(fingerprint) = prompt_fingerprint {
            directory.push(format!("p{fingerprint}"));
        }
    }
    fs::create_dir_all(&directory)?;
    Ok(directory)
}

#[derive(Debug, Clone, Copy)]
struct PreRedactionAstPurgeLimits {
    max_root_entries: usize,
    max_version_entries: usize,
    max_artifacts: usize,
}

const PRE_REDACTION_AST_PURGE_LIMITS: PreRedactionAstPurgeLimits = PreRedactionAstPurgeLimits {
    max_root_entries: MAX_AST_CACHE_ROOT_ENTRIES_FOR_PURGE,
    max_version_entries: MAX_PRE_REDACTION_AST_VERSION_ENTRIES_FOR_PURGE,
    max_artifacts: MAX_PRE_REDACTION_AST_ARTIFACTS_FOR_PURGE,
};

/// Prepare all persistent extraction caches for structured-value redaction.
///
/// Run this only while holding the project's exclusive rebuild lock and before
/// opening an active cache or publishing any extraction output. The fixed
/// sequence erases legacy JSON AST artifacts first, then retires runtime-v1
/// frames that can contain the same pre-redaction facts. A failed or interrupted
/// cleanup is safe to retry, but callers must stop publication on any error.
pub fn prepare_structured_redaction_cache_schema(output_dir: &Path) -> anyhow::Result<()> {
    purge_pre_redaction_ast_caches(output_dir).context("purge pre-redaction AST caches")?;
    graphoxide_index_runtime::cache::purge_retired_runtime_v1_cache(output_dir)
        .context("purge retired runtime-v1 cache")?;
    Ok(())
}

/// Irrecoverably remove AST JSON artifacts written before structured-value
/// redaction was introduced in cache schema version 30.
///
/// `output_dir` must be the Graphoxide output directory. The purge target is
/// intentionally not configurable beyond that boundary: only exact legacy
/// artifacts directly beneath `cache`, `cache/ast`, and `cache/ast/v0` through
/// `cache/ast/v29` are eligible. Callers must hold the output directory's
/// exclusive build lock so another process cannot replace entries between the
/// validation and unlink steps.
///
/// Every targeted file is opened without following its final component,
/// required to be a strongly identified regular file with one link, truncated,
/// synced, identity-checked again, and only then unlinked. Unexpected content
/// inside a targeted version directory rejects the purge instead of being
/// traversed. Missing or already-truncated artifacts are handled idempotently.
pub fn purge_pre_redaction_ast_caches(output_dir: &Path) -> anyhow::Result<()> {
    purge_pre_redaction_ast_caches_with_limits(output_dir, PRE_REDACTION_AST_PURGE_LIMITS)
        .map(|_| ())
        .map_err(|_| {
            anyhow::anyhow!(
                "pre-redaction AST cache migration rejected an unsafe, unexpected, unreadable, or over-limit managed layout"
            )
        })
}

fn purge_pre_redaction_ast_caches_with_limits(
    output_dir: &Path,
    limits: PreRedactionAstPurgeLimits,
) -> anyhow::Result<usize> {
    anyhow::ensure!(
        limits.max_root_entries > 0 && limits.max_version_entries > 0 && limits.max_artifacts > 0,
        "pre-redaction AST cache purge limits must be non-zero"
    );

    let Some(output_canonical) = validated_purge_directory(output_dir, None, "output directory")?
    else {
        return Ok(0);
    };
    let cache = output_dir.join("cache");
    let Some(cache_canonical) = validated_purge_directory(
        &cache,
        Some(&output_canonical.join("cache")),
        "cache directory",
    )?
    else {
        return Ok(0);
    };
    let ast = cache.join("ast");
    let ast_canonical = validated_purge_directory(
        &ast,
        Some(&cache_canonical.join("ast")),
        "AST cache directory",
    )?;

    let mut artifact_count = 0;
    inspect_pre_redaction_ast_directory(
        &cache,
        &cache_canonical,
        false,
        limits.max_root_entries,
        limits.max_artifacts,
        &mut artifact_count,
    )?;
    let mut existing_versions = [false; LAST_PRE_REDACTION_AST_CACHE_VERSION as usize + 1];
    if let Some(ast_canonical) = ast_canonical.as_deref() {
        inspect_pre_redaction_ast_directory(
            &ast,
            ast_canonical,
            false,
            limits.max_root_entries,
            limits.max_artifacts,
            &mut artifact_count,
        )?;
        for version in 0..=LAST_PRE_REDACTION_AST_CACHE_VERSION {
            let name = format!("v{version}");
            let directory = ast.join(&name);
            if let Some(canonical) = validated_purge_directory(
                &directory,
                Some(&ast_canonical.join(&name)),
                "pre-redaction AST cache version directory",
            )? {
                inspect_pre_redaction_ast_directory(
                    &directory,
                    &canonical,
                    true,
                    limits.max_version_entries,
                    limits.max_artifacts,
                    &mut artifact_count,
                )?;
                existing_versions[version as usize] = true;
            }
        }
    }

    // Preflight every targeted namespace before erasing the first byte. The
    // second pass repeats all entry checks to fail closed if a caller did not
    // honor the required exclusive-lock contract.
    let mut removed = 0;
    let mut deletion_count = 0;
    validated_purge_directory(
        &cache,
        Some(&output_canonical.join("cache")),
        "cache directory",
    )?
    .ok_or_else(|| anyhow::anyhow!("cache directory disappeared during purge"))?;
    purge_pre_redaction_ast_directory(
        &cache,
        &cache_canonical,
        false,
        limits.max_root_entries,
        limits.max_artifacts,
        &mut deletion_count,
        &mut removed,
    )?;
    if let Some(ast_canonical) = ast_canonical.as_deref() {
        validated_purge_directory(
            &ast,
            Some(&cache_canonical.join("ast")),
            "AST cache directory",
        )?
        .ok_or_else(|| anyhow::anyhow!("AST cache directory disappeared during purge"))?;
        purge_pre_redaction_ast_directory(
            &ast,
            ast_canonical,
            false,
            limits.max_root_entries,
            limits.max_artifacts,
            &mut deletion_count,
            &mut removed,
        )?;
        for version in 0..=LAST_PRE_REDACTION_AST_CACHE_VERSION {
            if !existing_versions[version as usize] {
                continue;
            }
            let name = format!("v{version}");
            let directory = ast.join(&name);
            let canonical = validated_purge_directory(
                &directory,
                Some(&ast_canonical.join(&name)),
                "pre-redaction AST cache version directory",
            )?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "pre-redaction AST cache version directory disappeared during purge"
                )
            })?;
            purge_pre_redaction_ast_directory(
                &directory,
                &canonical,
                true,
                limits.max_version_entries,
                limits.max_artifacts,
                &mut deletion_count,
                &mut removed,
            )?;
            validated_purge_directory(
                &directory,
                Some(&ast_canonical.join(&name)),
                "pre-redaction AST cache version directory",
            )?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "pre-redaction AST cache version directory disappeared during purge"
                )
            })?;
            match fs::remove_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "failed to remove emptied pre-redaction AST cache directory {}: {error}",
                        directory.display()
                    ));
                }
            }
        }
    }
    Ok(removed)
}

fn validated_purge_directory(
    path: &Path,
    expected_canonical: Option<&Path>,
    description: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "failed to inspect {description} {}: {error}",
                path.display()
            ));
        }
    };
    anyhow::ensure!(
        purge_directory_metadata_is_safe(&metadata),
        "refusing unsafe {description} {}",
        path.display()
    );
    let canonical = fs::canonicalize(path).map_err(|error| {
        anyhow::anyhow!(
            "failed to resolve {description} {}: {error}",
            path.display()
        )
    })?;
    if let Some(expected) = expected_canonical {
        anyhow::ensure!(
            canonical == expected,
            "refusing {description} outside its fixed cache namespace: {}",
            path.display()
        );
    }
    Ok(Some(canonical))
}

fn purge_directory_metadata_is_safe(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn inspect_pre_redaction_ast_directory(
    directory: &Path,
    canonical_directory: &Path,
    reject_unexpected: bool,
    max_entries: usize,
    max_artifacts: usize,
    artifact_count: &mut usize,
) -> anyhow::Result<()> {
    let mut entries = 0_usize;
    for entry in fs::read_dir(directory).map_err(|error| {
        anyhow::anyhow!(
            "failed to enumerate legacy AST cache directory {}: {error}",
            directory.display()
        )
    })? {
        entries = entries.saturating_add(1);
        anyhow::ensure!(
            entries <= max_entries,
            "legacy AST cache directory entry cap exceeded at {}",
            directory.display()
        );
        let entry = entry.map_err(|error| {
            anyhow::anyhow!(
                "failed to inspect a legacy AST cache entry in {}: {error}",
                directory.display()
            )
        })?;
        if legacy_ast_artifact_name_is_exact(&entry.file_name()) {
            open_validated_legacy_ast_artifact(&entry.path(), canonical_directory)?.ok_or_else(
                || {
                    anyhow::anyhow!(
                        "legacy AST cache artifact disappeared during preflight: {}",
                        entry.path().display()
                    )
                },
            )?;
            *artifact_count = artifact_count.saturating_add(1);
            anyhow::ensure!(
                *artifact_count <= max_artifacts,
                "legacy AST cache artifact cap exceeded"
            );
        } else if reject_unexpected {
            anyhow::bail!(
                "refusing unexpected content in pre-redaction AST cache directory: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn purge_pre_redaction_ast_directory(
    directory: &Path,
    canonical_directory: &Path,
    reject_unexpected: bool,
    max_entries: usize,
    max_artifacts: usize,
    artifact_count: &mut usize,
    removed: &mut usize,
) -> anyhow::Result<()> {
    let mut entries = 0_usize;
    for entry in fs::read_dir(directory).map_err(|error| {
        anyhow::anyhow!(
            "failed to enumerate legacy AST cache directory {}: {error}",
            directory.display()
        )
    })? {
        entries = entries.saturating_add(1);
        anyhow::ensure!(
            entries <= max_entries,
            "legacy AST cache directory entry cap exceeded at {}",
            directory.display()
        );
        let entry = entry.map_err(|error| {
            anyhow::anyhow!(
                "failed to inspect a legacy AST cache entry in {}: {error}",
                directory.display()
            )
        })?;
        if legacy_ast_artifact_name_is_exact(&entry.file_name()) {
            *artifact_count = artifact_count.saturating_add(1);
            anyhow::ensure!(
                *artifact_count <= max_artifacts,
                "legacy AST cache artifact cap exceeded"
            );
            if truncate_sync_unlink_legacy_ast_artifact(&entry.path(), canonical_directory)? {
                *removed = removed.saturating_add(1);
            }
        } else if reject_unexpected {
            anyhow::bail!(
                "refusing unexpected content in pre-redaction AST cache directory: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn legacy_ast_artifact_name_is_exact(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if let Some(hash) = name.strip_suffix(".json") {
        return lowercase_sha256_name_is_exact(hash);
    }
    // The original atomic writer used `<hash>.<pid>.tmp` before the shared
    // writer adopted the collision-resistant form below. A crash could leave
    // either spelling with the complete secret-bearing JSON payload.
    if let Some(temporary) = name.strip_suffix(".tmp")
        && let Some((hash, process)) = temporary.rsplit_once('.')
        && lowercase_sha256_name_is_exact(hash)
        && !process.is_empty()
        && process.bytes().all(|byte| byte.is_ascii_digit())
    {
        return true;
    }
    let Some(temporary) = name.strip_prefix('.') else {
        return false;
    };
    let Some(temporary) = temporary.strip_suffix(".tmp") else {
        return false;
    };
    let Some((hash, generation)) = temporary.split_once(".json.") else {
        return false;
    };
    let mut generation = generation.split('.');
    let process = generation.next().unwrap_or_default();
    let sequence = generation.next().unwrap_or_default();
    lowercase_sha256_name_is_exact(hash)
        && !process.is_empty()
        && process.bytes().all(|byte| byte.is_ascii_digit())
        && !sequence.is_empty()
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
        && generation.next().is_none()
}

fn lowercase_sha256_name_is_exact(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn purge_artifact_metadata_is_safe(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return false;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn open_legacy_ast_artifact_no_follow(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    options.open(path)
}

fn open_validated_legacy_ast_artifact(
    path: &Path,
    canonical_directory: &Path,
) -> anyhow::Result<Option<File>> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("legacy AST cache artifact has no final component"))?;
    anyhow::ensure!(
        legacy_ast_artifact_name_is_exact(name),
        "refusing non-canonical legacy AST cache artifact {}",
        path.display()
    );
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        purge_artifact_metadata_is_safe(&metadata),
        "refusing unsafe legacy AST cache artifact {}",
        path.display()
    );
    let expected_canonical = canonical_directory.join(name);
    anyhow::ensure!(
        fs::canonicalize(path).ok().as_deref() == Some(expected_canonical.as_path()),
        "refusing legacy AST cache artifact outside its fixed namespace: {}",
        path.display()
    );

    let file = match open_legacy_ast_artifact_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let opened_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem cannot strongly identify legacy AST cache artifact {}",
                path.display()
            )
        })?;
    let current = open_legacy_ast_artifact_no_follow(path)?;
    let current_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&current)
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem cannot strongly re-identify legacy AST cache artifact {}",
                path.display()
            )
        })?;
    anyhow::ensure!(
        current_identity == opened_identity
            && fs::canonicalize(path).ok().as_deref() == Some(expected_canonical.as_path()),
        "legacy AST cache artifact changed during validation: {}",
        path.display()
    );
    drop(current);
    Ok(Some(file))
}

fn truncate_sync_unlink_legacy_ast_artifact(
    path: &Path,
    canonical_directory: &Path,
) -> anyhow::Result<bool> {
    let Some(file) = open_validated_legacy_ast_artifact(path, canonical_directory)? else {
        return Ok(false);
    };
    file.set_len(0)?;
    file.sync_all()?;
    let after = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem lost strong identity for legacy AST cache artifact {}",
                path.display()
            )
        })?;
    anyhow::ensure!(
        after.length_bytes() == 0,
        "legacy AST cache artifact did not remain truncated: {}",
        path.display()
    );
    let name = path
        .file_name()
        .expect("validated legacy artifact has a final component");
    let expected_canonical = canonical_directory.join(name);
    let current = open_legacy_ast_artifact_no_follow(path)?;
    let current_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&current)
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem cannot strongly re-identify legacy AST cache artifact {}",
                path.display()
            )
        })?;
    anyhow::ensure!(
        current_identity == after
            && fs::canonicalize(path).ok().as_deref() == Some(expected_canonical.as_path()),
        "legacy AST cache artifact changed during purge: {}",
        path.display()
    );
    drop(current);
    drop(file);
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
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

/// Result of the bounded, no-follow manifest reader used by the isolated
/// runtime. Every non-loaded state is a safe full-rebuild miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeManifestLoadStatus {
    Loaded,
    Missing,
    UnsafeOrUnreadable,
    Oversize,
    Corrupt,
}

#[derive(Debug, Clone)]
pub struct RuntimeManifestLoad {
    pub manifest: Manifest,
    pub status: RuntimeManifestLoadStatus,
}

/// Load the committed manifest without following a final-component link and
/// without allocating from an attacker-controlled advertised file length.
#[must_use]
pub fn load_manifest_from_output_bounded(
    output_dir: &Path,
    max_bytes: usize,
) -> RuntimeManifestLoad {
    use std::io::Read as _;

    let reject = |status| RuntimeManifestLoad {
        manifest: Manifest::new(),
        status,
    };
    if max_bytes == 0 {
        return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable);
    }
    let lexical_root = absolute_lexical(output_dir);
    let canonical_root = match fs::canonicalize(&lexical_root) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return reject(RuntimeManifestLoadStatus::Missing);
        }
        Err(_) => return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable),
    };
    let path = lexical_root.join("manifest.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return reject(RuntimeManifestLoadStatus::Missing);
        }
        Err(_) => return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable),
    };
    if !runtime_manifest_metadata_is_safe(&metadata) {
        return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable);
    }
    if metadata.len() > max_bytes as u64 {
        return reject(RuntimeManifestLoadStatus::Oversize);
    }
    let expected_path = canonical_root.join("manifest.json");
    if fs::canonicalize(&path).ok().as_deref() != Some(expected_path.as_path()) {
        return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable);
    }

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(_) => return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable),
    };
    let opened_identity = match graphoxide_index_runtime::validate_opened_regular_single_link(&file)
    {
        Ok(Some(identity)) if identity.length_bytes() <= max_bytes as u64 => identity,
        Ok(Some(identity)) if identity.length_bytes() > max_bytes as u64 => {
            return reject(RuntimeManifestLoadStatus::Oversize);
        }
        Ok(Some(_)) => unreachable!("opened manifest length branches are exhaustive"),
        Ok(None) | Err(_) => return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable),
    };
    let capacity = usize::try_from(opened_identity.length_bytes())
        .unwrap_or(max_bytes)
        .min(max_bytes);
    let mut bytes = Vec::with_capacity(capacity);
    if file
        .by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable);
    }
    if bytes.len() > max_bytes {
        return reject(RuntimeManifestLoadStatus::Oversize);
    }
    let after_identity = match graphoxide_index_runtime::validate_opened_regular_single_link(&file)
    {
        Ok(Some(identity)) => identity,
        Ok(None) | Err(_) => return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable),
    };
    // Reopen the current path with the same no-follow policy. A held handle
    // stays perfectly stable when an attacker renames it aside and installs a
    // different single-link file at `manifest.json`; canonical path strings
    // alone cannot distinguish those generations.
    let current = match options.open(&path) {
        Ok(file) => file,
        Err(_) => return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable),
    };
    let current_identity =
        match graphoxide_index_runtime::validate_opened_regular_single_link(&current) {
            Ok(Some(identity)) => identity,
            Ok(None) | Err(_) => return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable),
        };
    if after_identity != opened_identity
        || current_identity != opened_identity
        || after_identity.length_bytes() != bytes.len() as u64
        || fs::canonicalize(&lexical_root).ok().as_deref() != Some(canonical_root.as_path())
        || fs::canonicalize(&path).ok().as_deref() != Some(expected_path.as_path())
    {
        return reject(RuntimeManifestLoadStatus::UnsafeOrUnreadable);
    }
    match serde_json::from_slice(&bytes) {
        Ok(manifest) => RuntimeManifestLoad {
            manifest,
            status: RuntimeManifestLoadStatus::Loaded,
        },
        Err(_) => RuntimeManifestLoad {
            manifest: Manifest::new(),
            status: RuntimeManifestLoadStatus::Corrupt,
        },
    }
}

fn runtime_manifest_metadata_is_safe(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return false;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
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

/// Build complete runtime-cache evidence from source bytes already admitted by
/// an I/O owner.
#[must_use]
pub fn runtime_ast_cache_evidence(
    relative: &str,
    bytes: &[u8],
    options: RuntimeAstCacheOptions,
) -> Option<RuntimeAstCacheEvidence> {
    runtime_ast_cache_evidence_from_digest(relative, *blake3::hash(bytes).as_bytes(), options)
}

/// Build runtime-cache evidence from a previously validated source digest.
///
/// The metadata-only fast path uses this only while a runtime-owned strong
/// identity guard is holding and revalidating the same source generation.
#[must_use]
pub fn runtime_ast_cache_evidence_from_digest(
    relative: &str,
    content_digest: [u8; 32],
    options: RuntimeAstCacheOptions,
) -> Option<RuntimeAstCacheEvidence> {
    let normalized_path = normalize_runtime_ast_path(relative)?;
    let extractor_id = runtime_ast_extractor_id(&normalized_path);
    let extractor_version = AST_CACHE_VERSION;
    let mut hasher = blake3::Hasher::new();
    hasher.update(RUNTIME_AST_CACHE_KEY_DOMAIN);
    update_len_prefixed(&mut hasher, extractor_id.as_bytes());
    hasher.update(&extractor_version.to_le_bytes());
    update_len_prefixed(&mut hasher, normalized_path.as_bytes());
    hasher.update(&content_digest);
    options.update_key(&mut hasher);
    let key = RuntimeCacheKey::new(*hasher.finalize().as_bytes());
    Some(RuntimeAstCacheEvidence {
        normalized_path,
        content_digest,
        extractor_id,
        extractor_version,
        options,
        key,
    })
}

fn update_len_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn normalize_runtime_ast_path(relative: &str) -> Option<String> {
    use unicode_normalization::UnicodeNormalization as _;

    if relative.is_empty() || relative.contains('\0') {
        return None;
    }
    let path = relative.replace('\\', "/").nfc().collect::<String>();
    if path.starts_with('/')
        || path.starts_with("//")
        || path.as_bytes().get(1).is_some_and(|byte| *byte == b':')
    {
        return None;
    }
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => return None,
            component => components.push(component),
        }
    }
    (!components.is_empty()).then(|| components.join("/"))
}

fn runtime_ast_extractor_id(normalized_path: &str) -> String {
    crate::format_registry::format_registry()
        .find_by_path(Path::new(normalized_path))
        .map_or_else(
            || format!("{RUNTIME_AST_CACHE_EXTRACTOR_PREFIX}unregistered/engine"),
            |spec| {
                format!(
                    "{RUNTIME_AST_CACHE_EXTRACTOR_PREFIX}{}/{}",
                    spec.id.as_str(),
                    spec.adapter().as_str()
                )
            },
        )
}

/// Whether this source is eligible for parser-result caching.
///
/// JavaScript-family raw extraction remains dependent on project context and
/// intentionally bypasses both legacy and runtime-v1 AST artifacts.
#[must_use]
pub fn runtime_ast_cache_is_eligible(relative: &str) -> bool {
    normalize_runtime_ast_path(relative).is_some() && !bypass(relative)
}

/// Encode a complete, self-validating runtime AST envelope.
pub fn encode_runtime_ast_cache_payload(
    evidence: &RuntimeAstCacheEvidence,
    extraction: &Extraction,
) -> Result<Vec<u8>, graphoxide_index_runtime::cache::RuntimeCacheError> {
    let encoded_bytes = runtime_ast_cache_payload_len(evidence, extraction)?;
    let mut output = Vec::with_capacity(encoded_bytes);
    encode_runtime_ast_cache_payload_into(&mut output, evidence, extraction)?;
    debug_assert_eq!(output.len(), encoded_bytes);
    Ok(output)
}

/// Serialize into caller-reserved storage. The runtime cache client invokes
/// this only after acquiring exact shared transfer credit from the pre-count
/// returned by [`runtime_ast_cache_payload_len`].
pub fn encode_runtime_ast_cache_payload_into(
    output: &mut impl std::io::Write,
    evidence: &RuntimeAstCacheEvidence,
    extraction: &Extraction,
) -> Result<(), graphoxide_index_runtime::cache::RuntimeCacheError> {
    validate_runtime_ast_cache_value(evidence, extraction)?;
    output
        .write_all(&RUNTIME_AST_CACHE_PREAMBLE_MAGIC)
        .and_then(|()| output.write_all(&[0; 8]))
        .map_err(|error| {
            graphoxide_index_runtime::cache::RuntimeCacheError::Encode(error.to_string())
        })?;
    serde_json::to_writer(
        output,
        &runtime_ast_cache_envelope_ref(evidence, extraction),
    )
    .map_err(|error| graphoxide_index_runtime::cache::RuntimeCacheError::Encode(error.to_string()))
}

/// Count the exact encoded envelope length without allocating the payload.
/// Cache I/O clients use this to reserve shared in-flight byte credit before
/// the serializer is allowed to create its `Vec`.
pub fn runtime_ast_cache_payload_len(
    evidence: &RuntimeAstCacheEvidence,
    extraction: &Extraction,
) -> Result<usize, graphoxide_index_runtime::cache::RuntimeCacheError> {
    validate_runtime_ast_cache_value(evidence, extraction)?;
    #[derive(Default)]
    struct CountingWriter(usize);
    impl std::io::Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = CountingWriter::default();
    serde_json::to_writer(
        &mut writer,
        &runtime_ast_cache_envelope_ref(evidence, extraction),
    )
    .map_err(|error| {
        graphoxide_index_runtime::cache::RuntimeCacheError::Encode(error.to_string())
    })?;
    Ok(RUNTIME_AST_CACHE_PREAMBLE_LEN.saturating_add(writer.0))
}

fn validate_runtime_ast_cache_value(
    evidence: &RuntimeAstCacheEvidence,
    extraction: &Extraction,
) -> Result<(), graphoxide_index_runtime::cache::RuntimeCacheError> {
    if extraction.nodes.is_empty() {
        return Err(graphoxide_index_runtime::cache::RuntimeCacheError::Encode(
            "runtime AST cache refuses an empty extraction".into(),
        ));
    }
    if !runtime_extraction_provenance_matches(extraction, &evidence.normalized_path) {
        return Err(graphoxide_index_runtime::cache::RuntimeCacheError::Encode(
            "runtime AST cache extraction provenance does not match its source".into(),
        ));
    }
    Ok(())
}

fn runtime_ast_cache_envelope_ref<'a>(
    evidence: &'a RuntimeAstCacheEvidence,
    extraction: &'a Extraction,
) -> RuntimeAstCacheEnvelopeRef<'a> {
    RuntimeAstCacheEnvelopeRef {
        envelope_version: RUNTIME_AST_CACHE_ENVELOPE_VERSION,
        extractor_id: &evidence.extractor_id,
        extractor_version: evidence.extractor_version,
        normalized_path: &evidence.normalized_path,
        content_digest: evidence.content_digest,
        options: evidence.options,
        // This marks completion of the extractor/serialization transaction,
        // not semantic parse status. A bounded partial extraction is valid
        // under the exact parser allowance carried in `options`.
        complete: true,
        extraction,
    }
}

/// Decode and validate one cache hit before any facts are replayed.
pub fn decode_runtime_ast_cache_hit(
    hit: RuntimeCacheHit,
    evidence: &RuntimeAstCacheEvidence,
) -> Result<Extraction, RuntimeAstCacheRejection> {
    decode_runtime_ast_cache_payload(hit.source, &hit.payload, evidence)
}

/// Validate the fixed versioned preamble and every fact-affecting
/// envelope field without allocating or replaying the extraction itself.
///
/// `IgnoredAny` scans the complete extraction JSON, so malformed or truncated
/// syntax is rejected. This header-only path is used only when a committed
/// graph is authoritative and strong identity proves the same source
/// generation.
pub fn validate_runtime_ast_cache_payload_header(
    source: RuntimeCacheSource,
    payload_bytes: &[u8],
    evidence: &RuntimeAstCacheEvidence,
) -> Result<(), RuntimeAstCacheRejection> {
    if source != RuntimeCacheSource::RuntimeV1 {
        return Err(RuntimeAstCacheRejection::Preamble);
    }
    let json = runtime_ast_cache_payload_parts(payload_bytes)?;
    let header: RuntimeAstCacheEnvelopeHeader =
        serde_json::from_slice(json).map_err(|_| RuntimeAstCacheRejection::Decode)?;
    validate_runtime_ast_cache_envelope_fields(
        header.envelope_version,
        &header.extractor_id,
        header.extractor_version,
        &header.normalized_path,
        header.content_digest,
        header.options,
        header.complete,
        evidence,
    )?;
    Ok(())
}

fn runtime_ast_cache_payload_parts(
    payload_bytes: &[u8],
) -> Result<&[u8], RuntimeAstCacheRejection> {
    if payload_bytes.len() <= RUNTIME_AST_CACHE_PREAMBLE_LEN
        || payload_bytes[..8] != RUNTIME_AST_CACHE_PREAMBLE_MAGIC
    {
        return Err(RuntimeAstCacheRejection::Preamble);
    }
    if payload_bytes[8..RUNTIME_AST_CACHE_PREAMBLE_LEN] != [0; 8] {
        return Err(RuntimeAstCacheRejection::Preamble);
    }
    Ok(&payload_bytes[RUNTIME_AST_CACHE_PREAMBLE_LEN..])
}

#[allow(clippy::too_many_arguments)]
fn validate_runtime_ast_cache_envelope_fields(
    envelope_version: u32,
    extractor_id: &str,
    extractor_version: u32,
    normalized_path: &str,
    content_digest: [u8; 32],
    options: RuntimeAstCacheOptions,
    complete: bool,
    evidence: &RuntimeAstCacheEvidence,
) -> Result<(), RuntimeAstCacheRejection> {
    if envelope_version != RUNTIME_AST_CACHE_ENVELOPE_VERSION {
        return Err(RuntimeAstCacheRejection::EnvelopeVersion);
    }
    if extractor_id != evidence.extractor_id {
        return Err(RuntimeAstCacheRejection::Extractor);
    }
    if extractor_version != evidence.extractor_version {
        return Err(RuntimeAstCacheRejection::ExtractorVersion);
    }
    if normalized_path != evidence.normalized_path {
        return Err(RuntimeAstCacheRejection::Path);
    }
    if content_digest != evidence.content_digest {
        return Err(RuntimeAstCacheRejection::ContentDigest);
    }
    if options != evidence.options {
        return Err(RuntimeAstCacheRejection::Options);
    }
    if !complete {
        return Err(RuntimeAstCacheRejection::Incomplete);
    }
    Ok(())
}

/// Validate borrowed payload bytes while the caller keeps any runtime transfer
/// credit alive around deserialization.
pub fn decode_runtime_ast_cache_payload(
    source: RuntimeCacheSource,
    payload_bytes: &[u8],
    evidence: &RuntimeAstCacheEvidence,
) -> Result<Extraction, RuntimeAstCacheRejection> {
    let extraction = match source {
        RuntimeCacheSource::RuntimeV1 => {
            let json = runtime_ast_cache_payload_parts(payload_bytes)?;
            let envelope: RuntimeAstCacheEnvelope =
                serde_json::from_slice(json).map_err(|_| RuntimeAstCacheRejection::Decode)?;
            validate_runtime_ast_cache_envelope_fields(
                envelope.envelope_version,
                &envelope.extractor_id,
                envelope.extractor_version,
                &envelope.normalized_path,
                envelope.content_digest,
                envelope.options,
                envelope.complete,
                evidence,
            )?;
            envelope.extraction
        }
        RuntimeCacheSource::Legacy => {
            let payload = cache_payload(payload_bytes).ok_or(RuntimeAstCacheRejection::Decode)?;
            serde_json::from_slice(payload).map_err(|_| RuntimeAstCacheRejection::Decode)?
        }
    };
    if extraction.nodes.is_empty() {
        return Err(RuntimeAstCacheRejection::Empty);
    }
    if !runtime_extraction_provenance_matches(&extraction, &evidence.normalized_path) {
        return Err(RuntimeAstCacheRejection::Provenance);
    }
    Ok(extraction)
}

fn runtime_extraction_provenance_matches(extraction: &Extraction, expected: &str) -> bool {
    fn source_matches(source_file: &str, extra: &BTreeMap<String, Value>, expected: &str) -> bool {
        if source_file == expected {
            return extra
                .get(graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE)
                .and_then(Value::as_str)
                .is_none_or(|container| container == expected);
        }
        let Some(member) = source_file.strip_prefix(expected) else {
            return false;
        };
        member.starts_with("!/")
            && member.len() > 2
            && extra
                .get(graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE)
                .and_then(Value::as_str)
                == Some(expected)
    }

    fn node_source_matches(node: &graphoxide_core::Node, expected: &str) -> bool {
        if !node.source_file.is_empty() {
            return source_matches(&node.source_file, &node.extra, expected);
        }
        // Language extractors use source-less nodes only for independently
        // owned unresolved references. Most retain their owner explicitly;
        // C#'s resolver-managed stub predates `origin_file` but carries both
        // an AST origin and its dedicated lifecycle marker.
        node.extra.get("origin_file").and_then(Value::as_str) == Some(expected)
            || (node.extra.get("_origin").and_then(Value::as_str) == Some("ast")
                && node
                    .extra
                    .get("_csharp_resolution_managed")
                    .and_then(Value::as_bool)
                    == Some(true))
    }

    extraction
        .nodes
        .iter()
        .all(|node| node_source_matches(node, expected))
        && extraction
            .edges
            .iter()
            .all(|edge| source_matches(&edge.source_file, &edge.extra, expected))
        && extraction.hyperedges.iter().all(|hyperedge| {
            let Some(object) = hyperedge.as_object() else {
                return false;
            };
            let Some(source_file) = object.get("source_file").and_then(Value::as_str) else {
                return false;
            };
            if source_file == expected {
                return object
                    .get(graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE)
                    .and_then(Value::as_str)
                    .is_none_or(|container| container == expected);
            }
            source_file
                .strip_prefix(expected)
                .is_some_and(|member| member.starts_with("!/") && member.len() > 2)
                && object
                    .get(graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE)
                    .and_then(Value::as_str)
                    == Some(expected)
        })
}

/// Return the cache-owner-relative legacy AST artifact path after source bytes
/// have been read and hashed. The dedicated cache I/O owner validates this
/// normal-component path again before opening it.
#[must_use]
pub fn runtime_ast_legacy_relative_path(relative: &str, bytes: &[u8]) -> Option<PathBuf> {
    if !runtime_ast_cache_is_eligible(relative) {
        return None;
    }
    Some(cache_relative_path(relative, bytes))
}

/// Compatibility key helper for tests and direct cache callers.
#[must_use]
pub fn runtime_ast_cache_key(relative: &str, bytes: &[u8]) -> Option<RuntimeCacheKey> {
    runtime_ast_cache_evidence(relative, bytes, RuntimeAstCacheOptions::default())
        .map(|evidence| evidence.key)
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
    let key = runtime_ast_cache_key(relative, source_bytes)?;
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
    evidence: &RuntimeAstCacheEvidence,
    extraction: &Extraction,
) -> Result<(), graphoxide_index_runtime::cache::RuntimeCacheError> {
    if !runtime_ast_cache_is_eligible(&evidence.normalized_path) || extraction.nodes.is_empty() {
        return Ok(());
    }
    let payload = encode_runtime_ast_cache_payload(evidence, extraction)?;
    runtime_cache.put(evidence.key, &payload)
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
    evidence: &RuntimeAstCacheEvidence,
    extraction: &Extraction,
) -> Result<Option<RuntimeCacheIoPersistOutcome>, RuntimeCacheIoServiceError> {
    if !runtime_ast_cache_is_eligible(&evidence.normalized_path) || extraction.nodes.is_empty() {
        return Ok(None);
    }
    let encoded_bytes = runtime_ast_cache_payload_len(evidence, extraction)
        .map_err(RuntimeCacheIoServiceError::Cache)?;
    service
        .client()
        .persist_encoded(evidence.key, encoded_bytes, false, |output| {
            encode_runtime_ast_cache_payload_into(output, evidence, extraction)
        })
        .map(Some)
}

fn cache_path(output_dir: &Path, relative: &str, bytes: &[u8]) -> std::path::PathBuf {
    output_dir.join(cache_relative_path(relative, bytes))
}

fn cache_relative_path(relative: &str, bytes: &[u8]) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.update(b"\0");
    hash.update(relative.to_lowercase().as_bytes());
    PathBuf::from(format!(
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
    fn runtime_cache_telemetry_original_shape_remains_constructible() {
        let telemetry = RuntimeCacheTelemetry {
            enabled: true,
            metadata_hits: 1,
            runtime_hits: 2,
            legacy_hits: 3,
            misses: 4,
            bypasses: 5,
            stale_or_corrupt: 6,
            probe_failures: 7,
            payload_reads_avoided: 8,
            parses_avoided: 9,
            stores: 10,
            already_present: 11,
            store_failures: 12,
        };
        assert_eq!(telemetry.store_failures, 12);
    }

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
            decode_runtime_ast_cache_hit(
                legacy,
                &runtime_ast_cache_evidence(relative, source, RuntimeAstCacheOptions::default())
                    .expect("cache evidence")
            )
            .expect("validated legacy payload")
            .nodes[0]
                .id,
            "example"
        );

        let evidence =
            runtime_ast_cache_evidence(relative, source, RuntimeAstCacheOptions::default())
                .expect("cache evidence");
        runtime_ast_cache_put(&mut runtime, &evidence, &extraction).expect("runtime cache write");
        let runtime_hit =
            runtime_ast_cache_payload_from_output(&runtime, &output, relative, source)
                .expect("runtime hit");
        assert_eq!(
            runtime_hit.source,
            graphoxide_index_runtime::cache::RuntimeCacheSource::RuntimeV1
        );
        assert_eq!(
            decode_runtime_ast_cache_hit(runtime_hit, &evidence)
                .expect("validated runtime payload")
                .nodes[0]
                .id,
            "example"
        );
    }

    fn runtime_fixture(relative: &str) -> Extraction {
        Extraction {
            nodes: vec![graphoxide_core::Node {
                id: "fixture".into(),
                label: "fixture()".into(),
                file_type: "code".into(),
                source_file: relative.into(),
                source_location: Some("L1".into()),
                community: None,
                extra: BTreeMap::new(),
            }],
            ..Extraction::default()
        }
    }

    fn runtime_payload_with_json(original: &[u8], value: &Value) -> Vec<u8> {
        let mut payload = original[..RUNTIME_AST_CACHE_PREAMBLE_LEN].to_vec();
        payload.extend(serde_json::to_vec(value).expect("runtime envelope JSON"));
        payload
    }

    #[test]
    fn runtime_ast_key_invalidates_content_path_and_parser_policy() {
        let source = b"def fixture(): pass\n";
        let first = runtime_ast_cache_evidence(
            "src/cafe\u{301}.py",
            source,
            RuntimeAstCacheOptions::isolated(4096),
        )
        .expect("first evidence");
        let normalized = runtime_ast_cache_evidence(
            "src/caf\u{e9}.py",
            source,
            RuntimeAstCacheOptions::isolated(4096),
        )
        .expect("normalized evidence");
        assert_eq!(first.key, normalized.key, "paths are keyed in NFC");
        assert_ne!(
            first.key,
            runtime_ast_cache_evidence(
                "moved/caf\u{e9}.py",
                source,
                RuntimeAstCacheOptions::isolated(4096)
            )
            .expect("moved evidence")
            .key
        );
        assert_ne!(
            first.key,
            runtime_ast_cache_evidence(
                "src/caf\u{e9}.py",
                b"def fixture(): return 1\n",
                RuntimeAstCacheOptions::isolated(4096)
            )
            .expect("changed evidence")
            .key
        );
        assert_ne!(
            first.key,
            runtime_ast_cache_evidence(
                "src/caf\u{e9}.py",
                source,
                RuntimeAstCacheOptions::isolated(8192)
            )
            .expect("different parser allowance")
            .key
        );
        assert_eq!(
            first.key,
            runtime_ast_cache_evidence(
                "src/caf\u{e9}.py",
                source,
                RuntimeAstCacheOptions::isolated(4096)
            )
            .expect("same bytes after metadata-only touch")
            .key,
            "strong source identity is manifest-only and cannot poison a content hit"
        );
    }

    #[test]
    fn runtime_ast_envelope_rejects_incomplete_wrong_path_and_corrupt_payloads() {
        let relative = "src/fixture.py";
        let source = b"def fixture(): pass\n";
        let evidence =
            runtime_ast_cache_evidence(relative, source, RuntimeAstCacheOptions::isolated(4096))
                .expect("cache evidence");
        let extraction = runtime_fixture(relative);
        let payload = encode_runtime_ast_cache_payload(&evidence, &extraction).expect("envelope");
        assert_eq!(
            runtime_ast_cache_payload_len(&evidence, &extraction).expect("payload length"),
            payload.len()
        );

        let mut incomplete: Value =
            serde_json::from_slice(&payload[RUNTIME_AST_CACHE_PREAMBLE_LEN..])
                .expect("envelope JSON");
        incomplete["complete"] = false.into();
        let incomplete = runtime_payload_with_json(&payload, &incomplete);
        assert_eq!(
            decode_runtime_ast_cache_payload(
                RuntimeCacheSource::RuntimeV1,
                &incomplete,
                &evidence,
            )
            .expect_err("incomplete envelope must miss"),
            RuntimeAstCacheRejection::Incomplete
        );

        let mut wrong_path: Value =
            serde_json::from_slice(&payload[RUNTIME_AST_CACHE_PREAMBLE_LEN..])
                .expect("envelope JSON");
        wrong_path["normalized_path"] = "src/other.py".into();
        let wrong_path = runtime_payload_with_json(&payload, &wrong_path);
        assert_eq!(
            decode_runtime_ast_cache_payload(
                RuntimeCacheSource::RuntimeV1,
                &wrong_path,
                &evidence,
            )
            .expect_err("wrong-path envelope must miss"),
            RuntimeAstCacheRejection::Path
        );
        let mut corrupt = payload[..RUNTIME_AST_CACHE_PREAMBLE_LEN].to_vec();
        corrupt.extend_from_slice(b"{not-json");
        assert_eq!(
            decode_runtime_ast_cache_payload(RuntimeCacheSource::RuntimeV1, &corrupt, &evidence,)
                .expect_err("corrupt envelope must miss"),
            RuntimeAstCacheRejection::Decode
        );
    }

    #[test]
    fn runtime_ast_envelope_rejects_cross_source_provenance() {
        let relative = "src/fixture.py";
        let evidence = runtime_ast_cache_evidence(
            relative,
            b"def fixture(): pass\n",
            RuntimeAstCacheOptions::isolated(4096),
        )
        .expect("cache evidence");
        let wrong = runtime_fixture("src/other.py");
        assert!(encode_runtime_ast_cache_payload(&evidence, &wrong).is_err());

        let mut value = serde_json::to_value(runtime_ast_cache_envelope_ref(
            &evidence,
            &runtime_fixture(relative),
        ))
        .expect("envelope JSON");
        value["extraction"]["nodes"][0]["source_file"] = "src/other.py".into();
        let valid = encode_runtime_ast_cache_payload(&evidence, &runtime_fixture(relative))
            .expect("valid runtime envelope");
        let poisoned = runtime_payload_with_json(&valid, &value);
        assert!(
            validate_runtime_ast_cache_payload_header(
                RuntimeCacheSource::RuntimeV1,
                &poisoned,
                &evidence,
            )
            .is_ok(),
            "baseline validation must scan but not allocate or replay extraction facts"
        );
        assert_eq!(
            decode_runtime_ast_cache_payload(RuntimeCacheSource::RuntimeV1, &poisoned, &evidence,)
                .expect_err("cross-source envelope must miss"),
            RuntimeAstCacheRejection::Provenance
        );
    }

    #[test]
    fn runtime_ast_envelope_accepts_owned_python_reference_stubs() {
        let relative = "src/fixture.py";
        let source = b"def fixture(value: MissingType):\n    return value\n";
        let extraction = crate::engine::extract_as_bytes_with_parser_allowance(
            Path::new(relative),
            relative,
            source,
            1024 * 1024,
        )
        .expect("Python extraction");
        assert!(extraction.nodes.iter().any(|node| {
            node.source_file.is_empty()
                && node.extra.get("origin_file").and_then(Value::as_str) == Some(relative)
        }));
        let evidence = runtime_ast_cache_evidence(
            relative,
            source,
            RuntimeAstCacheOptions::isolated(1024 * 1024),
        )
        .expect("cache evidence");
        let payload = encode_runtime_ast_cache_payload(&evidence, &extraction)
            .expect("owned reference provenance is cacheable");
        let replay =
            decode_runtime_ast_cache_payload(RuntimeCacheSource::RuntimeV1, &payload, &evidence)
                .expect("validated replay");
        assert_eq!(
            serde_json::to_value(replay).expect("replay JSON"),
            serde_json::to_value(extraction).expect("cold JSON")
        );
    }

    #[test]
    fn runtime_ast_envelope_accepts_owned_razor_reference_stubs() {
        let relative = "dotnet/WorkerPage.razor";
        let source = br#"@page "/worker"
@inject Matrix.Runtime.IWorker Worker

<WorkerPanel />

@code {
    private string Process() => Worker.Process("matrix");
}
"#;
        let extraction = crate::engine::extract_as_bytes_with_parser_allowance(
            Path::new(relative),
            relative,
            source,
            1024 * 1024,
        )
        .expect("Razor extraction");
        assert!(extraction.nodes.iter().any(|node| {
            node.source_file.is_empty()
                && node.extra.get("origin_file").and_then(Value::as_str) == Some(relative)
        }));
        let evidence = runtime_ast_cache_evidence(
            relative,
            source,
            RuntimeAstCacheOptions::isolated(1024 * 1024),
        )
        .expect("cache evidence");
        let payload = encode_runtime_ast_cache_payload(&evidence, &extraction)
            .expect("owned Razor reference provenance is cacheable");
        let replay =
            decode_runtime_ast_cache_payload(RuntimeCacheSource::RuntimeV1, &payload, &evidence)
                .expect("validated replay");
        assert_eq!(
            serde_json::to_value(replay).expect("replay JSON"),
            serde_json::to_value(extraction).expect("cold JSON")
        );
    }

    #[test]
    fn bounded_runtime_manifest_loads_valid_and_rejects_oversize_or_corrupt_input() {
        let temp = tempfile::tempdir().expect("temporary output root");
        let output = temp.path().join("graphoxide-out");
        let manifest = Manifest::from([(
            "src/main.py".to_owned(),
            ManifestEntry {
                mtime: 1.0,
                ast_version: AST_CACHE_VERSION,
                ast_hash: "content".into(),
                semantic_hash: String::new(),
                runtime_cache: None,
            },
        )]);
        save_manifest_to_output(&output, &manifest).expect("write valid manifest");
        let loaded = load_manifest_from_output_bounded(&output, 16 * 1024);
        assert_eq!(loaded.status, RuntimeManifestLoadStatus::Loaded);
        assert_eq!(
            serde_json::to_value(loaded.manifest).expect("loaded manifest JSON"),
            serde_json::to_value(manifest).expect("expected manifest JSON")
        );

        fs::write(output.join("manifest.json"), b"{not-json").expect("write corrupt manifest");
        let corrupt = load_manifest_from_output_bounded(&output, 16 * 1024);
        assert_eq!(corrupt.status, RuntimeManifestLoadStatus::Corrupt);
        assert!(corrupt.manifest.is_empty());

        let sparse = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(output.join("manifest.json"))
            .expect("open sparse manifest");
        sparse.set_len(1024 * 1024).expect("size sparse manifest");
        let oversize = load_manifest_from_output_bounded(&output, 4096);
        assert_eq!(oversize.status, RuntimeManifestLoadStatus::Oversize);
        assert!(oversize.manifest.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_runtime_manifest_rejects_final_symlinks_and_hardlinks() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary output root");
        let output = temp.path().join("graphoxide-out");
        fs::create_dir_all(&output).expect("create output root");
        let target = temp.path().join("manifest-target.json");
        fs::write(&target, b"{}").expect("write manifest target");
        symlink(&target, output.join("manifest.json")).expect("link manifest target");
        assert_eq!(
            load_manifest_from_output_bounded(&output, 4096).status,
            RuntimeManifestLoadStatus::UnsafeOrUnreadable
        );

        fs::remove_file(output.join("manifest.json")).expect("remove symlink");
        fs::hard_link(&target, output.join("manifest.json")).expect("hardlink manifest target");
        assert_eq!(
            load_manifest_from_output_bounded(&output, 4096).status,
            RuntimeManifestLoadStatus::UnsafeOrUnreadable
        );

        fs::remove_file(output.join("manifest.json")).expect("remove hardlink");
        let fifo_path = output.join("manifest.json");
        let fifo = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).expect("FIFO path");
        // SAFETY: `fifo` is a live NUL-terminated path and mkfifo does not
        // retain the pointer after returning.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert_eq!(
            load_manifest_from_output_bounded(&output, 4096).status,
            RuntimeManifestLoadStatus::UnsafeOrUnreadable,
            "a FIFO manifest must be rejected without a blocking open"
        );
    }

    #[test]
    fn opened_manifest_identity_detects_same_size_restored_mtime_rewrite() {
        use std::io::{Seek as _, SeekFrom, Write as _};

        let temp = tempfile::tempdir().expect("temporary output root");
        let path = temp.path().join("manifest.json");
        fs::write(&path, b"{\"a\":1}").expect("write first manifest generation");
        let original_mtime = filetime::FileTime::from_last_modification_time(
            &fs::metadata(&path).expect("first manifest metadata"),
        );
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open held manifest handle");
        let before = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
            .expect("validate first generation")
            .expect("strong filesystem identity");
        file.seek(SeekFrom::Start(0)).expect("rewind manifest");
        file.write_all(b"{\"b\":2}")
            .expect("rewrite equal-length manifest");
        file.sync_all().expect("sync rewritten manifest");
        filetime::set_file_mtime(&path, original_mtime).expect("restore manifest mtime");
        let after = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
            .expect("validate rewritten generation")
            .expect("strong filesystem identity");
        assert_ne!(
            before, after,
            "strong handle identity must include generation evidence beyond length and mtime"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reopened_manifest_identity_detects_rename_substitution() {
        let temp = tempfile::tempdir().expect("temporary output root");
        let path = temp.path().join("manifest.json");
        let backup = temp.path().join("manifest.previous.json");
        fs::write(&path, b"{\"a\":1}").expect("write first manifest generation");
        let held = fs::File::open(&path).expect("open held manifest generation");
        let held_before = graphoxide_index_runtime::validate_opened_regular_single_link(&held)
            .expect("validate held manifest")
            .expect("strong filesystem identity");

        fs::rename(&path, &backup).expect("rename held generation aside");
        fs::write(&path, b"{\"b\":2}").expect("install equal-length replacement");
        let held_after = graphoxide_index_runtime::validate_opened_regular_single_link(&held)
            .expect("revalidate held manifest")
            .expect("strong filesystem identity");
        let current = fs::File::open(&path).expect("reopen current manifest path");
        let current_identity =
            graphoxide_index_runtime::validate_opened_regular_single_link(&current)
                .expect("validate current manifest")
                .expect("strong filesystem identity");

        assert_ne!(
            held_after, current_identity,
            "the production reopen check must reject the replacement generation"
        );
        assert!(
            held_before != held_after || held_before != current_identity,
            "at least one strong generation check must observe rename substitution"
        );
    }

    fn purge_fixture_hash(byte: u8) -> String {
        std::iter::repeat_n(char::from(byte), 64).collect()
    }

    fn write_purge_fixture(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().expect("fixture parent"))
            .expect("create purge fixture parent");
        fs::write(path, contents).expect("write purge fixture");
    }

    fn tree_contains_extension(directory: &Path, extension: &str) -> bool {
        fs::read_dir(directory)
            .expect("enumerate cache fixture")
            .map(|entry| entry.expect("cache fixture entry").path())
            .any(|path| {
                if path.is_dir() {
                    tree_contains_extension(&path, extension)
                } else {
                    path.extension().is_some_and(|value| value == extension)
                }
            })
    }

    #[test]
    fn structured_redaction_schema_preparation_purges_json_and_runtime_v1() {
        let temp = tempfile::tempdir().expect("temporary output root");
        let output = temp.path().join("graphoxide-out");
        let legacy_json = output
            .join("cache/ast/v29")
            .join(format!("{}.json", purge_fixture_hash(b'a')));
        write_purge_fixture(&legacy_json, b"raw-json-secret");

        let relative = "config/secrets.json";
        let source = br#"{"password":"raw-runtime-secret"}"#;
        let evidence =
            runtime_ast_cache_evidence(relative, source, RuntimeAstCacheOptions::isolated(4096))
                .expect("runtime cache evidence");
        let mut runtime = RuntimeCache::open(&output).expect("active runtime cache");
        runtime_ast_cache_put(&mut runtime, &evidence, &runtime_fixture(relative))
            .expect("runtime cache payload");
        let active_runtime = runtime.root().to_path_buf();
        assert!(tree_contains_extension(&active_runtime, "gxa"));
        drop(runtime);
        let retired_runtime = output.join("cache/runtime-v1");
        fs::rename(active_runtime, &retired_runtime).expect("stage retired runtime-v1 fixture");

        prepare_structured_redaction_cache_schema(&output)
            .expect("prepare structured-redaction cache schema");
        assert!(!legacy_json.exists());
        assert!(retired_runtime.exists(), "retired owner-lock root remains");
        assert!(
            !tree_contains_extension(&retired_runtime, "gxa"),
            "retired runtime payload survived schema preparation"
        );
        prepare_structured_redaction_cache_schema(&output).expect("idempotent schema preparation");
    }

    #[test]
    fn structured_redaction_schema_preparation_hides_unexpected_runtime_cache_names() {
        let temp = tempfile::tempdir().expect("temporary output root");
        let output = temp.path().join("graphoxide-out");
        let shard = output.join("cache/runtime-v1/shards/00");
        fs::create_dir_all(&shard).expect("retired runtime shard");
        let valid = shard.join("active-0.gxa");
        fs::write(&valid, b"must-not-be-truncated").expect("valid retired payload");
        let planted_name = "sk_live_RUNTIME_CACHE_CLI_DIAGNOSTIC_SENTINEL_49";
        let unexpected = shard.join(planted_name);
        fs::write(&unexpected, b"unexpected").expect("unexpected retired entry");

        let error = prepare_structured_redaction_cache_schema(&output)
            .expect_err("unexpected retired runtime cache content");
        for rendered in [
            format!("{error}"),
            format!("{error:#}"),
            format!("{error:?}"),
        ] {
            assert!(
                !rendered.contains(planted_name),
                "CLI-formatted migration diagnostics exposed an attacker-controlled cache basename"
            );
        }
        assert_eq!(
            fs::read(&valid).expect("preflight preserved valid retired payload"),
            b"must-not-be-truncated"
        );
        assert_eq!(
            fs::read(&unexpected).expect("preflight preserved unexpected retired entry"),
            b"unexpected"
        );
    }

    #[test]
    fn opening_current_ast_cache_preserves_other_versions_and_root_entries() {
        let temp = tempfile::tempdir().expect("temporary cache root");
        let ast = temp.path().join("graphoxide-out/cache/ast");
        let v29 = ast
            .join("v29")
            .join(format!("{}.json", purge_fixture_hash(b'a')));
        let v31 = ast
            .join("v31")
            .join(format!("{}.json", purge_fixture_hash(b'b')));
        let unversioned = ast.join(format!("{}.json", purge_fixture_hash(b'c')));
        let unrelated = ast.join("operator-note.txt");
        for path in [&v29, &v31, &unversioned, &unrelated] {
            write_purge_fixture(path, b"preserve");
        }

        assert_eq!(
            cache_dir_with_ast_version(temp.path(), "ast", None, AST_CACHE_VERSION)
                .expect("open current AST cache"),
            ast.join(format!("v{AST_CACHE_VERSION}"))
        );
        for path in [v29, v31, unversioned, unrelated] {
            assert_eq!(
                fs::read(&path).expect("preserved AST entry"),
                b"preserve",
                "opening the current cache erased {}",
                path.display()
            );
        }
    }

    #[test]
    fn pre_redaction_ast_purge_is_exact_bounded_and_idempotent() {
        let temp = tempfile::tempdir().expect("temporary output root");
        let output = temp.path().join("graphoxide-out");
        let cache = output.join("cache");
        let ast = cache.join("ast");
        let semantic = cache.join("semantic");
        let hash_a = purge_fixture_hash(b'a');
        let hash_b = purge_fixture_hash(b'b');
        let hash_c = purge_fixture_hash(b'c');
        let hash_d = purge_fixture_hash(b'd');
        let hash_e = purge_fixture_hash(b'e');
        let hash_f = purge_fixture_hash(b'f');

        let cache_unversioned = cache.join(format!("{hash_a}.json"));
        let cache_interrupted = cache.join(format!(".{hash_b}.json.101.1.tmp"));
        let ast_unversioned = ast.join(format!("{hash_c}.json"));
        let ast_old_interrupted = ast.join(format!("{hash_d}.202.tmp"));
        let v0_artifact = ast.join("v0").join(format!("{hash_e}.json"));
        let v29_interrupted = ast.join("v29").join(format!(".{hash_f}.json.303.2.tmp"));
        for path in [
            &cache_unversioned,
            &cache_interrupted,
            &ast_unversioned,
            &ast_old_interrupted,
            &v0_artifact,
            &v29_interrupted,
        ] {
            write_purge_fixture(path, b"raw-secret");
        }
        fs::create_dir_all(ast.join("v12")).expect("empty interrupted version directory");

        let v30 = ast.join("v30").join(format!("{hash_a}.json"));
        let v31 = ast.join("v31").join(format!("{hash_b}.json"));
        let semantic_artifact = semantic.join(format!("{hash_c}.json"));
        let unrelated_cache = cache.join("operator-note.json");
        let unrelated_ast = ast.join("operator-note.json");
        let uppercase_hash = ast.join(format!("{}.json", hash_d.to_ascii_uppercase()));
        for path in [
            &v30,
            &v31,
            &semantic_artifact,
            &unrelated_cache,
            &unrelated_ast,
            &uppercase_hash,
        ] {
            write_purge_fixture(path, b"preserve");
        }

        assert_eq!(
            purge_pre_redaction_ast_caches_with_limits(&output, PRE_REDACTION_AST_PURGE_LIMITS)
                .expect("purge legacy AST artifacts"),
            6
        );
        for path in [
            cache_unversioned,
            cache_interrupted,
            ast_unversioned,
            ast_old_interrupted,
            v0_artifact,
            v29_interrupted,
        ] {
            assert!(
                !path.exists(),
                "legacy artifact survived: {}",
                path.display()
            );
        }
        for version in ["v0", "v12", "v29"] {
            assert!(
                !ast.join(version).exists(),
                "emptied legacy version directory survived"
            );
        }
        for path in [
            v30,
            v31,
            semantic_artifact,
            unrelated_cache,
            unrelated_ast,
            uppercase_hash,
        ] {
            assert_eq!(
                fs::read(&path).expect("preserved artifact"),
                b"preserve",
                "purge changed unrelated artifact {}",
                path.display()
            );
        }
        prepare_structured_redaction_cache_schema(&output)
            .expect("idempotent full schema preparation");
        assert_eq!(
            purge_pre_redaction_ast_caches_with_limits(&output, PRE_REDACTION_AST_PURGE_LIMITS)
                .expect("idempotent counted purge"),
            0
        );
    }

    #[test]
    fn pre_redaction_ast_purge_preflights_unexpected_nested_content() {
        let temp = tempfile::tempdir().expect("temporary output root");
        let output = temp.path().join("graphoxide-out");
        let ast = output.join("cache/ast");
        let first = ast
            .join("v0")
            .join(format!("{}.json", purge_fixture_hash(b'a')));
        write_purge_fixture(&first, b"must-not-be-truncated");
        let planted_name = "sk_live_AST_CACHE_DIAGNOSTIC_SENTINEL_49";
        let unexpected = ast.join("v29").join(planted_name);
        write_purge_fixture(&unexpected, b"unexpected");

        let error = prepare_structured_redaction_cache_schema(&output)
            .expect_err("unexpected pre-redaction cache content");
        assert!(
            !format!("{error:#}").contains(planted_name),
            "user-facing migration diagnostics exposed an attacker-controlled cache basename"
        );
        assert_eq!(
            fs::read(&first).expect("preflight preserved first artifact"),
            b"must-not-be-truncated"
        );
        assert_eq!(
            fs::read(&unexpected).expect("preflight preserved unexpected entry"),
            b"unexpected"
        );
    }

    #[test]
    fn pre_redaction_ast_purge_enforces_root_version_and_total_caps() {
        let root_cap = tempfile::tempdir().expect("root-cap output root");
        let root_output = root_cap.path().join("graphoxide-out");
        let root_cache = root_output.join("cache");
        let root_artifact = root_cache.join(format!("{}.json", purge_fixture_hash(b'a')));
        write_purge_fixture(&root_artifact, b"root-cap-secret");
        fs::create_dir_all(root_cache.join("ast")).expect("AST root");
        assert!(purge_pre_redaction_ast_caches_with_limits(
            &root_output,
            PreRedactionAstPurgeLimits {
                max_root_entries: 1,
                max_version_entries: 8,
                max_artifacts: 8,
            },
        )
        .is_err());
        assert_eq!(
            fs::read(root_artifact).expect("root cap preserved"),
            b"root-cap-secret"
        );

        let version_cap = tempfile::tempdir().expect("version-cap output root");
        let version_output = version_cap.path().join("graphoxide-out");
        let v0 = version_output.join("cache/ast/v0");
        let version_first = v0.join(format!("{}.json", purge_fixture_hash(b'a')));
        let version_second = v0.join(format!("{}.json", purge_fixture_hash(b'b')));
        write_purge_fixture(&version_first, b"first");
        write_purge_fixture(&version_second, b"second");
        assert!(purge_pre_redaction_ast_caches_with_limits(
            &version_output,
            PreRedactionAstPurgeLimits {
                max_root_entries: 8,
                max_version_entries: 1,
                max_artifacts: 8,
            },
        )
        .is_err());
        assert_eq!(
            fs::read(version_first).expect("version cap preserved"),
            b"first"
        );
        assert_eq!(
            fs::read(version_second).expect("version cap preserved"),
            b"second"
        );

        let total_cap = tempfile::tempdir().expect("total-cap output root");
        let total_output = total_cap.path().join("graphoxide-out");
        let direct = total_output
            .join("cache")
            .join(format!("{}.json", purge_fixture_hash(b'a')));
        let versioned = total_output
            .join("cache/ast/v29")
            .join(format!("{}.json", purge_fixture_hash(b'b')));
        write_purge_fixture(&direct, b"direct");
        write_purge_fixture(&versioned, b"versioned");
        assert!(purge_pre_redaction_ast_caches_with_limits(
            &total_output,
            PreRedactionAstPurgeLimits {
                max_root_entries: 8,
                max_version_entries: 8,
                max_artifacts: 1,
            },
        )
        .is_err());
        assert_eq!(fs::read(direct).expect("total cap preserved"), b"direct");
        assert_eq!(
            fs::read(versioned).expect("total cap preserved"),
            b"versioned"
        );
    }

    #[test]
    fn legacy_ast_purge_artifact_names_are_narrow() {
        let hash = purge_fixture_hash(b'a');
        for name in [
            format!("{hash}.json"),
            format!("{hash}.123.tmp"),
            format!(".{hash}.json.123.456.tmp"),
        ] {
            assert!(legacy_ast_artifact_name_is_exact(name.as_ref()), "{name}");
        }
        for name in [
            format!("{}.json", hash.to_ascii_uppercase()),
            format!("{hash}.json.bak"),
            format!("{hash}.tmp"),
            format!(".{hash}.json.123.tmp"),
            format!(".{hash}.json.123.456.789.tmp"),
            format!("{}.json", &hash[..63]),
        ] {
            assert!(!legacy_ast_artifact_name_is_exact(name.as_ref()), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn pre_redaction_ast_purge_rejects_links_and_special_files_without_following() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let cache_link = tempfile::tempdir().expect("cache-link output root");
        let cache_link_output = cache_link.path().join("graphoxide-out");
        fs::create_dir_all(&cache_link_output).expect("cache-link output");
        let outside_cache = cache_link.path().join("outside-cache");
        let outside_secret = outside_cache
            .join("ast/v29")
            .join(format!("{}.json", purge_fixture_hash(b'a')));
        write_purge_fixture(&outside_secret, b"outside-cache-secret");
        symlink(&outside_cache, cache_link_output.join("cache")).expect("symlink cache root");
        assert!(purge_pre_redaction_ast_caches(&cache_link_output).is_err());
        assert_eq!(
            fs::read(&outside_secret).expect("outside cache preserved"),
            b"outside-cache-secret"
        );

        let version_link = tempfile::tempdir().expect("version-link output root");
        let version_link_output = version_link.path().join("graphoxide-out");
        let version_link_ast = version_link_output.join("cache/ast");
        fs::create_dir_all(&version_link_ast).expect("version-link AST root");
        let outside_version = version_link.path().join("outside-version");
        let outside_version_secret =
            outside_version.join(format!("{}.json", purge_fixture_hash(b'b')));
        write_purge_fixture(&outside_version_secret, b"outside-version-secret");
        symlink(&outside_version, version_link_ast.join("v29")).expect("symlink version root");
        assert!(purge_pre_redaction_ast_caches(&version_link_output).is_err());
        assert_eq!(
            fs::read(&outside_version_secret).expect("outside version preserved"),
            b"outside-version-secret"
        );

        let artifact_link = tempfile::tempdir().expect("artifact-link output root");
        let artifact_link_output = artifact_link.path().join("graphoxide-out");
        let artifact_link_path = artifact_link_output
            .join("cache/ast/v29")
            .join(format!("{}.json", purge_fixture_hash(b'c')));
        fs::create_dir_all(artifact_link_path.parent().expect("artifact-link parent"))
            .expect("artifact-link parent");
        let artifact_link_target = artifact_link.path().join("outside-artifact.json");
        fs::write(&artifact_link_target, b"outside-artifact-secret").expect("outside artifact");
        symlink(&artifact_link_target, &artifact_link_path).expect("symlink artifact");
        assert!(purge_pre_redaction_ast_caches(&artifact_link_output).is_err());
        assert_eq!(
            fs::read(&artifact_link_target).expect("artifact target preserved"),
            b"outside-artifact-secret"
        );

        let hardlink = tempfile::tempdir().expect("hardlink output root");
        let hardlink_output = hardlink.path().join("graphoxide-out");
        let earlier_artifact = hardlink_output
            .join("cache/ast/v0")
            .join(format!("{}.json", purge_fixture_hash(b'a')));
        write_purge_fixture(&earlier_artifact, b"earlier-must-remain");
        let hardlink_path = hardlink_output
            .join("cache/ast/v29")
            .join(format!("{}.json", purge_fixture_hash(b'd')));
        fs::create_dir_all(hardlink_path.parent().expect("hardlink parent"))
            .expect("hardlink parent");
        let hardlink_target = hardlink.path().join("outside-hardlink.json");
        fs::write(&hardlink_target, b"outside-hardlink-secret").expect("hardlink target");
        fs::hard_link(&hardlink_target, &hardlink_path).expect("hardlink artifact");
        assert!(purge_pre_redaction_ast_caches(&hardlink_output).is_err());
        assert_eq!(
            fs::read(&hardlink_target).expect("hardlink target preserved"),
            b"outside-hardlink-secret"
        );
        assert_eq!(
            fs::read(&earlier_artifact).expect("earlier artifact preserved"),
            b"earlier-must-remain",
            "hardlink must be rejected during preflight before any truncation"
        );

        let special = tempfile::tempdir().expect("special output root");
        let special_output = special.path().join("graphoxide-out");
        let fifo_path = special_output
            .join("cache/ast/v29")
            .join(format!("{}.json", purge_fixture_hash(b'e')));
        fs::create_dir_all(fifo_path.parent().expect("FIFO parent")).expect("FIFO parent");
        let fifo = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).expect("FIFO path");
        // SAFETY: `fifo` is a live NUL-terminated path and mkfifo does not
        // retain the pointer after returning.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(purge_pre_redaction_ast_caches(&special_output).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn pre_redaction_ast_purge_rejects_windows_hardlinks() {
        let temp = tempfile::tempdir().expect("hardlink output root");
        let output = temp.path().join("graphoxide-out");
        let earlier_artifact = output
            .join("cache/ast/v0")
            .join(format!("{}.json", purge_fixture_hash(b'b')));
        write_purge_fixture(&earlier_artifact, b"earlier-must-remain");
        let artifact = output
            .join("cache/ast/v29")
            .join(format!("{}.json", purge_fixture_hash(b'a')));
        fs::create_dir_all(artifact.parent().expect("hardlink parent")).expect("hardlink parent");
        let target = temp.path().join("outside-hardlink.json");
        fs::write(&target, b"outside-hardlink-secret").expect("hardlink target");
        fs::hard_link(&target, &artifact).expect("hardlink artifact");
        assert!(purge_pre_redaction_ast_caches(&output).is_err());
        assert_eq!(
            fs::read(target).expect("hardlink target preserved"),
            b"outside-hardlink-secret"
        );
        assert_eq!(
            fs::read(earlier_artifact).expect("earlier artifact preserved"),
            b"earlier-must-remain",
            "Windows hardlink must fail during preflight"
        );
    }

    #[cfg(not(any(unix, windows)))]
    #[test]
    fn pre_redaction_ast_purge_fails_closed_without_strong_platform_identity() {
        let temp = tempfile::tempdir().expect("temporary output root");
        let output = temp.path().join("graphoxide-out");
        let artifact = output
            .join("cache/ast/v29")
            .join(format!("{}.json", purge_fixture_hash(b'a')));
        write_purge_fixture(&artifact, b"raw-secret");
        assert!(purge_pre_redaction_ast_caches(&output).is_err());
        assert_eq!(
            fs::read(artifact).expect("artifact preserved"),
            b"raw-secret"
        );
    }
}
