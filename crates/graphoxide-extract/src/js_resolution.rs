//! Project-aware JavaScript and TypeScript module and export resolution.
//!
//! The tree-sitter walker intentionally remains a per-file extractor. This
//! pass supplies the project context needed by ECMAScript module resolution:
//! extension/index probing, tsconfig paths, workspace packages, and recursive
//! barrel export resolution. All identities are minted from repo-relative
//! source paths so checkout prefixes cannot leak into a graph.

use crate::project_path::{normalize_project_path, source_relative_project_path, ProjectPath};
use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

const JS_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "mts", "cts", "svelte", "vue", "astro", "js", "jsx", "mjs", "cjs",
];
const JS_INDEX_FILES: &[&str] = &[
    "index.ts",
    "index.tsx",
    "index.svelte",
    "index.vue",
    "index.astro",
    "index.js",
    "index.jsx",
    "index.mjs",
];
const EXPORT_CONDITIONS: &[&str] = &[
    "source", "import", "module", "svelte", "types", "require", "default",
];
const MAX_TSCONFIG_EXTENDS_DEPTH: usize = 64;
const JS_RESOLVER_FIXED_SCRATCH_BYTES: usize = 512 * 1024;
const MAX_EXPORT_RESOLUTION_DEPTH: usize = 128;
const MAX_EXPORT_RESOLUTION_STEPS: usize = 1024;
const ID_NORMALIZATION_PREFLIGHT_FACTOR: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EsModuleSpecifier {
    ProjectRelative(String),
    Bare,
    Unsafe,
}

/// Classify an already-decoded static ECMAScript module specifier without
/// consulting the host filesystem. Project-relative specifiers carry their
/// normalized logical target; bare packages and aliases remain eligible for
/// explicit tsconfig/workspace resolution; unsafe spellings are non-bindable.
pub(crate) fn classify_es_module_specifier(
    source_file: &str,
    specifier: &str,
) -> EsModuleSpecifier {
    if specifier.is_empty()
        || specifier.trim() != specifier
        || specifier.contains("${")
        || specifier
            .chars()
            .any(|character| character.is_control() || character == '\\')
    {
        return EsModuleSpecifier::Unsafe;
    }
    let bytes = specifier.as_bytes();
    if specifier.starts_with('/') || bytes.get(1) == Some(&b':') {
        return EsModuleSpecifier::Unsafe;
    }
    if specifier.starts_with('.') {
        if !(specifier.starts_with("./") || specifier.starts_with("../")) {
            return EsModuleSpecifier::Unsafe;
        }
        if let Some(ProjectPath::Contained(logical)) =
            source_relative_project_path(source_file, specifier)
        {
            return EsModuleSpecifier::ProjectRelative(logical);
        }
        let source_path = Path::new(source_file);
        if source_path.is_absolute()
            && let Some(parent) = source_path.parent()
        {
            // The single-file compatibility API supplies a trusted absolute
            // physical source instead of a project-relative identity. Give
            // the shared parser an equivalent synthetic lexical depth so it
            // still owns reference validation and filesystem-root underflow.
            let depth = parent
                .components()
                .filter(|component| matches!(component, Component::Normal(_)))
                .count();
            let validation_source = format!("{}source", "directory/".repeat(depth));
            if matches!(
                source_relative_project_path(&validation_source, specifier),
                Some(ProjectPath::Contained(_))
            ) {
                let joined = lexical_normalize(&parent.join(specifier));
                if joined.is_absolute()
                    && !joined
                        .components()
                        .any(|component| matches!(component, Component::ParentDir))
                {
                    return EsModuleSpecifier::ProjectRelative(normalize_slashes(
                        joined.to_string_lossy().as_ref(),
                    ));
                }
            }
        }
        return EsModuleSpecifier::Unsafe;
    }
    EsModuleSpecifier::Bare
}

/// Bounded, I/O-preloaded project material used by the isolated resolver.
///
/// The snapshot deliberately keys files by their normalized project-relative
/// spelling. It does not retain a root path or an I/O capability, so resolver
/// code using this type cannot reopen sources, probe directories, or follow
/// symlinks. The control/I/O plane transfers the already-admitted allocations
/// into this map after byte extraction has completed.
#[derive(Debug)]
pub(crate) struct ProjectSnapshot {
    files: BTreeMap<String, Vec<u8>>,
    retained_bytes: usize,
    byte_limit: usize,
}

impl ProjectSnapshot {
    fn map_slot_bytes() -> usize {
        use std::mem::size_of;

        size_of::<String>()
            .saturating_add(size_of::<Vec<u8>>())
            .saturating_add(size_of::<usize>())
    }

    fn root_retained_bytes() -> usize {
        11usize.saturating_mul(Self::map_slot_bytes())
    }

    fn entry_retained_bytes(source_file: &str, source_capacity: usize) -> usize {
        source_capacity
            .saturating_add(source_file.len())
            .saturating_add(3usize.saturating_mul(Self::map_slot_bytes()))
    }

    pub(crate) fn admission_bytes(source_file: &str, source_capacity: usize) -> usize {
        Self::entry_retained_bytes(source_file, source_capacity)
    }

    /// Allocate an empty snapshot with an explicit post-extraction byte limit.
    #[must_use]
    pub(crate) fn with_byte_limit(byte_limit: usize) -> Self {
        Self {
            files: BTreeMap::new(),
            retained_bytes: 0,
            byte_limit,
        }
    }

    /// Return whether an admitted file contributes source or metadata needed
    /// for JavaScript-family resolution. Keeping all JSON permits arbitrary
    /// relative `tsconfig` `extends` chains without a resolver-side read.
    #[must_use]
    pub(crate) fn needs_file(path: &str) -> bool {
        let path = Path::new(path);
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        if extension
            .as_deref()
            .is_some_and(|extension| JS_EXTENSIONS.contains(&extension))
        {
            return true;
        }
        if extension.as_deref() == Some("json") {
            return true;
        }
        matches!(
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("pnpm-workspace.yaml" | "pnpm-workspace.yml")
        )
    }

    /// Return whether already-admitted bytes belong in the JavaScript project
    /// snapshot. MPEG transport streams share `.ts` with TypeScript, but must
    /// never be retained as resolver source text.
    #[must_use]
    pub(crate) fn needs_admitted_file(path: &str, source: &[u8]) -> bool {
        Self::needs_file(path)
            && !crate::detect::is_mpeg_transport_stream_bytes(Path::new(path), source)
    }

    /// Move a source allocation read by an I/O owner into this snapshot.
    ///
    /// An over-budget snapshot is an explicit resource error rather than a
    /// partial project view that could silently change import resolution.
    pub(crate) fn insert_owned(
        &mut self,
        source_file: String,
        bytes: Vec<u8>,
    ) -> Result<(), ProjectSnapshotError> {
        let Some(source_file) =
            normalize_snapshot_path(&source_file).filter(|source_file| !source_file.is_empty())
        else {
            return Err(ProjectSnapshotError::InvalidPath(source_file));
        };
        let previous =
            self.files
                .get_key_value(&source_file)
                .map_or(0, |(stored_source_file, bytes)| {
                    Self::entry_retained_bytes(stored_source_file, bytes.capacity())
                });
        let retained = self
            .retained_bytes
            .saturating_sub(previous)
            .saturating_add(Self::entry_retained_bytes(&source_file, bytes.capacity()))
            .saturating_add(
                (previous == 0 && self.files.is_empty())
                    .then(Self::root_retained_bytes)
                    .unwrap_or(0),
            );
        if retained > self.byte_limit {
            return Err(ProjectSnapshotError::ExceedsBudget {
                byte_limit: self.byte_limit,
            });
        }
        self.retained_bytes = retained;
        self.files.insert(source_file, bytes);
        Ok(())
    }

    fn bytes(&self, source_file: &str) -> Option<&[u8]> {
        self.files.get(source_file).map(Vec::as_slice)
    }

