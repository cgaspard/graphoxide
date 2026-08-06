//! Commit policy for complete versus partial extraction runs.
//!
//! A complete rebuild may legitimately shrink after deletions or deduplication.
//! A partial rebuild may not bypass the graph writer's shrink/corruption guard
//! unless the caller explicitly opts in. The manifest callback runs only after
//! the graph artifact is durably accepted, so a refused result remains retryable.

use fs2::FileExt;
use graphoxide_core::{Extraction, KnowledgeGraph};
use graphoxide_graph::{
    BuildOptions, FactBatch, FactBatchLimits, FactBatchMergeLimits, FactBatchRunBuilder,
    FactBatchRunLimits, FactBatchRunStore, StagedGraphOutput,
    DEFAULT_FACT_MATERIALIZATION_MAX_BYTES,
};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static FACT_RUN_STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const FACT_RUN_STAGING_PREFIX: &str = "fact-runs-v1-";
const FACT_RUN_ACTIVE_LOCK: &str = ".active.lock";
const MAX_RETAINED_STALE_FACT_RUN_DIRECTORIES: usize = 2;

struct FactRunStagingGuard {
    output_directory: PathBuf,
    path: PathBuf,
    active_lock: Option<File>,
    cleaned: bool,
}

impl FactRunStagingGuard {
    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(mut self) -> anyhow::Result<()> {
        self.active_lock.take();
        self.cleaned = true;
        remove_fact_run_staging(&self.output_directory, &self.path)
    }
}

impl Drop for FactRunStagingGuard {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        self.active_lock.take();
        let _ = remove_fact_run_staging(&self.output_directory, &self.path);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildProgress {
    total: usize,
    succeeded: usize,
}

impl BuildProgress {
    pub fn new(total: usize, succeeded: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            succeeded <= total,
            "successful extraction chunks ({succeeded}) exceed total chunks ({total})"
        );
        Ok(Self { total, succeeded })
    }

    pub const fn complete() -> Self {
        Self {
            total: 0,
            succeeded: 0,
        }
    }

    pub const fn is_complete(self) -> bool {
        self.succeeded == self.total
    }

    pub const fn force_write(self, allow_partial: bool) -> bool {
        allow_partial || self.is_complete()
    }

    /// Reject the misleading success case where semantic work was scheduled
    /// but every chunk failed. An empty workload remains a complete build.
    pub fn ensure_any_success(self, backend: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.total == 0 || self.succeeded > 0,
            "all semantic chunks failed for {backend}"
        );
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum BuildArtifact<'a> {
    Graph(&'a KnowledgeGraph),
    Raw(&'a [Extraction]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildCommitOutcome {
    Written,
    RefusedShrink,
}

/// Persist bounded extractor facts, externally merge them, and expose the
/// complete compatibility graph only after deterministic staging succeeds.
///
/// The run directory is unique per invocation beneath the managed output
/// directory, making interrupted staging recoverable for inspection without
/// risking reuse as a later build's input.
pub fn stage_graph_from_extractions(
    extractions: Vec<Extraction>,
    output_directory: &Path,
    options: BuildOptions,
) -> anyhow::Result<StagedGraphOutput> {
    stage_graph_from_extractions_with_materialization_limit(
        extractions,
        output_directory,
        options,
        DEFAULT_FACT_MATERIALIZATION_MAX_BYTES,
    )
}

/// Persist and externally merge bounded facts, refusing the compatibility
/// graph materialization before it exceeds `max_materialized_bytes`.
pub fn stage_graph_from_extractions_with_materialization_limit(
    extractions: Vec<Extraction>,
    output_directory: &Path,
    options: BuildOptions,
    max_materialized_bytes: usize,
) -> anyhow::Result<StagedGraphOutput> {
    stage_graph_from_extractions_with_materialization_limit_and_root(
        extractions,
        output_directory,
        options,
        max_materialized_bytes,
        None,
    )
}

/// As [`stage_graph_from_extractions_with_materialization_limit`], preserving
/// a project root for deterministic source normalization during incremental
/// compatibility graph construction.
pub fn stage_graph_from_extractions_with_materialization_limit_and_root(
    extractions: Vec<Extraction>,
    output_directory: &Path,
    options: BuildOptions,
    max_materialized_bytes: usize,
    root: Option<&Path>,
) -> anyhow::Result<StagedGraphOutput> {
    let staging = create_fact_run_staging(output_directory)?;
    let stage_result = (|| {
        let batch_limits = FactBatchLimits::default();
        let run_limits = FactBatchRunLimits::default();
        let mut store = FactBatchRunStore::create(staging.path(), batch_limits, run_limits)?;
        let mut builder = FactBatchRunBuilder::new(run_limits)?;
        for (source_ordinal, extraction) in extractions.into_iter().enumerate() {
            let source_ordinal = u64::try_from(source_ordinal)
                .map_err(|_| anyhow::anyhow!("source ordinal exceeds u64"))?;
            for batch in FactBatch::split_extraction(source_ordinal, extraction, batch_limits)? {
                if let Some(run) = builder.push(batch)? {
                    store.append_run(run)?;
                }
            }
        }
        if let Some(run) = builder.finish()? {
            store.append_run(run)?;
        }
        StagedGraphOutput::from_run_store_with_materialization_limit_and_root(
            &mut store,
            options,
            FactBatchMergeLimits::default(),
            max_materialized_bytes,
            root,
        )
    })();
    let cleanup_result = staging.cleanup();
    match (stage_result, cleanup_result) {
        (Ok(staged), Ok(())) => Ok(staged),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup.context("clean completed fact-run staging")),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("fact-run staging cleanup also failed: {cleanup:#}")))
        }
    }
}

