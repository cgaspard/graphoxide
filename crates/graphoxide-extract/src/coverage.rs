//! Deterministic, side-effect-free reporting for the file discovery boundary.
//!
//! This module reports what the current registry and scan policy declare. It
//! does not invoke extractors, mutate graph state, or materialize sidecars.

use crate::{
    detect::{
        self, is_ignored_with_cache, is_noise_dir, load_extra_ignore_patterns,
        load_ignore_patterns_bounded, DetectOptions, IgnorePattern, MAX_IGNORE_PATTERNS,
        MAX_IGNORE_RETAINED_BYTES,
    },
    format_registry::{format_registry, FormatCapability, FormatSpec},
};
use anyhow::{bail, Context as _};
use graphoxide_index_runtime::RuntimeCancellation;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
};

/// Maximum number of per-file outcomes retained in one coverage report.
///
/// The audit records the first omitted outcome and then stops deterministic
/// traversal, keeping both the retained state and reported counts truthful.
pub const MAX_RETAINED_COVERAGE_FILES: usize = 100_000;

/// Maximum number of ignored or pruned traversal boundaries retained in one
/// coverage report.
pub const MAX_RETAINED_COVERAGE_BOUNDARIES: usize = 20_000;

/// Maximum number of memoized ignore-policy decisions. Audits continue with
/// uncached policy evaluation after this limit.
pub const MAX_COVERAGE_IGNORE_CACHE_ENTRIES: usize = 120_000;

/// Maximum number of directory entries admitted across one deterministic
/// coverage traversal. A directory that would exceed the remaining budget is
/// not processed at all, so filesystem enumeration order cannot select which
/// of its children appear in the report.
pub const MAX_COVERAGE_DIRECTORY_ENTRIES: usize = 200_000;

const MAX_WALK_ERRORS: usize = 1_024;

/// Filesystem and ignore-policy controls for a coverage audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageOptions {
    pub follow_symlinks: bool,
    pub google_workspace: bool,
    pub code_only: bool,
    pub honor_gitignore: bool,
    pub extra_excludes: Vec<String>,
    pub output_dir: Option<PathBuf>,
}

impl Default for CoverageOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            google_workspace: false,
            code_only: false,
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

impl From<&DetectOptions> for CoverageOptions {
    fn from(options: &DetectOptions) -> Self {
        Self {
            follow_symlinks: options.follow_symlinks,
            google_workspace: options.google_workspace,
            code_only: false,
            honor_gitignore: options.honor_gitignore,
            extra_excludes: options.extra_excludes.clone(),
            output_dir: options.output_dir.clone(),
        }
    }
}

/// The exact accepted graph artifact associated with a coverage report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageGraphAssociation {
    pub path: String,
    pub sha256: String,
}

impl CoverageGraphAssociation {
    /// Create an association using a portable relative artifact path and a
    /// lowercase hexadecimal SHA-256 digest.
    pub fn new(path: impl AsRef<Path>, sha256: impl Into<String>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path.is_absolute() {
            bail!("coverage graph path must be a non-empty relative path");
        }
        let mut portable = Vec::new();
        for component in path.components() {
            let Component::Normal(component) = component else {
                bail!("coverage graph path must contain only relative normal components");
            };
            let component = component
                .to_str()
                .context("coverage graph path must be valid UTF-8")?;
            if component.is_empty() {
                bail!("coverage graph path components must not be empty");
            }
            portable.push(component);
        }
        if portable.is_empty() {
            bail!("coverage graph path must be a non-empty relative path");
        }
        let sha256 = sha256.into();
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("coverage graph SHA-256 must be 64 lowercase hexadecimal characters");
        }
        Ok(Self {
            path: portable.join("/"),
            sha256,
        })
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
    /// Whether traversal completed without unreadable inputs, walk errors, or
    /// retained-report truncation.
    pub complete: bool,
    /// Accepted graph bytes this report describes. Standalone audits omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<CoverageGraphAssociation>,
    pub files: Vec<CoverageFile>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub files_truncated: usize,
    pub boundaries: Vec<CoverageBoundary>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub boundaries_truncated: usize,
    /// Number of directory enumerations abandoned because their complete,
    /// sortable entry set exceeded the traversal budget.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub directory_walks_truncated: usize,
    /// Ignore-policy sources rejected intact after exceeding a byte or pattern
    /// ceiling. Their partial rule prefixes are never applied.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ignore_sources_truncated: usize,
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
            .saturating_add(self.files_truncated)
            .saturating_add(self.boundaries_truncated)
            .saturating_add(self.directory_walks_truncated)
            .saturating_add(self.ignore_sources_truncated)
    }

    /// Associate this coverage report with the exact graph artifact it
    /// describes.
    pub fn associate_graph(
        &mut self,
        path: impl AsRef<Path>,
        sha256: impl Into<String>,
    ) -> anyhow::Result<&mut Self> {
        self.graph = Some(CoverageGraphAssociation::new(path, sha256)?);
        Ok(self)
    }
}

