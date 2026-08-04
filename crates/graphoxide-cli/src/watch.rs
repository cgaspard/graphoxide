//! Deterministic watch/update coordination.
//!
//! Filesystem notification is deliberately kept at the outer CLI boundary.
//! This module owns the durable state machine underneath it: event filtering,
//! non-blocking rebuild locks, the pending-change journal, tier-aware graph
//! reconciliation, shrink protection, portable root markers, and manifests.

use fs2::FileExt as _;
use graphoxide_core::{Edge, Extraction, KnowledgeGraph, Node};
use graphoxide_extract::detect::{
    self, DetectOptions, DetectResult, FileType, ManifestKind, SaveManifestOptions,
};
use graphoxide_graph::{origin_is_structural, BuildOptions};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Instant,
};

pub const OUTPUT_DIRECTORY: &str = "graphoxide-out";
pub const NEEDS_UPDATE: &str = "needs_update";
pub const PENDING_CHANGES: &str = ".pending_changes";
pub const PENDING_CHANGES_LOCK: &str = ".pending_changes.lock";
pub const REBUILD_LOCK: &str = ".rebuild.lock";
pub const ROOT_MARKER: &str = ".graphoxide_root";
pub const COMPAT_ROOT_MARKER: &str = ".graphify_root";
pub const BUILD_CONFIG: &str = ".graphoxide_build.json";
pub const COMPAT_BUILD_CONFIG: &str = ".graphify_build.json";
pub const PENDING_DRAIN_MAX_PASSES: usize = 20;

/// The upstream watcher intentionally excludes audio/video even though the
/// semantic scanner can ingest them.
pub const WATCHED_EXTENSIONS: &[&str] = &[
    ".py",
    ".pyi",
    ".ts",
    ".tsx",
    ".mts",
    ".cts",
    ".js",
    ".jsx",
    ".mjs",
    ".cjs",
    ".ejs",
    ".ets",
    ".go",
    ".rs",
    ".java",
    ".groovy",
    ".gradle",
    ".cpp",
    ".cc",
    ".cxx",
    ".c",
    ".h",
    ".hpp",
    ".hh",
    ".cu",
    ".cuh",
    ".metal",
    ".rb",
    ".rake",
    ".swift",
    ".kt",
    ".kts",
    ".cs",
    ".scala",
    ".php",
    ".lua",
    ".luau",
    ".toc",
    ".zig",
    ".ps1",
    ".psm1",
    ".psd1",
    ".ex",
    ".exs",
    ".m",
    ".mm",
    ".jl",
    ".vue",
    ".svelte",
    ".astro",
    ".dart",
    ".v",
    ".sv",
    ".svh",
    ".sql",
    ".r",
    ".f",
    ".f90",
    ".f95",
    ".f03",
    ".f08",
    ".pas",
    ".pp",
    ".dpr",
    ".dpk",
    ".lpr",
    ".inc",
    ".dfm",
    ".lfm",
    ".lpk",
    ".sh",
    ".bash",
    ".json",
    ".tf",
    ".tfvars",
    ".hcl",
    ".dm",
    ".dme",
    ".dmi",
    ".dmm",
    ".dmf",
    ".sln",
    ".slnx",
    ".csproj",
    ".fsproj",
    ".vbproj",
    ".xaml",
    ".razor",
    ".cshtml",
    ".cls",
    ".trigger",
    ".md",
    ".markdown",
    ".mdx",
    ".qmd",
    ".skill",
    ".txt",
    ".rst",
    ".html",
    ".yaml",
    ".yml",
    ".toml",
    ".xml",
    ".pdf",
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".webp",
    ".svg",
];

pub fn is_watched_extension(extension: &str) -> bool {
    let normalized = if extension.starts_with('.') {
        extension.to_ascii_lowercase()
    } else {
        format!(".{}", extension.to_ascii_lowercase())
    };
    WATCHED_EXTENSIONS.contains(&normalized.as_str())
}

pub fn notify_only(root: &Path) -> anyhow::Result<PathBuf> {
    notify_only_in(&root.join(OUTPUT_DIRECTORY))
}

pub fn notify_only_in(output_directory: &Path) -> anyhow::Result<PathBuf> {
    let flag = output_directory.join(NEEDS_UPDATE);
    if let Some(parent) = flag.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&flag, b"1")?;
    Ok(flag)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotice {
    pub pending: bool,
    pub message: Option<String>,
}

pub fn check_update(root: &Path) -> UpdateNotice {
    check_update_in(root, &root.join(OUTPUT_DIRECTORY))
}

pub fn check_update_in(root: &Path, output_directory: &Path) -> UpdateNotice {
    let pending = output_directory.join(NEEDS_UPDATE).is_file();
    UpdateNotice {
        pending,
        message: pending.then(|| {
            format!(
                "Pending non-code changes in {}. Run `graphoxide update {}` to rebuild the offline graph.",
                root.display(),
                root.display()
            )
        }),
    }
}

pub fn output_directory_from_env(root: &Path) -> Option<PathBuf> {
    std::env::var_os("GRAPHOXIDE_OUT")
        .or_else(|| std::env::var_os("GRAPHIFY_OUT"))
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
}

/// Injectable availability check used by callers that make notification
/// support optional. The Rust binary ships `notify`; embedders may not.
pub fn require_watch_backend(available: bool) -> anyhow::Result<()> {
    anyhow::ensure!(available, "filesystem watch backend not installed");
    Ok(())
}

#[derive(Debug)]
pub struct RebuildLockGuard {
    file: File,
    path: PathBuf,
    acquired: bool,
}