fn create_fact_run_staging(output_directory: &Path) -> anyhow::Result<FactRunStagingGuard> {
    fs::create_dir_all(output_directory)?;
    let staging_root = output_directory.join("staging");
    match fs::symlink_metadata(&staging_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            anyhow::bail!(
                "refusing unsafe fact-run staging path {}",
                staging_root.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&staging_root)?;
        }
        Err(error) => return Err(error.into()),
    }

    for _ in 0..128 {
        let sequence = FACT_RUN_STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = staging_root.join(format!(
            "{FACT_RUN_STAGING_PREFIX}{}-{timestamp:032x}-{sequence:016x}",
            std::process::id()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                let lock_path = candidate.join(FACT_RUN_ACTIVE_LOCK);
                let active_lock = match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&lock_path)
                {
                    Ok(lock) => lock,
                    Err(error) => {
                        let _ = remove_fact_run_staging(output_directory, &candidate);
                        return Err(error.into());
                    }
                };
                if let Err(error) = FileExt::lock_exclusive(&active_lock) {
                    drop(active_lock);
                    let _ = remove_fact_run_staging(output_directory, &candidate);
                    return Err(error.into());
                }
                let guard = FactRunStagingGuard {
                    output_directory: output_directory.to_path_buf(),
                    path: candidate,
                    active_lock: Some(active_lock),
                    cleaned: false,
                };
                prune_stale_fact_run_staging(&staging_root)?;
                return Ok(guard);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!(
        "could not allocate a unique fact-run staging directory beneath {}",
        staging_root.display()
    )
}

fn remove_fact_run_staging(output_directory: &Path, staging: &Path) -> anyhow::Result<()> {
    let staging_root = output_directory.join("staging");
    let valid_name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(FACT_RUN_STAGING_PREFIX));
    anyhow::ensure!(
        staging.parent() == Some(staging_root.as_path()) && valid_name,
        "refusing to clean unexpected fact-run staging path {}",
        staging.display()
    );
    let metadata = match fs::symlink_metadata(staging) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "refusing to clean unsafe fact-run staging path {}",
        staging.display()
    );
    fs::remove_dir_all(staging)?;
    Ok(())
}

fn prune_stale_fact_run_staging(staging_root: &Path) -> anyhow::Result<()> {
    let mut stale = Vec::<(String, PathBuf, Option<File>)>::new();
    for entry in fs::read_dir(staging_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(FACT_RUN_STAGING_PREFIX) {
            continue;
        }
        let path = entry.path();
        anyhow::ensure!(
            path.parent() == Some(staging_root),
            "refusing unexpected fact-run staging path {}",
            path.display()
        );
        let metadata = fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "refusing unsafe stale fact-run staging path {}",
            path.display()
        );
        let lock_path = path.join(FACT_RUN_ACTIVE_LOCK);
        let lock = match fs::symlink_metadata(&lock_path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "refusing unsafe fact-run activity lock {}",
                    lock_path.display()
                );
                let lock = OpenOptions::new().read(true).write(true).open(&lock_path)?;
                match FileExt::try_lock_exclusive(&lock) {
                    Ok(()) => Some(lock),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        stale.push((name.to_owned(), path, lock));
    }
    stale.sort_by(|left, right| right.0.cmp(&left.0));
    let output_directory = staging_root
        .parent()
        .expect("staging root must have an output parent");
    for (_, path, lock) in stale
        .into_iter()
        .skip(MAX_RETAINED_STALE_FACT_RUN_DIRECTORIES)
    {
        drop(lock);
        remove_fact_run_staging(output_directory, &path)?;
    }
    Ok(())
}

impl std::fmt::Display for BuildCommitOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Written => formatter.write_str("build artifact written"),
            Self::RefusedShrink => formatter.write_str(
                "Refusing to overwrite a larger existing graph with an incomplete build",
            ),
        }
    }
}

