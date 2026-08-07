//! Deterministic, side-effect-free reporting for the file discovery boundary.
//!
//! This module reports what the current registry and scan policy declare. It
//! does not invoke extractors, mutate graph state, or materialize sidecars.

use crate::{
    detect::{
        self, is_ignored_with_cache, is_noise_dir, load_ignore_patterns, DetectOptions,
        IgnorePattern,
    },
    format_registry::{format_registry, FormatCapability, FormatSpec},
};
use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
};

const MAX_WALK_ERRORS: usize = 1_024;

/// Filesystem and ignore-policy controls for a coverage audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageOptions {
    pub follow_symlinks: bool,
    pub google_workspace: bool,
    pub honor_gitignore: bool,
    pub extra_excludes: Vec<String>,
    pub output_dir: Option<PathBuf>,
}

impl Default for CoverageOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            google_workspace: false,
            honor_gitignore: true,
            extra_excludes: Vec::new(),
            output_dir: None,
        }
    }
}

impl CoverageOptions {
    /// Match the detector's default of honoring Git ignore rules.
    #[must_use]
    pub fn detector_defaults() -> Self {
        Self {
            honor_gitignore: true,
            ..Self::default()
        }
    }
}

/// One terminal outcome for an in-scope logical file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStatus {
    Covered,
    InventoryOnly,
    Unsupported,
    ExcludedSensitive,
    ExcludedPolicy,
    Unreadable,
}

impl CoverageStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Covered => "covered",
            Self::InventoryOnly => "inventory_only",
            Self::Unsupported => "unsupported",
            Self::ExcludedSensitive => "excluded_sensitive",
            Self::ExcludedPolicy => "excluded_policy",
            Self::Unreadable => "unreadable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageFile {
    pub path: String,
    pub status: CoverageStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_capability: Option<FormatCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageBoundaryKind {
    Ignored,
    PrunedNoise,
}