impl RebuildLockGuard {
    pub fn acquire(out: &Path, blocking: bool) -> anyhow::Result<Option<Self>> {
        fs::create_dir_all(out)?;
        let path = out.join(REBUILD_LOCK);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let locked = if blocking {
            file.lock_exclusive().map(|_| true)
        } else {
            match file.try_lock_exclusive() {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
                Err(error) => Err(error),
            }
        }?;
        if !locked {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        file.flush()?;
        Ok(Some(Self {
            file,
            path,
            acquired: true,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RebuildLockGuard {
    fn drop(&mut self) {
        if !self.acquired {
            return;
        }
        let _ = self.file.unlock();
        self.acquired = false;
    }
}

fn lock_pending_journal(out: &Path) -> anyhow::Result<File> {
    fs::create_dir_all(out)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(out.join(PENDING_CHANGES_LOCK))?;
    lock.lock_exclusive()?;
    Ok(lock)
}

pub fn queue_pending(out: &Path, changed_paths: &[PathBuf]) -> anyhow::Result<()> {
    if changed_paths.is_empty() {
        return Ok(());
    }
    let _journal = lock_pending_journal(out)?;
    let mut pending = OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join(PENDING_CHANGES))?;
    for path in changed_paths {
        writeln!(pending, "{}", path.display())?;
    }
    pending.sync_data()?;
    Ok(())
}

pub fn drain_pending(out: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let _journal = lock_pending_journal(out)?;
    let pending = out.join(PENDING_CHANGES);
    let mut raw = String::new();
    match File::open(&pending).and_then(|mut file| file.read_to_string(&mut raw)) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    match fs::remove_file(&pending) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut seen = HashSet::new();
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| seen.insert((*line).to_owned()))
        .map(PathBuf::from)
        .collect())
}

pub fn merge_changed_paths(sources: &[Option<&[PathBuf]>]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for paths in sources.iter().flatten() {
        for path in *paths {
            let key = path.to_string_lossy().into_owned();
            if seen.insert(key) {
                merged.push(path.clone());
            }
        }
    }
    merged
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedBuildConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    #[serde(
        default = "default_true",
        alias = "gitignore",
        skip_serializing_if = "is_true"
    )]
    pub honor_gitignore: bool,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub cluster: bool,
}

const fn default_true() -> bool {
    true
}

const fn is_true(value: &bool) -> bool {
    *value
}

impl Default for PersistedBuildConfig {
    fn default() -> Self {
        Self {
            excludes: Vec::new(),
            honor_gitignore: true,
            cluster: true,
        }
    }
}

pub fn read_build_config(out: &Path) -> PersistedBuildConfig {
    for name in [BUILD_CONFIG, COMPAT_BUILD_CONFIG] {
        let Ok(bytes) = fs::read(out.join(name)) else {
            continue;
        };
        if let Ok(config) = serde_json::from_slice(&bytes) {
            return config;
        }
    }
    PersistedBuildConfig::default()
}

pub fn write_build_config(
    out: &Path,
    excludes: Option<&[String]>,
    honor_gitignore: Option<bool>,
) -> anyhow::Result<PersistedBuildConfig> {
    write_build_config_with_cluster(out, excludes, honor_gitignore, None)
}

pub fn write_build_config_with_cluster(
    out: &Path,
    excludes: Option<&[String]>,
    honor_gitignore: Option<bool>,
    cluster: Option<bool>,
) -> anyhow::Result<PersistedBuildConfig> {
    let mut config = read_build_config(out);
    if let Some(excludes) = excludes {
        config.excludes = excludes.to_vec();
    }
    if let Some(honor) = honor_gitignore {
        config.honor_gitignore = honor;
    }
    if let Some(cluster) = cluster {
        config.cluster = cluster;
    }
    fs::create_dir_all(out)?;
    for name in [BUILD_CONFIG, COMPAT_BUILD_CONFIG] {
        graphoxide_core::write_json_atomic(out.join(name), &config, false)?;
    }
    Ok(config)
}

#[derive(Debug, Clone)]
pub struct WatchContext {
    pub watch_root: PathBuf,
    pub project_root: PathBuf,
    pub output: PathBuf,
    pub marker_value: String,
}

pub fn resolve_watch_context(
    watch_path: &Path,
    invocation_cwd: Option<&Path>,
    repo_root_fallback: Option<&Path>,
) -> anyhow::Result<WatchContext> {
    let project_root = if watch_path.is_absolute() {
        canonicalize_with_missing_tail(watch_path)?
    } else {
        let cwd = invocation_cwd
            .filter(|path| path.is_dir())
            .or_else(|| repo_root_fallback.filter(|path| path.is_dir()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "current working directory no longer exists and GRAPHOXIDE_REPO_ROOT is not set"
                )
            })?;
        canonicalize_with_missing_tail(cwd)?
    };
    let watch_root = if watch_path.is_absolute() {
        canonicalize_with_missing_tail(watch_path)?
    } else {
        canonicalize_with_missing_tail(&project_root.join(watch_path))?
    };
    Ok(WatchContext {
        output: watch_root.join(OUTPUT_DIRECTORY),
        watch_root,
        project_root,
        marker_value: watch_path.to_string_lossy().into_owned(),
    })
}

#[derive(Debug, Clone)]
pub struct WatchEventFilter {
    lexical_root: PathBuf,
    root: PathBuf,
    output_directory: Option<PathBuf>,
    patterns: Vec<detect::IgnorePattern>,
}

impl WatchEventFilter {
    pub fn new(root: &Path, honor_gitignore: bool) -> Self {
        Self::with_output_directory(root, honor_gitignore, None)
    }

    pub fn with_output_directory(
        root: &Path,
        honor_gitignore: bool,
        output_directory: Option<&Path>,
    ) -> Self {
        let lexical_root = root.to_path_buf();
        let root = fs::canonicalize(root).unwrap_or_else(|_| lexical_root.clone());
        let patterns = detect::load_ignore_patterns(&root, honor_gitignore);
        let output_directory = output_directory.map(|output| {
            canonicalize_with_missing_tail(output).unwrap_or_else(|_| output.to_path_buf())
        });
        Self {
            lexical_root,
            root,
            output_directory,
            patterns,
        }
    }

    pub fn accepts(&self, path: &Path, is_directory: bool) -> bool {
        let anchored = path
            .strip_prefix(&self.lexical_root)
            .map(|relative| self.root.join(relative))
            .unwrap_or_else(|_| path.to_path_buf());
        if self
            .output_directory
            .as_ref()
            .is_some_and(|output| output.starts_with(&self.root) && anchored.starts_with(output))
        {
            return false;
        }
        if is_directory || detect::is_ignored(&anchored, &self.root, &self.patterns) {
            return false;
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if !is_watched_extension(extension) {
            return false;
        }
        let relative = path
            .strip_prefix(&self.lexical_root)
            .or_else(|_| anchored.strip_prefix(&self.root))
            .unwrap_or(path);
        !relative.components().any(|component| {
            let value = component.as_os_str().to_string_lossy();
            value.starts_with('.')
                || value == OUTPUT_DIRECTORY
                || value == "graphify-out"
                || value == ".git"
        })
    }
}

pub fn is_remote_source(source: &str) -> bool {
    let Some(colon) = source.find(':') else {
        return false;
    };
    let scheme = &source[..colon];
    if scheme.len() < 2 || !source[colon + 1..].starts_with('/') {
        return false;
    }
    let mut chars = scheme.chars();
    chars.next().is_some_and(|ch| ch.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '.' | '-'))
}

fn slash(value: impl AsRef<str>) -> String {
    value.as_ref().replace('\\', "/")
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn canonicalize_with_missing_tail(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        lexical_normalize(path)
    } else {
        lexical_normalize(&std::env::current_dir()?.join(path))
    };
    let mut existing = absolute.clone();
    let mut tail = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|name| name.to_owned()) else {
            return Ok(absolute);
        };
        tail.push(name);
        if !existing.pop() {
            return Ok(absolute);
        }
    }
    let mut canonical = fs::canonicalize(existing)?;
    for component in tail.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

/// Reject an output directory that contains the watched source tree.
///
/// Such a configuration would either suppress every source event or allow
/// generated markers and graph artifacts to land above the project boundary.
pub fn validate_watch_output_directory(
    watch_root: &Path,
    output_directory: &Path,
) -> anyhow::Result<()> {
    let watch_root = canonicalize_with_missing_tail(watch_root)?;
    let output_directory = canonicalize_with_missing_tail(output_directory)?;
    anyhow::ensure!(
        !watch_root.starts_with(&output_directory),
        "managed output directory must not be the watched project root or one of its ancestors"
    );
    Ok(())
}