/// Commit a build and then its manifest. A writer error or shrink refusal
/// returns before `persist_manifest`, preserving the previous manifest state.
pub fn commit_build<M>(
    graph_path: &Path,
    artifact: BuildArtifact<'_>,
    progress: BuildProgress,
    allow_partial: bool,
    persist_manifest: M,
) -> anyhow::Result<BuildCommitOutcome>
where
    M: FnOnce() -> anyhow::Result<()>,
{
    let force = progress.force_write(allow_partial);
    let wrote = match artifact {
        BuildArtifact::Graph(graph) => {
            graphoxide_core::write_graph_atomic(graph_path, graph, force)
        }
        BuildArtifact::Raw(extractions) => {
            graphoxide_core::write_raw_extractions_atomic(graph_path, extractions, force)
        }
    }?;
    if !wrote {
        return Ok(BuildCommitOutcome::RefusedShrink);
    }
    persist_manifest()?;
    Ok(BuildCommitOutcome::Written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_graph_stage_removes_its_unique_run_directory() {
        let temp = tempfile::tempdir().expect("temporary output");
        let staged = stage_graph_from_extractions(
            vec![Extraction::default()],
            temp.path(),
            BuildOptions::default(),
        )
        .expect("stage empty graph");
        assert!(staged.graph().nodes.is_empty());

        let staging_root = temp.path().join("staging");
        assert!(staging_root.is_dir());
        assert!(
            fs::read_dir(staging_root)
                .expect("read staging root")
                .next()
                .is_none(),
            "a successful stage must not retain a full fact-run copy"
        );
    }

    #[test]
    fn fact_run_staging_names_do_not_depend_on_pid_sequence_alone() {
        let temp = tempfile::tempdir().expect("temporary output");
        let first = create_fact_run_staging(temp.path()).expect("first staging directory");
        let second = create_fact_run_staging(temp.path()).expect("second staging directory");
        assert_ne!(first.path(), second.path());
        first.cleanup().expect("clean first staging");
        second.cleanup().expect("clean second staging");
    }

    #[test]
    fn failed_graph_stage_removes_its_exact_run_directory() {
        let temp = tempfile::tempdir().expect("temporary output");
        let extraction = Extraction {
            nodes: vec![graphoxide_core::Node {
                id: "node".into(),
                label: "Node".into(),
                file_type: "code".into(),
                source_file: "src/node.rs".into(),
                source_location: None,
                community: None,
                extra: Default::default(),
            }],
            ..Extraction::default()
        };
        stage_graph_from_extractions_with_materialization_limit(
            vec![extraction],
            temp.path(),
            BuildOptions::default(),
            1,
        )
        .expect_err("materialization budget must fail");

        assert!(
            fs::read_dir(temp.path().join("staging"))
                .expect("read staging root")
                .next()
                .is_none(),
            "ordinary errors must not retain a run directory"
        );
    }

    #[test]
    fn stale_crash_directories_are_retained_under_a_fixed_cap() {
        let temp = tempfile::tempdir().expect("temporary output");
        let staging_root = temp.path().join("staging");
        fs::create_dir(&staging_root).expect("create staging root");
        for ordinal in 0..6 {
            let stale = staging_root.join(format!("{FACT_RUN_STAGING_PREFIX}stale-{ordinal:016x}"));
            fs::create_dir(&stale).expect("create stale run directory");
            File::create(stale.join(FACT_RUN_ACTIVE_LOCK)).expect("create unlocked marker");
        }

        let active = create_fact_run_staging(temp.path()).expect("create active staging");
        let retained = fs::read_dir(&staging_root)
            .expect("read staging root")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(FACT_RUN_STAGING_PREFIX))
            })
            .count();
        assert_eq!(
            retained,
            MAX_RETAINED_STALE_FACT_RUN_DIRECTORIES + 1,
            "two stale crash directories plus the active build are retained"
        );
        active.cleanup().expect("clean active staging");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_staging_root_is_refused() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary output");
        let outside = tempfile::tempdir().expect("outside directory");
        symlink(outside.path(), temp.path().join("staging")).expect("malicious staging symlink");

        assert!(create_fact_run_staging(temp.path()).is_err());
        assert!(fs::read_dir(outside.path())
            .expect("outside remains readable")
            .next()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_stale_run_is_refused_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary output");
        let outside = tempfile::tempdir().expect("outside directory");
        let staging_root = temp.path().join("staging");
        fs::create_dir(&staging_root).expect("create staging root");
        let sentinel = outside.path().join("sentinel");
        File::create(&sentinel).expect("create outside sentinel");
        symlink(
            outside.path(),
            staging_root.join(format!("{FACT_RUN_STAGING_PREFIX}stale-link")),
        )
        .expect("create stale symlink");

        assert!(create_fact_run_staging(temp.path()).is_err());
        assert!(sentinel.is_file(), "outside target must remain untouched");
    }
}