impl CoverageBoundaryKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ignored => "ignored",
            Self::PrunedNoise => "pruned_noise",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CoverageBoundary {
    pub path: String,
    pub kind: CoverageBoundaryKind,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageOperation {
    ReadDirectory,
    ReadDirectoryEntry,
    InspectFileType,
}

impl CoverageOperation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadDirectory => "read_directory",
            Self::ReadDirectoryEntry => "read_directory_entry",
            Self::InspectFileType => "inspect_file_type",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CoverageDiagnostic {
    pub path: String,
    pub operation: CoverageOperation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageSummary {
    pub total_files: usize,
    pub covered: usize,
    pub inventory_only: usize,
    pub unsupported: usize,
    pub excluded_sensitive: usize,
    pub excluded_policy: usize,
    pub unreadable: usize,
    pub ignored_boundaries: usize,
    pub pruned_boundaries: usize,
    pub walk_errors: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageReport {
    /// Coverage paths are rooted here; this deliberately never leaks the
    /// machine-specific absolute scan path.
    pub root: String,
    pub schema_version: u32,
    /// Whether traversal completed without an unreadable in-scope file.
    pub complete: bool,
    pub files: Vec<CoverageFile>,
    pub boundaries: Vec<CoverageBoundary>,
    pub walk_errors: Vec<CoverageDiagnostic>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub walk_errors_truncated: usize,
    pub summary: CoverageSummary,
}

impl CoverageReport {
    /// Strict mode fails only when the scan is incomplete. Unsupported and
    /// inventory-only files are truthful terminal outcomes, not scan failures.
    #[must_use]
    pub fn strict_failure_count(&self) -> usize {
        self.summary
            .unreadable
            .saturating_add(self.summary.walk_errors)
    }
}

const fn is_zero(value: &usize) -> bool {
    *value == 0
}

struct AuditWalker {
    root: PathBuf,
    configured_output: PathBuf,
    options: CoverageOptions,
    patterns: Vec<IgnorePattern>,
    ignore_cache: HashMap<PathBuf, bool>,
    active_targets: HashSet<PathBuf>,
    seen_physical: HashMap<PathBuf, (usize, bool)>,
    files: Vec<CoverageFile>,
    boundaries: Vec<CoverageBoundary>,
    walk_errors: Vec<CoverageDiagnostic>,
    total_walk_errors: usize,
}

impl AuditWalker {
    fn walk(&mut self, directory: &Path, memory_tree: bool) {
        let target = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
        if !self.active_targets.insert(target.clone()) {
            self.boundary(directory, CoverageBoundaryKind::Ignored, "symlink_cycle");
            return;
        }
        if !memory_tree && directory != self.root {
            self.patterns.extend(detect::load_dir_ignore(
                directory,
                self.options.honor_gitignore,
            ));
        }
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(_) => {
                self.record_error(directory, CoverageOperation::ReadDirectory);
                self.active_targets.remove(&target);
                return;
            }
        };
        let mut collected = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => collected.push(entry),
                Err(_) => self.record_error(directory, CoverageOperation::ReadDirectoryEntry),
            }
        }
        collected.sort_by_key(fs::DirEntry::file_name);
        for entry in collected {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => {
                    self.record_error(&path, CoverageOperation::InspectFileType);
                    continue;
                }
            };
            if kind.is_dir() {
                self.visit_directory(&path, &name, false, memory_tree);
            } else if kind.is_symlink() {
                if path.is_dir() {
                    self.visit_directory(&path, &name, true, memory_tree);
                } else {
                    self.visit_symlink_file(&path, memory_tree);
                }
            } else if kind.is_file() {
                self.visit_regular_file(&path, None, memory_tree);
            }
        }
        self.active_targets.remove(&target);
    }

    fn visit_directory(&mut self, path: &Path, name: &str, symlink: bool, memory_tree: bool) {
        if name.ends_with('!') {
            self.boundary(
                path,
                CoverageBoundaryKind::Ignored,
                "reserved_virtual_member_name",
            );
            return;
        }
        if symlink && !self.options.follow_symlinks {
            self.boundary(path, CoverageBoundaryKind::Ignored, "symlink_not_followed");
            return;
        }
        if symlink && !fs::canonicalize(path).is_ok_and(|target| target.starts_with(&self.root)) {
            self.boundary(
                path,
                CoverageBoundaryKind::Ignored,
                "symlink_target_outside_root",
            );
            return;
        }
        if !memory_tree {
            let configured = path == self.configured_output
                || fs::canonicalize(path)
                    .ok()
                    .zip(fs::canonicalize(&self.configured_output).ok())
                    .is_some_and(|(left, right)| left == right);
            if configured {
                self.boundary(
                    path,
                    CoverageBoundaryKind::PrunedNoise,
                    "configured_output_except_memory",
                );
                return;
            }
            if is_noise_dir(name, path.parent()) {
                self.boundary(path, CoverageBoundaryKind::PrunedNoise, "noise_directory");
                return;
            }
            if is_ignored_with_cache(path, &self.root, &self.patterns, &mut self.ignore_cache) {
                self.boundary(path, CoverageBoundaryKind::Ignored, "ignore_rule");
                return;
            }
        }
        self.walk(path, memory_tree);
    }

    fn visit_symlink_file(&mut self, path: &Path, memory_tree: bool) {
        if !memory_tree
            && is_ignored_with_cache(path, &self.root, &self.patterns, &mut self.ignore_cache)
        {
            self.boundary(path, CoverageBoundaryKind::Ignored, "ignore_rule");
            return;
        }
        if !self.options.follow_symlinks {
            self.boundary(path, CoverageBoundaryKind::Ignored, "symlink_not_followed");
            return;
        }
        let Ok(physical) = fs::canonicalize(path) else {
            self.boundary(path, CoverageBoundaryKind::Ignored, "broken_symlink");
            return;
        };
        if !physical.starts_with(&self.root) {
            self.boundary(
                path,
                CoverageBoundaryKind::Ignored,
                "symlink_target_outside_root",
            );
            return;
        }
        if detect::is_sensitive_path_only(&physical) {
            self.boundary(
                path,
                CoverageBoundaryKind::Ignored,
                "sensitive_symlink_target",
            );
            return;
        }
        self.visit_regular_file(path, Some(physical), memory_tree);
    }

    fn visit_regular_file(&mut self, path: &Path, physical: Option<PathBuf>, memory_tree: bool) {
        if !memory_tree
            && is_ignored_with_cache(path, &self.root, &self.patterns, &mut self.ignore_cache)
        {
            self.boundary(path, CoverageBoundaryKind::Ignored, "ignore_rule");
            return;
        }
        if has_non_unicode_component(&self.root, path) {
            self.terminal(
                path,
                CoverageStatus::ExcludedPolicy,
                None,
                "non_unicode_path",
            );
            return;
        }
        if detect::is_sensitive_path_only(path) {
            self.terminal(
                path,
                CoverageStatus::ExcludedSensitive,
                None,
                "sensitive_path_policy",
            );
            return;
        }
        let declared = format_registry().find_by_path(path);
        if declared.is_some_and(|spec| spec.id.as_str() == "google-workspace-shortcut")
            && !self.options.google_workspace
        {
            self.terminal(
                path,
                CoverageStatus::ExcludedPolicy,
                declared,
                "google_workspace_disabled",
            );
            return;
        }
        if detect::is_policy_excluded_file(path) {
            self.terminal(
                path,
                CoverageStatus::ExcludedPolicy,
                declared,
                "legacy_scan_policy",
            );
            return;
        }
        let physical = match physical.or_else(|| fs::canonicalize(path).ok()) {
            Some(physical) => physical,
            None => {
                self.terminal(
                    path,
                    CoverageStatus::Unreadable,
                    declared,
                    "source_unreadable",
                );
                return;
            }
        };
        if !physical.starts_with(&self.root) {
            self.terminal(
                path,
                CoverageStatus::ExcludedSensitive,
                None,
                "sensitive_source_binding",
            );
            return;
        }
        if has_non_unicode_component(&self.root, &physical) {
            self.terminal(
                path,
                CoverageStatus::ExcludedPolicy,
                None,
                "non_unicode_source_binding",
            );
            return;
        }
        if detect::is_sensitive_path_only(&physical) {
            self.terminal(
                path,
                CoverageStatus::ExcludedSensitive,
                None,
                "sensitive_source_binding",
            );
            return;
        }
        if detect::open_source_nofollow(&physical).is_err() {
            self.terminal(
                path,
                CoverageStatus::Unreadable,
                declared,
                "source_unreadable",
            );
            return;
        }
        let is_alias = physical != path;
        let replacement =
            if let Some((index, existing_alias)) = self.seen_physical.get(&physical).copied() {
                if !existing_alias || is_alias {
                    return;
                }
                Some(index)
            } else {
                None
            };
        let spec = declared.or_else(|| {
            (path.extension().is_none() && detect::has_code_shebang(&physical))
                .then(|| format_registry().find_by_id("source-code"))
                .flatten()
        });
        let file = match spec {
            Some(spec) => self.registered_file(path, spec),
            None => {
                self.terminal_file(path, CoverageStatus::Unsupported, None, "unregistered_path")
            }
        };
        if let Some(index) = replacement {
            self.files[index] = file;
            self.seen_physical.insert(physical, (index, false));
        } else {
            let index = self.files.len();
            self.files.push(file);
            self.seen_physical.insert(physical, (index, is_alias));
        }
    }

    fn registered_file(&self, path: &Path, spec: &FormatSpec) -> CoverageFile {
        let status = if spec.capability == FormatCapability::InventoryOnly {
            CoverageStatus::InventoryOnly
        } else {
            CoverageStatus::Covered
        };
        CoverageFile {
            path: report_path(&self.root, path),
            status,
            format_id: Some(spec.id.as_str().to_owned()),
            declared_capability: Some(spec.capability),
            reason: None,
        }
    }

    fn terminal(
        &mut self,
        path: &Path,
        status: CoverageStatus,
        spec: Option<&FormatSpec>,
        reason: &str,
    ) {
        let file = self.terminal_file(path, status, spec, reason);
        self.files.push(file);
    }

    fn terminal_file(
        &self,
        path: &Path,
        status: CoverageStatus,
        spec: Option<&FormatSpec>,
        reason: &str,
    ) -> CoverageFile {
        CoverageFile {
            path: report_path(&self.root, path),
            status,
            format_id: spec.map(|spec| spec.id.as_str().to_owned()),
            declared_capability: spec.map(|spec| spec.capability),
            reason: Some(reason.to_owned()),
        }
    }

    fn boundary(&mut self, path: &Path, kind: CoverageBoundaryKind, reason: &str) {
        self.boundaries.push(CoverageBoundary {
            path: report_path(&self.root, path),
            kind,
            reason: reason.to_owned(),
        });
    }

    fn record_error(&mut self, path: &Path, operation: CoverageOperation) {
        self.total_walk_errors = self.total_walk_errors.saturating_add(1);
        if self.walk_errors.len() < MAX_WALK_ERRORS {
            self.walk_errors.push(CoverageDiagnostic {
                path: report_path(&self.root, path),
                operation,
            });
        }
    }
}