fn absolute_identity(path: &Path, root: &Path) -> Option<PathBuf> {
    if is_remote_source(&path.to_string_lossy()) {
        return None;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    canonicalize_with_missing_tail(&absolute).ok()
}

fn source_field_hyperedge(value: &Value) -> Option<&str> {
    value.get("source_file").and_then(Value::as_str)
}

fn hyperedge_members(value: &Value) -> Vec<&str> {
    ["nodes", "members", "node_ids"]
        .iter()
        .find_map(|field| value.get(field).and_then(Value::as_array))
        .map(|members| members.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
struct StoredSourcePaths {
    project_root: PathBuf,
    watch_root: PathBuf,
    existing_source_root: PathBuf,
    legacy_watch_relative: bool,
}

impl StoredSourcePaths {
    fn new(
        existing: Option<&KnowledgeGraph>,
        out: &Path,
        project_root: &Path,
        watch_root: &Path,
    ) -> Self {
        let mut existing_source_root = project_root.to_path_buf();
        let mut relative_prefix = None;
        for marker in [COMPAT_ROOT_MARKER, ROOT_MARKER] {
            let path = out.join(marker);
            let Ok(raw) = fs::read_to_string(path) else {
                continue;
            };
            let saved = PathBuf::from(raw.trim());
            if saved.is_absolute() {
                existing_source_root = canonicalize_with_missing_tail(&saved).unwrap_or(saved);
            } else if canonicalize_with_missing_tail(&project_root.join(&saved))
                .is_ok_and(|resolved| resolved == watch_root)
            {
                existing_source_root = project_root.to_path_buf();
                relative_prefix = Some(slash(saved.to_string_lossy()));
            }
            break;
        }
        let legacy_watch_relative = relative_prefix
            .as_deref()
            .filter(|prefix| *prefix != ".")
            .is_some_and(|prefix| {
                let Some(existing) = existing else {
                    return false;
                };
                !existing
                    .nodes
                    .iter()
                    .map(|node| node.source_file.as_str())
                    .chain(existing.links.iter().map(|edge| edge.source_file.as_str()))
                    .chain(
                        existing
                            .hyperedges
                            .iter()
                            .filter_map(source_field_hyperedge),
                    )
                    .filter(|source| !source.is_empty() && !Path::new(source).is_absolute())
                    .any(|source| {
                        let normalized = slash(source).trim_start_matches("./").to_owned();
                        normalized == prefix || normalized.starts_with(&format!("{prefix}/"))
                    })
            });
        Self {
            project_root: project_root.to_path_buf(),
            watch_root: watch_root.to_path_buf(),
            existing_source_root,
            legacy_watch_relative,
        }
    }

    fn identity(&self, source: &str) -> Option<PathBuf> {
        if source.is_empty() || is_remote_source(source) {
            return None;
        }
        let root = if self.legacy_watch_relative && !Path::new(source).is_absolute() {
            &self.watch_root
        } else {
            &self.existing_source_root
        };
        absolute_identity(Path::new(source), root)
    }

    fn stored(&self, identity: &Path) -> String {
        identity
            .strip_prefix(&self.project_root)
            .map(|relative| slash(relative.to_string_lossy()))
            .unwrap_or_else(|_| slash(identity.to_string_lossy()))
    }

    fn in_watch_root(&self, source: &str) -> bool {
        self.identity(source)
            .is_some_and(|identity| identity.starts_with(&self.watch_root))
    }
}

fn ast_location(value: Option<&str>) -> bool {
    value.is_some_and(|location| {
        let mut chars = location.chars();
        chars.next() == Some('L') && chars.next().is_some_and(|ch| ch.is_ascii_digit())
    })
}

fn is_ast_node(node: &Node) -> bool {
    node.extra
        .get("_origin")
        .and_then(Value::as_str)
        .and_then(origin_is_structural)
        .unwrap_or_else(|| ast_location(node.source_location.as_deref()))
}

fn is_ast_edge(edge: &Edge) -> bool {
    edge.extra
        .get("_origin")
        .and_then(Value::as_str)
        .and_then(origin_is_structural)
        .unwrap_or_else(|| ast_location(edge.extra.get("source_location").and_then(Value::as_str)))
}

fn set_hyperedge_source(value: &mut Value, source: String) {
    if let Some(object) = value.as_object_mut() {
        object.insert("source_file".into(), source.into());
    }
}

fn rewrite_extraction_source(
    extraction: &mut Extraction,
    lexical_path: &Path,
    project_root: &Path,
) {
    let identity = lexical_normalize(lexical_path);
    let stored = identity
        .strip_prefix(project_root)
        .map(|relative| slash(relative.to_string_lossy()))
        .unwrap_or_else(|_| slash(identity.to_string_lossy()));
    for node in &mut extraction.nodes {
        if !node.source_file.is_empty() {
            node.source_file = stored.clone();
        }
        node.extra.entry("_origin".into()).or_insert("ast".into());
    }
    for edge in &mut extraction.edges {
        if !edge.source_file.is_empty() {
            edge.source_file = stored.clone();
        }
        edge.extra.entry("_origin".into()).or_insert("ast".into());
    }
    for hyperedge in &mut extraction.hyperedges {
        if source_field_hyperedge(hyperedge).is_some() {
            set_hyperedge_source(hyperedge, stored.clone());
        }
    }
}

fn rebase_preserved_node(node: &mut Node, paths: &StoredSourcePaths) {
    if let Some(identity) = paths.identity(&node.source_file) {
        node.source_file = paths.stored(&identity);
    }
}

fn rebase_preserved_edge(edge: &mut Edge, paths: &StoredSourcePaths) {
    if let Some(identity) = paths.identity(&edge.source_file) {
        edge.source_file = paths.stored(&identity);
    }
}

fn rebase_preserved_hyperedge(hyperedge: &mut Value, paths: &StoredSourcePaths) {
    let Some(source) = source_field_hyperedge(hyperedge) else {
        return;
    };
    if let Some(identity) = paths.identity(source) {
        set_hyperedge_source(hyperedge, paths.stored(&identity));
    }
}

fn identity_in(source: &str, identities: &BTreeSet<PathBuf>, paths: &StoredSourcePaths) -> bool {
    paths
        .identity(source)
        .is_some_and(|identity| identities.contains(&identity))
}

#[derive(Debug, Clone, Default)]
pub struct ReconcileEvidence {
    pub full_rebuild: bool,
    pub current_sources: BTreeSet<PathBuf>,
    pub rebuilt_sources: BTreeSet<PathBuf>,
    pub deleted_sources: BTreeSet<PathBuf>,
}

fn reconcile_graph(
    existing: Option<&KnowledgeGraph>,
    mut fresh: Extraction,
    evidence: &ReconcileEvidence,
    paths: &StoredSourcePaths,
) -> Extraction {
    let Some(existing) = existing else {
        return fresh;
    };
    let fresh_ids = fresh
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    let mut preserved_nodes = existing
        .nodes
        .iter()
        .filter(|node| !fresh_ids.contains(&node.id))
        .filter(|node| {
            if identity_in(&node.source_file, &evidence.deleted_sources, paths) {
                return false;
            }
            let rebuilt = identity_in(&node.source_file, &evidence.rebuilt_sources, paths);
            if rebuilt {
                return evidence.full_rebuild && !is_ast_node(node);
            }
            !(is_ast_node(node)
                && (evidence.full_rebuild || evidence.current_sources.is_empty())
                && node.source_file.is_empty())
        })
        .cloned()
        .collect::<Vec<_>>();
    let all_ids = fresh_ids
        .iter()
        .cloned()
        .chain(preserved_nodes.iter().map(|node| node.id.clone()))
        .collect::<BTreeSet<_>>();
    let mut preserved_edges = existing
        .links
        .iter()
        .filter(|edge| all_ids.contains(edge.true_source()) && all_ids.contains(edge.true_target()))
        .filter(|edge| !identity_in(&edge.source_file, &evidence.deleted_sources, paths))
        .filter(|edge| {
            !(is_ast_edge(edge) && identity_in(&edge.source_file, &evidence.rebuilt_sources, paths))
        })
        .cloned()
        .collect::<Vec<_>>();
    let fresh_hyperedge_ids = fresh
        .hyperedges
        .iter()
        .filter_map(|value| value.get("id").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    let mut preserved_hyperedges = existing
        .hyperedges
        .iter()
        .filter(|value| {
            value
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(|id| !fresh_hyperedge_ids.contains(id))
        })
        .filter(|value| {
            source_field_hyperedge(value)
                .is_none_or(|source| !identity_in(source, &evidence.deleted_sources, paths))
        })
        .filter(|value| {
            hyperedge_members(value)
                .into_iter()
                .all(|member| all_ids.contains(member))
        })
        .cloned()
        .collect::<Vec<_>>();
    for node in &mut preserved_nodes {
        rebase_preserved_node(node, paths);
    }
    for edge in &mut preserved_edges {
        rebase_preserved_edge(edge, paths);
    }
    for hyperedge in &mut preserved_hyperedges {
        rebase_preserved_hyperedge(hyperedge, paths);
    }
    fresh.nodes.extend(preserved_nodes);
    fresh.edges.extend(preserved_edges);
    fresh.hyperedges.extend(preserved_hyperedges);
    fresh
}

fn normalized_source(value: &str) -> String {
    slash(value).trim_start_matches("./").to_owned()
}

fn sources_match(left: &str, right: &str, root: Option<&Path>) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if normalized_source(left) == normalized_source(right) {
        return true;
    }
    root.is_some_and(|root| {
        absolute_identity(Path::new(left), root) == absolute_identity(Path::new(right), root)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShrinkDecision {
    pub allowed: bool,
    pub warning: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn check_shrink(
    force: bool,
    existing: Option<&KnowledgeGraph>,
    candidate: &KnowledgeGraph,
    temporary: Option<&Path>,
    had_explicit_deletions: bool,
    rebuilt_sources: Option<&BTreeSet<String>>,
    root: Option<&Path>,
) -> ShrinkDecision {
    let Some(existing) = existing else {
        return ShrinkDecision {
            allowed: true,
            warning: None,
        };
    };
    if force || candidate.nodes.len() >= existing.nodes.len() {
        return ShrinkDecision {
            allowed: true,
            warning: None,
        };
    }
    if had_explicit_deletions && rebuilt_sources.is_none() {
        return ShrinkDecision {
            allowed: true,
            warning: None,
        };
    }
    if let Some(rebuilt) = rebuilt_sources {
        let candidate_ids = candidate
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<HashSet<_>>();
        let accounted = existing
            .nodes
            .iter()
            .filter(|node| !candidate_ids.contains(node.id.as_str()))
            .all(|node| {
                node.source_file.is_empty()
                    || rebuilt
                        .iter()
                        .any(|source| sources_match(&node.source_file, source, root))
            });
        if accounted {
            return ShrinkDecision {
                allowed: true,
                warning: None,
            };
        }
    }
    if let Some(temporary) = temporary {
        let _ = fs::remove_file(temporary);
    }
    ShrinkDecision {
        allowed: false,
        warning: Some(format!(
            "new graph has {} nodes but existing graph.json has {}; refusing to overwrite because the loss is not explained by rebuilt or deleted sources",
            candidate.nodes.len(),
            existing.nodes.len()
        )),
    }
}

fn sort_value_array(value: &mut Value, field: &str) {
    let Some(items) = value.get_mut(field).and_then(Value::as_array_mut) else {
        return;
    };
    items.sort_by_key(|item| serde_json::to_string(item).unwrap_or_default());
}

pub fn canonical_graph(graph: &KnowledgeGraph, topology_only: bool) -> Value {
    let mut value = serde_json::to_value(graph).expect("knowledge graph serializes");
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    object.remove("built_at_commit");
    if topology_only {
        if let Some(nodes) = object.get_mut("nodes").and_then(Value::as_array_mut) {
            for node in nodes.iter_mut().filter_map(Value::as_object_mut) {
                for key in ["community", "community_name", "norm_label"] {
                    node.remove(key);
                }
            }
        }
        for field in ["links", "edges"] {
            if let Some(edges) = object.get_mut(field).and_then(Value::as_array_mut) {
                for edge in edges.iter_mut().filter_map(Value::as_object_mut) {
                    if let (Some(source), Some(target)) = (edge.remove("_src"), edge.remove("_tgt"))
                    {
                        edge.insert("source".into(), source);
                        edge.insert("target".into(), target);
                    }
                    edge.remove("confidence_score");
                }
            }
        }
    }
    for field in ["nodes", "links", "edges", "hyperedges"] {
        sort_value_array(&mut value, field);
    }
    value
}

pub fn same_topology(left: &KnowledgeGraph, right: &KnowledgeGraph) -> bool {
    canonical_graph(left, true) == canonical_graph(right, true)
}

#[derive(Debug, Clone)]
pub struct CommitOptions {
    pub force: bool,
    pub had_explicit_deletions: bool,
    pub rebuilt_sources: Option<BTreeSet<String>>,
    pub source_root: Option<PathBuf>,
    pub marker_value: String,
}

pub fn commit_candidate(
    out: &Path,
    existing: Option<&KnowledgeGraph>,
    candidate: &KnowledgeGraph,
    options: &CommitOptions,
) -> anyhow::Result<bool> {
    let decision = check_shrink(
        options.force,
        existing,
        candidate,
        None,
        options.had_explicit_deletions,
        options.rebuilt_sources.as_ref(),
        options.source_root.as_deref(),
    );
    if !decision.allowed {
        return Ok(false);
    }
    fs::create_dir_all(out)?;
    graphoxide_core::write_graph_atomic(out.join("graph.json"), candidate, true)?;
    for marker in [ROOT_MARKER, COMPAT_ROOT_MARKER] {
        graphoxide_core::write_text_atomic(out.join(marker), &options.marker_value)?;
    }
    Ok(true)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebuildScope {
    #[default]
    Full,
    Incremental,
}

#[derive(Debug, Clone)]
pub struct RebuildOptions {
    pub changed_paths: Option<Vec<PathBuf>>,
    pub scope: RebuildScope,
    /// Override the managed output directory while keeping source identities
    /// anchored to `watch_path`.
    pub output_directory: Option<PathBuf>,
    pub follow_symlinks: bool,
    pub force: bool,
    pub no_cluster: bool,
    pub acquire_lock: bool,
    pub block_on_lock: bool,
    pub invocation_cwd: Option<PathBuf>,
    pub repo_root_fallback: Option<PathBuf>,
    pub max_graph_bytes: Option<u64>,
    pub viz_node_limit: Option<usize>,
}

impl Default for RebuildOptions {
    fn default() -> Self {
        Self {
            changed_paths: None,
            scope: RebuildScope::Full,
            output_directory: None,
            follow_symlinks: false,
            force: false,
            no_cluster: false,
            acquire_lock: true,
            block_on_lock: false,
            invocation_cwd: std::env::current_dir().ok(),
            repo_root_fallback: std::env::var_os("GRAPHOXIDE_REPO_ROOT")
                .or_else(|| std::env::var_os("GRAPHIFY_REPO_ROOT"))
                .map(PathBuf::from),
            max_graph_bytes: None,
            viz_node_limit: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebuildStatus {
    Rebuilt,
    Unchanged,
    NoTrackedChanges,
    Queued,
    RefusedShrink,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildStats {
    pub detected_files: usize,
    pub processed_files: usize,
    pub changed_files: usize,
    pub unchanged_files: usize,
    pub deleted_files: usize,
    pub nodes: usize,
    pub edges: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildTimings {
    pub detect_ms: u64,
    pub extract_ms: u64,
    pub build_ms: u64,
    pub cluster_ms: u64,
    pub write_ms: u64,
    pub total_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RebuildFileSets {
    current: BTreeSet<PathBuf>,
    processed: BTreeSet<PathBuf>,
    changed: BTreeSet<PathBuf>,
    deleted: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildResult {
    pub status: RebuildStatus,
    pub scope: RebuildScope,
    pub graph_path: PathBuf,
    pub manifest_path: PathBuf,
    pub passes: usize,
    pub clustered: bool,
    pub warnings: Vec<String>,
    pub stats: RebuildStats,
    pub timings: RebuildTimings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RebuildPass {
    result: RebuildResult,
    file_sets: RebuildFileSets,
}

impl RebuildResult {
    pub fn succeeded(&self) -> bool {
        matches!(
            self.status,
            RebuildStatus::Rebuilt | RebuildStatus::Unchanged | RebuildStatus::NoTrackedChanges
        )
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn requested_rebuild_scope(options: &RebuildOptions) -> RebuildScope {
    if options.changed_paths.is_some() {
        RebuildScope::Incremental
    } else {
        options.scope
    }
}

fn merge_rebuild_results(mut aggregate: RebuildPass, next: RebuildPass) -> RebuildPass {
    aggregate.result.status = match (aggregate.result.status, next.result.status) {
        (RebuildStatus::RefusedShrink, _) | (_, RebuildStatus::RefusedShrink) => {
            RebuildStatus::RefusedShrink
        }
        (RebuildStatus::Rebuilt, _) | (_, RebuildStatus::Rebuilt) => RebuildStatus::Rebuilt,
        (RebuildStatus::Unchanged, _) | (_, RebuildStatus::Unchanged) => RebuildStatus::Unchanged,
        (RebuildStatus::NoTrackedChanges, _) | (_, RebuildStatus::NoTrackedChanges) => {
            RebuildStatus::NoTrackedChanges
        }
        _ => RebuildStatus::Queued,
    };
    if next.result.scope == RebuildScope::Full {
        aggregate.result.scope = RebuildScope::Full;
    }
    aggregate.result.graph_path = next.result.graph_path;
    aggregate.result.manifest_path = next.result.manifest_path;
    aggregate.result.passes = next.result.passes;
    aggregate.result.clustered |= next.result.clustered;
    for warning in next.result.warnings {
        if !aggregate.result.warnings.contains(&warning) {
            aggregate.result.warnings.push(warning);
        }
    }
    aggregate.file_sets.current = next.file_sets.current;
    aggregate
        .file_sets
        .processed
        .extend(next.file_sets.processed);
    aggregate.file_sets.changed.extend(next.file_sets.changed);
    aggregate.file_sets.deleted.extend(next.file_sets.deleted);
    aggregate.result.stats.detected_files = next.result.stats.detected_files;
    aggregate.result.stats.processed_files = aggregate.file_sets.processed.len();
    aggregate.result.stats.changed_files = aggregate.file_sets.changed.len();
    let changed_in_final_corpus = aggregate
        .file_sets
        .changed
        .intersection(&aggregate.file_sets.current)
        .count();
    aggregate.result.stats.unchanged_files = aggregate
        .result
        .stats
        .detected_files
        .saturating_sub(changed_in_final_corpus.min(aggregate.result.stats.detected_files));
    aggregate.result.stats.deleted_files = aggregate.file_sets.deleted.len();
    aggregate.result.stats.nodes = next.result.stats.nodes;
    aggregate.result.stats.edges = next.result.stats.edges;
    aggregate.result.timings.detect_ms = aggregate
        .result
        .timings
        .detect_ms
        .saturating_add(next.result.timings.detect_ms);
    aggregate.result.timings.extract_ms = aggregate
        .result
        .timings
        .extract_ms
        .saturating_add(next.result.timings.extract_ms);
    aggregate.result.timings.build_ms = aggregate
        .result
        .timings
        .build_ms
        .saturating_add(next.result.timings.build_ms);
    aggregate.result.timings.cluster_ms = aggregate
        .result
        .timings
        .cluster_ms
        .saturating_add(next.result.timings.cluster_ms);
    aggregate.result.timings.write_ms = aggregate
        .result
        .timings
        .write_ms
        .saturating_add(next.result.timings.write_ms);
    aggregate.result.timings.total_ms = aggregate
        .result
        .timings
        .total_ms
        .saturating_add(next.result.timings.total_ms);
    aggregate
}

pub fn rebuild_project(
    watch_path: &Path,
    options: &RebuildOptions,
) -> anyhow::Result<RebuildResult> {
    rebuild_project_with_observer(watch_path, options, |_, _| {})
}

pub fn rebuild_project_with_observer<F>(
    watch_path: &Path,
    options: &RebuildOptions,
    mut after_pass: F,
) -> anyhow::Result<RebuildResult>
where
    F: FnMut(usize, &Path),
{
    let total_started = Instant::now();
    let mut context = resolve_watch_context(
        watch_path,
        options.invocation_cwd.as_deref(),
        options.repo_root_fallback.as_deref(),
    )?;
    if let Some(output_directory) = options.output_directory.as_deref() {
        context.output = if output_directory.is_absolute() {
            canonicalize_with_missing_tail(output_directory)?
        } else {
            canonicalize_with_missing_tail(&context.project_root.join(output_directory))?
        };
    }
    validate_watch_output_directory(&context.watch_root, &context.output)?;
    if !options.acquire_lock {
        let mut result = rebuild_once(&context, options, options.changed_paths.as_deref(), 1)?;
        result.result.timings.total_ms = elapsed_millis(total_started);
        return Ok(result.result);
    }
    if let Some(changed) = options.changed_paths.as_deref() {
        if !options.block_on_lock {
            queue_pending(&context.output, changed)?;
        }
    }
    let Some(_guard) = RebuildLockGuard::acquire(&context.output, options.block_on_lock)? else {
        return Ok(RebuildResult {
            status: RebuildStatus::Queued,
            scope: requested_rebuild_scope(options),
            graph_path: context.output.join("graph.json"),
            manifest_path: context.output.join("manifest.json"),
            passes: 0,
            clustered: false,
            warnings: Vec::new(),
            stats: RebuildStats::default(),
            timings: RebuildTimings {
                total_ms: elapsed_millis(total_started),
                ..RebuildTimings::default()
            },
        });
    };
    let merged = if let Some(changed) = options.changed_paths.as_deref() {
        let queued = drain_pending(&context.output)?;
        Some(merge_changed_paths(&[Some(changed), Some(&queued)]))
    } else {
        let _ = drain_pending(&context.output)?;
        None
    };
    let mut result = rebuild_once(&context, options, merged.as_deref(), 1)?;
    after_pass(1, &context.output);
    if merged.is_some() {
        for pass in 2..=PENDING_DRAIN_MAX_PASSES + 1 {
            let late = drain_pending(&context.output)?;
            if late.is_empty() {
                break;
            }
            let next = rebuild_once(&context, options, Some(&late), pass)?;
            result = merge_rebuild_results(result, next);
            after_pass(pass, &context.output);
        }
    }
    result.result.timings.total_ms = elapsed_millis(total_started);
    Ok(result.result)
}

fn ast_document(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "md" | "markdown" | "mdx" | "qmd"
    )
}

fn detected_ast_files_in(files_by_type: &detect::DetectedFiles) -> Vec<PathBuf> {
    let mut files = files_by_type
        .get(FileType::Code.as_str())
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    files.extend(
        files_by_type
            .get(FileType::Document.as_str())
            .into_iter()
            .flatten()
            .map(PathBuf::from)
            .filter(|path| ast_document(path)),
    );
    files.sort();
    files.dedup();
    files
}

fn detected_ast_files(detection: &DetectResult) -> Vec<PathBuf> {
    detected_ast_files_in(&detection.files)
}

fn usable_incremental_manifest(path: &Path) -> bool {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, Value>>(&bytes).ok())
        .is_some()
}

#[derive(Debug, Default)]
struct IncrementalSelection {
    changed: Vec<PathBuf>,
    unchanged: usize,
    deleted: Vec<PathBuf>,
}

fn path_equivalent(left: &Path, right: &Path) -> bool {
    lexical_normalize(left) == lexical_normalize(right)
        || canonicalize_with_missing_tail(left).ok() == canonicalize_with_missing_tail(right).ok()
}

fn changed_path_candidates(raw: &Path, project_root: &Path, watch_root: &Path) -> Vec<PathBuf> {
    if raw.is_absolute() {
        let lexical = lexical_normalize(raw);
        let resolved = canonicalize_with_missing_tail(raw).unwrap_or_else(|_| lexical.clone());
        return if lexical == resolved {
            vec![lexical]
        } else {
            vec![lexical, resolved]
        };
    }
    let mut values = Vec::new();
    for base in [project_root, watch_root] {
        let lexical = lexical_normalize(&base.join(raw));
        for candidate in [
            lexical.clone(),
            canonicalize_with_missing_tail(&lexical).unwrap_or(lexical),
        ] {
            if !values.contains(&candidate) {
                values.push(candidate);
            }
        }
    }
    values
}

fn semantic_doc_sources(
    existing: Option<&KnowledgeGraph>,
    ast_files: &[PathBuf],
    paths: &StoredSourcePaths,
) -> BTreeSet<PathBuf> {
    let ast_docs = ast_files
        .iter()
        .filter(|path| ast_document(path))
        .filter_map(|path| absolute_identity(path, &paths.project_root))
        .collect::<BTreeSet<_>>();
    existing
        .into_iter()
        .flat_map(|graph| &graph.nodes)
        .filter(|node| !is_ast_node(node))
        .filter(|node| {
            matches!(
                node.file_type.as_str(),
                "document" | "concept" | "rationale" | "paper" | "code"
            )
        })
        .filter_map(|node| paths.identity(&node.source_file))
        .filter(|identity| ast_docs.contains(identity))
        .collect()
}

fn load_existing(path: &Path, cap: Option<u64>) -> anyhow::Result<Option<KnowledgeGraph>> {
    if !path.exists() {
        return Ok(None);
    }
    cap.map_or_else(
        || graphoxide_core::read_graph(path),
        |cap| graphoxide_core::read_graph_with_cap(path, cap),
    )
    .map(Some)
}

fn flatten(chunks: Vec<Extraction>) -> Extraction {
    let mut output = Extraction::default();
    for chunk in chunks {
        output.nodes.extend(chunk.nodes);
        output.edges.extend(chunk.edges);
        output.hyperedges.extend(chunk.hyperedges);
    }
    output
}

fn full_scan_manifest(detection: &DetectResult, context: &WatchContext) -> anyhow::Result<()> {
    let scan_corpus = detection
        .files
        .values()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>();
    detect::save_manifest(
        &detection.files,
        &context.output.join("manifest.json"),
        &SaveManifestOptions {
            kind: ManifestKind::Ast,
            root: Some(context.watch_root.clone()),
            scan_corpus: Some(scan_corpus),
            clear_semantic: BTreeSet::new(),
        },
    )
}

fn git_head(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn community_signatures(graph: &KnowledgeGraph) -> BTreeMap<String, String> {
    graphoxide_graph::communities(graph)
        .into_iter()
        .map(|(community, members)| {
            let mut digest = Sha256::new();
            for member in members {
                digest.update(member.as_bytes());
                digest.update(b"\0");
            }
            (community.to_string(), format!("{:x}", digest.finalize()))
        })
        .collect()
}

fn read_string_map(path: &Path) -> BTreeMap<String, String> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn generated_labels(graph: &KnowledgeGraph) -> BTreeMap<i64, String> {
    graphoxide_export::community_labels_from_graph(graph)
}

fn apply_stable_labels(
    graph: &mut KnowledgeGraph,
    out: &Path,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let signatures = community_signatures(graph);
    let saved_labels = [".graphoxide_labels.json", ".graphify_labels.json"]
        .iter()
        .map(|name| read_string_map(&out.join(name)))
        .find(|labels| !labels.is_empty())
        .unwrap_or_default();
    let saved_signatures = [".graphoxide_labels.json.sig", ".graphify_labels.json.sig"]
        .iter()
        .map(|name| read_string_map(&out.join(name)))
        .find(|labels| !labels.is_empty())
        .unwrap_or_default();
    let generated = generated_labels(graph);
    let labels = generated
        .into_iter()
        .map(|(community, label)| {
            let key = community.to_string();
            let label = if saved_signatures.get(&key) == signatures.get(&key) {
                saved_labels.get(&key).cloned().unwrap_or(label)
            } else {
                label
            };
            (key, label)
        })
        .collect::<BTreeMap<_, _>>();
    for node in &mut graph.nodes {
        if let Some(label) = node
            .community
            .and_then(|community| labels.get(&community.to_string()))
        {
            node.extra
                .insert("community_name".into(), label.clone().into());
        }
    }
    (labels, signatures)
}

fn write_cluster_outputs(
    out: &Path,
    graph: &KnowledgeGraph,
    labels: &BTreeMap<String, String>,
    signatures: &BTreeMap<String, String>,
    viz_node_limit: Option<usize>,
) -> anyhow::Result<()> {
    for name in [".graphoxide_labels.json", ".graphify_labels.json"] {
        graphoxide_core::write_json_atomic(out.join(name), labels, true)?;
    }
    for name in [".graphoxide_labels.json.sig", ".graphify_labels.json.sig"] {
        graphoxide_core::write_json_atomic(out.join(name), signatures, false)?;
    }
    let report = graphoxide_export::render_report(graph, &graphoxide_graph::analyze(graph)?);
    graphoxide_core::write_text_atomic(out.join("GRAPH_REPORT.md"), &report)?;
    write_visualization(out, graph, viz_node_limit)
}

fn configured_viz_limit(explicit: Option<usize>) -> usize {
    explicit.unwrap_or_else(|| {
        std::env::var("GRAPHOXIDE_VIZ_NODE_LIMIT")
            .ok()
            .or_else(|| std::env::var("GRAPHIFY_VIZ_NODE_LIMIT").ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(2_500)
    })
}

fn aggregate_graph(graph: &KnowledgeGraph) -> KnowledgeGraph {
    let communities = graphoxide_graph::communities(graph);
    let labels = generated_labels(graph);
    let node_community = graph
        .nodes
        .iter()
        .filter_map(|node| Some((node.id.clone(), node.community?)))
        .collect::<BTreeMap<_, _>>();
    let nodes = communities
        .iter()
        .map(|(community, members)| Node {
            id: format!("community_{community}"),
            label: labels
                .get(community)
                .cloned()
                .unwrap_or_else(|| format!("Community {community}")),
            file_type: "concept".into(),
            source_file: String::new(),
            source_location: None,
            community: Some(*community),
            extra: BTreeMap::from([("member_count".into(), members.len().into())]),
        })
        .collect::<Vec<_>>();
    let mut pairs = BTreeSet::new();
    for edge in &graph.links {
        let (Some(source), Some(target)) = (
            node_community.get(edge.true_source()),
            node_community.get(edge.true_target()),
        ) else {
            continue;
        };
        if source != target {
            pairs.insert((*source, *target));
        }
    }
    let links = pairs
        .into_iter()
        .map(|(source, target)| Edge {
            source: format!("community_{source}"),
            target: format!("community_{target}"),
            relation: "connects".into(),
            confidence: graphoxide_core::Confidence::Extracted,
            source_file: String::new(),
            extra: BTreeMap::new(),
        })
        .collect();
    KnowledgeGraph {
        directed: graph.directed,
        nodes,
        links,
        ..Default::default()
    }
}

fn write_visualization(
    out: &Path,
    graph: &KnowledgeGraph,
    explicit_limit: Option<usize>,
) -> anyhow::Result<()> {
    let target = out.join("graph.html");
    let limit = configured_viz_limit(explicit_limit);
    if limit == 0 {
        match fs::remove_file(target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        return Ok(());
    }
    let render = if graph.nodes.len() <= limit {
        Some(graph.clone())
    } else {
        let aggregate = aggregate_graph(graph);
        (aggregate.nodes.len() > 1 && aggregate.nodes.len() <= limit).then_some(aggregate)
    };
    if let Some(render) = render {
        graphoxide_core::write_text_atomic(target, &graphoxide_export::render_html(&render)?)?;
    } else {
        let _ = fs::remove_file(target);
    }
    Ok(())
}

fn clear_needs_update(out: &Path) -> anyhow::Result<()> {
    match fs::remove_file(out.join(NEEDS_UPDATE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn finish_rebuild_result(
    mut result: RebuildResult,
    file_sets: RebuildFileSets,
    total_started: Instant,
) -> RebuildPass {
    result.timings.total_ms = elapsed_millis(total_started);
    RebuildPass { result, file_sets }
}

fn rebuild_once(
    context: &WatchContext,
    options: &RebuildOptions,
    changed_paths: Option<&[PathBuf]>,
    pass: usize,
) -> anyhow::Result<RebuildPass> {
    let total_started = Instant::now();
    let graph_path = context.output.join("graph.json");
    let manifest_path = context.output.join("manifest.json");
    let existing = load_existing(&graph_path, options.max_graph_bytes)?;
    let derive_incremental_paths = changed_paths.is_none()
        && options.scope == RebuildScope::Incremental
        && existing.is_some()
        && usable_incremental_manifest(&manifest_path);
    let use_explicit_changed_paths = changed_paths.is_some() && existing.is_some();
    let scope = if use_explicit_changed_paths || derive_incremental_paths {
        RebuildScope::Incremental
    } else {
        RebuildScope::Full
    };
    let mut stats = RebuildStats::default();
    let mut timings = RebuildTimings::default();
    let config = read_build_config(&context.output);
    let detect_options = DetectOptions {
        follow_symlinks: options.follow_symlinks,
        extra_excludes: config.excludes.clone(),
        honor_gitignore: config.honor_gitignore,
        output_dir: Some(context.output.clone()),
        ..Default::default()
    };
    let detect_started = Instant::now();
    let mut incremental_selection = None;
    let detection = if derive_incremental_paths {
        let incremental = detect::detect_incremental(
            &context.watch_root,
            &manifest_path,
            &detect_options,
            ManifestKind::Ast,
        )?;
        incremental_selection = Some(IncrementalSelection {
            changed: detected_ast_files_in(&incremental.new_files),
            unchanged: detected_ast_files_in(&incremental.unchanged_files).len(),
            deleted: incremental
                .deleted_files
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        });
        incremental.detection
    } else {
        detect::detect(&context.watch_root, &detect_options)?
    };
    timings.detect_ms = elapsed_millis(detect_started);
    if !detection.walk_errors.is_empty() {
        let preview = detection
            .walk_errors
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let remainder = detection.walk_errors.len().saturating_sub(5);
        let suffix = if remainder == 0 {
            String::new()
        } else {
            format!("; and {remainder} more")
        };
        anyhow::bail!(
            "refusing to rebuild from an incomplete filesystem scan ({} walk error(s)): {preview}{suffix}",
            detection.walk_errors.len()
        );
    }
    let ast_files = detected_ast_files(&detection);
    stats.detected_files = ast_files.len();
    if let Some(selection) = &incremental_selection {
        stats.changed_files = selection.changed.len();
        stats.unchanged_files = selection.unchanged;
        stats.deleted_files = selection.deleted.len();
    }
    if ast_files.is_empty() && existing.is_none() {
        return Ok(finish_rebuild_result(
            RebuildResult {
                status: RebuildStatus::NoTrackedChanges,
                scope,
                graph_path,
                manifest_path,
                passes: pass,
                clustered: false,
                warnings: Vec::new(),
                stats,
                timings,
            },
            RebuildFileSets::default(),
            total_started,
        ));
    }
    let source_paths = StoredSourcePaths::new(
        existing.as_ref(),
        &context.output,
        &context.project_root,
        &context.watch_root,
    );
    let semantic_docs = semantic_doc_sources(existing.as_ref(), &ast_files, &source_paths);
    let current_sources = ast_files
        .iter()
        .filter_map(|path| absolute_identity(path, &context.project_root))
        .collect::<BTreeSet<_>>();
    let mut deleted_sources = BTreeSet::new();
    let mut targets: Vec<PathBuf> = Vec::new();
    let mut automatic_paths = Vec::new();
    if let Some(selection) = &incremental_selection {
        automatic_paths.extend(selection.changed.iter().cloned());
        automatic_paths.extend(selection.deleted.iter().cloned());
    }
    let effective_changed_paths = if use_explicit_changed_paths {
        changed_paths
    } else {
        incremental_selection
            .as_ref()
            .map(|_| automatic_paths.as_slice())
    };
    if let Some(changed) = effective_changed_paths {
        for raw in changed {
            let candidates =
                changed_path_candidates(raw, &context.project_root, &context.watch_root);
            let tracked = ast_files.iter().find(|file| {
                candidates
                    .iter()
                    .any(|candidate| path_equivalent(candidate, file))
            });
            if let Some(file) = tracked {
                let identity = absolute_identity(file, &context.project_root);
                if identity
                    .as_ref()
                    .is_none_or(|identity| !semantic_docs.contains(identity))
                    && !targets.iter().any(|target| path_equivalent(target, file))
                {
                    targets.push((*file).clone());
                }
                continue;
            }
            if candidates
                .iter()
                .any(|candidate| candidate.exists() && candidate.starts_with(&context.watch_root))
            {
                continue;
            }
            if let Some(deleted) = candidates
                .into_iter()
                .find(|candidate| candidate.starts_with(&context.watch_root))
            {
                if let Some(identity) = absolute_identity(&deleted, &context.project_root) {
                    deleted_sources.insert(identity);
                }
            }
        }
        if incremental_selection.is_none() {
            stats.changed_files = targets.len();
            stats.unchanged_files = ast_files.len().saturating_sub(targets.len());
            stats.deleted_files = deleted_sources.len();
        }
        if targets.is_empty() && deleted_sources.is_empty() && incremental_selection.is_none() {
            if let Some(existing) = &existing {
                stats.nodes = existing.nodes.len();
                stats.edges = existing.links.len();
            }
            return Ok(finish_rebuild_result(
                RebuildResult {
                    status: RebuildStatus::NoTrackedChanges,
                    scope,
                    graph_path,
                    manifest_path,
                    passes: pass,
                    clustered: false,
                    warnings: Vec::new(),
                    stats,
                    timings,
                },
                RebuildFileSets {
                    current: current_sources.clone(),
                    ..RebuildFileSets::default()
                },
                total_started,
            ));
        }
    } else {
        targets = ast_files
            .iter()
            .filter(|file| {
                absolute_identity(file, &context.project_root)
                    .is_none_or(|identity| !semantic_docs.contains(&identity))
            })
            .cloned()
            .collect();
    }
    let mut excluded_alive = BTreeSet::new();
    if let Some(existing) = &existing {
        for node in &existing.nodes {
            if node.source_file.is_empty()
                || is_remote_source(&node.source_file)
                || !source_paths.in_watch_root(&node.source_file)
            {
                continue;
            }
            let Some(identity) = source_paths.identity(&node.source_file) else {
                continue;
            };
            if !current_sources.contains(&identity) {
                if identity.exists() {
                    excluded_alive.insert(identity);
                } else {
                    deleted_sources.insert(identity);
                }
            }
        }
    }
    let warnings = if excluded_alive.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "fail-closed: kept graph records from {} file(s) that left the scan corpus but still exist on disk",
            excluded_alive.len()
        )]
    };
    let rebuilt_sources = targets
        .iter()
        .filter_map(|path| absolute_identity(path, &context.project_root))
        .collect::<BTreeSet<_>>();
    let changed_sources = incremental_selection.as_ref().map_or_else(
        || {
            if scope == RebuildScope::Incremental && changed_paths.is_some() {
                rebuilt_sources.clone()
            } else {
                BTreeSet::new()
            }
        },
        |selection| {
            selection
                .changed
                .iter()
                .filter_map(|path| absolute_identity(path, &context.project_root))
                .collect()
        },
    );
    let file_sets = RebuildFileSets {
        current: current_sources.clone(),
        processed: rebuilt_sources.clone(),
        changed: changed_sources,
        deleted: deleted_sources.clone(),
    };
    stats.processed_files = file_sets.processed.len();
    stats.changed_files = file_sets.changed.len();
    stats.deleted_files = file_sets.deleted.len();
    if scope == RebuildScope::Incremental {
        let changed_in_current = file_sets.changed.intersection(&file_sets.current).count();
        stats.unchanged_files = stats
            .detected_files
            .saturating_sub(changed_in_current.min(stats.detected_files));
    }
    if incremental_selection.is_some() && targets.is_empty() && deleted_sources.is_empty() {
        if let Some(existing) = &existing {
            stats.nodes = existing.nodes.len();
            stats.edges = existing.links.len();
        }
        let write_started = Instant::now();
        full_scan_manifest(&detection, context)?;
        clear_needs_update(&context.output)?;
        timings.write_ms = elapsed_millis(write_started);
        return Ok(finish_rebuild_result(
            RebuildResult {
                status: RebuildStatus::Unchanged,
                scope,
                graph_path,
                manifest_path,
                passes: pass,
                clustered: false,
                warnings,
                stats,
                timings,
            },
            file_sets,
            total_started,
        ));
    }
    let extract_started = Instant::now();
    let mut chunks = if targets.is_empty() {
        Vec::new()
    } else {
        graphoxide_extract::extract_files_deferred_manifest(
            &targets,
            Some(&context.watch_root),
            true,
        )?
        .discard_manifest()
        .extractions
    };
    for (chunk, target) in chunks.iter_mut().zip(&targets) {
        rewrite_extraction_source(chunk, target, &context.project_root);
    }
    timings.extract_ms = elapsed_millis(extract_started);
    let build_started = Instant::now();
    let fresh = flatten(chunks);
    let merged = reconcile_graph(
        existing.as_ref(),
        fresh,
        &ReconcileEvidence {
            full_rebuild: scope == RebuildScope::Full,
            current_sources,
            rebuilt_sources: rebuilt_sources.clone(),
            deleted_sources: deleted_sources.clone(),
        },
        &source_paths,
    );
    let inherited_directed = existing.as_ref().is_some_and(|graph| graph.directed);
    let mut candidate = graphoxide_graph::build_graph_with_options_and_root(
        &[merged],
        &context.project_root,
        BuildOptions {
            directed: inherited_directed,
            deduplicate_semantic_nodes: true,
            collapse_undirected_reverse_edges: false,
        },
    )?;
    if let Some(commit) = git_head(&context.watch_root) {
        candidate
            .extra
            .insert("built_at_commit".into(), commit.into());
    }
    timings.build_ms = elapsed_millis(build_started);
    stats.nodes = candidate.nodes.len();
    stats.edges = candidate.links.len();
    if existing
        .as_ref()
        .is_some_and(|existing| same_topology(existing, &candidate))
    {
        let write_started = Instant::now();
        full_scan_manifest(&detection, context)?;
        clear_needs_update(&context.output)?;
        timings.write_ms = elapsed_millis(write_started);
        return Ok(finish_rebuild_result(
            RebuildResult {
                status: RebuildStatus::Unchanged,
                scope,
                graph_path,
                manifest_path,
                passes: pass,
                clustered: false,
                warnings,
                stats,
                timings,
            },
            file_sets,
            total_started,
        ));
    }
    let mut labels = BTreeMap::new();
    let mut signatures = BTreeMap::new();
    if !options.no_cluster {
        let cluster_started = Instant::now();
        graphoxide_graph::cluster(&mut candidate)?;
        if let Some(existing) = &existing {
            graphoxide_graph::remap_communities_to_previous(&mut candidate, existing);
        }
        (labels, signatures) = apply_stable_labels(&mut candidate, &context.output);
        timings.cluster_ms = elapsed_millis(cluster_started);
    }
    let rebuilt_for_guard = rebuilt_sources
        .iter()
        .chain(&deleted_sources)
        .map(|identity| source_paths.stored(identity))
        .collect::<BTreeSet<_>>();
    let write_started = Instant::now();
    let committed = commit_candidate(
        &context.output,
        existing.as_ref(),
        &candidate,
        &CommitOptions {
            force: options.force,
            had_explicit_deletions: !deleted_sources.is_empty(),
            rebuilt_sources: Some(rebuilt_for_guard),
            source_root: Some(context.project_root.clone()),
            marker_value: context.marker_value.clone(),
        },
    )?;
    if !committed {
        timings.write_ms = elapsed_millis(write_started);
        return Ok(finish_rebuild_result(
            RebuildResult {
                status: RebuildStatus::RefusedShrink,
                scope,
                graph_path,
                manifest_path,
                passes: pass,
                clustered: !options.no_cluster,
                warnings,
                stats,
                timings,
            },
            file_sets,
            total_started,
        ));
    }
    // The graph is now accepted. Publish the full-corpus manifest before
    // derived reports so a report/rendering failure cannot leave a new graph
    // paired with the pre-build manifest.
    full_scan_manifest(&detection, context)?;
    if !options.no_cluster {
        write_cluster_outputs(
            &context.output,
            &candidate,
            &labels,
            &signatures,
            options.viz_node_limit,
        )?;
    }
    clear_needs_update(&context.output)?;
    let mut persisted = read_build_config(&context.output);
    persisted.cluster = !options.no_cluster;
    for name in [BUILD_CONFIG, COMPAT_BUILD_CONFIG] {
        graphoxide_core::write_json_atomic(context.output.join(name), &persisted, false)?;
    }
    timings.write_ms = elapsed_millis(write_started);
    Ok(finish_rebuild_result(
        RebuildResult {
            status: RebuildStatus::Rebuilt,
            scope,
            graph_path,
            manifest_path,
            passes: pass,
            clustered: !options.no_cluster,
            warnings,
            stats,
            timings,
        },
        file_sets,
        total_started,
    ))
}
