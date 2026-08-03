//! Commit policy for complete versus partial extraction runs.
//!
//! A complete rebuild may legitimately shrink after deletions or deduplication.
//! A partial rebuild may not bypass the graph writer's shrink/corruption guard
//! unless the caller explicitly opts in. The manifest callback runs only after
//! the graph artifact is durably accepted, so a refused result remains retryable.

use graphoxide_core::{Extraction, KnowledgeGraph};
use std::path::Path;

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