    fn contains_file(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    fn contains_directory(&self, path: &str) -> bool {
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        self.files.keys().any(|key| key.starts_with(&prefix))
    }

    fn resolution_source(&self, source_file: &str) -> Option<String> {
        let bytes = self.bytes(source_file)?;
        let source = String::from_utf8_lossy(bytes);
        crate::sfc::resolution_source_bytes(Path::new(source_file), &source)
            .or_else(|| Some(source.into_owned()))
    }

    #[must_use]
    pub(crate) const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    fn max_path_len(&self) -> usize {
        // Exact project-relative imports may intentionally target admitted
        // JSON or other resolver metadata, so every snapshot path is a
        // possible cached target even though only JS-family files own modules.
        self.files.keys().map(String::len).max().unwrap_or(0)
    }

    fn decoded_source_len(&self, source_file: &str) -> Option<usize> {
        lossy_utf8_len(self.bytes(source_file)?)
    }

    fn metadata_parse_peak_bytes(&self) -> anyhow::Result<usize> {
        use std::mem::size_of;

        // Recursive tsconfig `extends` resolution may retain every ancestor
        // Value at once. Charge the sum, not only the largest file. Per input
        // byte, this covers one Value, one possible owned key, and B-tree/Vec
        // bookkeeping; copied string payload is covered by the input byte.
        let bytes_per_metadata_byte = size_of::<Value>()
            .saturating_add(size_of::<String>())
            .saturating_add(6usize.saturating_mul(size_of::<usize>()))
            .saturating_add(1);
        let total = self
            .files
            .iter()
            .filter(|(path, _)| is_resolver_metadata(path))
            .try_fold(0usize, |total, (_, bytes)| {
                // Parser growth follows initialized input length. Spare pooled
                // capacity is already charged once by `retained_bytes`.
                total.checked_add(bytes.len())
            })
            .ok_or_else(|| anyhow::anyhow!("JavaScript resolver metadata size exceeds usize"))?;
        total
            .checked_mul(bytes_per_metadata_byte)
            .ok_or_else(|| anyhow::anyhow!("JavaScript resolver metadata charge exceeds usize"))
    }

    fn resolve_module_specifier(&self, specifier: &str, importer: &str) -> Option<String> {
        let importer_dir = logical_parent(importer)?;
        match classify_es_module_specifier(importer, specifier) {
            EsModuleSpecifier::ProjectRelative(logical) => self.resolve_js_path(&logical),
            EsModuleSpecifier::Bare => self
                .resolve_tsconfig(specifier, &importer_dir)
                .or_else(|| self.resolve_workspace(specifier, &importer_dir)),
            EsModuleSpecifier::Unsafe => None,
        }
    }

    fn resolve_js_path(&self, candidate: &str) -> Option<String> {
        let candidate = normalize_snapshot_path(candidate)?;
        if self.contains_file(&candidate) {
            return Some(candidate);
        }
        match Path::new(&candidate)
            .extension()
            .and_then(|value| value.to_str())
        {
            Some("js") => {
                let value = replace_extension(&candidate, "ts");
                if self.contains_file(&value) {
                    return Some(value);
                }
            }
            Some("jsx") => {
                let value = replace_extension(&candidate, "tsx");
                if self.contains_file(&value) {
                    return Some(value);
                }
            }
            _ => {}
        }
        let name = Path::new(&candidate).file_name()?.to_string_lossy();
        let parent = logical_parent(&candidate).unwrap_or_default();
        for extension in JS_EXTENSIONS {
            let value = logical_join(&parent, &format!("{name}.{extension}"));
            if self.contains_file(&value) {
                return Some(value);
            }
        }
        if self.contains_directory(&candidate) {
            for index in JS_INDEX_FILES {
                let value = logical_join(&candidate, index);
                if self.contains_file(&value) {
                    return Some(value);
                }
            }
        }
        None
    }

    fn resolve_tsconfig(&self, specifier: &str, start: &str) -> Option<String> {
        let config = self.find_config(start)?;
        let parsed = self.read_tsconfig(&config, &mut BTreeSet::new(), 0);
        let mut best: Option<SnapshotMatchedAlias<'_>> = None;
        for (pattern, targets) in &parsed.aliases {
            let Some((score, captured, wildcard)) = match_alias(specifier, pattern) else {
                continue;
            };
            if best.as_ref().is_none_or(|current| score < current.0) {
                best = Some((score, captured, wildcard, targets));
            }
        }
        if let Some((_, captured, wildcard, targets)) = best {
            for target in targets {
                let candidate = if wildcard && !captured.is_empty() {
                    target.replacen('*', &captured, 1)
                } else if captured.is_empty() {
                    target.clone()
                } else {
                    logical_join(target, &captured)
                };
                if let Some(resolved) = self.resolve_js_path(&candidate) {
                    return Some(resolved);
                }
            }
            return None;
        }
        parsed
            .base_url
            .and_then(|base| self.resolve_js_path(&logical_join(&base, specifier)))
    }

    fn find_config(&self, start: &str) -> Option<String> {
        for directory in logical_ancestors(start) {
            for name in ["tsconfig.json", "jsconfig.json"] {
                let candidate = logical_join(&directory, name);
                if self.contains_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn read_tsconfig(
        &self,
        path: &str,
        seen: &mut BTreeSet<String>,
        depth: usize,
    ) -> SnapshotTsConfig {
        if depth >= MAX_TSCONFIG_EXTENDS_DEPTH {
            return SnapshotTsConfig::default();
        }
        let Some(path) = normalize_snapshot_path(path) else {
            return SnapshotTsConfig::default();
        };
        if !seen.insert(path.clone()) {
            return SnapshotTsConfig::default();
        }
        let Some(data) = self.read_jsonc(&path) else {
            return SnapshotTsConfig::default();
        };
        let base = logical_parent(&path).unwrap_or_default();
        let mut result = SnapshotTsConfig::default();
        let parents = match data.get("extends") {
            Some(Value::String(value)) => vec![value.as_str()],
            Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for parent in parents {
            if parent.starts_with('@') {
                continue;
            }
            let mut extended = logical_join(&base, parent);
            if Path::new(&extended).extension().is_none() {
                extended.push_str(".json");
            }
            if self.contains_file(&extended) {
                let inherited = self.read_tsconfig(&extended, seen, depth + 1);
                result.aliases.extend(inherited.aliases);
                if inherited.base_url.is_some() {
                    result.base_url = inherited.base_url;
                }
            }
        }
        let options = data.get("compilerOptions").and_then(Value::as_object);
        let local_base = options
            .and_then(|options| options.get("baseUrl"))
            .and_then(Value::as_str)
            .and_then(|value| normalize_snapshot_path(&logical_join(&base, value)));
        if local_base.is_some() {
            result.base_url = local_base.clone();
        }
        let paths_base = local_base.unwrap_or(base);
        if let Some(paths) = options
            .and_then(|options| options.get("paths"))
            .and_then(Value::as_object)
        {
            for (alias, targets) in paths {
                let values = targets
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .filter_map(|value| normalize_snapshot_pattern(&paths_base, value))
                    .collect::<Vec<_>>();
                if !values.is_empty() {
                    result.aliases.insert(alias.clone(), values);
                }
            }
        }
        result
    }

    fn resolve_workspace(&self, specifier: &str, start: &str) -> Option<String> {
        let root = self.find_workspace_root(start)?;
        let patterns = self.workspace_patterns(&root);
        let package_directories = self
            .files
            .keys()
            .filter_map(|path| path.strip_suffix("/package.json"))
            .chain(self.files.contains_key("package.json").then_some(""))
            .filter(|directory| workspace_pattern_matches(&root, directory, &patterns));
        for package in package_directories {
            let package_json = logical_join(package, "package.json");
            let Some(data) = self.read_jsonc(&package_json) else {
                continue;
            };
            let Some(name) = data.get("name").and_then(Value::as_str) else {
                continue;
            };
            let subpath = if specifier == name {
                ""
            } else if let Some(value) = specifier.strip_prefix(&format!("{name}/")) {
                value
            } else {
                continue;
            };
            for candidate in snapshot_package_entry_candidates(package, &data, subpath) {
                if let Some(resolved) = self.resolve_js_path(&candidate) {
                    return Some(resolved);
                }
            }
        }
        None
    }

    fn find_workspace_root(&self, start: &str) -> Option<String> {
        for directory in logical_ancestors(start) {
            let pnpm = logical_join(&directory, "pnpm-workspace.yaml");
            if self.contains_file(&pnpm) {
                return Some(directory);
            }
            let pnpm_yml = logical_join(&directory, "pnpm-workspace.yml");
            if self.contains_file(&pnpm_yml) {
                return Some(directory);
            }
            let package = logical_join(&directory, "package.json");
            if self
                .read_jsonc(&package)
                .is_some_and(|data| data.get("workspaces").is_some())
            {
                return Some(directory);
            }
        }
        None
    }

    fn workspace_patterns(&self, root: &str) -> Vec<String> {
        let pnpm = logical_join(root, "pnpm-workspace.yaml");
        let pnpm_yml = logical_join(root, "pnpm-workspace.yml");
        self.bytes(&pnpm)
            .or_else(|| self.bytes(&pnpm_yml))
            .map(|bytes| parse_pnpm_patterns(&String::from_utf8_lossy(bytes)))
            .unwrap_or_else(|| {
                self.read_jsonc(&logical_join(root, "package.json"))
                    .map_or_else(Vec::new, |data| match data.get("workspaces") {
                        Some(Value::Array(values)) => values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                        Some(Value::Object(value)) => value
                            .get("packages")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                        _ => Vec::new(),
                    })
            })
    }

    fn read_jsonc(&self, path: &str) -> Option<Value> {
        let bytes = self.bytes(path)?;
        graphoxide_core::parse_jsonc(&String::from_utf8_lossy(bytes)).ok()
    }
}

/// Atomic admission guard used by fixed compute workers before retaining an
/// I/O-owned source allocation for resolution. It prevents a parallel scan
/// from briefly retaining more than its reserved resolver budget.
#[derive(Debug)]
pub(crate) struct ProjectSnapshotAdmission {
    byte_limit: usize,
    retained_bytes: AtomicUsize,
}

impl ProjectSnapshotAdmission {
    #[must_use]
    pub(crate) const fn new(byte_limit: usize) -> Self {
        Self {
            byte_limit,
            retained_bytes: AtomicUsize::new(0),
        }
    }

    pub(crate) fn try_reserve(&self, bytes: usize) -> bool {
        self.retained_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |retained| {
                retained
                    .checked_add(
                        (retained == 0)
                            .then(ProjectSnapshot::root_retained_bytes)
                            .unwrap_or(0),
                    )
                    .and_then(|retained| retained.checked_add(bytes))
                    .filter(|next| *next <= self.byte_limit)
            })
            .is_ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectSnapshotError {
    ExceedsBudget { byte_limit: usize },
    InvalidPath(String),
}

#[derive(Default)]
struct SnapshotTsConfig {
    aliases: BTreeMap<String, Vec<String>>,
    base_url: Option<String>,
}

type SnapshotMatchedAlias<'a> = ((u8, usize), String, bool, &'a [String]);

fn normalize_snapshot_path(value: &str) -> Option<String> {
    normalize_project_path(value)
}

fn invalid_snapshot_path() -> String {
    // A NUL cannot occur in a valid filesystem path, and input admission
    // rejects it before an I/O request is constructed. It is therefore a safe
    // non-member sentinel for malformed/escaping logical candidates.
    "\0invalid-project-snapshot-path".into()
}

fn logical_join(base: &str, value: &str) -> String {
    normalize_snapshot_path(&logical_join_spelling(base, value))
        .unwrap_or_else(invalid_snapshot_path)
}

fn logical_join_spelling(base: &str, value: &str) -> String {
    match (base.is_empty(), value.is_empty()) {
        (true, _) => value.to_owned(),
        (_, true) => base.to_owned(),
        (false, false) => format!("{base}/{value}"),
    }
}

fn normalize_snapshot_pattern(base: &str, value: &str) -> Option<String> {
    match value.matches('*').count() {
        0 => return normalize_snapshot_path(&logical_join_spelling(base, value)),
        1 => {}
        _ => return None,
    }

    // TypeScript path targets permit one wildcard. Replace it with a unique,
    // portable private-use character while the shared path parser validates
    // and normalizes every real segment. Resolution substitutes the captured
    // value before the final candidate is admitted by `resolve_js_path`.
    let joined = logical_join_spelling(base, value);
    let used = joined.chars().collect::<BTreeSet<_>>();
    let placeholder = ('\u{e000}'..='\u{f8ff}').find(|value| !used.contains(value))?;
    let candidate = joined.replacen('*', &placeholder.to_string(), 1);
    normalize_snapshot_path(&candidate).map(|path| path.replace(placeholder, "*"))
}

fn logical_parent(path: &str) -> Option<String> {
    let path = normalize_snapshot_path(path)?;
    let parent = Path::new(&path).parent().unwrap_or_else(|| Path::new(""));
    normalize_snapshot_path(parent.to_string_lossy().as_ref())
}

fn logical_ancestors(start: &str) -> impl Iterator<Item = String> {
    let first = normalize_snapshot_path(start).unwrap_or_else(invalid_snapshot_path);
    std::iter::successors(Some(first), |current| {
        if current.is_empty() {
            return None;
        }
        let parent = logical_parent(current)?;
        (parent != *current).then_some(parent)
    })
}

fn replace_extension(path: &str, extension: &str) -> String {
    normalize_snapshot_path(
        Path::new(path)
            .with_extension(extension)
            .to_string_lossy()
            .as_ref(),
    )
    .unwrap_or_else(invalid_snapshot_path)
}

fn workspace_pattern_matches(root: &str, directory: &str, patterns: &[String]) -> bool {
    let relative = if root.is_empty() {
        directory
    } else {
        let prefix = format!("{root}/");
        let Some(relative) = directory.strip_prefix(&prefix) else {
            return false;
        };
        relative
    };
    patterns
        .iter()
        .filter(|pattern| !pattern.starts_with('!'))
        .any(|pattern| {
            let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
            if matches!(pattern, "." | "") {
                return relative.is_empty();
            }
            let Some(star) = pattern.find('*') else {
                return relative == pattern;
            };
            if pattern.matches('*').count() != 1 {
                return false;
            }
            let prefix = pattern[..star].trim_end_matches('/');
            let suffix = pattern[star + 1..].trim_start_matches('/');
            let Some(captured) = relative.strip_prefix(prefix) else {
                return false;
            };
            let captured = captured.trim_start_matches('/');
            if captured.is_empty() {
                return false;
            }
            let captured = if suffix.is_empty() {
                captured
            } else {
                let Some(captured) = captured.strip_suffix(suffix) else {
                    return false;
                };
                captured.trim_end_matches('/')
            };
            !captured.is_empty() && !captured.contains('/')
        })
}

fn snapshot_package_entry_candidates(package: &str, data: &Value, subpath: &str) -> Vec<String> {
    if !subpath.is_empty() {
        if let Some(exports) = data.get("exports").and_then(Value::as_object) {
            let key = format!("./{subpath}");
            if let Some(target) = exports.get(&key).and_then(resolve_export_target) {
                let candidate = logical_join(package, &target);
                if snapshot_path_contained(&candidate, package) {
                    return vec![candidate];
                }
            }
            for (pattern, value) in exports {
                let Some((prefix, suffix)) = pattern.split_once('*') else {
                    continue;
                };
                if pattern.matches('*').count() != 1
                    || !key.starts_with(prefix)
                    || !key.ends_with(suffix)
                {
                    continue;
                }
                let end = key.len().saturating_sub(suffix.len());
                let captured = &key[prefix.len()..end];
                let Some(target) = resolve_export_target(value) else {
                    continue;
                };
                let candidate = logical_join(package, &target.replacen('*', captured, 1));
                if snapshot_path_contained(&candidate, package) {
                    return vec![candidate];
                }
            }
        }
        return vec![logical_join(package, subpath)];
    }
    if let Some(exports) = data.get("exports") {
        if let Some(target) = exports.as_str() {
            return vec![logical_join(package, target)];
        }
        if let Some(target) = exports
            .as_object()
            .and_then(|values| values.get("."))
            .and_then(resolve_export_target)
        {
            return vec![logical_join(package, &target)];
        }
    }
    let mut candidates = ["svelte", "module", "main", "types"]
        .iter()
        .filter_map(|key| data.get(key).and_then(Value::as_str))
        .map(|value| logical_join(package, value))
        .collect::<Vec<_>>();
    candidates.push(logical_join(package, "src/index"));
    candidates.push(logical_join(package, "index"));
    candidates
}

fn snapshot_path_contained(candidate: &str, package: &str) -> bool {
    package.is_empty() || candidate == package || candidate.starts_with(&format!("{package}/"))
}

#[derive(Debug, Clone)]
struct ImportFact {
    specifier: Arc<str>,
    resolved: bool,
    imported: Option<String>,
    local: Option<String>,
    line: usize,
}

#[derive(Debug, Clone)]
enum ExportBinding {
    Local(String),
    Reexport { source: Arc<str>, imported: String },
    Namespace(Arc<str>),
}

#[derive(Debug, Clone)]
struct ReexportFact {
    specifier: Arc<str>,
    resolved: bool,
    imported: Option<String>,
    exported: Option<String>,
    namespace: bool,
    star: bool,
    line: usize,
}

#[derive(Debug)]
struct ModuleFacts {
    extraction: usize,
    source_file: String,
    source: String,
    file_id: String,
    stem: String,
    definitions: BTreeMap<String, String>,
    aliases: BTreeMap<String, String>,
    imports: Vec<ImportFact>,
    reexports: Vec<ReexportFact>,
    dynamic_imports: Vec<(usize, String)>,
    exports: BTreeMap<String, Vec<ExportBinding>>,
    stars: Vec<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExportResolution {
    Resolved(String),
    Ambiguous,
    Missing,
}

struct ExportResolutionState {
    visiting: BTreeSet<(Arc<str>, Arc<str>)>,
    remaining_steps: usize,
}

impl ExportResolutionState {
    fn new() -> Self {
        Self {
            visiting: BTreeSet::new(),
            remaining_steps: MAX_EXPORT_RESOLUTION_STEPS,
        }
    }

    fn consume_step(&mut self) -> bool {
        let Some(remaining) = self.remaining_steps.checked_sub(1) else {
            return false;
        };
        self.remaining_steps = remaining;
        true
    }
}

/// Rebuild JavaScript-family import edges with project context before the
/// language-neutral resolver consumes them.
pub(crate) fn resolve(extractions: &mut [Extraction], root: &Path) {
    let root = fs::canonicalize(root).unwrap_or_else(|_| lexical_normalize(root));
    let mut sources = BTreeMap::new();
    for extraction in extractions.iter() {
        let Some(file_node) = extraction.nodes.iter().find(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("file")
                && is_javascript_source(&node.source_file)
        }) else {
            continue;
        };
        let source_file = normalize_slashes(&file_node.source_file);
        let physical = root.join(&source_file);
        let Ok(text) = fs::read_to_string(&physical) else {
            continue;
        };
        let parse_source = crate::sfc::resolution_source(&physical, &text).unwrap_or(text);
        sources.insert(source_file, parse_source);
    }
    resolve_from_sources(extractions, sources, |specifier, source_file| {
        resolve_module_specifier(specifier, &root.join(source_file), &root)
    });
}

/// Worst-case per-source-byte allowance for all simultaneously retained JS
/// module indexes. Even compact comma-separated imports cannot create more
/// facts than input bytes; charging one entry from every parser collection per
/// byte deliberately overcounts their shared syntax while bounding fixed
/// struct/vector/B-tree overhead. Module specifiers shared by multiple named
/// bindings use `Arc<str>`, keeping retained string payload linear in source.
fn module_source_working_set_bytes(decoded_bytes: usize) -> anyhow::Result<usize> {
    use std::mem::size_of;

    let bytes_per_source_byte = size_of::<ImportFact>()
        .saturating_add(size_of::<ReexportFact>())
        .saturating_add(size_of::<ExportBinding>())
        .saturating_add(4usize.saturating_mul(size_of::<(String, String)>()));
    decoded_bytes
        .checked_mul(bytes_per_source_byte.saturating_add(16))
        .ok_or_else(|| anyhow::anyhow!("JavaScript resolver source charge exceeds usize"))
}

/// Conservative heap storage for freshly built std B-trees on the pinned
/// toolchain. Nodes reserve multiple key/value slots even when only one entry
/// is live; three slots per live entry plus one root node's eleven slots also
/// covers partially occupied internal/leaf nodes without relying on payload
/// allocations for the unused slots.
fn btree_reserved_slots(len: usize) -> usize {
    if len == 0 {
        0
    } else {
        len.saturating_mul(3).saturating_add(11)
    }
}

fn btree_map_storage_bytes<K, V>(len: usize) -> usize {
    use std::mem::size_of;

    btree_reserved_slots(len).saturating_mul(
        size_of::<K>()
            .saturating_add(size_of::<V>())
            .saturating_add(size_of::<usize>()),
    )
}

fn btree_set_storage_bytes<T>(len: usize) -> usize {
    use std::mem::size_of;

    btree_reserved_slots(len).saturating_mul(size_of::<T>().saturating_add(size_of::<usize>()))
}

/// Conservative retained charge for one fully indexed module, plus the
/// resolution and barrel-binding growth that can occur before it is dropped.
/// Shared specifier payload is counted once by allocation identity.
fn module_facts_admission_components(
    facts: &ModuleFacts,
    max_snapshot_path_len: usize,
) -> (usize, usize) {
    use std::mem::size_of;

    fn charge_arc(
        bytes: &mut usize,
        allocations: &mut BTreeSet<(usize, usize)>,
        specifier: &Arc<str>,
        allocation_overhead: usize,
    ) {
        let identity = (
            Arc::as_ptr(specifier) as *const () as usize,
            specifier.len(),
        );
        if allocations.insert(identity) {
            *bytes = bytes
                .saturating_add(specifier.len())
                .saturating_add(allocation_overhead);
        }
    }

    let arc_allocation_overhead = 2usize
        .saturating_mul(size_of::<AtomicUsize>())
        .saturating_add(size_of::<usize>());
    let mut bytes = size_of::<ModuleFacts>()
        .saturating_add(facts.source_file.capacity())
        .saturating_add(facts.source.capacity())
        .saturating_add(facts.file_id.capacity())
        .saturating_add(facts.stem.capacity());
    for map in [&facts.definitions, &facts.aliases] {
        bytes = bytes.saturating_add(btree_map_storage_bytes::<String, String>(map.len()));
        for (key, value) in map {
            bytes = bytes
                .saturating_add(key.capacity())
                .saturating_add(value.capacity());
        }
    }

    let mut arc_allocations = BTreeSet::<(usize, usize)>::new();
    bytes = bytes
        .saturating_add(
            facts
                .imports
                .capacity()
                .saturating_mul(size_of::<ImportFact>()),
        )
        .saturating_add(
            facts
                .reexports
                .capacity()
                .saturating_mul(size_of::<ReexportFact>()),
        )
        .saturating_add(
            facts
                .dynamic_imports
                .capacity()
                .saturating_mul(size_of::<(usize, String)>()),
        );
    for import in &facts.imports {
        charge_arc(
            &mut bytes,
            &mut arc_allocations,
            &import.specifier,
            arc_allocation_overhead,
        );
        bytes = bytes
            .saturating_add(import.imported.as_ref().map_or(0, String::capacity))
            .saturating_add(import.local.as_ref().map_or(0, String::capacity));
    }
    for reexport in &facts.reexports {
        charge_arc(
            &mut bytes,
            &mut arc_allocations,
            &reexport.specifier,
            arc_allocation_overhead,
        );
        bytes = bytes
            .saturating_add(reexport.imported.as_ref().map_or(0, String::capacity))
            .saturating_add(reexport.exported.as_ref().map_or(0, String::capacity));
    }
    for (_, specifier) in &facts.dynamic_imports {
        bytes = bytes.saturating_add(specifier.capacity());
    }
    let projected_export_entries = facts.exports.len().saturating_add(
        facts
            .reexports
            .iter()
            .filter(|reexport| !reexport.star)
            .count(),
    );
    bytes = bytes.saturating_add(btree_map_storage_bytes::<String, Vec<ExportBinding>>(
        projected_export_entries,
    ));
    for (name, bindings) in &facts.exports {
        bytes = bytes.saturating_add(name.capacity()).saturating_add(
            bindings
                .capacity()
                .saturating_mul(size_of::<ExportBinding>()),
        );
        for binding in bindings {
            match binding {
                ExportBinding::Local(local) => {
                    bytes = bytes.saturating_add(local.capacity());
                }
                ExportBinding::Reexport { source, imported } => {
                    charge_arc(
                        &mut bytes,
                        &mut arc_allocations,
                        source,
                        arc_allocation_overhead,
                    );
                    bytes = bytes.saturating_add(imported.capacity());
                }
                ExportBinding::Namespace(source) => charge_arc(
                    &mut bytes,
                    &mut arc_allocations,
                    source,
                    arc_allocation_overhead,
                ),
            }
        }
    }
    bytes = bytes.saturating_add(facts.stars.capacity().saturating_mul(size_of::<Arc<str>>()));
    for star in &facts.stars {
        charge_arc(
            &mut bytes,
            &mut arc_allocations,
            star,
            arc_allocation_overhead,
        );
    }

    // Resolution retains at most one target allocation/cache entry per unique
    // raw specifier. Materializing re-exports can add one binding and cloned
    // local name per fact, while the target Arc remains shared.
    let unique_specifiers = arc_allocations.len();
    bytes = bytes
        .saturating_add(btree_map_storage_bytes::<Arc<str>, Option<Arc<str>>>(
            unique_specifiers,
        ))
        .saturating_add(
            unique_specifiers
                .saturating_mul(max_snapshot_path_len.saturating_add(arc_allocation_overhead)),
        );
    bytes = bytes.saturating_add(
        facts
            .reexports
            .capacity()
            .saturating_mul(size_of::<ReexportFact>()),
    );
    for reexport in &facts.reexports {
        if reexport.star {
            // A first push commonly allocates four Arc slots; later geometric
            // growth has a smaller per-fact peak.
            bytes = bytes.saturating_add(4usize.saturating_mul(size_of::<Arc<str>>()));
        } else {
            // Charge a worst-case unique exports-map entry and its initial
            // four-slot binding Vec. This also covers the old+new buffers at
            // geometric growth when multiple bindings share one export name.
            bytes = bytes
                .saturating_add(4usize.saturating_mul(size_of::<ExportBinding>()))
                .saturating_add(reexport.imported.as_ref().map_or(0, String::capacity))
                .saturating_add(reexport.exported.as_ref().map_or(0, String::capacity));
        }
    }
    let static_fact_count = facts.imports.len().saturating_add(facts.reexports.len());
    bytes = bytes
        .saturating_add(btree_set_storage_bytes::<String>(static_fact_count))
        .saturating_add(static_fact_count.saturating_mul(32))
        .saturating_add(btree_set_storage_bytes::<String>(
            static_fact_count.saturating_mul(2),
        ));
    for (_, specifier) in &facts.dynamic_imports {
        let normalized_id_bytes = raw_dynamic_import_id(&facts.source_file, specifier).capacity();
        bytes = bytes.saturating_add(32).saturating_add(normalized_id_bytes);
    }
    bytes = bytes
        .saturating_add(btree_map_storage_bytes::<String, BTreeSet<String>>(
            facts.dynamic_imports.len(),
        ))
        .saturating_add(
            facts
                .dynamic_imports
                .len()
                .saturating_mul(btree_set_storage_bytes::<String>(1)),
        );
    for specifier in facts
        .imports
        .iter()
        .map(|fact| fact.specifier.as_ref())
        .chain(facts.reexports.iter().map(|fact| fact.specifier.as_ref()))
    {
        let normalized_id_bytes = raw_static_import_id(&facts.source_file, specifier)
            .capacity()
            .saturating_add(raw_dynamic_import_id(&facts.source_file, specifier).capacity());
        bytes = bytes.saturating_add(normalized_id_bytes);
    }
    let key_copy = facts.source_file.len();
    (bytes, key_copy)
}

fn module_facts_admission_bytes(facts: &ModuleFacts, max_snapshot_path_len: usize) -> usize {
    let (base, key_copy) = module_facts_admission_components(facts, max_snapshot_path_len);
    // `modules` owns a map key and `resolve_modules` clones every key into its
    // membership set while the map remains live.
    base.saturating_add(key_copy.saturating_mul(2))
}

#[derive(Clone, Copy, Default)]
struct ExportResolutionStringBounds {
    stored: usize,
    namespace_candidate_capacity: usize,
}

impl ExportResolutionStringBounds {
    fn include(&mut self, other: Self) {
        self.stored = self.stored.max(other.stored);
        self.namespace_candidate_capacity = self
            .namespace_candidate_capacity
            .max(other.namespace_candidate_capacity);
    }

    fn include_stored(&mut self, value: &str) {
        self.stored = self.stored.max(value.len());
    }

    fn candidate_len(self) -> usize {
        self.stored.max(self.namespace_candidate_capacity)
    }
}

/// Heap peak for any ID derived from module paths, specifiers, or symbols.
///
/// The pinned Unicode tables expand one scalar to at most 18 compatibility
/// decomposition scalars; lowercasing and the subsequent full case fold each
/// expand to at most three scalars. Even composing those independent maxima,
/// encoding every output scalar as four UTF-8 bytes, and doubling both the
/// intermediate and final buffers for geometric String capacity stays below
/// the 4096x byte factor with room for the path/stem and joined inputs, the
/// temporary slice Vec, and one sibling derived-ID output retained while the
/// next ID is normalized. No normalization occurs while calculating this
/// bound.
fn derived_id_normalization_scratch_bytes(
    facts: &ModuleFacts,
    max_snapshot_path_len: usize,
) -> anyhow::Result<usize> {
    use std::mem::size_of;

    let has_derived_id = !facts.imports.is_empty()
        || !facts.reexports.is_empty()
        || !facts.dynamic_imports.is_empty();
    if !has_derived_id {
        return Ok(0);
    }
    let max_specifier = facts
        .imports
        .iter()
        .map(|fact| fact.specifier.len())
        .chain(facts.reexports.iter().map(|fact| fact.specifier.len()))
        .chain(
            facts
                .dynamic_imports
                .iter()
                .map(|(_, specifier)| specifier.len()),
        )
        .max()
        .unwrap_or(0);
    let max_symbol = facts
        .imports
        .iter()
        .filter_map(|fact| fact.imported.as_ref())
        .map(String::len)
        .chain(
            facts
                .reexports
                .iter()
                .flat_map(|fact| [fact.imported.as_ref(), fact.exported.as_ref()])
                .flatten()
                .map(String::len),
        )
        .chain(facts.exports.keys().map(String::len))
        .max()
        .unwrap_or(0);
    let max_input = facts
        .source_file
        .len()
        .max(max_snapshot_path_len)
        .checked_add(max_specifier.max(max_symbol))
        // Separators plus the longest fixed unresolved-ID prefix
        // (`ref_unsafe`) fit comfortably in sixteen bytes.
        .and_then(|bytes| bytes.checked_add(16))
        .ok_or_else(|| anyhow::anyhow!("JavaScript derived-ID charge exceeds usize"))?;
    max_input
        .checked_mul(ID_NORMALIZATION_PREFLIGHT_FACTOR)
        .and_then(|bytes| bytes.checked_add(4usize.saturating_mul(size_of::<&str>())))
        .ok_or_else(|| anyhow::anyhow!("JavaScript derived-ID charge exceeds usize"))
}

fn module_namespace_candidate_capacity(facts: &ModuleFacts) -> usize {
    facts
        .reexports
        .iter()
        .filter(|reexport| reexport.namespace)
        .filter_map(|reexport| reexport.exported.as_deref())
        .map(|exported| make_id(&[&facts.stem, exported]).capacity())
        .max()
        .unwrap_or(0)
}

fn module_export_resolution_string_bounds(facts: &ModuleFacts) -> ExportResolutionStringBounds {
    let mut bounds = ExportResolutionStringBounds {
        stored: [
            facts.source_file.len(),
            facts.file_id.len(),
            facts.stem.len(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0),
        namespace_candidate_capacity: 0,
    };
    for (name, id) in facts.definitions.iter().chain(&facts.aliases) {
        bounds.include_stored(name);
        bounds.include_stored(id);
    }
    for import in &facts.imports {
        bounds.include_stored(&import.specifier);
        if let Some(imported) = &import.imported {
            bounds.include_stored(imported);
        }
        if let Some(local) = &import.local {
            bounds.include_stored(local);
        }
    }
    for reexport in &facts.reexports {
        bounds.include_stored(&reexport.specifier);
        if let Some(imported) = &reexport.imported {
            bounds.include_stored(imported);
        }
        if let Some(exported) = &reexport.exported {
            bounds.include_stored(exported);
        }
    }
    bounds.namespace_candidate_capacity = module_namespace_candidate_capacity(facts);
    for (name, bindings) in &facts.exports {
        bounds.include_stored(name);
        for binding in bindings {
            match binding {
                ExportBinding::Local(local) => bounds.include_stored(local),
                ExportBinding::Reexport { source, imported } => {
                    bounds.include_stored(source);
                    bounds.include_stored(imported);
                }
                ExportBinding::Namespace(source) => bounds.include_stored(source),
            }
        }
    }
    bounds
}

fn export_resolution_scratch_bytes(bounds: ExportResolutionStringBounds) -> anyhow::Result<usize> {
    use std::mem::size_of;

    let candidate = bounds
        .candidate_len()
        .checked_mul(MAX_EXPORT_RESOLUTION_STEPS)
        .and_then(|bytes| {
            bytes.checked_add(btree_set_storage_bytes::<String>(
                MAX_EXPORT_RESOLUTION_STEPS,
            ))
        })
        .ok_or_else(|| anyhow::anyhow!("JavaScript export-resolution charge exceeds usize"))?;
    let visiting_payload = bounds
        .stored
        .checked_mul(2)
        .and_then(|bytes| {
            bytes.checked_add(
                2usize.saturating_mul(
                    2usize
                        .saturating_mul(size_of::<AtomicUsize>())
                        .saturating_add(size_of::<usize>()),
                ),
            )
        })
        .and_then(|bytes| bytes.checked_mul(MAX_EXPORT_RESOLUTION_DEPTH))
        .ok_or_else(|| anyhow::anyhow!("JavaScript export-resolution charge exceeds usize"))?;
    let visiting = visiting_payload
        .checked_add(btree_set_storage_bytes::<(Arc<str>, Arc<str>)>(
            MAX_EXPORT_RESOLUTION_DEPTH,
        ))
        .ok_or_else(|| anyhow::anyhow!("JavaScript export-resolution charge exceeds usize"))?;
    candidate
        .checked_add(visiting)
        .ok_or_else(|| anyhow::anyhow!("JavaScript export-resolution charge exceeds usize"))
}

#[cfg(test)]
pub(crate) fn resolve_with_snapshot_prefix(
    extractions: &mut [Extraction],
    snapshot: &ProjectSnapshot,
    fresh_prefix: usize,
    cpu_arena_bytes: usize,
) -> anyhow::Result<()> {
    let fresh_prefix = fresh_prefix.min(extractions.len());
    let (fresh, context) = extractions.split_at_mut(fresh_prefix);
    resolve_with_snapshot_partitions(fresh, context, snapshot, cpu_arena_bytes)
}

pub(crate) fn resolve_with_snapshot_partitions(
    fresh: &mut [Extraction],
    context: &[Extraction],
    snapshot: &ProjectSnapshot,
    cpu_arena_bytes: usize,
) -> anyhow::Result<()> {
    let fresh_has_module = fresh.iter().any(|extraction| {
        extraction.nodes.iter().any(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("file")
                && is_javascript_source(&node.source_file)
                && snapshot
                    .decoded_source_len(&normalize_slashes(&node.source_file))
                    .is_some()
        })
    });
    if !fresh_has_module {
        return Ok(());
    }

    // Build and measure one throw-away unresolved module at a time. The broad
    // source multiplier bounds that parser's peak, while the aggregate CPU
    // admission uses measured fact/vector/string capacities so ordinary
    // multi-file projects are not charged hundreds of bytes per source byte.
    let mut module_retained_bytes = 0usize;
    let mut module_count = 0usize;
    let mut requires_metadata = false;
    let mut normalization_scratch_bytes = 0usize;
    let max_snapshot_path_len = snapshot.max_path_len();
    let mut export_resolution_bounds = ExportResolutionStringBounds {
        stored: max_snapshot_path_len,
        ..ExportResolutionStringBounds::default()
    };
    for extraction in fresh.iter().chain(context) {
        let Some(file_node) = extraction.nodes.iter().find(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("file")
                && is_javascript_source(&node.source_file)
        }) else {
            continue;
        };
        let source_file = normalize_slashes(&file_node.source_file);
        let Some(decoded_bytes) = snapshot.decoded_source_len(&source_file) else {
            continue;
        };
        let source_working_set = module_source_working_set_bytes(decoded_bytes)?
            .checked_add(definition_index_admission_bytes(extraction, &source_file))
            .ok_or_else(|| anyhow::anyhow!("JavaScript resolver source charge exceeds usize"))?;
        let prior_module_map_storage = btree_map_storage_bytes::<String, ModuleFacts>(module_count);
        let provisional_required = snapshot
            .retained_bytes()
            .checked_add(JS_RESOLVER_FIXED_SCRATCH_BYTES)
            .and_then(|bytes| bytes.checked_add(module_retained_bytes))
            .and_then(|bytes| bytes.checked_add(prior_module_map_storage))
            .and_then(|bytes| bytes.checked_add(source_working_set))
            .ok_or_else(|| anyhow::anyhow!("JavaScript resolver CPU-arena charge exceeds usize"))?;
        anyhow::ensure!(
            provisional_required <= cpu_arena_bytes,
            "JavaScript resolver requires at least {provisional_required} CPU-arena bytes while parsing {source_file}, exceeding its {cpu_arena_bytes}-byte budget"
        );
        let Some(source) = snapshot.resolution_source(&source_file) else {
            continue;
        };
        let facts = collect_module_facts(0, extraction, &source_file, source, &|_, _| None);
        let module_normalization_scratch =
            derived_id_normalization_scratch_bytes(&facts, max_snapshot_path_len)?;
        let normalization_required = provisional_required
            .checked_add(module_normalization_scratch)
            .ok_or_else(|| anyhow::anyhow!("JavaScript derived-ID charge exceeds usize"))?;
        anyhow::ensure!(
            normalization_required <= cpu_arena_bytes,
            "JavaScript resolver requires at least {normalization_required} CPU-arena bytes while preparing derived IDs for {source_file}, exceeding its {cpu_arena_bytes}-byte budget"
        );
        normalization_scratch_bytes = normalization_scratch_bytes.max(module_normalization_scratch);
        let source_requires_metadata = facts
            .imports
            .iter()
            .map(|fact| fact.specifier.as_ref())
            .chain(facts.reexports.iter().map(|fact| fact.specifier.as_ref()))
            .chain(
                facts
                    .dynamic_imports
                    .iter()
                    .map(|(_, specifier)| specifier.as_str()),
            )
            .any(|specifier| {
                matches!(
                    classify_es_module_specifier(&source_file, specifier),
                    EsModuleSpecifier::Bare
                )
            });
        requires_metadata |= source_requires_metadata;
        let retained = module_facts_admission_bytes(&facts, max_snapshot_path_len);
        export_resolution_bounds.include(module_export_resolution_string_bounds(&facts));
        anyhow::ensure!(
            retained != usize::MAX,
            "JavaScript resolver module charge exceeds usize for {source_file}"
        );
        module_retained_bytes = module_retained_bytes
            .checked_add(retained)
            .ok_or_else(|| anyhow::anyhow!("JavaScript resolver module charge exceeds usize"))?;
        module_count = module_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("JavaScript resolver module count exceeds usize"))?;
    }
    let metadata_temporary_bytes = if requires_metadata {
        snapshot.metadata_parse_peak_bytes()?
    } else {
        0
    };
    let export_resolution_temporary_bytes =
        export_resolution_scratch_bytes(export_resolution_bounds)?;
    let module_collection_temporary_bytes =
        btree_map_storage_bytes::<String, ModuleFacts>(module_count)
            .checked_add(btree_set_storage_bytes::<String>(module_count))
            .ok_or_else(|| anyhow::anyhow!("JavaScript module collection charge exceeds usize"))?;
    let temporary_bytes = JS_RESOLVER_FIXED_SCRATCH_BYTES
        .checked_add(module_retained_bytes)
        .and_then(|bytes| bytes.checked_add(module_collection_temporary_bytes))
        .and_then(|bytes| bytes.checked_add(metadata_temporary_bytes))
        .and_then(|bytes| bytes.checked_add(export_resolution_temporary_bytes))
        .and_then(|bytes| bytes.checked_add(normalization_scratch_bytes))
        .ok_or_else(|| anyhow::anyhow!("JavaScript resolver temporary charge exceeds usize"))?;
    let required_cpu_bytes = snapshot
        .retained_bytes()
        .checked_add(temporary_bytes)
        .ok_or_else(|| anyhow::anyhow!("JavaScript resolver CPU-arena charge exceeds usize"))?;
    anyhow::ensure!(
        required_cpu_bytes <= cpu_arena_bytes,
        "JavaScript resolver requires {required_cpu_bytes} CPU-arena bytes ({} snapshot + {temporary_bytes} decoded/module/metadata/export/normalization temporary, including {module_collection_temporary_bytes} module-collection storage, {metadata_temporary_bytes} metadata peak, {export_resolution_temporary_bytes} export-resolution scratch, and {normalization_scratch_bytes} ID-normalization scratch), exceeding its {cpu_arena_bytes}-byte budget",
        snapshot.retained_bytes()
    );

    let mut modules = BTreeMap::<String, ModuleFacts>::new();
    for (index, extraction) in fresh
        .iter()
        .enumerate()
        .chain(context.iter().map(|extraction| (usize::MAX, extraction)))
    {
        let Some(file_node) = extraction.nodes.iter().find(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("file")
                && is_javascript_source(&node.source_file)
        }) else {
            continue;
        };
        let source_file = normalize_slashes(&file_node.source_file);
        let Some(source) = snapshot.resolution_source(&source_file) else {
            continue;
        };
        let facts = collect_module_facts(
            index,
            extraction,
            &source_file,
            source,
            &|specifier, source_file| snapshot.resolve_module_specifier(specifier, source_file),
        );
        modules.insert(source_file, facts);
    }
    resolve_modules(fresh, modules, fresh.len(), &|specifier, source_file| {
        snapshot.resolve_module_specifier(specifier, source_file)
    });
    Ok(())
}

fn resolve_from_sources<F>(
    extractions: &mut [Extraction],
    mut sources: BTreeMap<String, String>,
    resolve_specifier: F,
) where
    F: Fn(&str, &str) -> Option<String>,
{
    let mut modules = BTreeMap::<String, ModuleFacts>::new();
    for (index, extraction) in extractions.iter().enumerate() {
        let Some(file_node) = extraction.nodes.iter().find(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("file")
                && is_javascript_source(&node.source_file)
        }) else {
            continue;
        };
        let source_file = normalize_slashes(&file_node.source_file);
        let Some(source) = sources.remove(&source_file) else {
            continue;
        };
        let facts =
            collect_module_facts(index, extraction, &source_file, source, &resolve_specifier);
        modules.insert(source_file, facts);
    }

    resolve_modules(extractions, modules, extractions.len(), &resolve_specifier);
}

fn resolve_modules<F>(
    extractions: &mut [Extraction],
    mut modules: BTreeMap<String, ModuleFacts>,
    fresh_prefix: usize,
    resolve_specifier: &F,
) where
    F: Fn(&str, &str) -> Option<String>,
{
    if modules.is_empty() {
        return;
    }

    let module_sources = modules.keys().cloned().collect::<BTreeSet<_>>();
    for facts in modules.values_mut() {
        materialize_export_bindings(facts, &module_sources);
    }

    for facts in modules
        .values()
        .filter(|facts| facts.extraction < fresh_prefix)
    {
        rebuild_module_edges(extractions, facts, &modules, resolve_specifier);
    }
}

fn lossy_utf8_len(mut bytes: &[u8]) -> Option<usize> {
    let mut decoded = 0usize;
    loop {
        match std::str::from_utf8(bytes) {
            Ok(valid) => return decoded.checked_add(valid.len()),
            Err(error) => {
                decoded = decoded
                    .checked_add(error.valid_up_to())?
                    .checked_add('\u{fffd}'.len_utf8())?;
                let Some(invalid_bytes) = error.error_len() else {
                    return Some(decoded);
                };
                bytes = bytes.get(error.valid_up_to().checked_add(invalid_bytes)?..)?;
            }
        }
    }
}

fn definition_index_admission_bytes(extraction: &Extraction, source_file: &str) -> usize {
    use std::mem::size_of;

    let matching_count = extraction
        .nodes
        .iter()
        .filter(|node| node.source_file == source_file)
        .count();
    extraction
        .nodes
        .iter()
        .filter(|node| node.source_file == source_file)
        .fold(
            source_file
                .len()
                // `collect_module_facts` derives both the retained stem and a
                // normalized file ID. Use the same pinned-Unicode expansion
                // bound as namespace IDs so joined, NFKC, and case-fold
                // intermediates are admitted even for a tiny source at a
                // highly expanding Unicode path.
                .saturating_mul(ID_NORMALIZATION_PREFLIGHT_FACTOR)
                .saturating_add(size_of::<ModuleFacts>())
                .saturating_add(2usize.saturating_mul(size_of::<String>()))
                .saturating_add(6usize.saturating_mul(size_of::<usize>()))
                .saturating_add(btree_map_storage_bytes::<String, String>(matching_count)),
            |bytes, node| {
                // Counting the file anchor and labels that trim to empty is a
                // deliberate overestimate. It avoids constructing a normalized
                // file ID before admission, while bounding every definitions-map
                // allocation `collect_module_facts` can retain.
                bytes
                    .saturating_add(node.label.len())
                    .saturating_add(node.id.len())
            },
        )
}

pub(crate) fn is_javascript_source(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    [
        ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".svelte", ".vue", ".astro",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
}

/// Remove legacy path-probe resolutions into sources whose committed code
/// ownership was invalidated by a verified classification transition.
/// File-level module evidence remains as an explicitly unresolved reference;
/// symbol-level bindings are dropped because the non-code replacement cannot
/// own exported declarations.
pub(crate) fn invalidate_resolved_targets_for_sources(
    extractions: &mut [Extraction],
    invalidated_sources: &BTreeSet<String>,
) {
    if invalidated_sources.is_empty() {
        return;
    }
    let mut sources_by_path = BTreeMap::<String, String>::new();
    let mut sources_by_stem = BTreeMap::<String, String>::new();
    let mut sources_by_anchor = BTreeMap::<String, String>::new();
    for source in invalidated_sources {
        let normalized = normalize_slashes(source);
        let stem = normalize_slashes(
            Path::new(&normalized)
                .with_extension("")
                .to_string_lossy()
                .as_ref(),
        );
        sources_by_path
            .entry(normalized.clone())
            .or_insert_with(|| source.clone());
        sources_by_stem
            .entry(stem.clone())
            .or_insert_with(|| source.clone());
        sources_by_anchor
            .entry(make_id(&[&stem]))
            .or_insert_with(|| source.clone());
    }
    for extraction in extractions {
        extraction.edges.retain_mut(|edge| {
            let target_file = edge
                .extra
                .get("target_file")
                .and_then(Value::as_str)
                .map(normalize_slashes);
            let invalidated_source = if let Some(target_file) = target_file.as_deref() {
                // Explicit path evidence is authoritative. Do not fall back to
                // a possibly colliding legacy anchor ID when it names another
                // source.
                sources_by_path.get(target_file).or_else(|| {
                    let source_parent = Path::new(&edge.source_file)
                        .parent()
                        .unwrap_or_else(|| Path::new(""));
                    let logical_stem = normalize_slashes(
                        lexical_normalize(
                            &source_parent.join(target_file.trim_start_matches("./")),
                        )
                        .with_extension("")
                        .to_string_lossy()
                        .as_ref(),
                    );
                    sources_by_stem.get(&logical_stem)
                })
            } else {
                sources_by_anchor.get(edge.true_target())
            };
            let Some(invalidated_source) = invalidated_source else {
                return true;
            };
            if matches!(
                edge.relation.as_str(),
                "imports_from" | "re_exports" | "dynamic_import"
            ) {
                let unresolved = make_id(&[
                    "ref",
                    &Path::new(invalidated_source)
                        .with_extension("")
                        .to_string_lossy(),
                ]);
                edge.target = unresolved.clone();
                edge.extra.insert("_tgt".into(), unresolved.into());
                edge.extra.remove("target_file");
                true
            } else {
                false
            }
        });
    }
}

fn is_resolver_metadata(source: &str) -> bool {
    let path = Path::new(source);
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        || matches!(
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("pnpm-workspace.yaml" | "pnpm-workspace.yml")
        )
}

fn is_sfc_source(source: &str) -> bool {
    matches!(
        Path::new(source)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("vue" | "astro" | "svelte")
    )
}

fn unresolved_module_id(source_file: &str, specifier: &str, sfc: bool) -> String {
    match classify_es_module_specifier(source_file, specifier) {
        EsModuleSpecifier::ProjectRelative(logical) if sfc => {
            make_id(&[&Path::new(&logical).with_extension("").to_string_lossy()])
        }
        EsModuleSpecifier::ProjectRelative(logical) => make_id(&[
            "ref",
            &Path::new(&logical).with_extension("").to_string_lossy(),
        ]),
        EsModuleSpecifier::Bare => make_id(&["ref", specifier]),
        EsModuleSpecifier::Unsafe => make_id(&["ref", "unsafe", source_file, specifier]),
    }
}

fn raw_dynamic_import_id(source_file: &str, specifier: &str) -> String {
    let base = Path::new(source_file)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    make_id(&[&base
        .join(specifier.trim_start_matches("./"))
        .to_string_lossy()])
}

fn raw_static_import_id(source_file: &str, specifier: &str) -> String {
    let base = Path::new(source_file)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let logical = lexical_normalize(&base.join(specifier));
    make_id(&[&logical.with_extension("").to_string_lossy()])
}

fn collect_module_facts<F>(
    extraction: usize,
    value: &Extraction,
    source_file: &str,
    text: String,
    resolve_specifier: &F,
) -> ModuleFacts
where
    F: Fn(&str, &str) -> Option<String>,
{
    let file_id = make_id(&[&path_without_extension(source_file)]);
    let stem = path_without_extension(source_file);
    let definitions = value
        .nodes
        .iter()
        .filter(|node| node.source_file == source_file && node.id != file_id)
        .filter_map(|node| {
            let label = node
                .label
                .trim()
                .trim_start_matches('.')
                .trim_end_matches("()")
                .to_owned();
            (!label.is_empty()).then(|| (label, node.id.clone()))
        })
        .collect::<BTreeMap<_, _>>();

    let mut imports = parse_imports(&text);
    let mut reexports = parse_reexports(&text);
    let dynamic_imports = parse_dynamic_imports(&text);
    let aliases = parse_local_aliases(&text);
    let mut exports = BTreeMap::<String, Vec<ExportBinding>>::new();

    for name in parse_direct_exports(&text) {
        exports
            .entry(name.clone())
            .or_default()
            .push(ExportBinding::Local(name));
    }
    for (local, exported) in parse_local_export_clauses(&text) {
        exports
            .entry(exported)
            .or_default()
            .push(ExportBinding::Local(local));
    }
    if let Some(default_name) = parse_default_export(&text) {
        exports
            .entry("default".into())
            .or_default()
            .push(ExportBinding::Local(default_name));
    }

    // Resolve specifiers while the project source identity is known. The
    // resolver implementation decides whether this is a legacy filesystem
    // context or an isolated byte snapshot.
    let mut resolved_specifiers = BTreeMap::<Arc<str>, Option<Arc<str>>>::new();
    for import in &mut imports {
        let target = resolved_specifiers
            .entry(Arc::clone(&import.specifier))
            .or_insert_with_key(|specifier| {
                (!matches!(
                    classify_es_module_specifier(source_file, specifier),
                    EsModuleSpecifier::Unsafe
                ))
                .then(|| resolve_specifier(specifier, source_file))
                .flatten()
                .map(Arc::<str>::from)
            });
        if let Some(target) = target {
            import.specifier = Arc::clone(target);
            import.resolved = true;
        }
    }
    for reexport in &mut reexports {
        let target = resolved_specifiers
            .entry(Arc::clone(&reexport.specifier))
            .or_insert_with_key(|specifier| {
                (!matches!(
                    classify_es_module_specifier(source_file, specifier),
                    EsModuleSpecifier::Unsafe
                ))
                .then(|| resolve_specifier(specifier, source_file))
                .flatten()
                .map(Arc::<str>::from)
            });
        if let Some(target) = target {
            reexport.specifier = Arc::clone(target);
            reexport.resolved = true;
        }
    }

    ModuleFacts {
        extraction,
        source_file: source_file.into(),
        source: text,
        file_id,
        stem,
        definitions,
        aliases,
        imports,
        reexports,
        dynamic_imports,
        exports,
        stars: Vec::new(),
    }
}

fn materialize_export_bindings(facts: &mut ModuleFacts, modules: &BTreeSet<String>) {
    for reexport in facts.reexports.clone() {
        if !reexport.resolved || !modules.contains(reexport.specifier.as_ref()) {
            continue;
        }
        if reexport.star {
            facts.stars.push(reexport.specifier);
            continue;
        }
        let (Some(imported), Some(exported)) = (reexport.imported, reexport.exported) else {
            continue;
        };
        let binding = if reexport.namespace {
            ExportBinding::Namespace(reexport.specifier)
        } else {
            ExportBinding::Reexport {
                source: reexport.specifier,
                imported,
            }
        };
        facts.exports.entry(exported).or_default().push(binding);
    }
}

fn rebuild_module_edges<F>(
    extractions: &mut [Extraction],
    facts: &ModuleFacts,
    modules: &BTreeMap<String, ModuleFacts>,
    resolve_specifier: &F,
) where
    F: Fn(&str, &str) -> Option<String>,
{
    let extraction = &mut extractions[facts.extraction];
    let rebuilt_locations = facts
        .imports
        .iter()
        .map(|fact| fact.line)
        .chain(facts.reexports.iter().map(|fact| fact.line))
        .map(|line| format!("L{line}"))
        .collect::<BTreeSet<_>>();
    let mut dynamic_raw_targets = BTreeMap::<String, BTreeSet<String>>::new();
    for (line, specifier) in &facts.dynamic_imports {
        dynamic_raw_targets
            .entry(format!("L{line}"))
            .or_default()
            .insert(raw_dynamic_import_id(&facts.source_file, specifier));
    }
    let unsafe_sfc_raw_targets = if is_sfc_source(&facts.source_file) {
        facts
            .imports
            .iter()
            .map(|fact| fact.specifier.as_ref())
            .chain(facts.reexports.iter().map(|fact| fact.specifier.as_ref()))
            .filter(|specifier| {
                matches!(
                    classify_es_module_specifier(&facts.source_file, specifier),
                    EsModuleSpecifier::Unsafe
                )
            })
            .flat_map(|specifier| {
                [
                    raw_static_import_id(&facts.source_file, specifier),
                    raw_dynamic_import_id(&facts.source_file, specifier),
                ]
            })
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    extraction.edges.retain(|edge| {
        !(edge.true_source() == facts.file_id
            && matches!(
                edge.relation.as_str(),
                "imports" | "imports_from" | "re_exports"
            )
            && edge
                .extra
                .get("context")
                .and_then(Value::as_str)
                .is_some_and(|context| matches!(context, "import" | "re-export"))
            && (unsafe_sfc_raw_targets.contains(edge.true_target())
                || (edge
                    .extra
                    .get("source_location")
                    .and_then(Value::as_str)
                    .is_some_and(|location| rebuilt_locations.contains(location))
                    && !edge
                        .extra
                        .get("source_location")
                        .and_then(Value::as_str)
                        .is_some_and(|location| {
                            dynamic_raw_targets
                                .get(location)
                                .is_some_and(|targets| targets.contains(edge.true_target()))
                        }))))
    });

    for import in &facts.imports {
        let target_file = import
            .resolved
            .then(|| modules.get(import.specifier.as_ref()))
            .flatten();
        let file_target = target_file
            .map(|target| target.file_id.clone())
            .unwrap_or_else(|| {
                if import.resolved {
                    make_id(&[&path_without_extension(&import.specifier)])
                } else {
                    unresolved_module_id(
                        &facts.source_file,
                        &import.specifier,
                        is_sfc_source(&facts.source_file),
                    )
                }
            });
        let mut file_edge = module_edge(
            &facts.file_id,
            &file_target,
            "imports_from",
            &facts.source_file,
            import.line,
            "import",
        );
        if let Some(target) = target_file {
            file_edge
                .extra
                .insert("target_file".into(), target.source_file.clone().into());
        } else if import.resolved {
            file_edge
                .extra
                .insert("target_file".into(), import.specifier.to_string().into());
        }
        push_unique_edge(&mut extraction.edges, file_edge);
        let (Some(imported), Some(local), Some(target)) =
            (&import.imported, &import.local, target_file)
        else {
            continue;
        };
        let target_id = match resolve_export(modules, &target.source_file, imported) {
            ExportResolution::Resolved(id) => id,
            ExportResolution::Ambiguous | ExportResolution::Missing => {
                make_id(&[&target.stem, imported])
            }
        };
        let mut edge = module_edge(
            &facts.file_id,
            &target_id,
            "imports",
            &facts.source_file,
            import.line,
            "import",
        );
        edge.extra
            .insert("local_alias".into(), local.clone().into());
        edge.extra
            .insert("imported_name".into(), imported.clone().into());
        edge.extra
            .insert("target_file".into(), target.source_file.clone().into());
        push_unique_edge(&mut extraction.edges, edge);
    }

    for reexport in &facts.reexports {
        let Some(target) = reexport
            .resolved
            .then(|| modules.get(reexport.specifier.as_ref()))
            .flatten()
        else {
            continue;
        };
        for relation in ["imports_from", "re_exports"] {
            let mut edge = module_edge(
                &facts.file_id,
                &target.file_id,
                relation,
                &facts.source_file,
                reexport.line,
                "re-export",
            );
            edge.extra
                .insert("target_file".into(), target.source_file.clone().into());
            push_unique_edge(&mut extraction.edges, edge);
        }
        if reexport.star {
            continue;
        }
        let (Some(imported), Some(exported)) = (&reexport.imported, &reexport.exported) else {
            continue;
        };
        if reexport.namespace {
            let namespace_id = make_id(&[&facts.stem, exported]);
            if extraction.nodes.iter().all(|node| node.id != namespace_id) {
                let mut extra = BTreeMap::new();
                extra.insert("_origin".into(), "ast".into());
                extra.insert("type".into(), "module".into());
                extra.insert("exported".into(), true.into());
                crate::resolution::push_resolved_node(
                    &mut extraction.nodes,
                    Node {
                        id: namespace_id.clone(),
                        label: exported.clone(),
                        file_type: "code".into(),
                        source_file: facts.source_file.clone(),
                        source_location: Some(format!("L{}", reexport.line)),
                        community: None,
                        extra,
                    },
                );
            }
            push_unique_edge(
                &mut extraction.edges,
                module_edge(
                    &facts.file_id,
                    &namespace_id,
                    "contains",
                    &facts.source_file,
                    reexport.line,
                    "re-export",
                ),
            );
            continue;
        }
        let target_id = match resolve_export(modules, &target.source_file, imported) {
            ExportResolution::Resolved(id) => id,
            ExportResolution::Ambiguous | ExportResolution::Missing => {
                make_id(&[&target.stem, imported])
            }
        };
        let mut edge = module_edge(
            &facts.file_id,
            &target_id,
            "re_exports",
            &facts.source_file,
            reexport.line,
            "re-export",
        );
        edge.extra
            .insert("exported_name".into(), exported.clone().into());
        edge.extra
            .insert("target_file".into(), target.source_file.clone().into());
        push_unique_edge(&mut extraction.edges, edge);
    }

    // Canonicalize and mark deferred import() facts emitted by the per-file
    // walker. A deferred dependency remains visible, but must not participate
    // in static import-cycle detection.
    for (line, specifier) in &facts.dynamic_imports {
        let classification = classify_es_module_specifier(&facts.source_file, specifier);
        let target_source = (!matches!(classification, EsModuleSpecifier::Unsafe))
            .then(|| resolve_specifier(specifier, &facts.source_file))
            .flatten();
        let target = target_source
            .as_ref()
            .and_then(|source| modules.get(source))
            .map(|target| target.file_id.clone())
            .or_else(|| {
                target_source
                    .as_ref()
                    .map(|source| make_id(&[&path_without_extension(source)]))
            })
            .unwrap_or_else(|| {
                unresolved_module_id(
                    &facts.source_file,
                    specifier,
                    is_sfc_source(&facts.source_file),
                )
            });
        let location = format!("L{line}");
        let raw_target = raw_dynamic_import_id(&facts.source_file, specifier);
        let existing = extraction.edges.iter_mut().rev().find(|edge| {
            edge.extra.get("source_location").and_then(Value::as_str) == Some(location.as_str())
                && edge.extra.get("context").and_then(Value::as_str) == Some("import")
                && (edge.relation == "dynamic_import"
                    || (edge.relation == "imports_from" && edge.true_target() == raw_target))
        });
        if let Some(edge) = existing {
            edge.target = target.clone();
            edge.extra.insert("_tgt".into(), target.clone().into());
            edge.extra.insert("deferred".into(), true.into());
            if let Some(target_source) = &target_source {
                edge.extra
                    .insert("target_file".into(), target_source.clone().into());
            } else {
                edge.extra.remove("target_file");
            }
        } else {
            let mut edge = module_edge(
                &facts.file_id,
                &target,
                "dynamic_import",
                &facts.source_file,
                *line,
                "import",
            );
            edge.extra.insert("deferred".into(), true.into());
            if let Some(target_source) = target_source {
                edge.extra
                    .insert("target_file".into(), target_source.into());
            }
            push_unique_edge(&mut extraction.edges, edge);
        }
    }

    augment_typescript_type_edges(extraction, facts, modules, &facts.source);
}

fn augment_typescript_type_edges(
    extraction: &mut Extraction,
    facts: &ModuleFacts,
    modules: &BTreeMap<String, ModuleFacts>,
    text: &str,
) {
    let lower = facts.source_file.to_ascii_lowercase();
    if ![".ts", ".tsx", ".mts", ".cts"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
    {
        return;
    }
    let declarations = Regex::new(
        r"(?m)(?:^|\n)\s*(?:export\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)([^\n{]*)",
    )
    .expect("TypeScript class relationship regex");
    let parent = Regex::new(r"\bextends\s+([A-Za-z_$][\w$]*)").expect("TypeScript extends regex");
    let interfaces = Regex::new(r"\bimplements\s+([^\n{]+)").expect("TypeScript implements regex");
    let type_name = Regex::new(r"[A-Za-z_$][\w$]*").expect("TypeScript type name regex");
    for capture in declarations.captures_iter(text) {
        let Some(owner) = facts.definitions.get(&capture[1]) else {
            continue;
        };
        let line = line_number(text, capture.get(0).expect("whole class").start());
        if let Some(name) = parent
            .captures(&capture[2])
            .and_then(|value| value.get(1))
            .map(|value| value.as_str())
            && let Some(target) = resolve_visible_symbol(modules, facts, name)
        {
            push_unique_edge(
                &mut extraction.edges,
                typed_edge(owner, &target, "inherits", &facts.source_file, line, None),
            );
        }
        if let Some(list) = interfaces
            .captures(&capture[2])
            .and_then(|value| value.get(1))
        {
            for interface in list.as_str().split(',') {
                let Some(name) = type_name.find(interface).map(|value| value.as_str()) else {
                    continue;
                };
                if let Some(target) = resolve_visible_symbol(modules, facts, name) {
                    push_unique_edge(
                        &mut extraction.edges,
                        typed_edge(owner, &target, "implements", &facts.source_file, line, None),
                    );
                }
            }
        }
    }

    let methods =
        Regex::new(r"(?m)([A-Za-z_$][\w$]*)\s*\(([^)]*)\)\s*:\s*([A-Za-z_$][\w$]*(?:\s*<[^>]+>)?)")
            .expect("TypeScript method signature regex");
    for capture in methods.captures_iter(text) {
        let Some(owner) = facts.definitions.get(&capture[1]) else {
            continue;
        };
        let line = line_number(text, capture.get(0).expect("whole method").start());
        for parameter in capture[2].split(',') {
            let Some((_, annotation)) = parameter.split_once(':') else {
                continue;
            };
            emit_type_expression(
                extraction,
                facts,
                modules,
                owner,
                annotation,
                "parameter_type",
                line,
            );
        }
        emit_type_expression(
            extraction,
            facts,
            modules,
            owner,
            &capture[3],
            "return_type",
            line,
        );
    }
}

fn emit_type_expression(
    extraction: &mut Extraction,
    facts: &ModuleFacts,
    modules: &BTreeMap<String, ModuleFacts>,
    owner: &str,
    expression: &str,
    context: &str,
    line: usize,
) {
    let names = Regex::new(r"[A-Za-z_$][\w$]*").expect("TypeScript type token regex");
    for (index, token) in names.find_iter(expression).enumerate() {
        let name = token.as_str();
        if matches!(
            name,
            "string" | "number" | "boolean" | "void" | "unknown" | "any"
        ) {
            continue;
        }
        let Some(target) = resolve_visible_symbol(modules, facts, name) else {
            continue;
        };
        push_unique_edge(
            &mut extraction.edges,
            typed_edge(
                owner,
                &target,
                "references",
                &facts.source_file,
                line,
                Some(if index == 0 { context } else { "generic_arg" }),
            ),
        );
    }
}

fn resolve_visible_symbol(
    modules: &BTreeMap<String, ModuleFacts>,
    module: &ModuleFacts,
    local: &str,
) -> Option<String> {
    let mut imports = module
        .imports
        .iter()
        .filter(|import| import.resolved)
        .filter(|import| import.local.as_deref() == Some(local))
        .filter_map(|import| Some((import.specifier.as_ref(), import.imported.as_deref()?)));
    if let Some(first) = imports.next()
        && imports.next().is_none()
        && let ExportResolution::Resolved(id) = resolve_export(modules, first.0, first.1)
    {
        return Some(id);
    }
    module.definitions.get(local).cloned()
}

fn typed_edge(
    source: &str,
    target: &str,
    relation: &str,
    source_file: &str,
    line: usize,
    context: Option<&str>,
) -> Edge {
    let mut edge = module_edge(source, target, relation, source_file, line, "type");
    match context {
        Some(context) => {
            edge.extra.insert("context".into(), context.into());
        }
        None => {
            edge.extra.remove("context");
        }
    }
    edge
}

fn resolve_export(
    modules: &BTreeMap<String, ModuleFacts>,
    source: &str,
    exported: &str,
) -> ExportResolution {
    resolve_export_inner(
        modules,
        source,
        exported,
        &mut ExportResolutionState::new(),
        0,
    )
}

fn resolve_export_inner(
    modules: &BTreeMap<String, ModuleFacts>,
    source: &str,
    exported: &str,
    state: &mut ExportResolutionState,
    depth: usize,
) -> ExportResolution {
    if depth >= MAX_EXPORT_RESOLUTION_DEPTH || !state.consume_step() {
        return ExportResolution::Ambiguous;
    }
    let key = (Arc::<str>::from(source), Arc::<str>::from(exported));
    if !state.visiting.insert(key.clone()) {
        return ExportResolution::Missing;
    }
    let Some(module) = modules.get(source) else {
        state.visiting.remove(&key);
        return ExportResolution::Missing;
    };

    let mut candidates = BTreeSet::new();
    let mut ambiguous = false;
    if let Some(bindings) = module.exports.get(exported) {
        for binding in bindings {
            if !state.consume_step() {
                ambiguous = true;
                break;
            }
            match resolve_binding(modules, module, exported, binding, state, depth) {
                ExportResolution::Resolved(id) => {
                    candidates.insert(id);
                    if candidates.len() > 1 {
                        ambiguous = true;
                        break;
                    }
                }
                ExportResolution::Ambiguous => {
                    ambiguous = true;
                    break;
                }
                ExportResolution::Missing => {}
            }
        }
    }
    if candidates.is_empty() && !ambiguous {
        for star in &module.stars {
            match resolve_export_inner(modules, star, exported, state, depth + 1) {
                ExportResolution::Resolved(id) => {
                    candidates.insert(id);
                    if candidates.len() > 1 {
                        ambiguous = true;
                        break;
                    }
                }
                ExportResolution::Ambiguous => {
                    ambiguous = true;
                    break;
                }
                ExportResolution::Missing => {}
            }
        }
    }
    state.visiting.remove(&key);
    if ambiguous || candidates.len() > 1 {
        ExportResolution::Ambiguous
    } else if let Some(id) = candidates.into_iter().next() {
        ExportResolution::Resolved(id)
    } else {
        ExportResolution::Missing
    }
}

fn resolve_binding(
    modules: &BTreeMap<String, ModuleFacts>,
    module: &ModuleFacts,
    exported: &str,
    binding: &ExportBinding,
    state: &mut ExportResolutionState,
    depth: usize,
) -> ExportResolution {
    match binding {
        ExportBinding::Local(local) => resolve_local(modules, module, local, state, depth),
        ExportBinding::Reexport { source, imported } => {
            resolve_export_inner(modules, source, imported, state, depth + 1)
        }
        ExportBinding::Namespace(_) => module
            .definitions
            .get(exported)
            .cloned()
            .map(ExportResolution::Resolved)
            .unwrap_or_else(|| ExportResolution::Resolved(make_id(&[&module.stem, exported]))),
    }
}

fn resolve_local(
    modules: &BTreeMap<String, ModuleFacts>,
    module: &ModuleFacts,
    local: &str,
    state: &mut ExportResolutionState,
    depth: usize,
) -> ExportResolution {
    let mut current = local;
    let mut aliases_remaining = module.aliases.len();
    while let Some(alias) = module.aliases.get(current) {
        if aliases_remaining == 0 || !state.consume_step() {
            return ExportResolution::Ambiguous;
        }
        aliases_remaining -= 1;
        current = alias;
    }
    let mut matching_imports = module
        .imports
        .iter()
        .filter(|import| import.resolved)
        .filter(|import| import.local.as_deref() == Some(current))
        .filter_map(|import| Some((import.specifier.as_ref(), import.imported.as_deref()?)));
    if let Some(first) = matching_imports.next() {
        if matching_imports.next().is_some() {
            return ExportResolution::Ambiguous;
        }
        return resolve_export_inner(modules, first.0, first.1, state, depth + 1);
    }
    module
        .definitions
        .get(current)
        .cloned()
        .map(ExportResolution::Resolved)
        .unwrap_or(ExportResolution::Missing)
}

fn parse_imports(text: &str) -> Vec<ImportFact> {
    let re =
        Regex::new(r#"(?m)(?:^|;)\s*import\s+(?:type\s+)?([^;\n]+?)\s+from\s+['\"]([^'\"]+)['\"]"#)
            .expect("JavaScript import regex");
    let mut facts = Vec::new();
    for capture in re.captures_iter(text) {
        let whole = capture.get(0).expect("whole import");
        let line = line_number(text, whole.start());
        let clause = capture[1].trim();
        let specifier: Arc<str> = Arc::from(&capture[2]);
        if let (Some(open), Some(close)) = (clause.find('{'), clause.rfind('}')) {
            for item in clause[open + 1..close].split(',') {
                let item = item.trim().trim_start_matches("type ").trim();
                if item.is_empty() {
                    continue;
                }
                let words = item.split_whitespace().collect::<Vec<_>>();
                let imported = words[0];
                let local = if words.get(1) == Some(&"as") {
                    words.get(2).copied().unwrap_or(imported)
                } else {
                    imported
                };
                facts.push(ImportFact {
                    specifier: specifier.clone(),
                    resolved: false,
                    imported: Some(imported.into()),
                    local: Some(local.into()),
                    line,
                });
            }
            let prefix = clause[..open].trim().trim_end_matches(',').trim();
            if !prefix.is_empty() && !prefix.starts_with('*') {
                facts.push(ImportFact {
                    specifier: specifier.clone(),
                    resolved: false,
                    imported: Some("default".into()),
                    local: Some(prefix.into()),
                    line,
                });
            }
        } else if let Some(namespace) = clause.strip_prefix("* as ") {
            facts.push(ImportFact {
                specifier,
                resolved: false,
                imported: None,
                local: Some(namespace.trim().into()),
                line,
            });
        } else {
            facts.push(ImportFact {
                specifier,
                resolved: false,
                imported: Some("default".into()),
                local: Some(clause.trim().into()),
                line,
            });
        }
    }
    let side_effect = Regex::new(r#"(?m)(?:^|;)\s*import\s*['\"]([^'\"]+)['\"]"#)
        .expect("JavaScript side-effect import regex");
    for capture in side_effect.captures_iter(text) {
        facts.push(ImportFact {
            specifier: Arc::from(&capture[1]),
            resolved: false,
            imported: None,
            local: None,
            line: line_number(
                text,
                capture.get(0).expect("whole side-effect import").start(),
            ),
        });
    }
    let import_require = Regex::new(
        r#"(?m)(?:^|;)\s*import\s+([A-Za-z_$][\w$]*)\s*=\s*require\s*\(\s*['\"]([^'\"]+)['\"]\s*\)"#,
    )
    .expect("TypeScript import-require regex");
    for capture in import_require.captures_iter(text) {
        facts.push(ImportFact {
            specifier: Arc::from(&capture[2]),
            resolved: false,
            imported: None,
            local: Some(capture[1].into()),
            line: line_number(text, capture.get(0).expect("whole import-require").start()),
        });
    }
    for require in [
        Regex::new(
            r#"(?m)(?:^|[;\n])\s*(?:(?:const|let|var)\s+([^=;\n]+?)\s*=\s*)?require\s*\(\s*'([^'\r\n]+)'\s*\)(?:\s*\.\s*([A-Za-z_$][\w$]*))?"#,
        ),
        Regex::new(
            r#"(?m)(?:^|[;\n])\s*(?:(?:const|let|var)\s+([^=;\n]+?)\s*=\s*)?require\s*\(\s*\"([^\"\r\n]+)\"\s*\)(?:\s*\.\s*([A-Za-z_$][\w$]*))?"#,
        ),
    ]
    .map(|regex| regex.expect("CommonJS require regex"))
    {
        for capture in require.captures_iter(text) {
            let line = line_number(
                text,
                capture
                    .get(2)
                    .expect("CommonJS require specifier")
                    .start(),
            );
            let specifier: Arc<str> = Arc::from(&capture[2]);
            let property = capture.get(3).map(|value| value.as_str());
            let Some(binding) = capture.get(1).map(|value| value.as_str().trim()) else {
                facts.push(ImportFact {
                    specifier,
                    resolved: false,
                    imported: None,
                    local: None,
                    line,
                });
                continue;
            };
            if binding.starts_with('{') && binding.ends_with('}') {
                for item in binding[1..binding.len() - 1].split(',') {
                    let mut names = item.trim().split([':', '=']).map(str::trim);
                    let Some(imported) = names.next().filter(|name| !name.is_empty()) else {
                        continue;
                    };
                    let local = names.next().filter(|name| !name.is_empty()).unwrap_or(imported);
                    facts.push(ImportFact {
                        specifier: specifier.clone(),
                        resolved: false,
                        imported: Some(imported.into()),
                        local: Some(local.into()),
                        line,
                    });
                }
            } else if binding
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "_$".contains(character))
            {
                facts.push(ImportFact {
                    specifier,
                    resolved: false,
                    imported: property.map(str::to_owned),
                    local: Some(binding.into()),
                    line,
                });
            }
        }
    }
    facts.sort_by(|left, right| {
        left.line
            .cmp(&right.line)
            .then_with(|| left.specifier.cmp(&right.specifier))
            .then_with(|| left.imported.cmp(&right.imported))
            .then_with(|| left.local.cmp(&right.local))
    });
    facts.dedup_by(|left, right| {
        left.line == right.line
            && left.specifier == right.specifier
            && left.imported == right.imported
            && left.local == right.local
    });
    facts
}

fn parse_reexports(text: &str) -> Vec<ReexportFact> {
    let mut facts = Vec::new();
    let named =
        Regex::new(r#"(?m)^\s*export\s+(?:type\s+)?\{([^}]*)\}\s+from\s+['\"]([^'\"]+)['\"]"#)
            .expect("named re-export regex");
    for capture in named.captures_iter(text) {
        let line = line_number(text, capture.get(0).expect("whole re-export").start());
        let specifier: Arc<str> = Arc::from(&capture[2]);
        for item in capture[1].split(',') {
            let item = item.trim().trim_start_matches("type ").trim();
            if item.is_empty() {
                continue;
            }
            let words = item.split_whitespace().collect::<Vec<_>>();
            let imported = words[0];
            let exported = if words.get(1) == Some(&"as") {
                words.get(2).copied().unwrap_or(imported)
            } else {
                imported
            };
            facts.push(ReexportFact {
                specifier: Arc::clone(&specifier),
                resolved: false,
                imported: Some(imported.into()),
                exported: Some(exported.into()),
                namespace: false,
                star: false,
                line,
            });
        }
    }
    let namespace =
        Regex::new(r#"(?m)^\s*export\s+\*\s+as\s+([A-Za-z_$][\w$]*)\s+from\s+['\"]([^'\"]+)['\"]"#)
            .expect("namespace re-export regex");
    for capture in namespace.captures_iter(text) {
        facts.push(ReexportFact {
            specifier: Arc::from(&capture[2]),
            resolved: false,
            imported: Some("*".into()),
            exported: Some(capture[1].into()),
            namespace: true,
            star: false,
            line: line_number(
                text,
                capture.get(0).expect("whole namespace export").start(),
            ),
        });
    }
    let star = Regex::new(r#"(?m)^\s*export\s+\*\s+from\s+['\"]([^'\"]+)['\"]"#)
        .expect("star re-export regex");
    for capture in star.captures_iter(text) {
        facts.push(ReexportFact {
            specifier: Arc::from(&capture[1]),
            resolved: false,
            imported: None,
            exported: None,
            namespace: false,
            star: true,
            line: line_number(text, capture.get(0).expect("whole star export").start()),
        });
    }
    facts
}

fn parse_direct_exports(text: &str) -> BTreeSet<String> {
    let re = Regex::new(
        r"(?m)^\s*export\s+(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:class|interface|type|function|const|let|var)\s+([A-Za-z_$][\w$]*)",
    )
    .expect("direct export regex");
    re.captures_iter(text)
        .map(|capture| capture[1].to_owned())
        .collect()
}

fn parse_local_export_clauses(text: &str) -> Vec<(String, String)> {
    let re = Regex::new(r"(?m)^\s*export\s+(?:type\s+)?\{([^}]*)\}\s*;?\s*$")
        .expect("local export regex");
    let mut exports = Vec::new();
    for capture in re.captures_iter(text) {
        for item in capture[1].split(',') {
            let item = item.trim().trim_start_matches("type ").trim();
            if item.is_empty() {
                continue;
            }
            let words = item.split_whitespace().collect::<Vec<_>>();
            let local = words[0];
            let exported = if words.get(1) == Some(&"as") {
                words.get(2).copied().unwrap_or(local)
            } else {
                local
            };
            exports.push((local.into(), exported.into()));
        }
    }
    exports
}

fn parse_default_export(text: &str) -> Option<String> {
    let declaration = Regex::new(
        r"(?m)^\s*export\s+default\s+(?:(?:abstract\s+)?class|function)\s+([A-Za-z_$][\w$]*)",
    )
    .expect("default declaration regex");
    if let Some(capture) = declaration.captures(text) {
        return Some(capture[1].into());
    }
    let identifier = Regex::new(r"(?m)^\s*export\s+default\s+([A-Za-z_$][\w$]*)\s*;?\s*$")
        .expect("default identifier regex");
    identifier.captures(text).map(|capture| capture[1].into())
}

fn parse_local_aliases(text: &str) -> BTreeMap<String, String> {
    Regex::new(
        r"(?m)^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*([A-Za-z_$][\w$]*)\s*;?\s*$",
    )
    .expect("module alias regex")
    .captures_iter(text)
    .map(|capture| (capture[1].into(), capture[2].into()))
    .collect()
}

fn parse_dynamic_imports(text: &str) -> Vec<(usize, String)> {
    let mut imports = Vec::new();
    for regex in [
        Regex::new(r#"import\s*\(\s*'([^'\r\n]+)'\s*\)"#),
        Regex::new(r#"import\s*\(\s*\"([^\"\r\n]+)\"\s*\)"#),
        Regex::new(r#"import\s*\(\s*`([^`\r\n]+)`\s*\)"#),
    ]
    .map(|regex| regex.expect("dynamic import regex"))
    {
        imports.extend(regex.captures_iter(text).filter_map(|capture| {
            let specifier = capture.get(1)?.as_str();
            (!specifier.contains("${")).then(|| {
                (
                    line_number(text, capture.get(0).expect("whole dynamic import").start()),
                    specifier.to_owned(),
                )
            })
        }));
    }
    imports.sort();
    imports.dedup();
    imports
}

fn resolve_module_specifier(specifier: &str, importer: &Path, root: &Path) -> Option<String> {
    let importer_dir = importer.parent()?;
    let source_file = importer.strip_prefix(root).ok()?;
    let source_file = normalize_slashes(source_file.to_string_lossy().as_ref());
    let resolved = match classify_es_module_specifier(&source_file, specifier) {
        EsModuleSpecifier::ProjectRelative(logical) => resolve_js_path(&root.join(logical)),
        EsModuleSpecifier::Bare => resolve_tsconfig(specifier, importer_dir)
            .or_else(|| resolve_workspace(specifier, importer_dir)),
        EsModuleSpecifier::Unsafe => None,
    }?;
    let resolved = fs::canonicalize(&resolved).unwrap_or_else(|_| lexical_normalize(&resolved));
    let relative = resolved.strip_prefix(root).ok()?;
    Some(normalize_slashes(relative.to_string_lossy().as_ref()))
}

pub(crate) fn resolve_import_path(
    specifier: &str,
    importer: &Path,
    source_file: &str,
) -> Option<PathBuf> {
    let importer_dir = importer.parent()?;
    match classify_es_module_specifier(source_file, specifier) {
        EsModuleSpecifier::ProjectRelative(logical) if !Path::new(source_file).is_absolute() => {
            let mut root = importer.to_path_buf();
            for _ in Path::new(source_file).components() {
                root.pop();
            }
            resolve_js_path(&root.join(logical))
        }
        EsModuleSpecifier::ProjectRelative(_) => {
            resolve_js_path(&lexical_normalize(&importer_dir.join(specifier)))
        }
        EsModuleSpecifier::Bare => resolve_tsconfig(specifier, importer_dir)
            .or_else(|| resolve_workspace(specifier, importer_dir)),
        EsModuleSpecifier::Unsafe => None,
    }
}

/// Resolve a JavaScript-family module path using Graphify's source-oriented
/// extension and directory-index precedence. Missing paths are returned
/// unchanged so callers can retain an explicit external/phantom reference.
pub fn resolve_js_module_path(candidate: &Path) -> PathBuf {
    let candidate = lexical_normalize(candidate);
    resolve_js_path(&candidate).unwrap_or(candidate)
}

fn resolve_js_path(candidate: &Path) -> Option<PathBuf> {
    let candidate = lexical_normalize(candidate);
    if candidate.is_file() {
        return Some(candidate);
    }
    match candidate.extension().and_then(|value| value.to_str()) {
        Some("js") => {
            let value = candidate.with_extension("ts");
            if value.is_file() {
                return Some(value);
            }
        }
        Some("jsx") => {
            let value = candidate.with_extension("tsx");
            if value.is_file() {
                return Some(value);
            }
        }
        _ => {}
    }
    let name = candidate.file_name()?.to_string_lossy();
    for extension in JS_EXTENSIONS {
        let value = candidate.with_file_name(format!("{name}.{extension}"));
        if value.is_file() {
            return Some(value);
        }
    }
    if candidate.is_dir() {
        for index in JS_INDEX_FILES {
            let value = candidate.join(index);
            if value.is_file() {
                return Some(value);
            }
        }
    }
    None
}

#[derive(Default)]
struct TsConfig {
    aliases: BTreeMap<String, Vec<PathBuf>>,
    base_url: Option<PathBuf>,
}

type MatchedAlias<'a> = ((u8, usize), String, bool, &'a [PathBuf]);

fn resolve_tsconfig(specifier: &str, start: &Path) -> Option<PathBuf> {
    let config = find_config(start)?;
    let parsed = read_tsconfig(&config, &mut BTreeSet::new(), 0);
    let mut best: Option<MatchedAlias<'_>> = None;
    for (pattern, targets) in &parsed.aliases {
        let Some((score, captured, wildcard)) = match_alias(specifier, pattern) else {
            continue;
        };
        if best.as_ref().is_none_or(|current| score < current.0) {
            best = Some((score, captured, wildcard, targets));
        }
    }
    if let Some((_, captured, wildcard, targets)) = best {
        for target in targets {
            let candidate = if wildcard && !captured.is_empty() {
                PathBuf::from(target.to_string_lossy().replacen('*', &captured, 1))
            } else if captured.is_empty() {
                target.clone()
            } else {
                target.join(captured.as_str())
            };
            if let Some(resolved) = resolve_js_path(&lexical_normalize(&candidate)) {
                return Some(resolved);
            }
        }
        return None;
    }
    parsed
        .base_url
        .and_then(|base| resolve_js_path(&lexical_normalize(&base.join(specifier))))
}

fn find_config(start: &Path) -> Option<PathBuf> {
    for directory in start.ancestors() {
        for name in ["tsconfig.json", "jsconfig.json"] {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn read_tsconfig(path: &Path, seen: &mut BTreeSet<PathBuf>, depth: usize) -> TsConfig {
    if depth >= MAX_TSCONFIG_EXTENDS_DEPTH {
        return TsConfig::default();
    }
    let path = fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path));
    if !seen.insert(path.clone()) {
        return TsConfig::default();
    }
    let Some(data) = read_jsonc(&path) else {
        return TsConfig::default();
    };
    let base = path.parent().unwrap_or_else(|| Path::new(""));
    let mut result = TsConfig::default();
    let parents = match data.get("extends") {
        Some(Value::String(value)) => vec![value.as_str()],
        Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    for parent in parents {
        if parent.starts_with('@') {
            continue;
        }
        let mut extended = lexical_normalize(&base.join(parent));
        if extended.extension().is_none() {
            extended.set_extension("json");
        }
        if extended.is_file() {
            let inherited = read_tsconfig(&extended, seen, depth + 1);
            result.aliases.extend(inherited.aliases);
            if inherited.base_url.is_some() {
                result.base_url = inherited.base_url;
            }
        }
    }
    let options = data.get("compilerOptions").and_then(Value::as_object);
    let local_base = options
        .and_then(|options| options.get("baseUrl"))
        .and_then(Value::as_str)
        .map(|value| lexical_normalize(&base.join(value)));
    if local_base.is_some() {
        result.base_url = local_base.clone();
    }
    let paths_base = local_base.unwrap_or_else(|| base.to_path_buf());
    if let Some(paths) = options
        .and_then(|options| options.get("paths"))
        .and_then(Value::as_object)
    {
        for (alias, targets) in paths {
            let values = targets
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| paths_base.join(value))
                .collect::<Vec<_>>();
            if !values.is_empty() {
                result.aliases.insert(alias.clone(), values);
            }
        }
    }
    result
}

fn match_alias(raw: &str, pattern: &str) -> Option<((u8, usize), String, bool)> {
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        if pattern.matches('*').count() != 1 || !raw.starts_with(prefix) || !raw.ends_with(suffix) {
            return None;
        }
        let end = raw.len().checked_sub(suffix.len())?;
        if end < prefix.len() {
            return None;
        }
        return Some((
            (1, usize::MAX - prefix.len()),
            raw[prefix.len()..end].into(),
            true,
        ));
    }
    if raw == pattern {
        return Some(((0, usize::MAX - pattern.len()), String::new(), false));
    }
    let prefix = pattern.trim_end_matches('/');
    raw.strip_prefix(prefix)
        .and_then(|tail| tail.strip_prefix('/'))
        .map(|captured| ((2, usize::MAX - prefix.len()), captured.into(), false))
}

fn resolve_workspace(specifier: &str, start: &Path) -> Option<PathBuf> {
    let root = find_workspace_root(start)?;
    for package in workspace_package_dirs(&root) {
        let Some(data) = read_jsonc(&package.join("package.json")) else {
            continue;
        };
        let Some(name) = data.get("name").and_then(Value::as_str) else {
            continue;
        };
        let subpath = if specifier == name {
            ""
        } else if let Some(value) = specifier.strip_prefix(&format!("{name}/")) {
            value
        } else {
            continue;
        };
        for candidate in package_entry_candidates(&package, &data, subpath) {
            if let Some(resolved) = resolve_js_path(&candidate) {
                return Some(resolved);
            }
        }
    }
    None
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    for directory in start.ancestors() {
        if directory.join("pnpm-workspace.yaml").is_file() {
            return Some(directory.into());
        }
        let package = directory.join("package.json");
        if read_jsonc(&package).is_some_and(|data| data.get("workspaces").is_some()) {
            return Some(directory.into());
        }
    }
    None
}

fn workspace_package_dirs(root: &Path) -> Vec<PathBuf> {
    let patterns = if root.join("pnpm-workspace.yaml").is_file() {
        parse_pnpm_patterns(
            &fs::read_to_string(root.join("pnpm-workspace.yaml")).unwrap_or_default(),
        )
    } else {
        let data = read_jsonc(&root.join("package.json")).unwrap_or(Value::Null);
        match data.get("workspaces") {
            Some(Value::Array(values)) => values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            Some(Value::Object(value)) => value
                .get("packages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    };
    let mut directories = Vec::new();
    for pattern in patterns
        .into_iter()
        .filter(|pattern| !pattern.starts_with('!'))
    {
        if matches!(pattern.as_str(), "." | "./") {
            directories.push(root.to_path_buf());
            continue;
        }
        let Some(star) = pattern.find('*') else {
            directories.push(root.join(pattern));
            continue;
        };
        let prefix = pattern[..star].trim_end_matches('/');
        let suffix = pattern[star + 1..].trim_start_matches('/');
        let parent = root.join(prefix);
        let Ok(entries) = fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten().filter(|entry| entry.path().is_dir()) {
            let candidate = if suffix.is_empty() {
                entry.path()
            } else {
                entry.path().join(suffix)
            };
            if candidate.is_dir() {
                directories.push(candidate);
            }
        }
    }
    directories.sort();
    directories.dedup();
    directories
}

fn parse_pnpm_patterns(text: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut packages = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("packages:") {
            packages = true;
        } else if packages && line.starts_with('-') {
            let value = line[1..].trim().trim_matches(['\'', '"']);
            if !value.is_empty() {
                patterns.push(value.into());
            }
        } else if packages && !raw.starts_with([' ', '\t']) && !line.is_empty() {
            break;
        }
    }
    patterns
}

fn package_entry_candidates(package: &Path, data: &Value, subpath: &str) -> Vec<PathBuf> {
    if !subpath.is_empty() {
        if let Some(exports) = data.get("exports").and_then(Value::as_object) {
            let key = format!("./{subpath}");
            if let Some(target) = exports.get(&key).and_then(resolve_export_target) {
                let candidate = lexical_normalize(&package.join(target));
                if path_contained(&candidate, package) {
                    return vec![candidate];
                }
            }
            for (pattern, value) in exports {
                let Some((prefix, suffix)) = pattern.split_once('*') else {
                    continue;
                };
                if pattern.matches('*').count() != 1
                    || !key.starts_with(prefix)
                    || !key.ends_with(suffix)
                {
                    continue;
                }
                let end = key.len().saturating_sub(suffix.len());
                let captured = &key[prefix.len()..end];
                let Some(target) = resolve_export_target(value) else {
                    continue;
                };
                let candidate = lexical_normalize(&package.join(target.replacen('*', captured, 1)));
                if path_contained(&candidate, package) {
                    return vec![candidate];
                }
            }
        }
        return vec![package.join(subpath)];
    }
    if let Some(exports) = data.get("exports") {
        if let Some(target) = exports.as_str() {
            return vec![package.join(target)];
        }
        if let Some(target) = exports
            .as_object()
            .and_then(|values| values.get("."))
            .and_then(resolve_export_target)
        {
            return vec![package.join(target)];
        }
    }
    let mut candidates = ["svelte", "module", "main", "types"]
        .iter()
        .filter_map(|key| data.get(key).and_then(Value::as_str))
        .map(|value| package.join(value))
        .collect::<Vec<_>>();
    candidates.push(package.join("src/index"));
    candidates.push(package.join("index"));
    candidates
}

fn resolve_export_target(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str() {
        return Some(value.into());
    }
    let object = value.as_object()?;
    for condition in EXPORT_CONDITIONS {
        if let Some(target) = object.get(*condition).and_then(resolve_export_target) {
            return Some(target);
        }
    }
    None
}

fn path_contained(candidate: &Path, package: &Path) -> bool {
    let candidate = fs::canonicalize(candidate).unwrap_or_else(|_| lexical_normalize(candidate));
    let package = fs::canonicalize(package).unwrap_or_else(|_| lexical_normalize(package));
    candidate.starts_with(package)
}

fn read_jsonc(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    graphoxide_core::parse_jsonc(&text).ok()
}

fn module_edge(
    source: &str,
    target: &str,
    relation: &str,
    source_file: &str,
    line: usize,
    context: &str,
) -> Edge {
    let mut extra = BTreeMap::new();
    extra.insert("_src".into(), source.into());
    extra.insert("_tgt".into(), target.into());
    extra.insert("source_location".into(), format!("L{line}").into());
    extra.insert("weight".into(), 1.0.into());
    extra.insert("context".into(), context.into());
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra,
    }
}

fn push_unique_edge(edges: &mut Vec<Edge>, edge: Edge) {
    if edges.iter().any(|existing| {
        existing.true_source() == edge.true_source()
            && existing.true_target() == edge.true_target()
            && existing.relation == edge.relation
            && existing.extra.get("source_location") == edge.extra.get("source_location")
            && existing.extra.get("context") == edge.extra.get("context")
    }) {
        return;
    }
    crate::resolution::push_resolved_edge(edges, edge);
}

fn line_number(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn path_without_extension(value: &str) -> String {
    normalize_slashes(
        Path::new(value)
            .with_extension("")
            .to_string_lossy()
            .as_ref(),
    )
}

fn normalize_slashes(value: &str) -> String {
    value.replace('\\', "/")
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push(component.as_os_str());
                }
            }
            _ => output.push(component.as_os_str()),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{
        classify_es_module_specifier, EsModuleSpecifier, ProjectSnapshot, ProjectSnapshotAdmission,
        ProjectSnapshotError,
    };
    use graphoxide_core::{make_id, Confidence, Edge, Extraction};
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::Path,
    };

    #[test]
    fn mpeg_transport_stream_is_excluded_from_project_snapshot_admission() {
        let mut media = vec![0xff; 5 * 188];
        for packet in 0..5 {
            let offset = packet * 188;
            media[offset..offset + 4].copy_from_slice(&[0x47, 0x40, packet as u8, 0x10]);
        }

        assert!(ProjectSnapshot::needs_file("public/camera/segment.ts"));
        assert!(!ProjectSnapshot::needs_admitted_file(
            "public/camera/segment.ts",
            &media
        ));
        assert!(ProjectSnapshot::needs_admitted_file(
            "src/main.ts",
            b"export const main = true;\n"
        ));
    }

    #[test]
    fn invalidated_target_cleanup_is_path_authoritative_and_rewrites_modules_only() {
        fn edge(source: &str, target: String, relation: &str, target_file: Option<&str>) -> Edge {
            let mut extra = BTreeMap::new();
            if let Some(target_file) = target_file {
                extra.insert("target_file".into(), target_file.into());
            }
            Edge {
                source: source.into(),
                target,
                relation: relation.into(),
                confidence: Confidence::Extracted,
                source_file: "src/main.ts".into(),
                extra,
            }
        }

        let segment_anchor = make_id(&["src/segment"]);
        let mut extractions = vec![Extraction {
            nodes: Vec::new(),
            edges: vec![
                edge(
                    "explicit-module",
                    segment_anchor.clone(),
                    "imports_from",
                    Some("segment.ts"),
                ),
                edge(
                    "symbol-binding",
                    make_id(&["src/segment", "value"]),
                    "imports",
                    Some("segment.ts"),
                ),
                edge("legacy-module", segment_anchor.clone(), "re_exports", None),
                edge(
                    "colliding-anchor",
                    segment_anchor,
                    "imports_from",
                    Some("other.ts"),
                ),
            ],
            hyperedges: Vec::new(),
        }];
        super::invalidate_resolved_targets_for_sources(
            &mut extractions,
            &BTreeSet::from(["src/segment.ts".into()]),
        );

        let edges = &extractions[0].edges;
        assert_eq!(edges.len(), 3, "symbol-level binding must be removed");
        let unresolved = make_id(&["ref", "src/segment"]);
        for source in ["explicit-module", "legacy-module"] {
            let rewritten = edges
                .iter()
                .find(|edge| edge.source == source)
                .expect("module edge remains as unresolved evidence");
            assert_eq!(rewritten.true_target(), unresolved);
            assert!(!rewritten.extra.contains_key("target_file"));
        }
        let collision = edges
            .iter()
            .find(|edge| edge.source == "colliding-anchor")
            .expect("path evidence protects an unrelated colliding anchor");
        assert_eq!(
            collision
                .extra
                .get("target_file")
                .and_then(serde_json::Value::as_str),
            Some("other.ts")
        );
    }

    fn byte_extraction(source_file: &str, source: &[u8]) -> Extraction {
        crate::engine::extract_as_bytes(
            &Path::new("/graphoxide-snapshot-does-not-exist").join(source_file),
            source_file,
            source,
        )
        .expect("extract admitted snapshot bytes")
    }

    fn assert_file_import(extraction: &Extraction, source: &str, target: &str) {
        let source = make_id(&[&Path::new(source).with_extension("").to_string_lossy()]);
        let target = make_id(&[&Path::new(target).with_extension("").to_string_lossy()]);
        assert!(
            extraction.edges.iter().any(|edge| {
                edge.relation == "imports_from"
                    && edge.true_source() == source
                    && edge.true_target() == target
            }),
            "expected snapshot-resolved file import {source} -> {target}; edges: {:?}",
            extraction.edges
        );
    }

    #[test]
    fn snapshot_resolves_relative_sfc_and_tsconfig_aliases_without_source_paths() {
        let mut snapshot = ProjectSnapshot::with_byte_limit(64 * 1024);
        let main = b"import { value } from '@shared/value'; export const use = value;\n";
        let component = b"<template><main /></template>\n<script lang=\"ts\">import { value } from './util'; export const display = value;</script>\n";
        let value = b"export const value = 1;\n";
        let util = b"export const value = 2;\n";
        let tsconfig =
            br#"{"compilerOptions":{"baseUrl":".","paths":{"@shared/*":["src/shared/*"]}}}"#;
        for (path, bytes) in [
            ("src/main.ts", main.as_slice()),
            ("src/component.vue", component.as_slice()),
            ("src/shared/value.ts", value.as_slice()),
            ("src/util.ts", util.as_slice()),
            ("tsconfig.json", tsconfig.as_slice()),
        ] {
            snapshot
                .insert_owned(path.into(), bytes.to_vec())
                .expect("snapshot input fits budget");
        }
        let mut extractions = vec![
            byte_extraction("src/main.ts", main),
            byte_extraction("src/component.vue", component),
            byte_extraction("src/shared/value.ts", value),
            byte_extraction("src/util.ts", util),
        ];

        // These logical paths have never existed on disk. Successful relative,
        // SFC, and tsconfig resolution therefore proves the resolver consumed
        // only the explicit snapshot bytes.
        crate::resolution::resolve_with_snapshot_bounded(
            &mut extractions,
            &snapshot,
            usize::MAX,
            64 * 1024 * 1024,
        )
        .expect("resolve admitted snapshot");

        assert_file_import(&extractions[0], "src/main.ts", "src/shared/value.ts");
        assert_file_import(&extractions[1], "src/component.vue", "src/util.ts");
    }

    #[test]
    fn snapshot_resolves_workspace_package_exports_without_directory_probes() {
        let mut snapshot = ProjectSnapshot::with_byte_limit(64 * 1024);
        let app = b"import { library } from '@scope/library'; export const run = library;\n";
        let library = b"export const library = 1;\n";
        for (path, bytes) in [
            (
                "package.json",
                br#"{"workspaces":["packages/*"]}"#.as_slice(),
            ),
            (
                "packages/library/package.json",
                br#"{"name":"@scope/library","exports":"./src/index.ts"}"#.as_slice(),
            ),
            ("apps/main.ts", app.as_slice()),
            ("packages/library/src/index.ts", library.as_slice()),
        ] {
            snapshot
                .insert_owned(path.into(), bytes.to_vec())
                .expect("snapshot input fits budget");
        }
        let mut extractions = vec![
            byte_extraction("apps/main.ts", app),
            byte_extraction("packages/library/src/index.ts", library),
        ];

        crate::resolution::resolve_with_snapshot_bounded(
            &mut extractions,
            &snapshot,
            usize::MAX,
            64 * 1024 * 1024,
        )
        .expect("resolve admitted snapshot");

        assert_file_import(
            &extractions[0],
            "apps/main.ts",
            "packages/library/src/index.ts",
        );
    }

    #[test]
    fn snapshot_rebuilds_commonjs_and_static_template_imports_safely() {
        let mut snapshot = ProjectSnapshot::with_byte_limit(64 * 1024);
        let app = br#"const worker = require('../worker.js');
const package = require('foo');
const absolute = require('/foo.js');
const drive = require('C:foo.js');
const unc = require('\\server\share\foo.js');
const escape = require('../../foo.js');
export async function load() { return import(`./lazy.js`); }
"#;
        let worker = b"module.exports = { work() {} };\n";
        let lazy = b"export const value = 1;\n";
        let foo = b"export const local = true;\n";
        for (path, bytes) in [
            ("src/app.js", app.as_slice()),
            ("worker.js", worker.as_slice()),
            ("src/lazy.js", lazy.as_slice()),
            ("foo.js", foo.as_slice()),
        ] {
            snapshot
                .insert_owned(path.into(), bytes.to_vec())
                .expect("snapshot input fits budget");
        }
        let mut extractions = vec![
            byte_extraction("src/app.js", app),
            byte_extraction("worker.js", worker),
            byte_extraction("src/lazy.js", lazy),
            byte_extraction("foo.js", foo),
        ];
        crate::resolution::resolve_with_snapshot_bounded(
            &mut extractions,
            &snapshot,
            usize::MAX,
            64 * 1024 * 1024,
        )
        .expect("resolve admitted snapshot");

        assert_file_import(&extractions[0], "src/app.js", "worker.js");
        let local_foo = make_id(&["foo"]);
        let guarded = extractions[0]
            .edges
            .iter()
            .filter(|edge| {
                edge.relation == "imports_from"
                    && matches!(
                        edge.extra
                            .get("source_location")
                            .and_then(|value| value.as_str()),
                        Some("L2" | "L3" | "L4" | "L5" | "L6")
                    )
            })
            .collect::<Vec<_>>();
        assert_eq!(guarded.len(), 5);
        assert!(guarded
            .iter()
            .all(|edge| edge.true_target() != local_foo && edge.true_target().starts_with("ref_")));
        assert!(guarded
            .iter()
            .find(|edge| {
                edge.extra
                    .get("source_location")
                    .and_then(|value| value.as_str())
                    == Some("L2")
            })
            .is_some_and(|edge| edge.true_target() == "ref_foo"));
        assert!(extractions[0].edges.iter().any(|edge| {
            edge.true_target() == "src_lazy"
                && edge.extra.get("deferred").and_then(|value| value.as_bool()) == Some(true)
        }));
    }

    #[test]
    fn same_line_static_and_dynamic_imports_keep_distinct_targets() {
        let mut snapshot = ProjectSnapshot::with_byte_limit(32 * 1024);
        let app = b"import x from './x.js'; const y = import('./y.js');\n";
        let x = b"export default 1;\n";
        let y = b"export default 2;\n";
        for (path, bytes) in [
            ("src/app.js", app.as_slice()),
            ("src/x.js", x.as_slice()),
            ("src/y.js", y.as_slice()),
        ] {
            snapshot
                .insert_owned(path.into(), bytes.to_vec())
                .expect("snapshot input fits budget");
        }
        let mut extractions = vec![
            byte_extraction("src/app.js", app),
            byte_extraction("src/x.js", x),
            byte_extraction("src/y.js", y),
        ];
        crate::resolution::resolve_with_snapshot_bounded(
            &mut extractions,
            &snapshot,
            usize::MAX,
            64 * 1024 * 1024,
        )
        .expect("resolve admitted snapshot");

        let x_id = make_id(&["src/x"]);
        let y_id = make_id(&["src/y"]);
        assert!(extractions[0].edges.iter().any(|edge| {
            edge.relation == "imports_from"
                && edge.true_target() == x_id
                && !edge.extra.contains_key("deferred")
        }));
        assert!(extractions[0].edges.iter().any(|edge| {
            edge.true_target() == y_id
                && edge.extra.get("deferred").and_then(|value| value.as_bool()) == Some(true)
        }));
    }

    #[test]
    fn unsafe_specifier_id_collisions_never_bind_admitted_js_or_sfc_files() {
        let mut snapshot = ProjectSnapshot::with_byte_limit(64 * 1024);
        let sources = [
            (
                "src/static.js",
                b"import value from './aux:worker'; void value;\n".as_slice(),
            ),
            (
                "src/commonjs.cjs",
                b"const value = require('./aux:worker'); void value;\n".as_slice(),
            ),
            (
                "src/dynamic.js",
                b"export const load = () => import('./aux:worker');\n".as_slice(),
            ),
            (
                "src/component.vue",
                b"<script>\nimport value from './aux:worker';\nconst load = () => import('./aux:worker');\nvoid value; void load;\n</script>\n"
                    .as_slice(),
            ),
        ];
        let collision = b"export default 1;\n";
        for (path, bytes) in sources
            .iter()
            .copied()
            .chain([("src/aux_worker.js", collision.as_slice())])
        {
            snapshot
                .insert_owned(path.into(), bytes.to_vec())
                .expect("portable snapshot input fits budget");
        }
        assert_eq!(
            snapshot.insert_owned("src/aux:worker.js".into(), Vec::new()),
            Err(ProjectSnapshotError::InvalidPath(
                "src/aux:worker.js".into()
            ))
        );

        let collision_id = make_id(&["src/aux_worker"]);
        assert_eq!(
            super::raw_dynamic_import_id("src/dynamic.js", "./aux:worker"),
            collision_id,
            "the raw walker target must exercise the intended ID collision"
        );
        let mut extractions = sources
            .iter()
            .map(|(path, bytes)| byte_extraction(path, bytes))
            .chain([byte_extraction("src/aux_worker.js", collision)])
            .collect::<Vec<_>>();
        crate::resolution::resolve_with_snapshot_bounded(
            &mut extractions,
            &snapshot,
            usize::MAX,
            64 * 1024 * 1024,
        )
        .expect("resolve admitted snapshot");

        for ((source_file, _), extraction) in sources.iter().zip(&extractions) {
            let import_edges = extraction
                .edges
                .iter()
                .filter(|edge| matches!(edge.relation.as_str(), "imports_from" | "dynamic_import"))
                .collect::<Vec<_>>();
            assert!(
                !import_edges.is_empty(),
                "expected import evidence for {source_file}; edges: {:?}",
                extraction.edges
            );
            assert!(import_edges.iter().all(|edge| {
                edge.true_target() != collision_id
                    && edge.true_target().starts_with("ref_unsafe_")
                    && !edge.extra.contains_key("target_file")
            }), "unsafe import from {source_file} bound a colliding admitted file: {import_edges:?}");
        }
    }

    #[test]
    fn unsafe_sfc_relative_import_cannot_bind_an_admitted_root_file() {
        let mut snapshot = ProjectSnapshot::with_byte_limit(32 * 1024);
        let component = b"<script>import value from '../../foo.vue';</script>\n";
        let foo = b"<script>export default 1;</script>\n";
        for (path, bytes) in [
            ("components/App.vue", component.as_slice()),
            ("foo.vue", foo.as_slice()),
        ] {
            snapshot
                .insert_owned(path.into(), bytes.to_vec())
                .expect("snapshot input fits budget");
        }
        let mut extractions = vec![
            byte_extraction("components/App.vue", component),
            byte_extraction("foo.vue", foo),
        ];
        crate::resolution::resolve_with_snapshot_bounded(
            &mut extractions,
            &snapshot,
            usize::MAX,
            64 * 1024 * 1024,
        )
        .expect("resolve admitted snapshot");

        let foo_id = make_id(&["foo"]);
        let targets = extractions[0]
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports_from")
            .map(|edge| edge.true_target())
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1);
        assert_ne!(targets[0], foo_id);
        assert!(targets[0].starts_with("ref_unsafe_"));
    }

    #[test]
    fn escaping_relative_specifier_is_classified_unsafe() {
        assert!(matches!(
            classify_es_module_specifier("components/App.vue", "../../foo.vue"),
            EsModuleSpecifier::Unsafe
        ));
        assert_eq!(
            classify_es_module_specifier("src/nested/app.js", "../../worker.js"),
            EsModuleSpecifier::ProjectRelative("worker.js".into())
        );
    }

    #[test]
    fn module_specifier_classification_enforces_portable_project_paths() {
        for (source_file, specifier, expected) in [
            (
                "src/features/main.ts",
                "./worker.ts",
                EsModuleSpecifier::ProjectRelative("src/features/worker.ts".into()),
            ),
            (
                "src/features/main.ts",
                "../shared.ts",
                EsModuleSpecifier::ProjectRelative("src/shared.ts".into()),
            ),
            (
                "src/features/main.ts",
                "../../root.ts",
                EsModuleSpecifier::ProjectRelative("root.ts".into()),
            ),
            ("src/main.ts", "node:fs", EsModuleSpecifier::Bare),
            ("src/main.ts", "@scope/package", EsModuleSpecifier::Bare),
            ("src/main.ts", "package/subpath", EsModuleSpecifier::Bare),
        ] {
            assert_eq!(
                classify_es_module_specifier(source_file, specifier),
                expected,
                "source={source_file:?}, specifier={specifier:?}"
            );
        }

        for specifier in [
            "../../../outside.ts",
            "./C:worker.ts",
            "./dir/node:worker.ts",
            "C:worker.ts",
            "C:/worker.ts",
            r"C:\worker.ts",
            "./con.ts",
            "./AUX.component.ts",
            "./nul",
            "./COM1.js",
            "./LPT9.log",
            "./worker.",
            "./worker ",
            "/worker.ts",
            "//server/share/worker.ts",
            r"\\server\share\worker.ts",
            "./dir//worker.ts",
            "./work*er.ts",
            r".\worker.ts",
        ] {
            assert_eq!(
                classify_es_module_specifier("src/features/main.ts", specifier),
                EsModuleSpecifier::Unsafe,
                "accepted unsafe specifier {specifier:?}"
            );
        }
    }

    #[test]
    fn absolute_compatibility_source_still_rejects_filesystem_root_underflow() {
        let current = std::env::current_dir().expect("current directory");
        let root = current
            .ancestors()
            .last()
            .expect("an absolute current directory has a root");
        let source = root.join("source.ts");
        let worker = root.join("worker.ts").to_string_lossy().replace('\\', "/");

        assert_eq!(
            classify_es_module_specifier(source.to_string_lossy().as_ref(), "./worker.ts"),
            EsModuleSpecifier::ProjectRelative(worker)
        );
        assert_eq!(
            classify_es_module_specifier(source.to_string_lossy().as_ref(), "../outside.ts"),
            EsModuleSpecifier::Unsafe
        );
    }

    #[test]
    fn snapshot_admission_rejects_nonportable_logical_paths() {
        let mut snapshot = ProjectSnapshot::with_byte_limit(64 * 1024);
        for source_file in [
            "",
            ".",
            "../escape.ts",
            "src/../../escape.ts",
            "src/..",
            "/root.ts",
            "//server/share/root.ts",
            r"\root.ts",
            r"\\server\share\root.ts",
            "C:drive.ts",
            "C:/drive.ts",
            r"C:\drive.ts",
            "src/aux:worker.ts",
            "src/CON.ts",
            "src/nul.js",
            "src/COM1.ts",
            "src/LPT9.ts",
            "src/worker.",
            "src/worker ",
            "src//worker.ts",
            r"src\\worker.ts",
            "src/work*er.ts",
        ] {
            assert_eq!(
                snapshot.insert_owned(source_file.into(), Vec::new()),
                Err(ProjectSnapshotError::InvalidPath(source_file.into())),
                "admitted unsafe logical path {source_file:?}"
            );
        }

        snapshot
            .insert_owned(r"src\portable.ts".into(), b"export {};\n".to_vec())
            .expect("a portable alternate separator is normalized at admission");
        assert!(snapshot.contains_file("src/portable.ts"));
    }

    #[test]
    fn snapshot_rejects_over_budget_or_escaping_inputs_without_partial_view() {
        let exact = ProjectSnapshot::root_retained_bytes()
            + ProjectSnapshot::admission_bytes("src/ok.ts", 3);
        let mut snapshot = ProjectSnapshot::with_byte_limit(exact);
        snapshot
            .insert_owned("src/ok.ts".into(), vec![1, 2, 3])
            .expect("exact budget fits");
        assert_eq!(
            snapshot.insert_owned("src/next.ts".into(), vec![4]),
            Err(ProjectSnapshotError::ExceedsBudget { byte_limit: exact })
        );
        assert_eq!(
            snapshot.insert_owned("../escape.ts".into(), Vec::new()),
            Err(ProjectSnapshotError::InvalidPath("../escape.ts".into()))
        );
    }

    #[test]
    fn snapshot_budget_charges_retained_allocation_capacity() {
        let mut pooled = Vec::with_capacity(4 * 1024);
        pooled.push(b'{');
        let retained = ProjectSnapshot::admission_bytes("tiny.json", pooled.capacity());

        let admission = ProjectSnapshotAdmission::new(retained - 1);
        assert!(!admission.try_reserve(retained));

        let mut snapshot = ProjectSnapshot::with_byte_limit(retained - 1);
        assert_eq!(
            snapshot.insert_owned("tiny.json".into(), pooled),
            Err(ProjectSnapshotError::ExceedsBudget {
                byte_limit: retained - 1
            })
        );
    }

    #[test]
    fn snapshot_many_tiny_files_charge_reserved_btree_slots() {
        use std::mem::size_of;

        let paths = (0..128)
            .map(|index| format!("src/tiny-{index}.ts"))
            .collect::<Vec<_>>();
        let legacy_total = paths.iter().fold(0usize, |bytes, path| {
            bytes
                .saturating_add(path.len())
                .saturating_add(size_of::<String>())
                .saturating_add(size_of::<Vec<u8>>())
                .saturating_add(3 * size_of::<usize>())
        });
        let structural_total = ProjectSnapshot::root_retained_bytes().saturating_add(
            paths
                .len()
                .saturating_mul(3usize.saturating_mul(ProjectSnapshot::map_slot_bytes())),
        );
        let new_total = paths
            .iter()
            .map(|path| path.len())
            .sum::<usize>()
            .saturating_add(structural_total);
        assert!(new_total > legacy_total);
        let budget = legacy_total.saturating_add((new_total - legacy_total) / 2);
        let mut snapshot = ProjectSnapshot::with_byte_limit(budget);

        let admitted = paths
            .into_iter()
            .take_while(|path| snapshot.insert_owned(path.clone(), Vec::new()).is_ok())
            .count();
        assert!(admitted < 128, "reserved B-tree slots must be admitted");
    }

    #[test]
    fn snapshot_skips_metadata_and_context_when_there_is_no_fresh_js_module() {
        let metadata = vec![b' '; 256 * 1024];
        let context_source = b"export const existing = 1;\n";
        let limit = ProjectSnapshot::admission_bytes("package-lock.json", metadata.capacity())
            + ProjectSnapshot::admission_bytes("context.ts", context_source.len())
            + 1024;
        let mut snapshot = ProjectSnapshot::with_byte_limit(limit);
        snapshot
            .insert_owned("package-lock.json".into(), metadata)
            .expect("large unrelated metadata fits snapshot budget");
        snapshot
            .insert_owned("context.ts".into(), context_source.to_vec())
            .expect("context source fits snapshot budget");
        let mut context_only = vec![byte_extraction("context.ts", context_source)];

        super::resolve_with_snapshot_prefix(&mut context_only, &snapshot, 0, 1)
            .expect("deletion-only/no-change resolution does not inspect baseline metadata");
    }

    #[test]
    fn tiny_fresh_module_rejects_cpu_below_fixed_regex_scratch() {
        let source = b"export {};\n";
        let mut snapshot = ProjectSnapshot::with_byte_limit(64 * 1024);
        snapshot
            .insert_owned("main.ts".into(), source.to_vec())
            .expect("tiny source fits snapshot");
        let mut extractions = vec![byte_extraction("main.ts", source)];

        let error = super::resolve_with_snapshot_prefix(
            &mut extractions,
            &snapshot,
            1,
            super::JS_RESOLVER_FIXED_SCRATCH_BYTES - 1,
        )
        .expect_err("fixed regex scratch must be admitted before parsing");
        assert!(error.to_string().contains("CPU-arena"));
    }

    #[test]
    fn definition_index_reserves_btree_nodes_before_collecting_facts() {
        use std::mem::size_of;

        let source_file = "main.ts";
        let source = b"export {};\n";
        let mut snapshot = ProjectSnapshot::with_byte_limit(64 * 1024);
        snapshot
            .insert_owned(source_file.into(), source.to_vec())
            .expect("tiny source fits snapshot");
        let mut extraction = byte_extraction(source_file, source);
        let file_node = extraction.nodes[0].clone();
        for index in 0..512 {
            let mut definition = file_node.clone();
            definition.id = format!("main_definition_{index}");
            definition.label = format!("definition_{index}");
            extraction.nodes.push(definition);
        }

        let file_id = make_id(&["main"]);
        let legacy_definition_bytes = extraction
            .nodes
            .iter()
            .filter(|node| node.source_file == source_file && node.id != file_id)
            .fold(
                source_file
                    .len()
                    .saturating_mul(5)
                    .saturating_add(size_of::<super::ModuleFacts>())
                    .saturating_add(2usize.saturating_mul(size_of::<String>()))
                    .saturating_add(6usize.saturating_mul(size_of::<usize>())),
                |bytes, node| {
                    bytes
                        .saturating_add(node.label.len())
                        .saturating_add(node.id.len())
                        .saturating_add(size_of::<(String, String)>())
                        .saturating_add(3usize.saturating_mul(size_of::<usize>()))
                },
            );
        let reserved_definition_bytes =
            super::definition_index_admission_bytes(&extraction, source_file);
        assert!(reserved_definition_bytes > legacy_definition_bytes);
        let source_working = super::module_source_working_set_bytes(source.len())
            .expect("tiny source working-set charge");
        let legacy_required = snapshot
            .retained_bytes()
            .saturating_add(super::JS_RESOLVER_FIXED_SCRATCH_BYTES)
            .saturating_add(source_working)
            .saturating_add(legacy_definition_bytes);
        let budget = legacy_required
            .saturating_add((reserved_definition_bytes - legacy_definition_bytes) / 2);

        let error = super::resolve_with_snapshot_prefix(
            std::slice::from_mut(&mut extraction),
            &snapshot,
            1,
            budget,
        )
        .expect_err("definition B-tree storage must be admitted before collection");
        assert!(error.to_string().contains("while parsing main.ts"));
    }

    #[test]
    fn unicode_source_path_normalization_is_admitted_before_collecting_facts() {
        use std::mem::size_of;

        // U+FDFA has a long NFKC expansion, so the old 5x path heuristic does
        // not cover the simultaneous joined/normalized/case-fold allocations.
        let source_file = format!("src/{}/main.ts", "\u{fdfa}".repeat(64));
        let source = b"export {};\n";
        let mut snapshot = ProjectSnapshot::with_byte_limit(256 * 1024);
        snapshot
            .insert_owned(source_file.clone(), source.to_vec())
            .expect("portable Unicode path fits snapshot");
        let mut extraction = byte_extraction(&source_file, source);

        let matching_count = extraction
            .nodes
            .iter()
            .filter(|node| node.source_file == source_file)
            .count();
        let payload = extraction
            .nodes
            .iter()
            .filter(|node| node.source_file == source_file)
            .fold(0usize, |bytes, node| {
                bytes
                    .saturating_add(node.label.len())
                    .saturating_add(node.id.len())
            });
        let fixed_without_path = size_of::<super::ModuleFacts>()
            .saturating_add(2usize.saturating_mul(size_of::<String>()))
            .saturating_add(6usize.saturating_mul(size_of::<usize>()));
        let structural = super::btree_map_storage_bytes::<String, String>(matching_count);
        let legacy_required = snapshot
            .retained_bytes()
            .saturating_add(super::JS_RESOLVER_FIXED_SCRATCH_BYTES)
            .saturating_add(
                super::module_source_working_set_bytes(source.len())
                    .expect("tiny source working-set charge"),
            )
            .saturating_add(source_file.len().saturating_mul(5))
            .saturating_add(fixed_without_path)
            .saturating_add(structural)
            .saturating_add(payload);
        let admitted_required = snapshot
            .retained_bytes()
            .saturating_add(super::JS_RESOLVER_FIXED_SCRATCH_BYTES)
            .saturating_add(
                super::module_source_working_set_bytes(source.len())
                    .expect("tiny source working-set charge"),
            )
            .saturating_add(super::definition_index_admission_bytes(
                &extraction,
                &source_file,
            ));
        assert!(admitted_required > legacy_required);
        let budget = legacy_required.saturating_add((admitted_required - legacy_required) / 2);

        let error = super::resolve_with_snapshot_prefix(
            std::slice::from_mut(&mut extraction),
            &snapshot,
            1,
            budget,
        )
        .expect_err("Unicode path normalization must be admitted before collection");
        assert!(error
            .to_string()
            .contains(&format!("while parsing {source_file}")));
    }

    #[test]
    fn unicode_specifier_normalization_is_admitted_before_capacity_measurement() {
        let source_file = "main.ts";
        let specifier = format!("./{}.ts", "\u{fdfa}".repeat(32));
        let source = format!("import '{specifier}';\n");
        let mut snapshot = ProjectSnapshot::with_byte_limit(256 * 1024);
        snapshot
            .insert_owned(source_file.into(), source.as_bytes().to_vec())
            .expect("Unicode import source fits snapshot");
        let mut extraction = byte_extraction(source_file, source.as_bytes());
        let facts =
            super::collect_module_facts(0, &extraction, source_file, source.clone(), &|_, _| None);
        let normalization_scratch =
            super::derived_id_normalization_scratch_bytes(&facts, snapshot.max_path_len())
                .expect("derived-ID normalization charge");
        assert!(normalization_scratch > 0);
        let legacy_required = snapshot
            .retained_bytes()
            .saturating_add(super::JS_RESOLVER_FIXED_SCRATCH_BYTES)
            .saturating_add(
                super::module_source_working_set_bytes(source.len())
                    .expect("Unicode source working-set charge"),
            )
            .saturating_add(super::definition_index_admission_bytes(
                &extraction,
                source_file,
            ));
        let budget = legacy_required.saturating_add(normalization_scratch / 2);

        let error = super::resolve_with_snapshot_prefix(
            std::slice::from_mut(&mut extraction),
            &snapshot,
            1,
            budget,
        )
        .expect_err("specifier normalization must precede exact ID measurement");
        assert!(error
            .to_string()
            .contains("while preparing derived IDs for main.ts"));
    }

    #[test]
    fn exact_non_javascript_target_participates_in_derived_id_admission() {
        let source_file = "main.ts";
        let source = b"import value from '@data';\n";
        let long_json_path = format!("data/{}.json", "metadata-segment".repeat(24));
        let tsconfig = format!(
            r#"{{"compilerOptions":{{"baseUrl":".","paths":{{"@data":["{long_json_path}"]}}}}}}"#
        );
        let mut snapshot = ProjectSnapshot::with_byte_limit(2 * 1024 * 1024);
        for (path, bytes) in [
            (source_file, source.as_slice()),
            ("tsconfig.json", tsconfig.as_bytes()),
            (&long_json_path, br#"{"value":1}"#.as_slice()),
        ] {
            snapshot
                .insert_owned(path.into(), bytes.to_vec())
                .expect("resolver source or metadata fits snapshot");
        }
        assert_eq!(
            snapshot.resolve_module_specifier("@data", source_file),
            Some(long_json_path.clone()),
            "the exact admitted JSON path must exercise non-JS target resolution"
        );
        let mut extraction = byte_extraction(source_file, source);
        let facts = super::collect_module_facts(
            0,
            &extraction,
            source_file,
            String::from_utf8(source.to_vec()).expect("UTF-8 source"),
            &|_, _| None,
        );
        let js_only_scratch =
            super::derived_id_normalization_scratch_bytes(&facts, source_file.len())
                .expect("old JS-only normalization charge");
        let all_target_scratch =
            super::derived_id_normalization_scratch_bytes(&facts, snapshot.max_path_len())
                .expect("all-target normalization charge");
        assert!(all_target_scratch > js_only_scratch);
        let provisional = snapshot
            .retained_bytes()
            .saturating_add(super::JS_RESOLVER_FIXED_SCRATCH_BYTES)
            .saturating_add(
                super::module_source_working_set_bytes(source.len())
                    .expect("tiny source working-set charge"),
            )
            .saturating_add(super::definition_index_admission_bytes(
                &extraction,
                source_file,
            ));
        let budget = provisional
            .saturating_add(js_only_scratch)
            .saturating_add((all_target_scratch - js_only_scratch) / 2);

        let error = super::resolve_with_snapshot_prefix(
            std::slice::from_mut(&mut extraction),
            &snapshot,
            1,
            budget,
        )
        .expect_err("long exact JSON target must be admitted before measurement");
        assert!(error
            .to_string()
            .contains("while preparing derived IDs for main.ts"));
    }

    #[test]
    fn final_phase_includes_global_id_normalization_scratch() {
        let module_count = 96usize;
        let importer_path = "unicode-importer.ts";
        let specifier = format!("./{}.ts", "\u{fdfa}".repeat(24));
        let importer_source = format!("import '{specifier}';\n");
        let mut snapshot = ProjectSnapshot::with_byte_limit(2 * 1024 * 1024);
        snapshot
            .insert_owned(importer_path.into(), importer_source.as_bytes().to_vec())
            .expect("Unicode importer fits snapshot");
        let mut extractions = vec![byte_extraction(importer_path, importer_source.as_bytes())];
        for index in 1..module_count {
            let path = format!("module-{index}.ts");
            let source = format!("export const value{index} = {index};\n");
            snapshot
                .insert_owned(path.clone(), source.as_bytes().to_vec())
                .expect("tiny module fits snapshot");
            extractions.push(byte_extraction(&path, source.as_bytes()));
        }

        let max_path_len = snapshot.max_path_len();
        let mut retained_modules = 0usize;
        let mut global_normalization_scratch = 0usize;
        let mut max_provisional = 0usize;
        let mut resolution_bounds = super::ExportResolutionStringBounds {
            stored: max_path_len,
            ..super::ExportResolutionStringBounds::default()
        };
        for (index, extraction) in extractions.iter().enumerate() {
            let path = &extraction.nodes[0].source_file;
            let source = snapshot
                .resolution_source(path)
                .expect("module source remains admitted");
            let decoded_len = source.len();
            let facts = super::collect_module_facts(index, extraction, path, source, &|_, _| None);
            let normalization = super::derived_id_normalization_scratch_bytes(&facts, max_path_len)
                .expect("bounded derived-ID normalization");
            let source_working = super::module_source_working_set_bytes(decoded_len)
                .expect("module working-set charge")
                .saturating_add(super::definition_index_admission_bytes(extraction, path));
            max_provisional = max_provisional.max(
                snapshot
                    .retained_bytes()
                    .saturating_add(super::JS_RESOLVER_FIXED_SCRATCH_BYTES)
                    .saturating_add(retained_modules)
                    .saturating_add(
                        super::btree_map_storage_bytes::<String, super::ModuleFacts>(index),
                    )
                    .saturating_add(source_working)
                    .saturating_add(normalization),
            );
            retained_modules = retained_modules
                .saturating_add(super::module_facts_admission_bytes(&facts, max_path_len));
            global_normalization_scratch = global_normalization_scratch.max(normalization);
            resolution_bounds.include(super::module_export_resolution_string_bounds(&facts));
        }
        let collection_storage =
            super::btree_map_storage_bytes::<String, super::ModuleFacts>(module_count)
                .saturating_add(super::btree_set_storage_bytes::<String>(module_count));
        let legacy_final = snapshot
            .retained_bytes()
            .saturating_add(super::JS_RESOLVER_FIXED_SCRATCH_BYTES)
            .saturating_add(retained_modules)
            .saturating_add(collection_storage)
            .saturating_add(
                super::export_resolution_scratch_bytes(resolution_bounds)
                    .expect("bounded export-resolution scratch"),
            );
        let corrected_final = legacy_final.saturating_add(global_normalization_scratch);
        let legacy_ceiling = legacy_final.max(max_provisional);
        assert!(legacy_ceiling < corrected_final);
        let budget = legacy_ceiling.saturating_add((corrected_final - legacy_ceiling) / 2);

        let error =
            super::resolve_with_snapshot_prefix(&mut extractions, &snapshot, module_count, budget)
                .expect_err("global normalization scratch must coexist with all module state");
        assert!(error.to_string().contains("ID-normalization scratch"));
    }

    #[test]
    fn provisional_phase_includes_the_retained_modules_map_structure() {
        let prior_count = 96usize;
        let mut snapshot = ProjectSnapshot::with_byte_limit(2 * 1024 * 1024);
        let mut extractions = Vec::new();
        for index in 0..prior_count {
            let path = format!("tiny-{index}.ts");
            let source = format!("export const value{index} = {index};\n");
            snapshot
                .insert_owned(path.clone(), source.as_bytes().to_vec())
                .expect("tiny module fits snapshot");
            extractions.push(byte_extraction(&path, source.as_bytes()));
        }
        let final_path = "final-large.ts";
        let final_source = format!("/*{}*/\n", "x".repeat(64 * 1024));
        snapshot
            .insert_owned(final_path.into(), final_source.as_bytes().to_vec())
            .expect("large final source fits snapshot");
        extractions.push(byte_extraction(final_path, final_source.as_bytes()));

        let max_path_len = snapshot.max_path_len();
        let prior_module_bytes = extractions[..prior_count]
            .iter()
            .enumerate()
            .map(|(index, extraction)| {
                let path = &extraction.nodes[0].source_file;
                let source = snapshot
                    .resolution_source(path)
                    .expect("prior source remains admitted");
                let facts =
                    super::collect_module_facts(index, extraction, path, source, &|_, _| None);
                super::module_facts_admission_bytes(&facts, max_path_len)
            })
            .sum::<usize>();
        let final_working = super::module_source_working_set_bytes(final_source.len())
            .expect("large source working-set charge")
            .saturating_add(super::definition_index_admission_bytes(
                &extractions[prior_count],
                final_path,
            ));
        let legacy_required = snapshot
            .retained_bytes()
            .saturating_add(super::JS_RESOLVER_FIXED_SCRATCH_BYTES)
            .saturating_add(prior_module_bytes)
            .saturating_add(final_working);
        let prior_map_storage =
            super::btree_map_storage_bytes::<String, super::ModuleFacts>(prior_count);
        let corrected_required = legacy_required.saturating_add(prior_map_storage);
        let budget = legacy_required.saturating_add(prior_map_storage / 2);
        assert!(legacy_required < budget && budget < corrected_required);

        let error = super::resolve_with_snapshot_prefix(
            &mut extractions,
            &snapshot,
            prior_count + 1,
            budget,
        )
        .expect_err("prior modules-map storage must remain admitted during the next parse");
        assert!(error.to_string().contains("while parsing final-large.ts"));
    }

    #[test]
    fn bare_dynamic_import_admits_metadata_peak_before_resolution() {
        let source = b"void import('@scope/package');\n";
        let metadata = vec![b' '; 32 * 1024];
        let mut snapshot = ProjectSnapshot::with_byte_limit(128 * 1024);
        snapshot
            .insert_owned("main.ts".into(), source.to_vec())
            .expect("source fits snapshot");
        snapshot
            .insert_owned("package-lock.json".into(), metadata)
            .expect("metadata fits snapshot");
        let mut extractions = vec![byte_extraction("main.ts", source)];

        let error =
            super::resolve_with_snapshot_prefix(&mut extractions, &snapshot, 1, 1024 * 1024)
                .expect_err("bare dynamic imports must admit resolver metadata");
        assert!(error.to_string().contains("metadata peak"));
    }

    #[test]
    fn snapshot_tsconfig_extends_chain_stops_at_the_deterministic_depth_limit() {
        let mut snapshot = ProjectSnapshot::with_byte_limit(2 * 1024 * 1024);
        snapshot
            .insert_owned("main.ts".into(), b"import '@deep';\n".to_vec())
            .expect("main source fits snapshot");
        snapshot
            .insert_owned("target.ts".into(), b"export const value = 1;\n".to_vec())
            .expect("target source fits snapshot");
        for depth in 0..=super::MAX_TSCONFIG_EXTENDS_DEPTH {
            let path = if depth == 0 {
                "tsconfig.json".to_owned()
            } else {
                format!("config-{depth}.json")
            };
            let contents = if depth == super::MAX_TSCONFIG_EXTENDS_DEPTH {
                br#"{"compilerOptions":{"baseUrl":".","paths":{"@deep":["target"]}}}"#.to_vec()
            } else {
                format!(r#"{{"extends":"config-{}.json"}}"#, depth + 1).into_bytes()
            };
            snapshot
                .insert_owned(path, contents)
                .expect("config chain fits snapshot");
        }

        assert_eq!(snapshot.resolve_module_specifier("@deep", "main.ts"), None);
    }

    #[test]
    fn dense_named_imports_share_long_raw_and_resolved_specifiers() {
        let bindings = (0..512)
            .map(|index| format!("value{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let long_specifier = format!("@scope/{}", "nested".repeat(256));
        let source = format!("import {{ {bindings} }} from '{long_specifier}';\n");
        let extraction = byte_extraction("main.ts", source.as_bytes());
        let facts = super::collect_module_facts(0, &extraction, "main.ts", source, &|_, _| {
            Some("target.ts".to_owned())
        });

        assert_eq!(facts.imports.len(), 512);
        assert!(facts.imports.iter().all(|fact| fact.resolved));
        assert!(facts
            .imports
            .windows(2)
            .all(|pair| std::sync::Arc::ptr_eq(&pair[0].specifier, &pair[1].specifier)));
    }

    #[test]
    fn dense_named_reexports_share_long_raw_and_resolved_specifiers() {
        let bindings = (0..512)
            .map(|index| format!("value{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let long_specifier = format!("@scope/{}", "nested".repeat(256));
        let source = format!("export {{ {bindings} }} from '{long_specifier}';\n");
        let extraction = byte_extraction("barrel.ts", source.as_bytes());
        let mut facts =
            super::collect_module_facts(0, &extraction, "barrel.ts", source, &|_, _| {
                Some("target.ts".to_owned())
            });

        assert_eq!(facts.reexports.len(), 512);
        assert!(facts.reexports.iter().all(|fact| fact.resolved));
        assert!(facts
            .reexports
            .windows(2)
            .all(|pair| std::sync::Arc::ptr_eq(&pair[0].specifier, &pair[1].specifier)));
        super::materialize_export_bindings(
            &mut facts,
            &std::collections::BTreeSet::from(["target.ts".to_owned()]),
        );
        assert_eq!(facts.exports.len(), 512);
    }

    #[test]
    fn changed_importer_resolves_the_correct_duplicate_namespace_from_context() {
        let initial_main = b"import { two } from './barrel'; export const use = two;\n";
        let changed_main = b"import { two } from './barrel'; export const changed = two;\n";
        let barrel = b"export * as one from './target';\nexport * as two from './target';\n";
        let target = b"export const value = 1;\n";
        let mut snapshot = ProjectSnapshot::with_byte_limit(256 * 1024);
        for (path, source) in [
            ("main.ts", initial_main.as_slice()),
            ("barrel.ts", barrel.as_slice()),
            ("target.ts", target.as_slice()),
        ] {
            snapshot
                .insert_owned(path.into(), source.to_vec())
                .expect("namespace baseline fits snapshot");
        }
        let mut baseline = vec![
            byte_extraction("main.ts", initial_main),
            byte_extraction("barrel.ts", barrel),
            byte_extraction("target.ts", target),
        ];
        super::resolve_with_snapshot_prefix(&mut baseline, &snapshot, 3, 64 * 1024 * 1024)
            .expect("resolve duplicate namespace baseline");
        snapshot
            .insert_owned("main.ts".into(), changed_main.to_vec())
            .expect("replace changed importer snapshot");
        let mut fresh = vec![byte_extraction("main.ts", changed_main)];
        let context = &baseline[1..];

        super::resolve_with_snapshot_partitions(&mut fresh, context, &snapshot, 64 * 1024 * 1024)
            .expect("resolve changed importer against namespace context");
        let expected = make_id(&["barrel", "two"]);
        let wrong = make_id(&["barrel", "one"]);
        assert!(fresh[0]
            .edges
            .iter()
            .any(|edge| edge.relation == "imports" && edge.true_target() == expected));
        assert!(fresh[0]
            .edges
            .iter()
            .all(|edge| edge.relation != "imports" || edge.true_target() != wrong));
    }

    #[test]
    fn export_resolution_depth_boundary_is_deterministic() {
        fn chain(module_count: usize) -> super::ExportResolution {
            let mut modules = std::collections::BTreeMap::new();
            for index in 0..module_count {
                let path = format!("m{index}.ts");
                let source = if index + 1 == module_count {
                    "export const value = 1;\n".to_owned()
                } else {
                    format!("export {{ value }} from './m{}';\n", index + 1)
                };
                let extraction = byte_extraction(&path, source.as_bytes());
                let facts = super::collect_module_facts(
                    index,
                    &extraction,
                    &path,
                    source,
                    &|specifier, _| {
                        specifier
                            .strip_prefix("./")
                            .map(|target| format!("{target}.ts"))
                    },
                );
                modules.insert(path, facts);
            }
            let sources = modules.keys().cloned().collect();
            for facts in modules.values_mut() {
                super::materialize_export_bindings(facts, &sources);
            }
            super::resolve_export(&modules, "m0.ts", "value")
        }

        assert!(matches!(
            chain(super::MAX_EXPORT_RESOLUTION_DEPTH),
            super::ExportResolution::Resolved(_)
        ));
        assert_eq!(
            chain(super::MAX_EXPORT_RESOLUTION_DEPTH + 1),
            super::ExportResolution::Ambiguous
        );
    }

    #[test]
    fn export_resolution_branch_work_exhaustion_fails_closed() {
        let root_source = (0..=super::MAX_EXPORT_RESOLUTION_STEPS)
            .map(|index| format!("export * from './leaf-{index}';\n"))
            .collect::<String>();
        let root_extraction = byte_extraction("root.ts", root_source.as_bytes());
        let mut modules = std::collections::BTreeMap::new();
        modules.insert(
            "root.ts".to_owned(),
            super::collect_module_facts(
                0,
                &root_extraction,
                "root.ts",
                root_source,
                &|specifier, _| {
                    specifier
                        .strip_prefix("./")
                        .map(|target| format!("{target}.ts"))
                },
            ),
        );
        for index in 0..=super::MAX_EXPORT_RESOLUTION_STEPS {
            let path = format!("leaf-{index}.ts");
            let source = "export const other = 1;\n";
            let extraction = byte_extraction(&path, source.as_bytes());
            modules.insert(
                path.clone(),
                super::collect_module_facts(
                    index + 1,
                    &extraction,
                    &path,
                    source.to_owned(),
                    &|_, _| None,
                ),
            );
        }
        let sources = modules.keys().cloned().collect();
        for facts in modules.values_mut() {
            super::materialize_export_bindings(facts, &sources);
        }

        assert_eq!(
            super::resolve_export(&modules, "root.ts", "missing"),
            super::ExportResolution::Ambiguous
        );
    }

    #[test]
    fn export_resolution_scratch_is_admitted_independently() {
        let source = b"export const value = 1;\n";
        let mut snapshot = ProjectSnapshot::with_byte_limit(64 * 1024);
        snapshot
            .insert_owned("main.ts".into(), source.to_vec())
            .expect("tiny module fits snapshot");
        let mut extractions = vec![byte_extraction("main.ts", source)];
        let facts = super::collect_module_facts(
            0,
            &extractions[0],
            "main.ts",
            String::from_utf8(source.to_vec()).expect("UTF-8 source"),
            &|_, _| None,
        );
        let module_bytes = super::module_facts_admission_bytes(&facts, "main.ts".len());
        let collection_bytes = super::btree_map_storage_bytes::<String, super::ModuleFacts>(1)
            .saturating_add(super::btree_set_storage_bytes::<String>(1));
        let scratch = super::export_resolution_scratch_bytes(
            super::module_export_resolution_string_bounds(&facts),
        )
        .expect("bounded export scratch");
        let without_scratch = snapshot
            .retained_bytes()
            .saturating_add(super::JS_RESOLVER_FIXED_SCRATCH_BYTES)
            .saturating_add(module_bytes)
            .saturating_add(collection_bytes);
        let budget = without_scratch.saturating_add(scratch / 2);

        let error = super::resolve_with_snapshot_prefix(&mut extractions, &snapshot, 1, budget)
            .expect_err("export-resolution scratch must be admitted");
        assert!(error.to_string().contains("export-resolution scratch"));
    }

    #[test]
    fn sparse_multi_module_project_uses_measured_aggregate_cpu_charge() {
        let mut snapshot = ProjectSnapshot::with_byte_limit(2 * 1024 * 1024);
        let mut extractions = Vec::new();
        for index in 0..48 {
            let path = format!("src/module-{index}.ts");
            let source = format!(
                "export const value{index} = {index};\n/*{}*/\n",
                "comment filler ".repeat(1024)
            );
            snapshot
                .insert_owned(path.clone(), source.as_bytes().to_vec())
                .expect("sparse source fits aggregate snapshot");
            extractions.push(byte_extraction(&path, source.as_bytes()));
        }

        super::resolve_with_snapshot_prefix(&mut extractions, &snapshot, 48, 16 * 1024 * 1024)
            .expect("measured sparse facts fit despite a large aggregate decoded corpus");
    }

    #[test]
    fn long_sparse_module_paths_charge_the_second_membership_key_copy() {
        let mut snapshot = ProjectSnapshot::with_byte_limit(4 * 1024 * 1024);
        let mut extractions = Vec::new();
        for index in 0..256 {
            let path = format!(
                "{}/module-{index}.ts",
                "deep-segment/".repeat(24).trim_end_matches('/')
            );
            let source = format!("export const value{index} = {index};\n");
            snapshot
                .insert_owned(path.clone(), source.as_bytes().to_vec())
                .expect("long sparse path fits snapshot");
            extractions.push(byte_extraction(&path, source.as_bytes()));
        }
        let max_path_len = snapshot.max_path_len();
        let mut old_one_key_modules = 0usize;
        let mut second_key_charge = 0usize;
        let mut resolution_bounds = super::ExportResolutionStringBounds {
            stored: max_path_len,
            ..super::ExportResolutionStringBounds::default()
        };
        for extraction in &extractions {
            let path = extraction.nodes[0].source_file.clone();
            let source = snapshot
                .resolution_source(&path)
                .expect("snapshot source remains admitted");
            let facts = super::collect_module_facts(0, extraction, &path, source, &|_, _| None);
            let (base, one_key) = super::module_facts_admission_components(&facts, max_path_len);
            assert_eq!(
                super::module_facts_admission_bytes(&facts, max_path_len),
                base.saturating_add(one_key.saturating_mul(2))
            );
            old_one_key_modules = old_one_key_modules
                .saturating_add(base)
                .saturating_add(one_key);
            second_key_charge = second_key_charge.saturating_add(one_key);
            resolution_bounds.include(super::module_export_resolution_string_bounds(&facts));
        }
        let module_collection_storage =
            super::btree_map_storage_bytes::<String, super::ModuleFacts>(256)
                .saturating_add(super::btree_set_storage_bytes::<String>(256));
        let old_one_key_requirement = snapshot
            .retained_bytes()
            .saturating_add(super::JS_RESOLVER_FIXED_SCRATCH_BYTES)
            .saturating_add(old_one_key_modules)
            .saturating_add(module_collection_storage)
            .saturating_add(
                super::export_resolution_scratch_bytes(resolution_bounds)
                    .expect("bounded export scratch"),
            );
        let budget = old_one_key_requirement.saturating_add(second_key_charge / 2);

        let error = super::resolve_with_snapshot_prefix(&mut extractions, &snapshot, 256, budget)
            .expect_err("the simultaneous module membership-key copy must be admitted");
        assert!(error.to_string().contains("CPU-arena"));
    }
}