/// Audit every regular file admitted by the current scan boundary.
pub fn audit_coverage(root: &Path, options: &CoverageOptions) -> anyhow::Result<CoverageReport> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve coverage root {}", root.display()))?;
    if !fs::metadata(&root).is_ok_and(|metadata| metadata.is_dir()) {
        bail!("coverage root is not a directory: {}", root.display());
    }
    let mut patterns = load_ignore_patterns(&root, options.honor_gitignore);
    patterns.extend(options.extra_excludes.iter().filter_map(|raw| {
        detect::parse_ignore_line(raw).map(|pattern| IgnorePattern {
            anchor: root.clone(),
            pattern,
        })
    }));
    let detect_options = DetectOptions {
        follow_symlinks: options.follow_symlinks,
        convert_office_sidecars: false,
        extra_excludes: options.extra_excludes.clone(),
        output_dir: options.output_dir.clone(),
        honor_gitignore: options.honor_gitignore,
        ..DetectOptions::default()
    };
    let configured_output = detect::output_dir(&root, &detect_options);
    let mut walker = AuditWalker {
        root,
        configured_output,
        options: options.clone(),
        patterns,
        ignore_cache: HashMap::new(),
        active_targets: HashSet::new(),
        seen_physical: HashMap::new(),
        files: Vec::new(),
        boundaries: Vec::new(),
        walk_errors: Vec::new(),
        total_walk_errors: 0,
    };
    let root = walker.root.clone();
    walker.walk(&root, false);
    let memory = walker.configured_output.join("memory");
    if memory.is_dir() && fs::canonicalize(&memory).is_ok_and(|path| path.starts_with(&walker.root))
    {
        walker.walk(&memory, true);
    }
    walker
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
    walker.files.dedup_by(|left, right| left.path == right.path);
    walker.boundaries.sort();
    walker.boundaries.dedup();
    walker.walk_errors.sort();
    walker.walk_errors.dedup();
    let walk_errors_truncated = walker
        .total_walk_errors
        .saturating_sub(walker.walk_errors.len());

    let mut summary = CoverageSummary {
        total_files: walker.files.len(),
        ignored_boundaries: walker
            .boundaries
            .iter()
            .filter(|boundary| boundary.kind == CoverageBoundaryKind::Ignored)
            .count(),
        pruned_boundaries: walker
            .boundaries
            .iter()
            .filter(|boundary| boundary.kind == CoverageBoundaryKind::PrunedNoise)
            .count(),
        walk_errors: walker.total_walk_errors,
        ..CoverageSummary::default()
    };
    for file in &walker.files {
        match file.status {
            CoverageStatus::Covered => summary.covered += 1,
            CoverageStatus::InventoryOnly => summary.inventory_only += 1,
            CoverageStatus::Unsupported => summary.unsupported += 1,
            CoverageStatus::ExcludedSensitive => summary.excluded_sensitive += 1,
            CoverageStatus::ExcludedPolicy => summary.excluded_policy += 1,
            CoverageStatus::Unreadable => summary.unreadable += 1,
        }
    }
    Ok(CoverageReport {
        root: ".".to_owned(),
        schema_version: 1,
        complete: summary.unreadable == 0 && summary.walk_errors == 0,
        files: walker.files,
        boundaries: walker.boundaries,
        walk_errors: walker.walk_errors,
        walk_errors_truncated,
        summary,
    })
}

