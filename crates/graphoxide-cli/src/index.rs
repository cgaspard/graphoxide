//! Publication helpers for the first-class indexing workflow.

use anyhow::Context as _;
use graphoxide_extract::coverage::CoverageReport;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read as _},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const COVERAGE_ARTIFACT: &str = "coverage.json";
pub const MAX_INDEX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const HASH_BUFFER_BYTES: usize = 64 * 1024;
const MAX_INDEX_BUILD_CONFIG_BYTES: u64 = 1024 * 1024;
static COVERAGE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const PROTECTED_BUILD_FILES: &[&str] = &[
    "graph.json",
    "manifest.json",
    COVERAGE_ARTIFACT,
    ".rebuild.lock",
    ".graphoxide_build.json",
    ".graphify_build.json",
    "needs_update",
    ".pending_changes",
    ".pending_changes.lock",
    ".graphoxide_root",
    ".graphify_root",
];

/// One successfully published coverage artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublishedCoverage {
    pub path: String,
    pub graph_sha256: String,
    pub complete: bool,
}

/// Machine-readable stdout contract for `graphoxide index --json`.
///
/// The existing build report remains nested and unchanged. Index-only artifact
/// evidence is additive and cannot perturb `extract --json` consumers.
#[derive(Debug, Serialize)]
pub struct IndexBuildReport<'a> {
    pub schema_version: u8,
    pub build: &'a crate::build_telemetry::BuildTelemetry,
    pub coverage: &'a PublishedCoverage,
}

impl<'a> IndexBuildReport<'a> {
    #[must_use]
    pub const fn new(
        build: &'a crate::build_telemetry::BuildTelemetry,
        coverage: &'a PublishedCoverage,
    ) -> Self {
        Self {
            schema_version: 1,
            build,
            coverage,
        }
    }
}

/// Reject a telemetry sidecar that could overwrite or alias managed build
/// state. This check runs before lock creation or any other build mutation.
pub fn validate_runtime_report_destination(
    runtime_report: &Path,
    output_directory: &Path,
) -> anyhow::Result<()> {
    let base = std::env::current_dir().context("resolve current directory")?;
    validate_runtime_report_destination_from(runtime_report, output_directory, &base)
}

/// Reject coverage that crossed a hard retention or traversal ceiling before
/// the legacy detector can perform a less tightly bounded extraction pass.
/// `--allow-partial` may acknowledge ordinary unreadable inputs, but it never
/// expands these resource ceilings.
pub fn validate_coverage_for_index(
    report: &CoverageReport,
    allow_partial: bool,
) -> anyhow::Result<()> {
    let hard_truncations = report
        .files_truncated
        .saturating_add(report.boundaries_truncated)
        .saturating_add(report.directory_walks_truncated)
        .saturating_add(report.ignore_sources_truncated)
        .saturating_add(report.walk_errors_truncated);
    anyhow::ensure!(
        hard_truncations == 0,
        "refusing to publish an index because coverage exceeded hard resource ceilings ({hard_truncations} truncated result(s)); --allow-partial cannot waive resource limits"
    );
    anyhow::ensure!(
        report.complete || allow_partial,
        "refusing to publish an index from incomplete coverage; pass --allow-partial after reviewing the coverage failures"
    );
    Ok(())
}

