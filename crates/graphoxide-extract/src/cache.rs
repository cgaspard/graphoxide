//! Incremental manifest and content-addressed AST cache.

use graphoxide_core::Extraction;
use md5::{Digest as _, Md5};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
pub const AST_CACHE_VERSION: u32 = 25;
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub mtime: f64,
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

#[derive(Debug, Clone, Default)]
struct StatIndex {
    anchor: PathBuf,
    entries: BTreeMap<PathBuf, StatIndexEntry>,
}

type StatIndexKey = (PathBuf, PathBuf);

static STAT_INDEXES: OnceLock<Mutex<BTreeMap<StatIndexKey, StatIndex>>> = OnceLock::new();

fn stat_indexes() -> &'static Mutex<BTreeMap<StatIndexKey, StatIndex>> {
    STAT_INDEXES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Strip a leading, well-formed YAML frontmatter block without changing any
/// byte after the closing delimiter. Delimiters must be whole `---` lines;
/// thematic breaks (`----`) and prose (`--- title`) are ordinary content.
pub fn body_content(content: &[u8]) -> Vec<u8> {
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
        return content.to_vec();
    }

    let mut start = first_end;
    while start < content.len() {
        let end = content[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(content.len(), |index| start + index + 1);
        if delimiter(&content[start..end]) {
            return content[start + 3..].to_vec();
        }
        start = end;
    }
    content.to_vec()
}

/// SHA-256 over effective file contents plus a lower-cased, root-relative path.
/// Markdown frontmatter is intentionally excluded from the content portion so
/// metadata-only edits do not invalidate expensive extraction results.
pub fn file_hash(path: &Path, root: &Path) -> anyhow::Result<String> {
    file_hash_at(path, root, root)
}

fn file_hash_at(path: &Path, root: &Path, index_root: &Path) -> anyhow::Result<String> {
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
    if let Some(hash) = stat_indexes()
        .lock()
        .expect("stat index mutex poisoned")
        .get(&index_key)
        .and_then(|index| index.entries.get(&resolved))
        .filter(|entry| entry.size == size && entry.mtime_ns == mtime_ns)
        .and_then(|entry| entry.hashes.get(&salt))
        .cloned()
    {
        return Ok(hash);
    }

    let raw = fs::read(path)?;
    let content = if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
    {
        body_content(&raw)
    } else {
        raw
    };
    let mut hash = Sha256::new();
    hash.update(content);
    hash.update(b"\0");
    hash.update(salt.as_bytes());
    let digest = hex::encode(hash.finalize());
    {
        let mut indexes = stat_indexes().lock().expect("stat index mutex poisoned");
        let index = indexes.get_mut(&index_key).expect("stat index was loaded");
        let entry = index.entries.entry(resolved).or_default();
        if entry.size != size || entry.mtime_ns != mtime_ns {
            *entry = StatIndexEntry {
                size,
                mtime_ns,
                ..StatIndexEntry::default()
            };
        }
        entry.hashes.insert(salt, digest.clone());
    }
    flush_stat_index_at(&root_resolved, &index_root)?;
    Ok(digest)
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
        .lock()
        .expect("stat index mutex poisoned")
        .get(&index_key)
        .and_then(|index| index.entries.get(&resolved))
        .filter(|entry| entry.size == size && entry.mtime_ns == mtime_ns)
        .and_then(|entry| entry.word_count)
    {
        return Ok(word_count);
    }
    let word_count = compute(path)?;
    {
        let mut indexes = stat_indexes().lock().expect("stat index mutex poisoned");
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
    let stored = {
        let mut indexes = stat_indexes().lock().expect("stat index mutex poisoned");
        let index = indexes.get_mut(&key).expect("stat index was loaded");
        index.entries.retain(|path, _| path.is_file());
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
    let mut indexes = stat_indexes().lock().expect("stat index mutex poisoned");
    let key = (anchor.to_path_buf(), index_root.to_path_buf());
    if indexes.contains_key(&key) {
        return Ok(());
    }
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
    indexes.insert(
        key,
        StatIndex {
            anchor: anchor.to_owned(),
            entries,
        },
    );
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
    if let Ok(canonical) = fs::canonicalize(&lexical) {
        if canonical != lexical {
            spellings.push(canonical);
        }
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
    let entry = cache_dir_with_ast_version(cache_root, kind, prompt_fingerprint, ast_version)
        .ok()?
        .join(format!("{hash}.json"));
    let mut value: Value = fs::read(entry)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())?;
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
        .lock()
        .expect("stat index mutex poisoned")
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
        if options.merge_existing {
            if let Some(existing) = load_cached_value_internal(
                &path,
                root,
                cache_root,
                &kind,
                fingerprint.as_deref(),
                AST_CACHE_VERSION,
                true,
            ) {
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
}