const fn is_zero(value: &usize) -> bool {
    *value == 0
}

struct AuditWalker<'a> {
    root: PathBuf,
    configured_output: PathBuf,
    options: CoverageOptions,
    cancellation: &'a RuntimeCancellation,
    patterns: Vec<IgnorePattern>,
    patterns_retained_bytes: usize,
    ignore_cache: HashMap<PathBuf, bool>,
    active_targets: HashSet<PathBuf>,
    seen_physical: HashMap<PathBuf, (usize, bool)>,
    files: Vec<CoverageFile>,
    boundaries: Vec<CoverageBoundary>,
    walk_errors: Vec<CoverageDiagnostic>,
    total_walk_errors: usize,
    max_files: usize,
    max_boundaries: usize,
    max_ignore_cache_entries: usize,
    max_directory_entries: usize,
    visited_directory_entries: usize,
    files_truncated: usize,
    boundaries_truncated: usize,
    omitted_file_summary: CoverageSummary,
    omitted_ignored_boundaries: usize,
    omitted_pruned_boundaries: usize,
    directory_walks_truncated: usize,
    ignore_sources_truncated: usize,
    traversal_stopped: bool,
}

impl AuditWalker<'_> {
    fn check_cancellation(&self) -> anyhow::Result<()> {
        if self.cancellation.is_cancelled() {
            bail!("coverage audit cancelled");
        }
        Ok(())
    }

    fn walk(&mut self, directory: &Path, memory_tree: bool) -> anyhow::Result<()> {
        if self.traversal_stopped {
            return Ok(());
        }
        self.check_cancellation()?;
        let target = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
        if !self.active_targets.insert(target.clone()) {
            self.boundary(directory, CoverageBoundaryKind::Ignored, "symlink_cycle");
            return Ok(());
        }
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(_) => {
                self.record_error(directory, CoverageOperation::ReadDirectory);
                self.active_targets.remove(&target);
                return Ok(());
            }
        };
        if !memory_tree && directory != self.root {
            let remaining = MAX_IGNORE_PATTERNS.saturating_sub(self.patterns.len());
            let remaining_bytes =
                MAX_IGNORE_RETAINED_BYTES.saturating_sub(self.patterns_retained_bytes);
            let mut loaded = detect::load_dir_ignore(
                directory,
                self.options.honor_gitignore,
                remaining,
                remaining_bytes,
            );
            self.patterns.append(&mut loaded.patterns);
            self.patterns_retained_bytes = self
                .patterns_retained_bytes
                .saturating_add(loaded.retained_bytes);
            self.ignore_sources_truncated = self
                .ignore_sources_truncated
                .saturating_add(loaded.truncated_sources);
            if loaded.truncated_sources > 0 {
                self.boundary(
                    directory,
                    CoverageBoundaryKind::Ignored,
                    "ignore_policy_incomplete",
                );
                self.active_targets.remove(&target);
                return Ok(());
            }
        }
        let mut collected = Vec::new();
        let mut enumerated = 0usize;
        let mut read_entry_errors = 0usize;
        let remaining_entries = self
            .max_directory_entries
            .saturating_sub(self.visited_directory_entries);
        for entry in entries {
            self.check_cancellation()?;
            if enumerated >= remaining_entries {
                self.directory_walks_truncated = self.directory_walks_truncated.saturating_add(1);
                self.traversal_stopped = true;
                self.active_targets.remove(&target);
                return Ok(());
            }
            enumerated = enumerated.saturating_add(1);
            match entry {
                Ok(entry) => collected.push(entry),
                Err(_) => read_entry_errors = read_entry_errors.saturating_add(1),
            }
        }
        self.visited_directory_entries = self.visited_directory_entries.saturating_add(enumerated);
        for _ in 0..read_entry_errors {
            self.record_error(directory, CoverageOperation::ReadDirectoryEntry);
        }
        collected.sort_by_key(fs::DirEntry::file_name);
        for entry in collected {
            if self.traversal_stopped {
                break;
            }
            self.check_cancellation()?;
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
                self.visit_directory(&path, &name, false, memory_tree)?;
            } else if kind.is_symlink() {
                if path.is_dir() {
                    self.visit_directory(&path, &name, true, memory_tree)?;
                } else {
                    self.visit_symlink_file(&path, memory_tree)?;
                }
            } else if kind.is_file() {
                self.visit_regular_file(&path, None, memory_tree)?;
            }
        }
        self.active_targets.remove(&target);
        Ok(())
    }

    fn visit_directory(
        &mut self,
        path: &Path,
        name: &str,
        symlink: bool,
        memory_tree: bool,
    ) -> anyhow::Result<()> {
        if name.ends_with('!') {
            self.boundary(
                path,
                CoverageBoundaryKind::Ignored,
                "reserved_virtual_member_name",
            );
            return Ok(());
        }
        if symlink && !self.options.follow_symlinks {
            self.boundary(path, CoverageBoundaryKind::Ignored, "symlink_not_followed");
            return Ok(());
        }
        if has_non_unicode_component(&self.root, path) {
            self.boundary(path, CoverageBoundaryKind::Ignored, "non_unicode_path");
            return Ok(());
        }
        if detect::is_sensitive_directory(path) {
            self.boundary(path, CoverageBoundaryKind::Ignored, "sensitive_directory");
            return Ok(());
        }
        if symlink {
            let Ok(target) = fs::canonicalize(path) else {
                self.boundary(path, CoverageBoundaryKind::Ignored, "broken_symlink");
                return Ok(());
            };
            if !target.starts_with(&self.root) {
                self.boundary(
                    path,
                    CoverageBoundaryKind::Ignored,
                    "symlink_target_outside_root",
                );
                return Ok(());
            }
            if has_non_unicode_component(&self.root, &target) {
                self.boundary(
                    path,
                    CoverageBoundaryKind::Ignored,
                    "non_unicode_source_binding",
                );
                return Ok(());
            }
            if detect::is_sensitive_directory(&target) {
                self.boundary(
                    path,
                    CoverageBoundaryKind::Ignored,
                    "sensitive_symlink_target",
                );
                return Ok(());
            }
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
                return Ok(());
            }
            if is_noise_dir(name, path.parent()) {
                self.boundary(path, CoverageBoundaryKind::PrunedNoise, "noise_directory");
                return Ok(());
            }
            if self.is_ignored(path) {
                self.boundary(path, CoverageBoundaryKind::Ignored, "ignore_rule");
                return Ok(());
            }
        }
        self.walk(path, memory_tree)
    }

    fn visit_symlink_file(&mut self, path: &Path, memory_tree: bool) -> anyhow::Result<()> {
        if !memory_tree && self.is_ignored(path) {
            self.boundary(path, CoverageBoundaryKind::Ignored, "ignore_rule");
            return Ok(());
        }
        if !self.options.follow_symlinks {
            self.boundary(path, CoverageBoundaryKind::Ignored, "symlink_not_followed");
            return Ok(());
        }
        let Ok(physical) = fs::canonicalize(path) else {
            self.boundary(path, CoverageBoundaryKind::Ignored, "broken_symlink");
            return Ok(());
        };
        if !physical.starts_with(&self.root) {
            self.boundary(
                path,
                CoverageBoundaryKind::Ignored,
                "symlink_target_outside_root",
            );
            return Ok(());
        }
        if detect::is_sensitive_path_only(&physical) {
            self.boundary(
                path,
                CoverageBoundaryKind::Ignored,
                "sensitive_symlink_target",
            );
            return Ok(());
        }
        self.visit_regular_file(path, Some(physical), memory_tree)
    }

    fn visit_regular_file(
        &mut self,
        path: &Path,
        physical: Option<PathBuf>,
        memory_tree: bool,
    ) -> anyhow::Result<()> {
        if self.traversal_stopped {
            return Ok(());
        }
        self.check_cancellation()?;
        if !memory_tree && self.is_ignored(path) {
            self.boundary(path, CoverageBoundaryKind::Ignored, "ignore_rule");
            return Ok(());
        }
        if has_non_unicode_component(&self.root, path) {
            self.terminal(
                path,
                CoverageStatus::ExcludedPolicy,
                None,
                "non_unicode_path",
            );
            return Ok(());
        }
        if detect::is_sensitive_path_only(path) {
            self.terminal(
                path,
                CoverageStatus::ExcludedSensitive,
                None,
                "sensitive_path_policy",
            );
            return Ok(());
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
            return Ok(());
        }
        if detect::is_policy_excluded_file(path) {
            self.terminal(
                path,
                CoverageStatus::ExcludedPolicy,
                declared,
                "legacy_scan_policy",
            );
            return Ok(());
        }
        if self.options.code_only
            && declared.is_some_and(|spec| {
                detect::classify_file(path) != Some(crate::format_registry::FileType::Code)
                    && spec.legacy_file_type != Some(crate::format_registry::FileType::Code)
            })
        {
            self.terminal(path, CoverageStatus::ExcludedPolicy, declared, "code_only");
            return Ok(());
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
                return Ok(());
            }
        };
        if !physical.starts_with(&self.root) {
            self.terminal(
                path,
                CoverageStatus::ExcludedSensitive,
                None,
                "sensitive_source_binding",
            );
            return Ok(());
        }
        if has_non_unicode_component(&self.root, &physical) {
            self.terminal(
                path,
                CoverageStatus::ExcludedPolicy,
                None,
                "non_unicode_source_binding",
            );
            return Ok(());
        }
        if detect::is_sensitive_path_only(&physical) {
            self.terminal(
                path,
                CoverageStatus::ExcludedSensitive,
                None,
                "sensitive_source_binding",
            );
            return Ok(());
        }
        self.check_cancellation()?;
        if detect::open_source_nofollow(&physical).is_err() {
            self.terminal(
                path,
                CoverageStatus::Unreadable,
                declared,
                "source_unreadable",
            );
            return Ok(());
        }
        let is_alias = physical != path;
        let replacement =
            if let Some((index, existing_alias)) = self.seen_physical.get(&physical).copied() {
                if !existing_alias || is_alias {
                    return Ok(());
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
            Some(spec)
                if self.options.code_only
                    && detect::classify_file(path)
                        != Some(crate::format_registry::FileType::Code)
                    && spec.legacy_file_type != Some(crate::format_registry::FileType::Code) =>
            {
                self.terminal_file(
                    path,
                    CoverageStatus::ExcludedPolicy,
                    Some(spec),
                    "code_only",
                )
            }
            Some(spec) => self.registered_file(path, spec),
            None => {
                self.terminal_file(path, CoverageStatus::Unsupported, None, "unregistered_path")
            }
        };
        if let Some(index) = replacement {
            self.files[index] = file;
            self.seen_physical.insert(physical, (index, false));
        } else if let Some(index) = self.retain_file(file) {
            self.seen_physical.insert(physical, (index, is_alias));
        }
        Ok(())
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
        self.retain_file(file);
    }

    fn retain_file(&mut self, file: CoverageFile) -> Option<usize> {
        if self.traversal_stopped {
            return None;
        }
        if self.files.len() < self.max_files {
            let index = self.files.len();
            self.files.push(file);
            Some(index)
        } else {
            self.files_truncated = self.files_truncated.saturating_add(1);
            increment_file_summary(&mut self.omitted_file_summary, file.status);
            self.traversal_stopped = true;
            None
        }
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
        if self.traversal_stopped {
            return;
        }
        if self.boundaries.len() < self.max_boundaries {
            self.boundaries.push(CoverageBoundary {
                path: report_path(&self.root, path),
                kind,
                reason: reason.to_owned(),
            });
        } else {
            self.boundaries_truncated = self.boundaries_truncated.saturating_add(1);
            match kind {
                CoverageBoundaryKind::Ignored => {
                    self.omitted_ignored_boundaries =
                        self.omitted_ignored_boundaries.saturating_add(1);
                }
                CoverageBoundaryKind::PrunedNoise => {
                    self.omitted_pruned_boundaries =
                        self.omitted_pruned_boundaries.saturating_add(1);
                }
            }
            self.traversal_stopped = true;
        }
    }

    fn is_ignored(&mut self, path: &Path) -> bool {
        let potential_entries = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .components()
            .count()
            .max(1);
        if potential_entries
            > self
                .max_ignore_cache_entries
                .saturating_sub(self.ignore_cache.len())
        {
            detect::is_ignored(path, &self.root, &self.patterns)
        } else {
            is_ignored_with_cache(path, &self.root, &self.patterns, &mut self.ignore_cache)
        }
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

    fn into_report(mut self) -> CoverageReport {
        self.files.sort_by(|left, right| left.path.cmp(&right.path));
        self.files.dedup_by(|left, right| left.path == right.path);
        self.boundaries.sort();
        self.boundaries.dedup();
        self.walk_errors.sort();
        self.walk_errors.dedup();
        let walk_errors_truncated = self
            .total_walk_errors
            .saturating_sub(self.walk_errors.len());

        let mut summary = self.omitted_file_summary.clone();
        for file in &self.files {
            increment_file_summary(&mut summary, file.status);
        }
        summary.ignored_boundaries = self
            .boundaries
            .iter()
            .filter(|boundary| boundary.kind == CoverageBoundaryKind::Ignored)
            .count()
            .saturating_add(self.omitted_ignored_boundaries);
        summary.pruned_boundaries = self
            .boundaries
            .iter()
            .filter(|boundary| boundary.kind == CoverageBoundaryKind::PrunedNoise)
            .count()
            .saturating_add(self.omitted_pruned_boundaries);
        summary.walk_errors = self.total_walk_errors;
        let complete = summary.unreadable == 0
            && summary.walk_errors == 0
            && self.files_truncated == 0
            && self.boundaries_truncated == 0
            && self.directory_walks_truncated == 0
            && self.ignore_sources_truncated == 0;
        CoverageReport {
            root: ".".to_owned(),
            schema_version: 1,
            complete,
            graph: None,
            files: self.files,
            files_truncated: self.files_truncated,
            boundaries: self.boundaries,
            boundaries_truncated: self.boundaries_truncated,
            directory_walks_truncated: self.directory_walks_truncated,
            ignore_sources_truncated: self.ignore_sources_truncated,
            walk_errors: self.walk_errors,
            walk_errors_truncated,
            summary,
        }
    }
}