/// Fail closed before Index reads or publishes either compatibility build
/// policy. Legacy Extract keeps its historical symlink-following behavior.
pub fn validate_index_build_config_destinations(output_directory: &Path) -> anyhow::Result<()> {
    for name in [
        crate::watch::BUILD_CONFIG,
        crate::watch::COMPAT_BUILD_CONFIG,
    ] {
        let path = output_directory.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_file() && !metadata_is_reparse_point(&metadata),
                    "refusing unsafe index build config destination {}",
                    path.display()
                );
                anyhow::ensure!(
                    metadata.len() <= MAX_INDEX_BUILD_CONFIG_BYTES,
                    "refusing oversized index build config {} ({} bytes exceeds the {} byte ceiling)",
                    path.display(),
                    metadata.len(),
                    MAX_INDEX_BUILD_CONFIG_BYTES
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Validate every previously published Index artifact before any path-based
/// existence check or legacy manifest load can follow an unsafe destination.
pub fn validate_index_prior_artifacts(output_directory: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(output_directory) {
        Ok(metadata) => anyhow::ensure!(
            metadata.file_type().is_dir() && !metadata_is_reparse_point(&metadata),
            "refusing unsafe index output directory {}",
            output_directory.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    for name in ["graph.json", "manifest.json", COVERAGE_ARTIFACT] {
        let path = output_directory.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_file() && !metadata_is_reparse_point(&metadata),
                    "refusing unsafe prior index artifact {}",
                    path.display()
                );
                if name == "manifest.json" {
                    anyhow::ensure!(
                        metadata.len() <= MAX_INDEX_MANIFEST_BYTES,
                        "refusing oversized index manifest {} ({} bytes exceeds the {} byte ceiling)",
                        path.display(),
                        metadata.len(),
                        MAX_INDEX_MANIFEST_BYTES
                    );
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Read persisted Index policy only after verifying both managed config paths.
pub fn read_index_build_config(
    output_directory: &Path,
) -> anyhow::Result<crate::watch::PersistedBuildConfig> {
    validate_index_build_config_destinations(output_directory)?;
    for name in [
        crate::watch::BUILD_CONFIG,
        crate::watch::COMPAT_BUILD_CONFIG,
    ] {
        let path = output_directory.join(name);
        let mut file = match open_index_build_config(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.file_type().is_file() && !metadata_is_reparse_point(&metadata),
            "refusing unsafe index build config source {}",
            path.display()
        );
        anyhow::ensure!(
            metadata.len() <= MAX_INDEX_BUILD_CONFIG_BYTES,
            "refusing oversized index build config {} ({} bytes exceeds the {} byte ceiling)",
            path.display(),
            metadata.len(),
            MAX_INDEX_BUILD_CONFIG_BYTES
        );
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_INDEX_BUILD_CONFIG_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= MAX_INDEX_BUILD_CONFIG_BYTES,
            "refusing oversized index build config {} (content grew beyond the {} byte ceiling)",
            path.display(),
            MAX_INDEX_BUILD_CONFIG_BYTES
        );
        if let Ok(config) = serde_json::from_slice(&bytes) {
            return Ok(config);
        }
    }
    Ok(crate::watch::PersistedBuildConfig::default())
}

/// Construct and size-check the exact Index policy before graph publication.
pub fn prepare_index_build_config(
    mut config: crate::watch::PersistedBuildConfig,
    excludes: Option<&[String]>,
    honor_gitignore: Option<bool>,
    cluster: bool,
) -> anyhow::Result<crate::watch::PersistedBuildConfig> {
    if let Some(excludes) = excludes {
        config.excludes = excludes.to_vec();
    }
    if let Some(honor_gitignore) = honor_gitignore {
        config.honor_gitignore = honor_gitignore;
    }
    config.cluster = cluster;
    validate_index_build_config_size(&config)?;
    Ok(config)
}

/// Strictly replace both persisted Index policy files without the legacy
/// Windows in-place-copy fallback.
pub fn write_prepared_index_build_config(
    output_directory: &Path,
    config: &crate::watch::PersistedBuildConfig,
) -> anyhow::Result<()> {
    validate_index_build_config_size(config)?;
    validate_index_build_config_destinations(output_directory)?;
    fs::create_dir_all(output_directory)?;
    for name in [
        crate::watch::BUILD_CONFIG,
        crate::watch::COMPAT_BUILD_CONFIG,
    ] {
        graphoxide_core::write_json_atomic_strict(output_directory.join(name), &config, false)?;
    }
    Ok(())
}

fn validate_index_build_config_size(
    config: &crate::watch::PersistedBuildConfig,
) -> anyhow::Result<()> {
    let serialized = serde_json::to_vec(config)?;
    anyhow::ensure!(
        u64::try_from(serialized.len()).unwrap_or(u64::MAX) <= MAX_INDEX_BUILD_CONFIG_BYTES,
        "refusing index build config whose serialized size exceeds the {} byte ceiling",
        MAX_INDEX_BUILD_CONFIG_BYTES
    );
    Ok(())
}

fn open_index_build_config(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn validate_runtime_report_destination_from(
    runtime_report: &Path,
    output_directory: &Path,
    base: &Path,
) -> anyhow::Result<()> {
    #[cfg(windows)]
    reject_windows_alternate_data_stream(runtime_report)?;
    let unresolved_report = absolute_lexical_path(runtime_report, base);
    reject_runtime_report_reparse_destination(&unresolved_report)?;
    if let Ok(metadata) = fs::symlink_metadata(&unresolved_report) {
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "runtime report destination {} is not a regular file",
            runtime_report.display()
        );
    }
    let report = resolve_alias_path(runtime_report, base);
    let output = resolve_alias_path(output_directory, base);
    let staging = output.join("staging");
    if paths_equal(&report, &output) {
        anyhow::bail!(
            "runtime report destination {} collides with managed build directory {}",
            runtime_report.display(),
            output.display()
        );
    }
    if paths_overlap(&report, &staging) {
        anyhow::bail!(
            "runtime report destination {} collides with managed build staging {}",
            runtime_report.display(),
            staging.display()
        );
    }
    for name in PROTECTED_BUILD_FILES {
        let protected = output.join(name);
        if paths_overlap(&report, &protected) || same_existing_file(&report, &protected) {
            anyhow::bail!(
                "runtime report destination {} collides with managed build artifact {}",
                runtime_report.display(),
                protected.display()
            );
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn reject_windows_alternate_data_stream(path: &Path) -> anyhow::Result<()> {
    for component in path.components() {
        if let Component::Normal(value) = component {
            anyhow::ensure!(
                !value.to_string_lossy().contains(':'),
                "runtime report destination {} contains Windows alternate data stream syntax",
                path.display()
            );
        }
    }
    Ok(())
}

fn absolute_lexical_path(path: &Path, base: &Path) -> PathBuf {
    lexical_normalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

fn reject_runtime_report_reparse_destination(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_reparse_point(&metadata) => anyhow::bail!(
            "runtime report destination {} is a symlink or reparse point",
            path.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect runtime report destination {}", path.display()));
        }
    }
    Ok(())
}

fn resolve_alias_path(path: &Path, base: &Path) -> PathBuf {
    let absolute = absolute_lexical_path(path, base);
    let mut cursor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        if let Ok(mut existing) = fs::canonicalize(cursor) {
            for component in missing.iter().rev() {
                existing.push(component);
            }
            return lexical_normalize(existing);
        }
        let Some(name) = cursor.file_name() else {
            return absolute;
        };
        missing.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            return absolute;
        };
        cursor = parent;
    }
}

fn lexical_normalize(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn components_equal(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    #[cfg(windows)]
    {
        let normalize = |value: &std::ffi::OsStr| {
            value
                .to_string_lossy()
                .trim_end_matches([' ', '.'])
                .to_lowercase()
        };
        normalize(left) == normalize(right)
    }
    #[cfg(target_os = "macos")]
    {
        left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        left == right
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = left.components().collect::<Vec<_>>();
    let right = right.components().collect::<Vec<_>>();
    left.len() == right.len()
        && left
            .iter()
            .zip(&right)
            .all(|(left, right)| components_equal(left.as_os_str(), right.as_os_str()))
}

fn path_is_within(candidate: &Path, directory: &Path) -> bool {
    let candidate = candidate.components().collect::<Vec<_>>();
    let directory = directory.components().collect::<Vec<_>>();
    candidate.len() > directory.len()
        && candidate
            .iter()
            .zip(&directory)
            .all(|(candidate, directory)| {
                components_equal(candidate.as_os_str(), directory.as_os_str())
            })
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    paths_equal(left, right) || path_is_within(left, right) || path_is_within(right, left)
}

#[cfg(unix)]
fn same_existing_file(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_existing_file(left: &Path, right: &Path) -> bool {
    windows_file_identity(left)
        .zip(windows_file_identity(right))
        .is_some_and(|(left, right)| left == right)
}

#[cfg(windows)]
fn windows_file_identity(path: &Path) -> Option<(u32, u64)> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let file = File::open(path).ok()?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file owns the live handle and the output structure is valid.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) };
    (succeeded != 0).then(|| {
        (
            information.dwVolumeSerialNumber,
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn same_existing_file(_left: &Path, _right: &Path) -> bool {
    false
}

/// Stream the digest of the exact accepted graph artifact with fixed memory.
pub fn graph_file_sha256(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)
        .map_err(|error| anyhow::anyhow!("open accepted graph {}: {error}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| anyhow::anyhow!("hash accepted graph {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Associate and atomically publish coverage after graph and manifest commit.
pub fn publish_associated_coverage(
    output_directory: &Path,
    graph_path: &Path,
    mut report: CoverageReport,
) -> anyhow::Result<PublishedCoverage> {
    let digest = graph_file_sha256(graph_path)?;
    let complete = report.complete;
    report.associate_graph(Path::new("graph.json"), digest.clone())?;
    let coverage_path = output_directory.join(COVERAGE_ARTIFACT);
    write_coverage_atomic(&coverage_path, &report)?;
    Ok(PublishedCoverage {
        path: coverage_path.to_string_lossy().into_owned(),
        graph_sha256: digest,
        complete,
    })
}

/// Write coverage with a same-directory temporary and strict atomic replace.
///
/// Unlike the compatibility JSON writer, this never falls back to copying
/// over an existing destination on Windows. If atomic replacement is not
/// available, the previous coverage remains intact and index publication
/// reports an error.
fn write_coverage_atomic(path: &Path, report: &CoverageReport) -> anyhow::Result<()> {
    write_coverage_atomic_with(path, report, graphoxide_core::replace_file_strict)
}

fn write_coverage_atomic_with<R>(
    path: &Path,
    report: &CoverageReport,
    replace: R,
) -> anyhow::Result<()>
where
    R: FnOnce(&Path, &Path) -> io::Result<()>,
{
    let destination = match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "refusing to replace symlinked coverage destination {}",
                path.display()
            );
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            anyhow::bail!(
                "refusing to replace non-file coverage destination {}",
                path.display()
            );
        }
        Ok(_) => path.to_path_buf(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(error.into()),
    };
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(COVERAGE_ARTIFACT);
    let mut temporary = None;
    let mut file = None;
    for _ in 0..128 {
        let sequence = COVERAGE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{name}.coverage.{}.{sequence}.tmp",
            std::process::id()
        ));
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
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let temporary = temporary.ok_or_else(|| {
        anyhow::anyhow!(
            "could not allocate a unique coverage temporary beside {}",
            destination.display()
        )
    })?;
    let mut file = file.expect("coverage temporary path and file are created together");
    let result = (|| -> anyhow::Result<()> {
        if let Ok(metadata) = fs::metadata(&destination) {
            fs::set_permissions(&temporary, metadata.permissions())?;
        }
        serde_json::to_writer_pretty(&mut file, report).map_err(io::Error::other)?;
        file.sync_all()?;
        drop(file);
        replace(&temporary, &destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| error.context(format!("publish coverage {}", destination.display())))
}

/// Validate an on-disk coverage association against the current graph bytes.
pub fn coverage_matches_graph(report: &CoverageReport, graph_path: &Path) -> anyhow::Result<bool> {
    let Some(association) = report.graph.as_ref() else {
        return Ok(false);
    };
    Ok(association.path == "graph.json" && association.sha256 == graph_file_sha256(graph_path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_extract::coverage::{audit_coverage, CoverageOptions};
    use std::{fs, path::PathBuf};

    #[test]
    fn graph_digest_streams_the_exact_accepted_bytes() {
        let temp = tempfile::tempdir().expect("temporary output");
        let graph = temp.path().join("graph.json");
        let bytes = vec![b'x'; HASH_BUFFER_BYTES * 2 + 17];
        fs::write(&graph, &bytes).expect("graph bytes");
        assert_eq!(
            graph_file_sha256(&graph).expect("digest"),
            format!("{:x}", Sha256::digest(&bytes))
        );
    }

    #[test]
    fn associated_coverage_matches_only_its_exact_graph() {
        let temp = tempfile::tempdir().expect("temporary output");
        let source = temp.path().join("source");
        let output = temp.path().join("managed/graphoxide-out");
        fs::create_dir_all(&source).expect("source directory");
        fs::create_dir_all(&output).expect("output directory");
        fs::write(source.join("main.rs"), "fn main() {}\n").expect("source");
        let graph = output.join("graph.json");
        fs::write(&graph, b"accepted graph bytes\n").expect("graph");
        let report = audit_coverage(
            &source,
            &CoverageOptions {
                output_dir: Some(output.clone()),
                ..CoverageOptions::default()
            },
        )
        .expect("coverage");

        let published = publish_associated_coverage(&output, &graph, report).expect("publish");
        let bytes = fs::read(output.join(COVERAGE_ARTIFACT)).expect("coverage bytes");
        let report: CoverageReport = serde_json::from_slice(&bytes).expect("coverage JSON");
        assert!(coverage_matches_graph(&report, &graph).expect("association"));
        assert_eq!(
            published.path,
            output.join(COVERAGE_ARTIFACT).to_string_lossy()
        );

        fs::write(&graph, b"changed graph bytes\n").expect("changed graph");
        assert!(!coverage_matches_graph(&report, &graph).expect("stale association"));
    }

    #[test]
    fn associated_coverage_can_atomically_replace_an_existing_report() {
        let temp = tempfile::tempdir().expect("temporary output");
        let source = temp.path().join("source");
        let output = temp.path().join("managed/graphoxide-out");
        fs::create_dir_all(&source).expect("source directory");
        fs::create_dir_all(&output).expect("output directory");
        fs::write(source.join("main.rs"), "fn first() {}\n").expect("source");
        let graph = output.join("graph.json");
        fs::write(&graph, b"first accepted graph\n").expect("first graph");
        let first = audit_coverage(&source, &CoverageOptions::default()).expect("first report");
        publish_associated_coverage(&output, &graph, first).expect("first publication");
        let first_bytes = fs::read(output.join(COVERAGE_ARTIFACT)).expect("first coverage");

        fs::write(source.join("second.rs"), "fn second() {}\n").expect("second source");
        fs::write(&graph, b"second accepted graph\n").expect("second graph");
        let second = audit_coverage(&source, &CoverageOptions::default()).expect("second report");
        let published =
            publish_associated_coverage(&output, &graph, second).expect("replacement publication");
        let second_bytes = fs::read(output.join(COVERAGE_ARTIFACT)).expect("second coverage");
        assert_ne!(second_bytes, first_bytes);
        let report: CoverageReport =
            serde_json::from_slice(&second_bytes).expect("replacement JSON");
        assert!(coverage_matches_graph(&report, &graph).expect("association"));
        assert_eq!(report.files.len(), 2);
        assert_eq!(published.graph_sha256, graph_file_sha256(&graph).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_replace_supports_unicode_and_space_paths() {
        let temp = tempfile::tempdir().expect("temporary output");
        let destination = temp.path().join("accepted coverage \u{2603}.json");
        let replacement = temp.path().join(".replacement \u{2603}.tmp");
        fs::write(&destination, b"old\n").expect("old destination");
        fs::write(&replacement, b"new\n").expect("replacement");

        graphoxide_core::replace_file_strict(&replacement, &destination)
            .expect("atomic Windows replacement");
        assert_eq!(fs::read(&destination).unwrap(), b"new\n");
        assert!(!replacement.exists());
    }

    #[test]
    fn strict_atomic_coverage_failure_preserves_previous_bytes_and_cleans_temporary() {
        let temp = tempfile::tempdir().expect("temporary output");
        let source = temp.path().join("source");
        let destination = temp.path().join(COVERAGE_ARTIFACT);
        fs::create_dir(&source).expect("source directory");
        fs::write(source.join("main.rs"), "fn main() {}\n").expect("source");
        fs::write(&destination, b"previous accepted coverage\n").expect("old coverage");
        let report = audit_coverage(&source, &CoverageOptions::default()).expect("coverage");

        let error = write_coverage_atomic_with(&destination, &report, |_, _| {
            Err(std::io::Error::other("injected strict replace failure"))
        })
        .expect_err("replace failure");
        assert!(error.to_string().contains("publish coverage"));
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"previous accepted coverage\n"
        );
        assert!(fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().contains(".coverage.")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_coverage_destination_is_rejected_without_touching_external_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary output");
        let source = temp.path().join("source");
        let output = temp.path().join("managed");
        let external = temp.path().join("external-coverage.json");
        fs::create_dir(&source).expect("source directory");
        fs::create_dir(&output).expect("output directory");
        fs::write(source.join("main.rs"), "fn main() {}\n").expect("source");
        fs::write(&external, b"external accepted bytes\n").expect("external target");
        let destination = output.join(COVERAGE_ARTIFACT);
        symlink(&external, &destination).expect("coverage symlink");
        let report = audit_coverage(&source, &CoverageOptions::default()).expect("coverage");

        let error = write_coverage_atomic(&destination, &report)
            .expect_err("symlinked destination must fail closed");
        assert!(error.to_string().contains("symlinked coverage destination"));
        assert_eq!(fs::read(&external).unwrap(), b"external accepted bytes\n");
        assert!(destination
            .symlink_metadata()
            .expect("retained symlink")
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&destination).unwrap(), external);
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
    }

    #[test]
    fn non_file_coverage_destination_is_rejected_without_mutation() {
        let temp = tempfile::tempdir().expect("temporary output");
        let source = temp.path().join("source");
        let destination = temp.path().join(COVERAGE_ARTIFACT);
        fs::create_dir(&source).expect("source directory");
        fs::create_dir(&destination).expect("directory destination");
        fs::write(source.join("main.rs"), "fn main() {}\n").expect("source");
        let report = audit_coverage(&source, &CoverageOptions::default()).expect("coverage");

        let error = write_coverage_atomic(&destination, &report)
            .expect_err("directory destination must fail closed");
        assert!(error.to_string().contains("non-file coverage destination"));
        assert!(destination.is_dir());
        assert!(fs::read_dir(&destination).unwrap().next().is_none());
    }

    #[test]
    fn index_json_report_is_additive_to_the_stable_build_report() {
        use crate::build_telemetry::{BuildMode, BuildOperation, BuildStatus, BuildTelemetry};

        let build = BuildTelemetry::new(
            BuildOperation::Index,
            BuildMode::Full,
            BuildStatus::Rebuilt,
            PathBuf::from("graphoxide-out/graph.json"),
        );
        let coverage = PublishedCoverage {
            path: "graphoxide-out/coverage.json".to_owned(),
            graph_sha256: "a".repeat(64),
            complete: false,
        };
        let value = serde_json::to_value(IndexBuildReport::new(&build, &coverage))
            .expect("index report JSON");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["build"]["operation"], "index");
        assert_eq!(value["coverage"]["path"], "graphoxide-out/coverage.json");
        assert_eq!(value["coverage"]["graph_sha256"], "a".repeat(64));
        assert_eq!(value["coverage"]["complete"], false);
    }

    #[test]
    fn missing_graph_is_a_contextual_error() {
        let error = graph_file_sha256(Path::new("definitely-missing-graph.json"))
            .expect_err("missing graph must fail");
        assert!(error.to_string().contains("open accepted graph"));
        assert!(error.downcast_ref::<std::io::Error>().is_none());
    }

    #[test]
    fn allow_partial_never_waives_coverage_resource_truncation() {
        let temp = tempfile::tempdir().expect("temporary project");
        fs::write(temp.path().join("main.rs"), "fn main() {}\n").expect("source");
        let baseline = audit_coverage(temp.path(), &CoverageOptions::default()).expect("coverage");
        assert!(baseline.complete);

        for field in [
            "files",
            "boundaries",
            "directory_walks",
            "ignore_sources",
            "walk_errors",
        ] {
            let mut report = baseline.clone();
            report.complete = false;
            match field {
                "files" => report.files_truncated = 1,
                "boundaries" => report.boundaries_truncated = 1,
                "directory_walks" => report.directory_walks_truncated = 1,
                "ignore_sources" => report.ignore_sources_truncated = 1,
                "walk_errors" => report.walk_errors_truncated = 1,
                _ => unreachable!(),
            }
            let error = validate_coverage_for_index(&report, true)
                .expect_err("allow-partial must not waive hard ceilings");
            assert!(
                error.to_string().contains("hard resource ceilings"),
                "{field}: {error}"
            );
        }
    }

    #[test]
    fn allow_partial_can_acknowledge_ordinary_incomplete_coverage() {
        let temp = tempfile::tempdir().expect("temporary project");
        fs::write(temp.path().join("main.rs"), "fn main() {}\n").expect("source");
        let mut report =
            audit_coverage(temp.path(), &CoverageOptions::default()).expect("coverage");
        report.complete = false;
        report.summary.unreadable = 1;

        validate_coverage_for_index(&report, true).expect("explicit partial coverage");
        let error = validate_coverage_for_index(&report, false)
            .expect_err("incomplete coverage requires opt-in");
        assert!(error.to_string().contains("--allow-partial"), "{error}");
    }

    #[test]
    fn oversized_index_build_config_is_rejected_with_bounded_read() {
        let temp = tempfile::tempdir().expect("temporary output");
        let output = temp.path().join("graphoxide-out");
        fs::create_dir(&output).expect("output directory");
        let config = output.join(crate::watch::BUILD_CONFIG);
        fs::write(
            &config,
            vec![b'x'; usize::try_from(MAX_INDEX_BUILD_CONFIG_BYTES).unwrap() + 1],
        )
        .expect("oversized config");

        let error = read_index_build_config(&output).expect_err("oversized config must fail");
        assert!(error.to_string().contains("byte ceiling"), "{error}");
        assert_eq!(
            fs::metadata(config).unwrap().len(),
            MAX_INDEX_BUILD_CONFIG_BYTES + 1
        );
    }

    #[test]
    fn index_never_publishes_a_self_produced_config_above_its_read_cap() {
        let oversized = vec!["x".repeat(usize::try_from(MAX_INDEX_BUILD_CONFIG_BYTES).unwrap())];
        let error = prepare_index_build_config(
            crate::watch::PersistedBuildConfig::default(),
            Some(&oversized),
            Some(false),
            false,
        )
        .expect_err("prospective oversized config must fail before publication");
        assert!(
            error.to_string().contains("serialized size exceeds"),
            "{error}"
        );

        let temp = tempfile::tempdir().expect("temporary output");
        let prepared = prepare_index_build_config(
            crate::watch::PersistedBuildConfig::default(),
            Some(&["generated/**".to_owned()]),
            Some(false),
            false,
        )
        .expect("bounded prospective config");
        write_prepared_index_build_config(temp.path(), &prepared).expect("strict config publish");
        let reread = read_index_build_config(temp.path()).expect("bounded config reread");
        assert_eq!(reread, prepared);
        for name in [
            crate::watch::BUILD_CONFIG,
            crate::watch::COMPAT_BUILD_CONFIG,
        ] {
            assert!(
                fs::metadata(temp.path().join(name)).unwrap().len() <= MAX_INDEX_BUILD_CONFIG_BYTES
            );
        }
    }

    #[test]
    fn runtime_report_rejects_managed_artifacts_and_staging_before_creation() {
        let temp = tempfile::tempdir().expect("temporary project");
        let base = temp.path();
        let output = base.join("graphoxide-out");
        for report in [
            Path::new("graphoxide-out"),
            Path::new("graphoxide-out/graph.json"),
            Path::new("graphoxide-out/graph.json/report.json"),
            Path::new("graphoxide-out/./manifest.json"),
            Path::new("graphoxide-out/nested/../coverage.json"),
            Path::new("graphoxide-out/.rebuild.lock"),
            Path::new("graphoxide-out/.graphoxide_build.json"),
            Path::new("graphoxide-out/.graphify_build.json"),
            Path::new("graphoxide-out/needs_update"),
            Path::new("graphoxide-out/.pending_changes"),
            Path::new("graphoxide-out/.pending_changes.lock"),
            Path::new("graphoxide-out/.graphoxide_root"),
            Path::new("graphoxide-out/.graphify_root"),
            Path::new("graphoxide-out/staging"),
            Path::new("graphoxide-out/staging/run/report.json"),
        ] {
            let error = validate_runtime_report_destination_from(report, &output, base)
                .expect_err("managed collision");
            assert!(
                error.to_string().contains("collides"),
                "{report:?}: {error}"
            );
        }
        assert!(
            !output.exists(),
            "validation must not create managed build state"
        );
        validate_runtime_report_destination_from(Path::new("telemetry/index.json"), &output, base)
            .expect("separate telemetry path");
    }

    #[test]
    fn windows_stream_syntax_detector_rejects_managed_artifact_ads_components() {
        for report in [
            Path::new("graphoxide-out/graph.json::$DATA"),
            Path::new("graphoxide-out/manifest.json:telemetry"),
            Path::new("graphoxide-out/coverage.json::$DATA"),
        ] {
            let error = reject_windows_alternate_data_stream(report)
                .expect_err("NTFS stream syntax must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("Windows alternate data stream syntax"),
                "{report:?}: {error}"
            );
        }
        reject_windows_alternate_data_stream(Path::new("telemetry/index.json"))
            .expect("ordinary relative path");
    }

    #[cfg(windows)]
    #[test]
    fn runtime_report_rejects_ads_but_preserves_drive_prefixes() {
        reject_windows_alternate_data_stream(Path::new(r"C:\project\telemetry\index.json"))
            .expect("drive-letter prefix is not a normal component");

        let temp = tempfile::tempdir().expect("temporary project");
        let output = temp.path().join("graphoxide-out");
        let error = validate_runtime_report_destination_from(
            Path::new(r"graphoxide-out\coverage.json::$DATA"),
            &output,
            temp.path(),
        )
        .expect_err("ADS collision");
        assert!(
            error
                .to_string()
                .contains("Windows alternate data stream syntax"),
            "{error}"
        );
        assert!(!output.exists(), "ADS rejection must precede mutation");
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn runtime_report_rejects_case_folded_first_run_alias() {
        let temp = tempfile::tempdir().expect("temporary project");
        let output = temp.path().join("graphoxide-out");
        let error = validate_runtime_report_destination_from(
            Path::new("GRAPHOXIDE-OUT/Coverage.JSON"),
            &output,
            temp.path(),
        )
        .expect_err("case-folded collision");
        assert!(error.to_string().contains("coverage.json"), "{error}");
        assert!(!output.exists());
    }

    #[cfg(windows)]
    #[test]
    fn runtime_report_rejects_trailing_dot_and_space_first_run_alias() {
        let temp = tempfile::tempdir().expect("temporary project");
        let output = temp.path().join("graphoxide-out");
        let error = validate_runtime_report_destination_from(
            Path::new("graphoxide-out/coverage.json. "),
            &output,
            temp.path(),
        )
        .expect_err("Windows-normalized collision");
        assert!(error.to_string().contains("coverage.json"), "{error}");
        assert!(!output.exists());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_report_rejects_symlink_and_hardlink_aliases() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary project");
        let output = temp.path().join("graphoxide-out");
        fs::create_dir(&output).expect("managed output");
        let coverage = output.join(COVERAGE_ARTIFACT);
        fs::write(&coverage, b"accepted coverage\n").expect("coverage");
        let symlink_report = temp.path().join("coverage-link.json");
        symlink(&coverage, &symlink_report).expect("symlink alias");
        let hardlink_report = temp.path().join("coverage-hardlink.json");
        fs::hard_link(&coverage, &hardlink_report).expect("hardlink alias");

        let symlink_error =
            validate_runtime_report_destination_from(&symlink_report, &output, temp.path())
                .expect_err("symlink alias collision");
        assert!(
            symlink_error.to_string().contains("symlink"),
            "{symlink_error}"
        );
        let hardlink_error =
            validate_runtime_report_destination_from(&hardlink_report, &output, temp.path())
                .expect_err("hardlink alias collision");
        assert!(
            hardlink_error.to_string().contains("collides"),
            "{hardlink_error}"
        );
        assert_eq!(fs::read(&coverage).unwrap(), b"accepted coverage\n");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_report_rejects_dangling_relative_symlink_before_first_publish() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary project");
        let output = temp.path().join("graphoxide-out");
        fs::create_dir(&output).expect("managed output");
        let report = output.join("runtime.json");
        symlink(COVERAGE_ARTIFACT, &report).expect("dangling relative report symlink");
        assert!(!output.join(COVERAGE_ARTIFACT).exists());

        let error = validate_runtime_report_destination_from(&report, &output, temp.path())
            .expect_err("dangling symlink must fail closed");
        assert!(error.to_string().contains("symlink"), "{error}");
        assert!(report.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(!output.join(COVERAGE_ARTIFACT).exists());
    }
}