fn report_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    if relative.as_os_str().is_empty() {
        return ".".to_owned();
    }
    relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(encode_component(value)),
            Component::ParentDir => Some("..".to_owned()),
            Component::CurDir => None,
            Component::RootDir | Component::Prefix(_) => Some("%2F".to_owned()),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn has_non_unicode_component(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .any(|component| matches!(component, Component::Normal(value) if value.to_str().is_none()))
}

fn encode_valid_component(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character == '%' || character == '\\' || character.is_control() {
            let mut bytes = [0_u8; 4];
            for byte in character.encode_utf8(&mut bytes).as_bytes() {
                output.push_str(&format!("%{byte:02X}"));
            }
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(unix)]
fn encode_component(value: &OsStr) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    if let Some(value) = value.to_str() {
        return encode_valid_component(value);
    }
    let mut output = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_graphic() && !matches!(*byte, b'%' | b'\\') {
            output.push(char::from(*byte));
        } else if *byte == b' ' {
            output.push(' ');
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

#[cfg(windows)]
fn encode_component(value: &OsStr) -> String {
    use std::os::windows::ffi::OsStrExt as _;
    if let Some(value) = value.to_str() {
        return encode_valid_component(value);
    }
    value
        .encode_wide()
        .map(|unit| format!("%u{unit:04X}"))
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn encode_component(value: &OsStr) -> String {
    encode_valid_component(&value.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_retention_is_bounded_and_omissions_remain_truthful_after_dedup() {
        let root = tempfile::tempdir().expect("temporary root");
        let root = fs::canonicalize(root.path()).expect("canonical root");
        let mut walker = AuditWalker {
            configured_output: root.join("graphoxide-out"),
            root: root.clone(),
            options: CoverageOptions::default(),
            patterns: Vec::new(),
            ignore_cache: HashMap::new(),
            active_targets: HashSet::new(),
            seen_physical: HashMap::new(),
            files: Vec::new(),
            boundaries: Vec::new(),
            walk_errors: Vec::new(),
            total_walk_errors: 0,
        };
        for _ in 0..MAX_WALK_ERRORS + 6 {
            walker.record_error(&root, CoverageOperation::ReadDirectory);
        }
        assert_eq!(walker.walk_errors.len(), MAX_WALK_ERRORS);
        assert_eq!(walker.total_walk_errors, MAX_WALK_ERRORS + 6);

        walker.walk_errors.sort();
        walker.walk_errors.dedup();
        let omitted = walker
            .total_walk_errors
            .saturating_sub(walker.walk_errors.len());
        assert_eq!(walker.walk_errors.len(), 1);
        assert_eq!(omitted, MAX_WALK_ERRORS + 5);
        assert_eq!(walker.walk_errors[0].path, ".");
        assert_eq!(
            walker.walk_errors[0].operation,
            CoverageOperation::ReadDirectory
        );
    }
}