fn increment_file_summary(summary: &mut CoverageSummary, status: CoverageStatus) {
    summary.total_files = summary.total_files.saturating_add(1);
    let value = match status {
        CoverageStatus::Covered => &mut summary.covered,
        CoverageStatus::InventoryOnly => &mut summary.inventory_only,
        CoverageStatus::Unsupported => &mut summary.unsupported,
        CoverageStatus::ExcludedSensitive => &mut summary.excluded_sensitive,
        CoverageStatus::ExcludedPolicy => &mut summary.excluded_policy,
        CoverageStatus::Unreadable => &mut summary.unreadable,
    };
    *value = value.saturating_add(1);
}

/// Audit every regular file admitted by the current scan boundary.
pub fn audit_coverage(root: &Path, options: &CoverageOptions) -> anyhow::Result<CoverageReport> {
    audit_coverage_with_cancellation(root, options, &RuntimeCancellation::new())
}

/// Audit coverage while cooperatively observing a shared indexing
/// cancellation token.
pub fn audit_coverage_with_cancellation(
    root: &Path,
    options: &CoverageOptions,
    cancellation: &RuntimeCancellation,
) -> anyhow::Result<CoverageReport> {
    if cancellation.is_cancelled() {
        bail!("coverage audit cancelled");
    }
    let root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve coverage root {}", root.display()))?;
    if !fs::metadata(&root).is_ok_and(|metadata| metadata.is_dir()) {
        bail!("coverage root is not a directory: {}", root.display());
    }
    let mut ignore_policy = load_ignore_patterns_bounded(&root, options.honor_gitignore);
    let remaining = MAX_IGNORE_PATTERNS.saturating_sub(ignore_policy.patterns.len());
    let remaining_bytes = MAX_IGNORE_RETAINED_BYTES.saturating_sub(ignore_policy.retained_bytes);
    ignore_policy.merge(load_extra_ignore_patterns(
        &root,
        &options.extra_excludes,
        remaining,
        remaining_bytes,
    ));
    let detect_options = DetectOptions {
        follow_symlinks: options.follow_symlinks,
        google_workspace: options.google_workspace,
        convert_office_sidecars: false,
        extra_excludes: options.extra_excludes.clone(),
        output_dir: options.output_dir.clone(),
        honor_gitignore: options.honor_gitignore,
    };
    let configured_output = detect::output_dir(&root, &detect_options);
    let memory = detect::managed_memory_directory(&root, &configured_output);
    let mut walker = AuditWalker {
        root,
        configured_output,
        options: options.clone(),
        cancellation,
        patterns: ignore_policy.patterns,
        patterns_retained_bytes: ignore_policy.retained_bytes,
        ignore_cache: HashMap::new(),
        active_targets: HashSet::new(),
        seen_physical: HashMap::new(),
        files: Vec::new(),
        boundaries: Vec::new(),
        walk_errors: Vec::new(),
        total_walk_errors: 0,
        max_files: MAX_RETAINED_COVERAGE_FILES,
        max_boundaries: MAX_RETAINED_COVERAGE_BOUNDARIES,
        max_ignore_cache_entries: MAX_COVERAGE_IGNORE_CACHE_ENTRIES,
        max_directory_entries: MAX_COVERAGE_DIRECTORY_ENTRIES,
        visited_directory_entries: 0,
        files_truncated: 0,
        boundaries_truncated: 0,
        omitted_file_summary: CoverageSummary::default(),
        omitted_ignored_boundaries: 0,
        omitted_pruned_boundaries: 0,
        directory_walks_truncated: 0,
        ignore_sources_truncated: ignore_policy.truncated_sources,
        traversal_stopped: false,
    };
    let root = walker.root.clone();
    if walker.ignore_sources_truncated == 0 {
        walker.walk(&root, false)?;
    }
    if walker.ignore_sources_truncated == 0
        && !walker.traversal_stopped
        && let Some(memory) = &memory
    {
        walker.walk(memory, true)?;
    }
    Ok(walker.into_report())
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

    fn test_walker<'a>(root: PathBuf, cancellation: &'a RuntimeCancellation) -> AuditWalker<'a> {
        AuditWalker {
            configured_output: root.join("graphoxide-out"),
            root,
            options: CoverageOptions::default(),
            cancellation,
            patterns: Vec::new(),
            patterns_retained_bytes: 0,
            ignore_cache: HashMap::new(),
            active_targets: HashSet::new(),
            seen_physical: HashMap::new(),
            files: Vec::new(),
            boundaries: Vec::new(),
            walk_errors: Vec::new(),
            total_walk_errors: 0,
            max_files: MAX_RETAINED_COVERAGE_FILES,
            max_boundaries: MAX_RETAINED_COVERAGE_BOUNDARIES,
            max_ignore_cache_entries: MAX_COVERAGE_IGNORE_CACHE_ENTRIES,
            max_directory_entries: MAX_COVERAGE_DIRECTORY_ENTRIES,
            visited_directory_entries: 0,
            files_truncated: 0,
            boundaries_truncated: 0,
            omitted_file_summary: CoverageSummary::default(),
            omitted_ignored_boundaries: 0,
            omitted_pruned_boundaries: 0,
            directory_walks_truncated: 0,
            ignore_sources_truncated: 0,
            traversal_stopped: false,
        }
    }

    #[test]
    fn coverage_options_copy_every_effective_detector_boundary_option() {
        let detect = DetectOptions {
            follow_symlinks: true,
            google_workspace: true,
            convert_office_sidecars: false,
            extra_excludes: vec!["generated/**".to_owned(), "*.secret".to_owned()],
            output_dir: Some(PathBuf::from("custom-output")),
            honor_gitignore: false,
        };

        assert_eq!(
            CoverageOptions::from(&detect),
            CoverageOptions {
                follow_symlinks: true,
                google_workspace: true,
                code_only: false,
                honor_gitignore: false,
                extra_excludes: vec!["generated/**".to_owned(), "*.secret".to_owned()],
                output_dir: Some(PathBuf::from("custom-output")),
            }
        );
    }

    #[test]
    fn standalone_json_stays_compatible_and_graph_association_is_validated() {
        let root = tempfile::tempdir().expect("temporary root");
        let root = fs::canonicalize(root.path()).expect("canonical root");
        let cancellation = RuntimeCancellation::new();
        let mut report = test_walker(root, &cancellation).into_report();
        let standalone = serde_json::to_string(&report).expect("standalone JSON");
        assert_eq!(
            standalone,
            r#"{"root":".","schema_version":1,"complete":true,"files":[],"boundaries":[],"walk_errors":[],"summary":{"total_files":0,"covered":0,"inventory_only":0,"unsupported":0,"excluded_sensitive":0,"excluded_policy":0,"unreadable":0,"ignored_boundaries":0,"pruned_boundaries":0,"walk_errors":0}}"#
        );

        let digest = "ab".repeat(32);
        report
            .associate_graph("nested/graph.json", digest.clone())
            .expect("valid graph association");
        let associated = serde_json::to_string(&report).expect("associated JSON");
        assert!(associated.contains(&format!(
            r#""graph":{{"path":"nested/graph.json","sha256":"{digest}"}}"#
        )));
        assert_eq!(
            serde_json::from_str::<CoverageReport>(&associated).expect("deserialize association"),
            report
        );

        for invalid in ["", ".", "../graph.json", "/graph.json"] {
            assert!(CoverageGraphAssociation::new(invalid, digest.clone()).is_err());
        }
        assert!(CoverageGraphAssociation::new("graph.json", "A".repeat(64)).is_err());
        assert!(CoverageGraphAssociation::new("graph.json", "a".repeat(63)).is_err());
    }

    #[test]
    fn cancellation_stops_coverage_before_traversal() {
        let root = tempfile::tempdir().expect("temporary root");
        fs::write(root.path().join("main.rs"), "fn main() {}\n").expect("fixture");
        let cancellation = RuntimeCancellation::new();
        cancellation.cancel();

        let error = audit_coverage_with_cancellation(
            root.path(),
            &CoverageOptions::default(),
            &cancellation,
        )
        .expect_err("cancelled audit");
        assert!(error.to_string().contains("coverage audit cancelled"));
    }

    #[test]
    fn retained_limits_preserve_truthful_totals_and_mark_incomplete() {
        let root = tempfile::tempdir().expect("temporary root");
        let root = fs::canonicalize(root.path()).expect("canonical root");
        let cancellation = RuntimeCancellation::new();
        let mut file_walker = test_walker(root.clone(), &cancellation);
        file_walker.max_files = 2;
        for (path, status) in [
            ("a.rs", CoverageStatus::Covered),
            ("b.unknown", CoverageStatus::Unsupported),
            ("c.pdf", CoverageStatus::InventoryOnly),
            ("alias-of-c.pdf", CoverageStatus::InventoryOnly),
        ] {
            file_walker.retain_file(CoverageFile {
                path: path.to_owned(),
                status,
                format_id: None,
                declared_capability: None,
                reason: None,
            });
        }
        let file_report = file_walker.into_report();
        assert_eq!(file_report.files.len(), 2);
        assert_eq!(file_report.files_truncated, 1);
        assert_eq!(file_report.summary.total_files, 3);
        assert_eq!(file_report.summary.covered, 1);
        assert_eq!(file_report.summary.unsupported, 1);
        assert_eq!(file_report.summary.inventory_only, 1);
        assert!(!file_report.complete);
        assert_eq!(file_report.strict_failure_count(), 1);

        let mut boundary_walker = test_walker(root.clone(), &cancellation);
        boundary_walker.max_boundaries = 1;
        boundary_walker.boundary(&root.join("ignored"), CoverageBoundaryKind::Ignored, "rule");
        boundary_walker.boundary(
            &root.join("target"),
            CoverageBoundaryKind::PrunedNoise,
            "noise",
        );
        boundary_walker.boundary(
            &root.join("another-target"),
            CoverageBoundaryKind::PrunedNoise,
            "noise",
        );

        let boundary_report = boundary_walker.into_report();
        assert_eq!(boundary_report.boundaries.len(), 1);
        assert_eq!(boundary_report.boundaries_truncated, 1);
        assert_eq!(boundary_report.summary.ignored_boundaries, 1);
        assert_eq!(boundary_report.summary.pruned_boundaries, 1);
        assert!(!boundary_report.complete);
        assert_eq!(boundary_report.strict_failure_count(), 1);
    }

    #[test]
    fn ignore_policy_cache_falls_back_without_growing_past_its_cap() {
        let root = tempfile::tempdir().expect("temporary root");
        let root = fs::canonicalize(root.path()).expect("canonical root");
        let cancellation = RuntimeCancellation::new();
        let mut walker = test_walker(root.clone(), &cancellation);
        walker.max_ignore_cache_entries = 1;

        assert!(!walker.is_ignored(&root.join("first.rs")));
        assert_eq!(walker.ignore_cache.len(), 1);
        assert!(!walker.is_ignored(&root.join("second.rs")));
        assert_eq!(walker.ignore_cache.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn traversal_stops_at_the_file_cap_before_untracked_aliases_can_inflate_totals() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary root");
        fs::write(root.path().join("a.rs"), "fn a() {}\n").expect("first source");
        fs::write(root.path().join("b.rs"), "fn b() {}\n").expect("second source");
        symlink("b.rs", root.path().join("alias-b.rs")).expect("source alias");
        let root = fs::canonicalize(root.path()).expect("canonical root");
        let cancellation = RuntimeCancellation::new();
        let mut walker = test_walker(root.clone(), &cancellation);
        walker.max_files = 1;
        walker.options.follow_symlinks = true;

        walker
            .visit_regular_file(&root.join("a.rs"), None, false)
            .expect("first source");
        walker
            .visit_regular_file(&root.join("b.rs"), None, false)
            .expect("cap-triggering source");
        walker
            .visit_symlink_file(&root.join("alias-b.rs"), false)
            .expect("post-cap alias");

        assert_eq!(walker.seen_physical.len(), 1);
        let report = walker.into_report();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files_truncated, 1);
        assert_eq!(report.summary.total_files, 2);
        assert!(!report.complete);
    }

    #[test]
    fn oversized_directory_is_not_partially_selected_by_readdir_order() {
        let root = tempfile::tempdir().expect("temporary root");
        for name in ["a.rs", "b.rs", "c.rs"] {
            fs::write(root.path().join(name), "fn item() {}\n").expect("source");
        }
        let root = fs::canonicalize(root.path()).expect("canonical root");
        let cancellation = RuntimeCancellation::new();
        let mut walker = test_walker(root.clone(), &cancellation);
        walker.max_directory_entries = 2;

        walker.walk(&root, false).expect("bounded walk");
        let report = walker.into_report();
        assert!(report.files.is_empty());
        assert!(report.boundaries.is_empty());
        assert_eq!(report.summary.total_files, 0);
        assert_eq!(report.directory_walks_truncated, 1);
        assert!(!report.complete);
        assert_eq!(report.strict_failure_count(), 1);
    }

    #[test]
    fn global_directory_entry_budget_stops_an_empty_directory_farm() {
        let root = tempfile::tempdir().expect("temporary root");
        fs::create_dir_all(root.path().join("a/b/c/d")).expect("empty directory chain");
        let root = fs::canonicalize(root.path()).expect("canonical root");
        let cancellation = RuntimeCancellation::new();
        let mut walker = test_walker(root.clone(), &cancellation);
        walker.max_directory_entries = 2;

        walker.walk(&root, false).expect("bounded walk");
        assert_eq!(walker.visited_directory_entries, 2);
        let report = walker.into_report();
        assert!(report.files.is_empty());
        assert!(report.boundaries.is_empty());
        assert_eq!(report.directory_walks_truncated, 1);
        assert!(!report.complete);
    }

    #[test]
    fn diagnostic_retention_is_bounded_and_omissions_remain_truthful_after_dedup() {
        let root = tempfile::tempdir().expect("temporary root");
        let root = fs::canonicalize(root.path()).expect("canonical root");
        let cancellation = RuntimeCancellation::new();
        let mut walker = test_walker(root.clone(), &cancellation);
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
