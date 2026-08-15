//! Bounded, deterministic graph-build input preparation and external runs.
//!
//! Extractor results are split into bounded [`FactBatch`] values, persisted as
//! CRC-framed runs, and merged by their stable producer keys. Graph
//! normalization itself intentionally still materializes graph state: endpoint
//! repair, semantic deduplication, and clustering all operate on the complete
//! graph. The run store keeps the input side bounded and makes that remaining
//! materialization explicit at the staging boundary.

use crate::{
    build_graph_with_report_and_options,
    build_graph_with_report_and_options_and_root_with_callback, BuildOptions, BuildReport,
};
use crc32fast::Hasher as Crc32;
use graphoxide_core::{write_graph_atomic, Edge, Extraction, KnowledgeGraph, Node};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const FACT_RUN_MAGIC: [u8; 8] = *b"GOXFRUN1";
const FACT_RUN_VERSION: u32 = 1;
const FACT_RUN_HEADER_BYTES: usize = FACT_RUN_MAGIC.len() + 4;
const FACT_RUN_FRAME_HEADER_BYTES: usize = 8 + 4 + 8 + 4;
const FACT_RUN_MANIFEST: &str = "manifest.json";
const FACT_RUN_DIRECTORY: &str = "runs";
const FACT_RUN_MANIFEST_MAX_BYTES: u64 = 16 * 1024 * 1024;
static FACT_RUN_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Default maximum number of facts (nodes, edges, and hyperedges) in one batch.
pub const DEFAULT_FACT_BATCH_MAX_FACTS: usize = 4_096;

/// Default upper bound for the serialized payload estimate of one batch.
pub const DEFAULT_FACT_BATCH_MAX_BYTES: usize = 1024 * 1024;

/// Default maximum number of batches retained while preparing one sorted run.
///
/// The I/O plane owns persistence of a completed run. Keeping this value
/// modest bounds the temporary sort working set without imposing a filesystem
/// policy on this graph-only module.
pub const DEFAULT_FACT_RUN_MAX_BATCHES: usize = 64;

/// Default maximum estimated payload retained while preparing one sorted run.
pub const DEFAULT_FACT_RUN_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Default maximum compatibility-graph materialization working set. The
/// default matches the cache/run partition of the 512 MiB default runtime
/// budget; callers with an explicit runtime budget pass its resolved partition
/// instead.
pub const DEFAULT_FACT_MATERIALIZATION_MAX_BYTES: usize = 128 * 1024 * 1024;

/// Conservative expansion from persisted JSON facts to the peak compatibility
/// builder working set. Materialization holds the regrouped source facts, a
/// normalized clone, graph nodes/edges, and ID/deduplication indexes at the
/// same time. Charging only the run payload therefore understates the memory
/// contract even though the external merge itself is bounded.
const FACT_MATERIALIZATION_WORKING_SET_MULTIPLIER: u64 = 8;

/// Bounded-memory admission policy for one [`FactBatch`].
///
/// `max_estimated_bytes` is measured from the JSON representation of each
/// fact. It intentionally includes values in flattened metadata maps, which
/// are often the dominant source of extractor output size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactBatchLimits {
    pub max_facts: usize,
    pub max_estimated_bytes: usize,
}

impl Default for FactBatchLimits {
    fn default() -> Self {
        Self {
            max_facts: DEFAULT_FACT_BATCH_MAX_FACTS,
            max_estimated_bytes: DEFAULT_FACT_BATCH_MAX_BYTES,
        }
    }
}

impl FactBatchLimits {
    /// Validate limits before admitting untrusted extractor output.
    pub fn validate(self) -> Result<Self, FactBatchError> {
        if self.max_facts == 0 {
            return Err(FactBatchError::InvalidLimits { field: "max_facts" });
        }
        if self.max_estimated_bytes == 0 {
            return Err(FactBatchError::InvalidLimits {
                field: "max_estimated_bytes",
            });
        }
        Ok(self)
    }
}

/// Stable origin and sequence key for a batch.
///
/// The producer assigns `source_ordinal` from the control-plane's normalized
/// file ordering and increments `batch_ordinal` when one source is split. The
/// graph builder consumes batches in this exact order, preserving the current
/// last-wins semantics for duplicate input records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FactBatchKey {
    pub source_ordinal: u64,
    pub batch_ordinal: u32,
}

impl FactBatchKey {
    pub const fn new(source_ordinal: u64, batch_ordinal: u32) -> Self {
        Self {
            source_ordinal,
            batch_ordinal,
        }
    }
}

/// A bounded, owned group of extracted graph facts.
///
/// Ownership moves from the extractor stage to the graph stage. The payload
/// remains an [`Extraction`] so the existing graph builder keeps its precise
/// normalization and merge semantics.
#[derive(Debug, Clone)]
pub struct FactBatch {
    key: FactBatchKey,
    extraction: Extraction,
    estimated_bytes: usize,
}

impl FactBatch {
    /// Create a batch when the supplied extraction already fits the limits.
    pub fn try_new(
        key: FactBatchKey,
        extraction: Extraction,
        limits: FactBatchLimits,
    ) -> Result<Self, FactBatchError> {
        let limits = limits.validate()?;
        let estimated_bytes = extraction_estimated_bytes(&extraction)?;
        let fact_count = fact_count(&extraction);
        ensure_fits(limits, fact_count, estimated_bytes)?;
        Ok(Self {
            key,
            extraction,
            estimated_bytes,
        })
    }

    /// Split one extraction into bounded batches without changing fact order.
    ///
    /// The resulting batches are ordered nodes, then edges, then hyperedges,
    /// which is the native order of [`Extraction`]. A single oversized fact is
    /// rejected instead of bypassing the configured memory bound.
    pub fn split_extraction(
        source_ordinal: u64,
        extraction: Extraction,
        limits: FactBatchLimits,
    ) -> Result<Vec<Self>, FactBatchError> {
        let limits = limits.validate()?;
        let mut batches = Vec::new();
        let mut current = Extraction::default();
        let mut current_bytes = 0usize;
        let mut batch_ordinal = 0u32;

        for node in extraction.nodes {
            append_fact(
                &mut batches,
                &mut current,
                &mut current_bytes,
                &mut batch_ordinal,
                source_ordinal,
                limits,
                Fact::Node(node),
            )?;
        }
        for edge in extraction.edges {
            append_fact(
                &mut batches,
                &mut current,
                &mut current_bytes,
                &mut batch_ordinal,
                source_ordinal,
                limits,
                Fact::Edge(edge),
            )?;
        }
        for hyperedge in extraction.hyperedges {
            append_fact(
                &mut batches,
                &mut current,
                &mut current_bytes,
                &mut batch_ordinal,
                source_ordinal,
                limits,
                Fact::Hyperedge(hyperedge),
            )?;
        }

        if fact_count(&current) > 0 {
            batches.push(Self {
                key: FactBatchKey::new(source_ordinal, batch_ordinal),
                extraction: current,
                estimated_bytes: current_bytes,
            });
        }
        Ok(batches)
    }

    pub const fn key(&self) -> FactBatchKey {
        self.key
    }

    pub fn fact_count(&self) -> usize {
        fact_count(&self.extraction)
    }

    pub const fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub fn extraction(&self) -> &Extraction {
        &self.extraction
    }

    pub fn into_extraction(self) -> Extraction {
        self.extraction
    }
}

/// Failure while admitting a fact batch.
#[derive(Debug)]
pub enum FactBatchError {
    InvalidLimits {
        field: &'static str,
    },
    FactTooLarge {
        kind: FactKind,
        estimated_bytes: usize,
        max_estimated_bytes: usize,
    },
    BatchLimitExceeded {
        fact_count: usize,
        max_facts: usize,
        estimated_bytes: usize,
        max_estimated_bytes: usize,
    },
    BatchOrdinalOverflow {
        source_ordinal: u64,
    },
    Serialization(serde_json::Error),
}

impl fmt::Display for FactBatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits { field } => {
                write!(formatter, "fact batch limit {field} must be greater than zero")
            }
            Self::FactTooLarge {
                kind,
                estimated_bytes,
                max_estimated_bytes,
            } => write!(
                formatter,
                "{kind} fact is {estimated_bytes} bytes, exceeding the {max_estimated_bytes}-byte batch limit"
            ),
            Self::BatchLimitExceeded {
                fact_count,
                max_facts,
                estimated_bytes,
                max_estimated_bytes,
            } => write!(
                formatter,
                "batch has {fact_count} facts/{estimated_bytes} bytes, exceeding limits of {max_facts} facts/{max_estimated_bytes} bytes"
            ),
            Self::BatchOrdinalOverflow { source_ordinal } => write!(
                formatter,
                "source ordinal {source_ordinal} requires more fact batches than u32 can represent"
            ),
            Self::Serialization(error) => write!(formatter, "failed to size fact batch payload: {error}"),
        }
    }
}

impl std::error::Error for FactBatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for FactBatchError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

/// The type of a fact that exceeded the configured batch payload bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactKind {
    Node,
    Edge,
    Hyperedge,
}

impl fmt::Display for FactKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Node => "node",
            Self::Edge => "edge",
            Self::Hyperedge => "hyperedge",
        })
    }
}

enum Fact {
    Node(Node),
    Edge(Edge),
    Hyperedge(serde_json::Value),
}

impl Fact {
    fn kind(&self) -> FactKind {
        match self {
            Self::Node(_) => FactKind::Node,
            Self::Edge(_) => FactKind::Edge,
            Self::Hyperedge(_) => FactKind::Hyperedge,
        }
    }

    fn estimated_bytes(&self) -> Result<usize, FactBatchError> {
        match self {
            Self::Node(value) => serialized_len(value),
            Self::Edge(value) => serialized_len(value),
            Self::Hyperedge(value) => serialized_len(value),
        }
    }

    fn append_to(self, extraction: &mut Extraction) {
        match self {
            Self::Node(value) => extraction.nodes.push(value),
            Self::Edge(value) => extraction.edges.push(value),
            Self::Hyperedge(value) => extraction.hyperedges.push(value),
        }
    }
}

fn append_fact(
    batches: &mut Vec<FactBatch>,
    current: &mut Extraction,
    current_bytes: &mut usize,
    batch_ordinal: &mut u32,
    source_ordinal: u64,
    limits: FactBatchLimits,
    fact: Fact,
) -> Result<(), FactBatchError> {
    let kind = fact.kind();
    let bytes = fact.estimated_bytes()?;
    if bytes > limits.max_estimated_bytes {
        return Err(FactBatchError::FactTooLarge {
            kind,
            estimated_bytes: bytes,
            max_estimated_bytes: limits.max_estimated_bytes,
        });
    }

    let next_fact_count = fact_count(current) + 1;
    let byte_limit_exceeded = current_bytes
        .checked_add(bytes)
        .is_none_or(|next_bytes| next_bytes > limits.max_estimated_bytes);
    if fact_count(current) > 0 && (next_fact_count > limits.max_facts || byte_limit_exceeded) {
        batches.push(FactBatch {
            key: FactBatchKey::new(source_ordinal, *batch_ordinal),
            extraction: std::mem::take(current),
            estimated_bytes: *current_bytes,
        });
        *batch_ordinal = batch_ordinal
            .checked_add(1)
            .ok_or(FactBatchError::BatchOrdinalOverflow { source_ordinal })?;
        *current_bytes = 0;
    }

    fact.append_to(current);
    *current_bytes = current_bytes
        .checked_add(bytes)
        .expect("fact admission must not overflow after an empty bounded batch");
    Ok(())
}

fn ensure_fits(
    limits: FactBatchLimits,
    facts: usize,
    estimated_bytes: usize,
) -> Result<(), FactBatchError> {
    if facts > limits.max_facts || estimated_bytes > limits.max_estimated_bytes {
        return Err(FactBatchError::BatchLimitExceeded {
            fact_count: facts,
            max_facts: limits.max_facts,
            estimated_bytes,
            max_estimated_bytes: limits.max_estimated_bytes,
        });
    }
    Ok(())
}

fn fact_count(extraction: &Extraction) -> usize {
    extraction.nodes.len() + extraction.edges.len() + extraction.hyperedges.len()
}

fn extraction_estimated_bytes(extraction: &Extraction) -> Result<usize, FactBatchError> {
    extraction
        .nodes
        .iter()
        .map(serialized_len)
        .chain(extraction.edges.iter().map(serialized_len))
        .chain(extraction.hyperedges.iter().map(serialized_len))
        .try_fold(0usize, |total, value| {
            value.map(|value| total.saturating_add(value))
        })
}

fn serialized_len<T: Serialize>(value: &T) -> Result<usize, FactBatchError> {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.0)
}

#[derive(Default)]
struct CountingWriter(usize);

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Sort batches into the only order accepted by the deterministic graph stage.
///
/// Duplicate keys are rejected because accepting them would revive scheduler
/// timing as an implicit tie-breaker.
pub fn sort_fact_batches(batches: &mut [FactBatch]) -> Result<(), FactBatchOrderError> {
    batches.sort_by_key(FactBatch::key);
    for pair in batches.windows(2) {
        if pair[0].key == pair[1].key {
            return Err(FactBatchOrderError::DuplicateKey(pair[0].key));
        }
    }
    Ok(())
}

/// Failure while establishing a deterministic batch order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactBatchOrderError {
    DuplicateKey(FactBatchKey),
}

impl fmt::Display for FactBatchOrderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateKey(key) => write!(
                formatter,
                "duplicate fact batch key ({}, {})",
                key.source_ordinal, key.batch_ordinal
            ),
        }
    }
}

impl std::error::Error for FactBatchOrderError {}

/// Bounded admission policy for a sorted run of [`FactBatch`] values.
///
/// A completed run is intentionally an owned in-memory value. The caller that
/// owns I/O can append it to a framed artifact or pass it directly to a merge
/// stage. This module never performs path I/O, and therefore does not claim
/// to implement an external-memory graph builder by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactBatchRunLimits {
    pub max_batches: usize,
    pub max_estimated_bytes: usize,
}

impl Default for FactBatchRunLimits {
    fn default() -> Self {
        Self {
            max_batches: DEFAULT_FACT_RUN_MAX_BATCHES,
            max_estimated_bytes: DEFAULT_FACT_RUN_MAX_BYTES,
        }
    }
}

impl FactBatchRunLimits {
    pub fn validate(self) -> Result<Self, FactBatchRunError> {
        if self.max_batches == 0 {
            return Err(FactBatchRunError::InvalidLimits {
                field: "max_batches",
            });
        }
        if self.max_estimated_bytes == 0 {
            return Err(FactBatchRunError::InvalidLimits {
                field: "max_estimated_bytes",
            });
        }
        Ok(self)
    }
}

/// An internally sorted, bounded hand-off unit for an I/O-owned run sink.
#[derive(Debug)]
pub struct OrderedFactBatchRun {
    batches: Vec<FactBatch>,
    estimated_bytes: usize,
}

impl OrderedFactBatchRun {
    pub fn batches(&self) -> &[FactBatch] {
        &self.batches
    }

    pub fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub fn first_key(&self) -> Option<FactBatchKey> {
        self.batches.first().map(FactBatch::key)
    }

    pub fn last_key(&self) -> Option<FactBatchKey> {
        self.batches.last().map(FactBatch::key)
    }

    pub fn into_batches(self) -> Vec<FactBatch> {
        self.batches
    }
}

/// Bounded sorter that emits deterministic runs for an I/O-owned spill sink.
///
/// Runs are sorted and reject duplicate keys *within the run*. A future
/// external merge must still reject duplicate keys across runs, because
/// arrival order may cause overlapping key ranges. The legacy compatibility
/// adapter continues to sort all batches globally before invoking the existing
/// whole-graph builder.
#[derive(Debug)]
pub struct FactBatchRunBuilder {
    limits: FactBatchRunLimits,
    batches: Vec<FactBatch>,
    estimated_bytes: usize,
}

impl FactBatchRunBuilder {
    pub fn new(limits: FactBatchRunLimits) -> Result<Self, FactBatchRunError> {
        Ok(Self {
            limits: limits.validate()?,
            batches: Vec::new(),
            estimated_bytes: 0,
        })
    }

    /// Admit a batch and return a completed sorted run when the next batch
    /// would exceed either configured bound. The caller must hand any returned
    /// run to its I/O-owned sink before submitting further work.
    pub fn push(
        &mut self,
        batch: FactBatch,
    ) -> Result<Option<OrderedFactBatchRun>, FactBatchRunError> {
        if batch.estimated_bytes() > self.limits.max_estimated_bytes {
            return Err(FactBatchRunError::BatchTooLarge {
                key: batch.key(),
                estimated_bytes: batch.estimated_bytes(),
                max_estimated_bytes: self.limits.max_estimated_bytes,
            });
        }

        let next_batches = self.batches.len() + 1;
        let byte_limit_exceeded = self
            .estimated_bytes
            .checked_add(batch.estimated_bytes())
            .is_none_or(|next_bytes| next_bytes > self.limits.max_estimated_bytes);
        if !self.batches.is_empty()
            && (next_batches > self.limits.max_batches || byte_limit_exceeded)
        {
            let run = self.finish_current()?;
            self.estimated_bytes = batch.estimated_bytes();
            self.batches.push(batch);
            return Ok(Some(run));
        }

        self.estimated_bytes = self
            .estimated_bytes
            .checked_add(batch.estimated_bytes())
            .expect("an empty bounded run cannot overflow its byte estimate");
        self.batches.push(batch);
        Ok(None)
    }

    /// Finish the remaining sorted run, if any.
    pub fn finish(&mut self) -> Result<Option<OrderedFactBatchRun>, FactBatchRunError> {
        if self.batches.is_empty() {
            return Ok(None);
        }
        self.finish_current().map(Some)
    }

    fn finish_current(&mut self) -> Result<OrderedFactBatchRun, FactBatchRunError> {
        let mut batches = std::mem::take(&mut self.batches);
        let estimated_bytes = std::mem::take(&mut self.estimated_bytes);
        if let Err(error) = sort_fact_batches(&mut batches) {
            self.batches = batches;
            self.estimated_bytes = estimated_bytes;
            return Err(FactBatchRunError::Ordering(error));
        }
        Ok(OrderedFactBatchRun {
            batches,
            estimated_bytes,
        })
    }
}

/// Failure while preparing a bounded ordered run.
#[derive(Debug)]
pub enum FactBatchRunError {
    InvalidLimits {
        field: &'static str,
    },
    BatchTooLarge {
        key: FactBatchKey,
        estimated_bytes: usize,
        max_estimated_bytes: usize,
    },
    Ordering(FactBatchOrderError),
}

impl fmt::Display for FactBatchRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits { field } => {
                write!(formatter, "fact batch run limit {field} must be greater than zero")
            }
            Self::BatchTooLarge {
                key,
                estimated_bytes,
                max_estimated_bytes,
            } => write!(
                formatter,
                "fact batch ({}, {}) is {estimated_bytes} bytes, exceeding the {max_estimated_bytes}-byte run limit",
                key.source_ordinal, key.batch_ordinal
            ),
            Self::Ordering(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for FactBatchRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ordering(error) => Some(error),
            _ => None,
        }
    }
}

/// Bound on the number of run files held open by one external merge.
///
/// A merge compacts larger run sets before opening readers, so the limit bounds
/// both file descriptors and the one-batch-per-run merge frontier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactBatchMergeLimits {
    pub max_open_runs: usize,
}

impl Default for FactBatchMergeLimits {
    fn default() -> Self {
        Self { max_open_runs: 32 }
    }
}

impl FactBatchMergeLimits {
    pub fn validate(self) -> Result<Self, FactBatchRunStoreError> {
        if self.max_open_runs < 2 {
            return Err(FactBatchRunStoreError::InvalidMergeLimit);
        }
        Ok(self)
    }
}

/// On-disk storage for sorted, bounded fact-batch runs.
///
/// The caller supplies an I/O-owned staging directory. Every completed run is
/// immutable, frame-checked, and listed in an atomically replaced manifest.
/// Interrupted writes leave only unreferenced temporary files; reopening the
/// store reads the last durable manifest and never treats those files as input.
#[derive(Debug)]
pub struct FactBatchRunStore {
    root: PathBuf,
    batch_limits: FactBatchLimits,
    run_limits: FactBatchRunLimits,
    runs: Vec<RunDescriptor>,
    next_run_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunStoreManifest {
    version: u32,
    batch_limits: FactBatchLimits,
    run_limits: FactBatchRunLimits,
    runs: Vec<RunDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunDescriptor {
    file: String,
    batches: usize,
    payload_bytes: u64,
}

/// Failure while persisting or merging fact-batch runs.
#[derive(Debug, thiserror::Error)]
pub enum FactBatchRunStoreError {
    #[error("fact-batch run store I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("fact-batch run store JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("fact-batch run store contains an invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("fact-batch run store refuses unsafe owned path {path}", path = .path.display())]
    UnsafePath { path: PathBuf },
    #[error(
        "fact-batch run manifest is {observed_bytes} bytes, exceeding the {max_bytes}-byte limit"
    )]
    ManifestTooLarge { observed_bytes: u64, max_bytes: u64 },
    #[error(
        "run file {path} descriptor reports {reported_batches} batches/{reported_payload_bytes} bytes but contains {actual_batches} batches/{actual_payload_bytes} bytes",
        path = .path.display()
    )]
    DescriptorMismatch {
        path: PathBuf,
        reported_batches: usize,
        reported_payload_bytes: u64,
        actual_batches: usize,
        actual_payload_bytes: u64,
    },
    #[error("fact-batch merge requires at least two open run slots")]
    InvalidMergeLimit,
    #[error("fact-batch materialization limit must be greater than zero")]
    InvalidMaterializationLimit,
    #[error(
        "fact-batch runs require {estimated_bytes} bytes, exceeding the {max_materialized_bytes}-byte graph-stage budget"
    )]
    MaterializationLimitExceeded {
        estimated_bytes: u64,
        max_materialized_bytes: usize,
    },
    #[error("run file {path} has an invalid header")]
    InvalidHeader { path: PathBuf },
    #[error("run file {path} uses unsupported format version {version}")]
    UnsupportedVersion { path: PathBuf, version: u32 },
    #[error("run file {path} has a truncated frame")]
    TruncatedFrame { path: PathBuf },
    #[error("run file {path} has a frame exceeding addressable memory")]
    FrameTooLarge { path: PathBuf },
    #[error("run file {path} frame checksum does not match")]
    ChecksumMismatch { path: PathBuf },
    #[error("run file {path} is not strictly ordered at key ({key_source}, {key_batch})")]
    NonIncreasingKey {
        path: PathBuf,
        key_source: u64,
        key_batch: u32,
    },
    #[error("merged fact-batch runs contain duplicate key ({key_source}, {key_batch})")]
    DuplicateKey { key_source: u64, key_batch: u32 },
    #[error("fact batch could not be restored from a persisted run: {0}")]
    FactBatch(#[from] FactBatchError),
    #[error("fact-batch run could not be persisted: {0}")]
    Run(#[from] FactBatchRunError),
}

impl FactBatchRunStore {
    /// Create an empty persistent run store beneath `root`.
    ///
    /// Creation refuses to replace an existing manifest. Call [`Self::open`]
    /// to continue a previously committed staging directory instead.
    pub fn create(
        root: impl Into<PathBuf>,
        batch_limits: FactBatchLimits,
        run_limits: FactBatchRunLimits,
    ) -> Result<Self, FactBatchRunStoreError> {
        let root = root.into();
        let batch_limits = batch_limits.validate()?;
        let run_limits = run_limits.validate()?;
        ensure_directory_not_symlink(&root)?;
        ensure_owned_subdirectory(&root, Path::new(FACT_RUN_DIRECTORY))?;
        let manifest = root.join(FACT_RUN_MANIFEST);
        if fs::symlink_metadata(&manifest).is_ok() {
            return Err(FactBatchRunStoreError::InvalidManifest(format!(
                "{} already exists",
                manifest.display()
            )));
        }
        let store = Self {
            root,
            batch_limits,
            run_limits,
            runs: Vec::new(),
            next_run_id: 0,
        };
        store.persist_manifest()?;
        Ok(store)
    }

    /// Reopen the last complete run-set manifest in `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, FactBatchRunStoreError> {
        let root = root.into();
        ensure_existing_directory_not_symlink(&root)?;
        ensure_existing_directory_not_symlink(&root.join(FACT_RUN_DIRECTORY))?;
        let manifest_path = root.join(FACT_RUN_MANIFEST);
        let manifest_len = safe_regular_file_len(&manifest_path)?.ok_or_else(|| {
            FactBatchRunStoreError::InvalidManifest(format!(
                "{} is missing",
                manifest_path.display()
            ))
        })?;
        if manifest_len > FACT_RUN_MANIFEST_MAX_BYTES {
            return Err(FactBatchRunStoreError::ManifestTooLarge {
                observed_bytes: manifest_len,
                max_bytes: FACT_RUN_MANIFEST_MAX_BYTES,
            });
        }
        let bytes = fs::read(&manifest_path)?;
        let manifest: RunStoreManifest = serde_json::from_slice(&bytes)?;
        if manifest.version != FACT_RUN_VERSION {
            return Err(FactBatchRunStoreError::InvalidManifest(format!(
                "unsupported manifest version {}",
                manifest.version
            )));
        }
        let batch_limits = manifest.batch_limits.validate()?;
        let run_limits = manifest.run_limits.validate()?;
        let mut next_run_id = 0u64;
        let mut run_files = BTreeSet::new();
        for run in &manifest.runs {
            validate_run_file_name(&run.file)?;
            if !run_files.insert(run.file.clone()) {
                return Err(FactBatchRunStoreError::InvalidManifest(format!(
                    "run file {} is listed more than once",
                    run.file
                )));
            }
            let id = run
                .file
                .strip_prefix("run-")
                .and_then(|value| value.strip_suffix(".gxr"))
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    FactBatchRunStoreError::InvalidManifest(format!(
                        "run file {} does not use the expected name",
                        run.file
                    ))
                })?;
            next_run_id = next_run_id.max(id.saturating_add(1));
            let path = root.join(FACT_RUN_DIRECTORY).join(&run.file);
            if safe_regular_file_len(&path)?.is_none() {
                return Err(FactBatchRunStoreError::InvalidManifest(format!(
                    "referenced run {} is missing",
                    path.display()
                )));
            }
            validate_run_descriptor(&path, batch_limits, run)?;
        }
        Ok(Self {
            root,
            batch_limits,
            run_limits,
            runs: manifest.runs,
            next_run_id,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    pub fn run_paths(&self) -> Vec<PathBuf> {
        self.runs.iter().map(|run| self.run_path(run)).collect()
    }

    /// Persist one bounded, internally sorted run and publish it in the
    /// manifest only after its frames are durable.
    pub fn append_run(&mut self, run: OrderedFactBatchRun) -> Result<(), FactBatchRunStoreError> {
        if run.batches().len() > self.run_limits.max_batches
            || run.estimated_bytes() > self.run_limits.max_estimated_bytes
        {
            return Err(FactBatchRunStoreError::InvalidManifest(
                "attempted to persist a run outside its configured bounds".into(),
            ));
        }
        let descriptor = self.write_batches(run.into_batches())?;
        let mut next_runs = self.runs.clone();
        next_runs.push(descriptor);
        self.persist_manifest_with_runs(&next_runs)?;
        self.runs = next_runs;
        Ok(())
    }

    /// Compact run files until a deterministic merge can hold no more than
    /// `max_open_runs` readers. Compaction streams each input group into one
    /// new immutable run and updates the manifest only after every replacement
    /// run has been durably written.
    pub fn compact_for_merge(
        &mut self,
        limits: FactBatchMergeLimits,
    ) -> Result<(), FactBatchRunStoreError> {
        let limits = limits.validate()?;
        while self.runs.len() > limits.max_open_runs {
            let input = self.runs.clone();
            let mut compacted = Vec::with_capacity(input.len().div_ceil(limits.max_open_runs));
            for group in input.chunks(limits.max_open_runs) {
                compacted.push(self.write_merged_group(group)?);
            }
            self.persist_manifest_with_runs(&compacted)?;
            self.runs = compacted;
        }
        Ok(())
    }

    /// Create a bounded-frontier deterministic external merge.
    pub fn merged_batches(
        &mut self,
        limits: FactBatchMergeLimits,
    ) -> Result<MergedFactBatchRuns, FactBatchRunStoreError> {
        self.compact_for_merge(limits)?;
        self.validate_descriptors()?;
        MergedFactBatchRuns::open(
            self.runs
                .iter()
                .map(|descriptor| self.run_path(descriptor))
                .collect(),
            self.batch_limits,
        )
    }

    /// Build the compatibility graph through the persistent, externally
    /// merged input path. The graph builder retains its intentional complete
    /// graph state only after this method has consumed the bounded run stream.
    pub fn stage_graph(
        &mut self,
        options: BuildOptions,
        merge_limits: FactBatchMergeLimits,
    ) -> anyhow::Result<StagedGraphOutput> {
        self.stage_graph_with_materialization_limit(
            options,
            merge_limits,
            DEFAULT_FACT_MATERIALIZATION_MAX_BYTES,
        )
    }

    /// Build the compatibility graph only when a conservative expansion of the
    /// persisted fact payload fits the caller's graph-stage budget. The
    /// external merge remains bounded by `merge_limits`; this explicit gate
    /// accounts for the simultaneous source, normalized, graph, and index
    /// representations used by the legacy whole-graph adapter.
    pub fn stage_graph_with_materialization_limit(
        &mut self,
        options: BuildOptions,
        merge_limits: FactBatchMergeLimits,
        max_materialized_bytes: usize,
    ) -> anyhow::Result<StagedGraphOutput> {
        self.stage_graph_with_materialization_limit_and_root(
            options,
            merge_limits,
            max_materialized_bytes,
            None,
        )
    }

    /// As [`Self::stage_graph_with_materialization_limit`], retaining a
    /// caller-provided source root for the compatibility normalizer. This is
    /// required by incremental callers whose existing graph was normalized
    /// against a project root.
    pub fn stage_graph_with_materialization_limit_and_root(
        &mut self,
        options: BuildOptions,
        merge_limits: FactBatchMergeLimits,
        max_materialized_bytes: usize,
        root: Option<&Path>,
    ) -> anyhow::Result<StagedGraphOutput> {
        self.stage_graph_with_materialization_limit_and_root_and_callback(
            options,
            merge_limits,
            max_materialized_bytes,
            root,
            None,
        )
    }

    pub fn stage_graph_with_materialization_limit_and_root_and_callback(
        &mut self,
        options: BuildOptions,
        merge_limits: FactBatchMergeLimits,
        max_materialized_bytes: usize,
        root: Option<&Path>,
        on_sub_stage: Option<&crate::build::BuildSubStageCallback<'_>>,
    ) -> anyhow::Result<StagedGraphOutput> {
        if max_materialized_bytes == 0 {
            return Err(FactBatchRunStoreError::InvalidMaterializationLimit.into());
        }
        let merge_limits = merge_limits.validate()?;
        self.compact_for_merge(merge_limits)?;
        self.validate_descriptors()?;
        let persisted_payload_bytes = self.runs.iter().try_fold(0_u64, |total, run| {
            total.checked_add(run.payload_bytes).ok_or(
                FactBatchRunStoreError::MaterializationLimitExceeded {
                    estimated_bytes: u64::MAX,
                    max_materialized_bytes,
                },
            )
        })?;
        let estimated_bytes = persisted_payload_bytes
            .checked_mul(FACT_MATERIALIZATION_WORKING_SET_MULTIPLIER)
            .ok_or(FactBatchRunStoreError::MaterializationLimitExceeded {
                estimated_bytes: u64::MAX,
                max_materialized_bytes,
            })?;
        if estimated_bytes > max_materialized_bytes as u64 {
            return Err(FactBatchRunStoreError::MaterializationLimitExceeded {
                estimated_bytes,
                max_materialized_bytes,
            }
            .into());
        }
        let paths = self
            .runs
            .iter()
            .map(|descriptor| self.run_path(descriptor))
            .collect();
        let mut merge = MergedFactBatchRuns::open(paths, self.batch_limits)?;
        let mut batches = Vec::new();
        while let Some(batch) = merge.next_batch()? {
            batches.push(batch);
        }
        StagedGraphOutput::from_fact_batches_with_root_and_callback(batches, options, root, on_sub_stage)
    }

    fn write_merged_group(
        &mut self,
        descriptors: &[RunDescriptor],
    ) -> Result<RunDescriptor, FactBatchRunStoreError> {
        let paths = descriptors
            .iter()
            .map(|descriptor| self.run_path(descriptor))
            .collect();
        let mut merge = MergedFactBatchRuns::open(paths, self.batch_limits)?;
        let mut writer = self.new_run_writer()?;
        while let Some(batch) = merge.next_batch()? {
            writer.write_batch(&batch)?;
        }
        writer.finish()
    }

    fn write_batches(
        &mut self,
        batches: impl IntoIterator<Item = FactBatch>,
    ) -> Result<RunDescriptor, FactBatchRunStoreError> {
        let mut writer = self.new_run_writer()?;
        for batch in batches {
            writer.write_batch(&batch)?;
        }
        writer.finish()
    }

    fn new_run_writer(&mut self) -> Result<RunFileWriter, FactBatchRunStoreError> {
        let file = format!("run-{:020}.gxr", self.next_run_id);
        self.next_run_id = self.next_run_id.checked_add(1).ok_or_else(|| {
            FactBatchRunStoreError::InvalidManifest("run identifier overflow".into())
        })?;
        RunFileWriter::new(self.root.join(FACT_RUN_DIRECTORY).join(file))
    }

    fn run_path(&self, descriptor: &RunDescriptor) -> PathBuf {
        self.root.join(FACT_RUN_DIRECTORY).join(&descriptor.file)
    }

    fn validate_descriptors(&self) -> Result<(), FactBatchRunStoreError> {
        for descriptor in &self.runs {
            let path = self.run_path(descriptor);
            safe_regular_file_len(&path)?.ok_or_else(|| {
                FactBatchRunStoreError::InvalidManifest(format!(
                    "referenced run {} is missing",
                    path.display()
                ))
            })?;
            validate_run_descriptor(&path, self.batch_limits, descriptor)?;
        }
        Ok(())
    }

    fn persist_manifest(&self) -> Result<(), FactBatchRunStoreError> {
        self.persist_manifest_with_runs(&self.runs)
    }

    fn persist_manifest_with_runs(
        &self,
        runs: &[RunDescriptor],
    ) -> Result<(), FactBatchRunStoreError> {
        let manifest = RunStoreManifest {
            version: FACT_RUN_VERSION,
            batch_limits: self.batch_limits,
            run_limits: self.run_limits,
            runs: runs.to_vec(),
        };
        graphoxide_core::write_json_atomic(self.root.join(FACT_RUN_MANIFEST), &manifest, false)
            .map_err(|error| FactBatchRunStoreError::Io(io::Error::other(error)))
    }
}

fn validate_run_file_name(file: &str) -> Result<(), FactBatchRunStoreError> {
    let path = Path::new(file);
    if path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(FactBatchRunStoreError::InvalidManifest(format!(
            "run filename {file:?} is not a plain filename"
        )));
    }
    Ok(())
}

fn ensure_directory_not_symlink(path: &Path) -> Result<(), FactBatchRunStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(FactBatchRunStoreError::UnsafePath {
                path: path.to_path_buf(),
            })
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            ensure_existing_directory_not_symlink(path)
        }
        Err(error) => Err(error.into()),
    }
}

fn ensure_existing_directory_not_symlink(path: &Path) -> Result<(), FactBatchRunStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(FactBatchRunStoreError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn ensure_owned_subdirectory(
    base: &Path,
    relative: &Path,
) -> Result<PathBuf, FactBatchRunStoreError> {
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(FactBatchRunStoreError::UnsafePath {
                path: base.join(relative),
            });
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(FactBatchRunStoreError::UnsafePath {
                    path: current.clone(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                ensure_existing_directory_not_symlink(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

fn safe_regular_file_len(path: &Path) -> Result<Option<u64>, FactBatchRunStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(FactBatchRunStoreError::UnsafePath {
                path: path.to_path_buf(),
            })
        }
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

struct RunFileWriter {
    temporary: Option<PathBuf>,
    destination: PathBuf,
    writer: Option<BufWriter<File>>,
    batches: usize,
    payload_bytes: u64,
}

impl RunFileWriter {
    fn new(destination: PathBuf) -> Result<Self, FactBatchRunStoreError> {
        let parent = destination.parent().ok_or_else(|| {
            FactBatchRunStoreError::InvalidManifest("run destination has no parent".into())
        })?;
        ensure_existing_directory_not_symlink(parent)?;
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(FactBatchRunStoreError::UnsafePath { path: destination });
        }
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                FactBatchRunStoreError::InvalidManifest("run destination is not UTF-8".into())
            })?;
        let temporary = loop {
            let sequence = FACT_RUN_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => break (candidate, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let mut writer = BufWriter::new(temporary.1);
        writer.write_all(&FACT_RUN_MAGIC)?;
        writer.write_all(&FACT_RUN_VERSION.to_le_bytes())?;
        Ok(Self {
            temporary: Some(temporary.0),
            destination,
            writer: Some(writer),
            batches: 0,
            payload_bytes: 0,
        })
    }

    fn write_batch(&mut self, batch: &FactBatch) -> Result<(), FactBatchRunStoreError> {
        let payload = serde_json::to_vec(batch.extraction())?;
        let payload_len = u64::try_from(payload.len()).map_err(|_| {
            FactBatchRunStoreError::InvalidManifest("batch payload exceeds u64".into())
        })?;
        let mut checksum = Crc32::new();
        checksum.update(&payload);
        let writer = self
            .writer
            .as_mut()
            .expect("run writer is available before finish");
        writer.write_all(&batch.key().source_ordinal.to_le_bytes())?;
        writer.write_all(&batch.key().batch_ordinal.to_le_bytes())?;
        writer.write_all(&payload_len.to_le_bytes())?;
        writer.write_all(&checksum.finalize().to_le_bytes())?;
        writer.write_all(&payload)?;
        self.batches = self.batches.checked_add(1).ok_or_else(|| {
            FactBatchRunStoreError::InvalidManifest("batch count overflow".into())
        })?;
        self.payload_bytes = self.payload_bytes.checked_add(payload_len).ok_or_else(|| {
            FactBatchRunStoreError::InvalidManifest("payload byte count overflow".into())
        })?;
        Ok(())
    }

    fn finish(mut self) -> Result<RunDescriptor, FactBatchRunStoreError> {
        let mut writer = self
            .writer
            .take()
            .expect("run writer is available before finish");
        writer.flush()?;
        writer.get_ref().sync_all()?;
        let temporary = self
            .temporary
            .take()
            .expect("run writer owns its temporary path");
        drop(writer);
        fs::rename(&temporary, &self.destination)?;
        let file = self
            .destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                FactBatchRunStoreError::InvalidManifest("run destination is not UTF-8".into())
            })?
            .to_owned();
        Ok(RunDescriptor {
            file,
            batches: self.batches,
            payload_bytes: self.payload_bytes,
        })
    }
}

impl Drop for RunFileWriter {
    fn drop(&mut self) {
        if let Some(temporary) = self.temporary.take() {
            let _ = fs::remove_file(temporary);
        }
    }
}

#[derive(Debug)]
struct FactBatchRunReader {
    path: PathBuf,
    reader: BufReader<File>,
    batch_limits: FactBatchLimits,
    previous: Option<FactBatchKey>,
    batches_read: usize,
    payload_bytes_read: u64,
}

impl FactBatchRunReader {
    fn open(path: PathBuf, batch_limits: FactBatchLimits) -> Result<Self, FactBatchRunStoreError> {
        safe_regular_file_len(&path)?.ok_or_else(|| {
            FactBatchRunStoreError::InvalidManifest(format!(
                "referenced run {} is missing",
                path.display()
            ))
        })?;
        let mut reader = BufReader::new(File::open(&path)?);
        let mut header = [0u8; FACT_RUN_HEADER_BYTES];
        reader
            .read_exact(&mut header)
            .map_err(|error| match error.kind() {
                io::ErrorKind::UnexpectedEof => {
                    FactBatchRunStoreError::TruncatedFrame { path: path.clone() }
                }
                _ => error.into(),
            })?;
        if header[..FACT_RUN_MAGIC.len()] != FACT_RUN_MAGIC {
            return Err(FactBatchRunStoreError::InvalidHeader { path });
        }
        let version = u32::from_le_bytes(
            header[FACT_RUN_MAGIC.len()..]
                .try_into()
                .expect("header slice"),
        );
        if version != FACT_RUN_VERSION {
            return Err(FactBatchRunStoreError::UnsupportedVersion { path, version });
        }
        Ok(Self {
            path,
            reader,
            batch_limits,
            previous: None,
            batches_read: 0,
            payload_bytes_read: 0,
        })
    }

    fn next_batch(&mut self) -> Result<Option<FactBatch>, FactBatchRunStoreError> {
        let mut prefix = [0u8; 8];
        let count = self.reader.read(&mut prefix)?;
        if count == 0 {
            return Ok(None);
        }
        self.reader.read_exact(&mut prefix[count..]).map_err(|_| {
            FactBatchRunStoreError::TruncatedFrame {
                path: self.path.clone(),
            }
        })?;
        let mut rest = [0u8; FACT_RUN_FRAME_HEADER_BYTES - 8];
        self.reader
            .read_exact(&mut rest)
            .map_err(|_| FactBatchRunStoreError::TruncatedFrame {
                path: self.path.clone(),
            })?;
        let source_ordinal = u64::from_le_bytes(prefix);
        let batch_ordinal = u32::from_le_bytes(rest[..4].try_into().expect("frame slice"));
        let payload_len_u64 = u64::from_le_bytes(rest[4..12].try_into().expect("frame slice"));
        let checksum = u32::from_le_bytes(rest[12..].try_into().expect("frame slice"));
        let payload_len = usize::try_from(payload_len_u64).map_err(|_| {
            FactBatchRunStoreError::FrameTooLarge {
                path: self.path.clone(),
            }
        })?;
        let maximum_frame_bytes = self
            .batch_limits
            .max_estimated_bytes
            .saturating_add(self.batch_limits.max_facts)
            .saturating_add(4 * 1024);
        if payload_len > maximum_frame_bytes {
            return Err(FactBatchRunStoreError::FrameTooLarge {
                path: self.path.clone(),
            });
        }
        let mut payload = vec![0u8; payload_len];
        self.reader.read_exact(&mut payload).map_err(|_| {
            FactBatchRunStoreError::TruncatedFrame {
                path: self.path.clone(),
            }
        })?;
        let mut hasher = Crc32::new();
        hasher.update(&payload);
        if hasher.finalize() != checksum {
            return Err(FactBatchRunStoreError::ChecksumMismatch {
                path: self.path.clone(),
            });
        }
        let key = FactBatchKey::new(source_ordinal, batch_ordinal);
        if self.previous.is_some_and(|previous| previous >= key) {
            return Err(FactBatchRunStoreError::NonIncreasingKey {
                path: self.path.clone(),
                key_source: source_ordinal,
                key_batch: batch_ordinal,
            });
        }
        let extraction = serde_json::from_slice(&payload)?;
        let batch = FactBatch::try_new(key, extraction, self.batch_limits)?;
        self.previous = Some(key);
        self.batches_read = self.batches_read.checked_add(1).ok_or_else(|| {
            FactBatchRunStoreError::InvalidManifest("run batch count overflow".into())
        })?;
        self.payload_bytes_read = self
            .payload_bytes_read
            .checked_add(payload_len_u64)
            .ok_or_else(|| {
                FactBatchRunStoreError::InvalidManifest("run payload byte count overflow".into())
            })?;
        Ok(Some(batch))
    }
}

fn validate_run_descriptor(
    path: &Path,
    batch_limits: FactBatchLimits,
    descriptor: &RunDescriptor,
) -> Result<(), FactBatchRunStoreError> {
    let mut reader = FactBatchRunReader::open(path.to_path_buf(), batch_limits)?;
    while reader.next_batch()?.is_some() {}
    if reader.batches_read != descriptor.batches
        || reader.payload_bytes_read != descriptor.payload_bytes
    {
        return Err(FactBatchRunStoreError::DescriptorMismatch {
            path: path.to_path_buf(),
            reported_batches: descriptor.batches,
            reported_payload_bytes: descriptor.payload_bytes,
            actual_batches: reader.batches_read,
            actual_payload_bytes: reader.payload_bytes_read,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MergeHead {
    key: FactBatchKey,
    reader_index: usize,
}

/// Deterministic k-way merge over persisted run files.
///
/// At most one decoded batch is retained per run file, and all duplicate keys
/// are rejected before they can make scheduler timing observable in graph
/// output.
#[derive(Debug)]
pub struct MergedFactBatchRuns {
    readers: Vec<FactBatchRunReader>,
    pending: Vec<Option<FactBatch>>,
    heads: BinaryHeap<Reverse<MergeHead>>,
    previous: Option<FactBatchKey>,
}

impl MergedFactBatchRuns {
    fn open(
        paths: Vec<PathBuf>,
        batch_limits: FactBatchLimits,
    ) -> Result<Self, FactBatchRunStoreError> {
        let mut readers = Vec::with_capacity(paths.len());
        let mut pending = Vec::with_capacity(paths.len());
        let mut heads = BinaryHeap::with_capacity(paths.len());
        for path in paths {
            let mut reader = FactBatchRunReader::open(path, batch_limits)?;
            let index = readers.len();
            if let Some(batch) = reader.next_batch()? {
                heads.push(Reverse(MergeHead {
                    key: batch.key(),
                    reader_index: index,
                }));
                pending.push(Some(batch));
            } else {
                pending.push(None);
            }
            readers.push(reader);
        }
        Ok(Self {
            readers,
            pending,
            heads,
            previous: None,
        })
    }

    pub fn next_batch(&mut self) -> Result<Option<FactBatch>, FactBatchRunStoreError> {
        let Some(Reverse(head)) = self.heads.pop() else {
            return Ok(None);
        };
        let batch = self.pending[head.reader_index]
            .take()
            .expect("merge head must own one pending batch");
        if self
            .previous
            .is_some_and(|previous| previous == batch.key())
        {
            return Err(FactBatchRunStoreError::DuplicateKey {
                key_source: batch.key().source_ordinal,
                key_batch: batch.key().batch_ordinal,
            });
        }
        self.previous = Some(batch.key());
        if let Some(next) = self.readers[head.reader_index].next_batch()? {
            self.heads.push(Reverse(MergeHead {
                key: next.key(),
                reader_index: head.reader_index,
            }));
            self.pending[head.reader_index] = Some(next);
        }
        Ok(Some(batch))
    }
}

/// Build a graph from bounded batches while preserving the existing graph
/// builder's normalization, merge, deduplication, and output behavior.
pub fn build_graph_from_fact_batches(
    batches: impl IntoIterator<Item = FactBatch>,
    options: BuildOptions,
) -> anyhow::Result<(KnowledgeGraph, BuildReport)> {
    build_graph_from_fact_batches_with_root(batches, options, None)
}

/// Build a graph from bounded fact batches while retaining the caller's
/// project-root normalization policy.
pub fn build_graph_from_fact_batches_with_root(
    batches: impl IntoIterator<Item = FactBatch>,
    options: BuildOptions,
    root: Option<&Path>,
) -> anyhow::Result<(KnowledgeGraph, BuildReport)> {
    build_graph_from_fact_batches_with_root_and_callback(batches, options, root, None)
}

pub fn build_graph_from_fact_batches_with_root_and_callback(
    batches: impl IntoIterator<Item = FactBatch>,
    options: BuildOptions,
    root: Option<&Path>,
    on_sub_stage: Option<&crate::build::BuildSubStageCallback<'_>>,
) -> anyhow::Result<(KnowledgeGraph, BuildReport)> {
    let mut batches: Vec<_> = batches.into_iter().collect();
    sort_fact_batches(&mut batches)?;
    let mut sources = BTreeMap::<u64, Extraction>::new();
    for batch in batches {
        let extraction = sources.entry(batch.key().source_ordinal).or_default();
        let batch = batch.into_extraction();
        extraction.nodes.extend(batch.nodes);
        extraction.edges.extend(batch.edges);
        extraction.hyperedges.extend(batch.hyperedges);
    }
    let extractions = sources.into_values().collect::<Vec<_>>();
    if let Some(root) = root {
        build_graph_with_report_and_options_and_root_with_callback(&extractions, root, options, on_sub_stage)
    } else {
        build_graph_with_report_and_options(&extractions, options)
    }
}

/// Explicit resource bounds for full-graph clustering.
///
/// Clustering currently requires an in-memory graph view. The gate runs before
/// clustering begins, so a large graph fails before output publication rather
/// than overcommitting the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterResourceLimits {
    pub max_nodes: usize,
    pub max_edges: usize,
}

impl Default for ClusterResourceLimits {
    fn default() -> Self {
        Self {
            max_nodes: 1_000_000,
            max_edges: 8_000_000,
        }
    }
}

impl ClusterResourceLimits {
    pub fn validate(self) -> Result<Self, ClusterResourceError> {
        if self.max_nodes == 0 {
            return Err(ClusterResourceError::InvalidLimit { field: "max_nodes" });
        }
        if self.max_edges == 0 {
            return Err(ClusterResourceError::InvalidLimit { field: "max_edges" });
        }
        Ok(self)
    }

    pub fn check(self, graph: &KnowledgeGraph) -> Result<(), ClusterResourceError> {
        let limits = self.validate()?;
        if graph.nodes.len() > limits.max_nodes || graph.links.len() > limits.max_edges {
            return Err(ClusterResourceError::Exceeded {
                nodes: graph.nodes.len(),
                edges: graph.links.len(),
                max_nodes: limits.max_nodes,
                max_edges: limits.max_edges,
            });
        }
        Ok(())
    }
}

/// Failure while checking the bounded in-memory clustering view.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClusterResourceError {
    #[error("cluster resource limit {field} must be greater than zero")]
    InvalidLimit { field: &'static str },
    #[error(
        "clustering requires {nodes} nodes and {edges} edges, exceeding configured limits of {max_nodes} nodes and {max_edges} edges"
    )]
    Exceeded {
        nodes: usize,
        edges: usize,
        max_nodes: usize,
        max_edges: usize,
    },
}

/// A complete, validated graph awaiting an atomic output commit.
///
/// This is intentionally a staging boundary rather than a streaming JSON
/// writer. The current graph export applies whole-graph transformations before
/// serialization; exposing a stage object lets the future I/O plane own the
/// actual write without changing graph shape or commit atomicity today.
#[derive(Debug, Clone)]
pub struct StagedGraphOutput {
    graph: KnowledgeGraph,
    report: BuildReport,
}

impl StagedGraphOutput {
    pub fn from_fact_batches(
        batches: impl IntoIterator<Item = FactBatch>,
        options: BuildOptions,
    ) -> anyhow::Result<Self> {
        Self::from_fact_batches_with_root(batches, options, None)
    }

    /// Build a staged graph from bounded facts using the same root-aware
    /// normalization policy as the compatibility builder.
    pub fn from_fact_batches_with_root(
        batches: impl IntoIterator<Item = FactBatch>,
        options: BuildOptions,
        root: Option<&Path>,
    ) -> anyhow::Result<Self> {
        Self::from_fact_batches_with_root_and_callback(batches, options, root, None)
    }

    pub fn from_fact_batches_with_root_and_callback(
        batches: impl IntoIterator<Item = FactBatch>,
        options: BuildOptions,
        root: Option<&Path>,
        on_sub_stage: Option<&crate::build::BuildSubStageCallback<'_>>,
    ) -> anyhow::Result<Self> {
        let (graph, report) =
            build_graph_from_fact_batches_with_root_and_callback(batches, options, root, on_sub_stage)?;
        Ok(Self { graph, report })
    }

    /// Build a staged graph from persisted, externally merged fact runs.
    pub fn from_run_store(
        store: &mut FactBatchRunStore,
        options: BuildOptions,
        merge_limits: FactBatchMergeLimits,
    ) -> anyhow::Result<Self> {
        store.stage_graph(options, merge_limits)
    }

    /// Stage a compatibility graph only when the persisted input fits the
    /// caller's explicit materialization budget.
    pub fn from_run_store_with_materialization_limit(
        store: &mut FactBatchRunStore,
        options: BuildOptions,
        merge_limits: FactBatchMergeLimits,
        max_materialized_bytes: usize,
    ) -> anyhow::Result<Self> {
        Self::from_run_store_with_materialization_limit_and_root(
            store,
            options,
            merge_limits,
            max_materialized_bytes,
            None,
        )
    }

    /// Stage a compatibility graph from persisted runs under an explicit
    /// materialization budget while retaining source-root normalization.
    pub fn from_run_store_with_materialization_limit_and_root(
        store: &mut FactBatchRunStore,
        options: BuildOptions,
        merge_limits: FactBatchMergeLimits,
        max_materialized_bytes: usize,
        root: Option<&Path>,
    ) -> anyhow::Result<Self> {
        store.stage_graph_with_materialization_limit_and_root(
            options,
            merge_limits,
            max_materialized_bytes,
            root,
        )
    }

    pub fn from_run_store_with_materialization_limit_and_root_and_callback(
        store: &mut FactBatchRunStore,
        options: BuildOptions,
        merge_limits: FactBatchMergeLimits,
        max_materialized_bytes: usize,
        root: Option<&Path>,
        on_sub_stage: Option<&crate::build::BuildSubStageCallback<'_>>,
    ) -> anyhow::Result<Self> {
        store.stage_graph_with_materialization_limit_and_root_and_callback(
            options,
            merge_limits,
            max_materialized_bytes,
            root,
            on_sub_stage,
        )
    }

    pub fn graph(&self) -> &KnowledgeGraph {
        &self.graph
    }

    pub fn graph_mut(&mut self) -> &mut KnowledgeGraph {
        &mut self.graph
    }

    pub fn report(&self) -> &BuildReport {
        &self.report
    }

    pub fn into_parts(self) -> (KnowledgeGraph, BuildReport) {
        (self.graph, self.report)
    }

    /// Run clustering only after the full graph fits the explicit stage bound.
    pub fn cluster_with_limits(&mut self, limits: ClusterResourceLimits) -> anyhow::Result<()> {
        limits.check(&self.graph)?;
        crate::cluster(&mut self.graph)
    }

    /// Commit the staged graph through the established symlink-safe atomic
    /// writer. `false` preserves the existing shrink-protection behavior.
    pub fn commit_atomic(self, path: impl AsRef<Path>, force: bool) -> anyhow::Result<bool> {
        write_graph_atomic(path, &self.graph, force)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn batch(key: FactBatchKey, id: &str) -> FactBatch {
        FactBatch::try_new(
            key,
            Extraction {
                nodes: vec![Node {
                    id: id.into(),
                    label: id.into(),
                    file_type: "code".into(),
                    source_file: "src/example.rs".into(),
                    source_location: None,
                    community: None,
                    extra: BTreeMap::new(),
                }],
                ..Extraction::default()
            },
            FactBatchLimits::default(),
        )
        .expect("small test batch")
    }

    #[test]
    fn run_builder_spills_sorted_bounded_runs() {
        let mut builder = FactBatchRunBuilder::new(FactBatchRunLimits {
            max_batches: 2,
            max_estimated_bytes: DEFAULT_FACT_RUN_MAX_BYTES,
        })
        .expect("valid run limits");

        assert!(builder
            .push(batch(FactBatchKey::new(2, 0), "two"))
            .expect("first admission")
            .is_none());
        assert!(builder
            .push(batch(FactBatchKey::new(1, 0), "one"))
            .expect("second admission")
            .is_none());
        let first = builder
            .push(batch(FactBatchKey::new(3, 0), "three"))
            .expect("spill admission")
            .expect("run is spilled at the batch bound");

        assert_eq!(first.first_key(), Some(FactBatchKey::new(1, 0)));
        assert_eq!(first.last_key(), Some(FactBatchKey::new(2, 0)));
        assert_eq!(first.batches().len(), 2);
        assert!(first.estimated_bytes() <= DEFAULT_FACT_RUN_MAX_BYTES);

        let final_run = builder.finish().expect("finish").expect("remaining run");
        assert_eq!(final_run.first_key(), Some(FactBatchKey::new(3, 0)));
        assert_eq!(final_run.batches().len(), 1);
        assert!(builder.finish().expect("empty finish").is_none());
    }

    #[test]
    fn run_builder_rejects_duplicate_keys_before_spill() {
        let mut builder =
            FactBatchRunBuilder::new(FactBatchRunLimits::default()).expect("valid run limits");
        let key = FactBatchKey::new(7, 0);
        builder.push(batch(key, "one")).expect("first admission");
        builder.push(batch(key, "two")).expect("second admission");

        assert!(matches!(
            builder.finish(),
            Err(FactBatchRunError::Ordering(FactBatchOrderError::DuplicateKey(found))) if found == key
        ));
        assert!(matches!(
            builder.finish(),
            Err(FactBatchRunError::Ordering(FactBatchOrderError::DuplicateKey(found))) if found == key
        ));
    }

    #[test]
    fn run_builder_enforces_byte_bound_before_retaining_batch() {
        let batch = batch(FactBatchKey::new(1, 0), "one");
        let limit = batch.estimated_bytes().saturating_sub(1);
        let mut builder = FactBatchRunBuilder::new(FactBatchRunLimits {
            max_batches: 1,
            max_estimated_bytes: limit,
        })
        .expect("non-zero run limit");

        assert!(matches!(
            builder.push(batch),
            Err(FactBatchRunError::BatchTooLarge { .. })
        ));
        assert!(builder.finish().expect("empty builder").is_none());
    }

    fn store(temp: &TempDir) -> FactBatchRunStore {
        FactBatchRunStore::create(
            temp.path().join("fact-runs"),
            FactBatchLimits::default(),
            FactBatchRunLimits {
                max_batches: 1,
                max_estimated_bytes: DEFAULT_FACT_RUN_MAX_BYTES,
            },
        )
        .expect("create persistent run store")
    }

    fn append_single_batch_runs(store: &mut FactBatchRunStore, batches: Vec<FactBatch>) {
        for batch in batches {
            let mut builder = FactBatchRunBuilder::new(FactBatchRunLimits {
                max_batches: 1,
                max_estimated_bytes: DEFAULT_FACT_RUN_MAX_BYTES,
            })
            .expect("valid run limits");
            builder.push(batch).expect("admit batch");
            store
                .append_run(builder.finish().expect("finish").expect("one run"))
                .expect("persist run");
        }
    }

    #[test]
    fn persisted_runs_reopen_compact_and_merge_in_global_key_order() {
        let temp = TempDir::new().expect("temporary directory");
        let mut run_store = store(&temp);
        append_single_batch_runs(
            &mut run_store,
            vec![
                batch(FactBatchKey::new(4, 0), "four"),
                batch(FactBatchKey::new(1, 0), "one"),
                batch(FactBatchKey::new(3, 0), "three"),
                batch(FactBatchKey::new(0, 0), "zero"),
                batch(FactBatchKey::new(2, 0), "two"),
            ],
        );
        let root = run_store.root().to_path_buf();
        drop(run_store);

        let mut reopened = FactBatchRunStore::open(root).expect("reopen durable manifest");
        assert_eq!(reopened.run_count(), 5);
        let mut merged = reopened
            .merged_batches(FactBatchMergeLimits { max_open_runs: 2 })
            .expect("compact and merge");
        let mut keys = Vec::new();
        while let Some(batch) = merged.next_batch().expect("valid frame") {
            keys.push(batch.key());
        }
        assert_eq!(
            keys,
            vec![
                FactBatchKey::new(0, 0),
                FactBatchKey::new(1, 0),
                FactBatchKey::new(2, 0),
                FactBatchKey::new(3, 0),
                FactBatchKey::new(4, 0),
            ]
        );
        assert_eq!(reopened.run_count(), 2);
    }

    #[test]
    fn reopen_validates_manifest_descriptors_against_actual_frames() {
        let temp = TempDir::new().expect("temporary directory");
        let mut run_store = store(&temp);
        append_single_batch_runs(&mut run_store, vec![batch(FactBatchKey::new(0, 0), "one")]);
        let root = run_store.root().to_path_buf();
        drop(run_store);

        let manifest_path = root.join(FACT_RUN_MANIFEST);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
                .expect("decode manifest");
        manifest["runs"][0]["payload_bytes"] = serde_json::json!(0);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("encode tampered manifest"),
        )
        .expect("write tampered manifest");

        assert!(matches!(
            FactBatchRunStore::open(root),
            Err(FactBatchRunStoreError::DescriptorMismatch { .. })
        ));
    }

    #[test]
    fn failed_manifest_publication_does_not_mutate_append_or_compaction_state() {
        let temp = TempDir::new().expect("temporary directory");
        let mut run_store = store(&temp);
        let root = run_store.root().to_path_buf();
        let manifest = root.join(FACT_RUN_MANIFEST);
        let manifest_backup = root.join("manifest.backup");
        fs::rename(&manifest, &manifest_backup).expect("move manifest aside");
        fs::create_dir(&manifest).expect("block manifest replacement");

        let mut builder = FactBatchRunBuilder::new(FactBatchRunLimits::default()).expect("builder");
        builder
            .push(batch(FactBatchKey::new(0, 0), "failed"))
            .expect("admit failed append");
        assert!(run_store
            .append_run(builder.finish().expect("finish").expect("run"))
            .is_err());
        assert_eq!(run_store.run_count(), 0);

        fs::remove_dir(&manifest).expect("remove manifest blocker");
        fs::rename(&manifest_backup, &manifest).expect("restore manifest");
        append_single_batch_runs(
            &mut run_store,
            vec![
                batch(FactBatchKey::new(1, 0), "one"),
                batch(FactBatchKey::new(2, 0), "two"),
                batch(FactBatchKey::new(3, 0), "three"),
            ],
        );
        assert_eq!(run_store.run_count(), 3);

        fs::rename(&manifest, &manifest_backup).expect("move manifest aside again");
        fs::create_dir(&manifest).expect("block compaction manifest replacement");
        assert!(run_store
            .compact_for_merge(FactBatchMergeLimits { max_open_runs: 2 })
            .is_err());
        assert_eq!(run_store.run_count(), 3);
        fs::remove_dir(&manifest).expect("remove second blocker");
        fs::rename(&manifest_backup, &manifest).expect("restore second manifest");
        drop(run_store);

        assert_eq!(
            FactBatchRunStore::open(root)
                .expect("reopen last published state")
                .run_count(),
            3
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_run_directory_is_refused() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temporary directory");
        let outside = TempDir::new().expect("outside directory");
        let root = temp.path().join("unsafe-store");
        fs::create_dir(&root).expect("store root");
        symlink(outside.path(), root.join(FACT_RUN_DIRECTORY)).expect("malicious runs symlink");

        assert!(matches!(
            FactBatchRunStore::create(
                root,
                FactBatchLimits::default(),
                FactBatchRunLimits::default()
            ),
            Err(FactBatchRunStoreError::UnsafePath { .. })
        ));
        assert!(fs::read_dir(outside.path())
            .expect("outside remains readable")
            .next()
            .is_none());
    }

    #[test]
    fn persisted_frame_checksum_failure_is_a_safe_merge_error() {
        let temp = TempDir::new().expect("temporary directory");
        let mut run_store = store(&temp);
        append_single_batch_runs(&mut run_store, vec![batch(FactBatchKey::new(0, 0), "one")]);
        let path = run_store.run_paths().pop().expect("one run path");
        let mut bytes = fs::read(&path).expect("read run");
        let last = bytes.last_mut().expect("payload byte");
        *last ^= 0xff;
        fs::write(&path, bytes).expect("corrupt frame for test");

        let error = run_store
            .merged_batches(FactBatchMergeLimits::default())
            .expect_err("checksum failure must not be accepted");
        assert!(matches!(
            error,
            FactBatchRunStoreError::ChecksumMismatch { .. }
        ));
    }

    #[test]
    fn persisted_frame_length_is_bounded_before_allocation() {
        let temp = TempDir::new().expect("temporary directory");
        let mut run_store = store(&temp);
        append_single_batch_runs(&mut run_store, vec![batch(FactBatchKey::new(0, 0), "one")]);
        let path = run_store.run_paths().pop().expect("one run path");
        let mut bytes = fs::read(&path).expect("read run");
        let payload_length_offset = FACT_RUN_HEADER_BYTES + 8 + 4;
        bytes[payload_length_offset..payload_length_offset + 8]
            .copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(&path, bytes).expect("replace frame length for test");

        let error = run_store
            .merged_batches(FactBatchMergeLimits::default())
            .expect_err("oversized frame must be rejected before allocation");
        assert!(matches!(
            error,
            FactBatchRunStoreError::FrameTooLarge { .. }
        ));
    }

    #[test]
    fn graph_stage_budget_rejects_before_compatibility_materialization() {
        let temp = TempDir::new().expect("temporary directory");
        let mut run_store = store(&temp);
        append_single_batch_runs(
            &mut run_store,
            vec![batch(FactBatchKey::new(0, 0), "materialized")],
        );
        let persisted_payload_bytes = run_store
            .runs
            .iter()
            .map(|run| run.payload_bytes)
            .sum::<u64>();
        let payload_only_limit = usize::try_from(persisted_payload_bytes.saturating_mul(2))
            .expect("small fixture payload");
        assert!(
            persisted_payload_bytes < payload_only_limit as u64,
            "the fixture must fit a payload-only gate"
        );

        let error = run_store
            .stage_graph_with_materialization_limit(
                BuildOptions::default(),
                FactBatchMergeLimits::default(),
                payload_only_limit,
            )
            .expect_err("a graph stage cannot exceed its explicit byte budget");
        assert!(matches!(
            error.downcast_ref::<FactBatchRunStoreError>(),
            Some(FactBatchRunStoreError::MaterializationLimitExceeded {
                max_materialized_bytes,
                ..
            }) if *max_materialized_bytes == payload_only_limit
        ));
    }

    #[test]
    fn persisted_streaming_stage_matches_compatibility_graph_bytes() {
        let first = Extraction {
            nodes: vec![
                Node {
                    id: "a".into(),
                    label: "A".into(),
                    file_type: "code".into(),
                    source_file: "src/a.rs".into(),
                    source_location: None,
                    community: None,
                    extra: BTreeMap::new(),
                },
                Node {
                    id: "b".into(),
                    label: "B".into(),
                    file_type: "code".into(),
                    source_file: "src/a.rs".into(),
                    source_location: None,
                    community: None,
                    extra: BTreeMap::new(),
                },
            ],
            edges: vec![Edge {
                source: "a".into(),
                target: "b".into(),
                relation: "calls".into(),
                confidence: graphoxide_core::Confidence::Extracted,
                source_file: "src/a.rs".into(),
                extra: BTreeMap::new(),
            }],
            ..Extraction::default()
        };
        let second = Extraction {
            nodes: vec![Node {
                id: "c".into(),
                label: "C".into(),
                file_type: "code".into(),
                source_file: "src/c.rs".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::new(),
            }],
            edges: vec![Edge {
                source: "b".into(),
                target: "c".into(),
                relation: "calls".into(),
                confidence: graphoxide_core::Confidence::Extracted,
                source_file: "src/c.rs".into(),
                extra: BTreeMap::new(),
            }],
            ..Extraction::default()
        };
        let expected = build_graph_with_report_and_options(
            &[first.clone(), second.clone()],
            BuildOptions::default(),
        )
        .expect("compatibility graph")
        .0;

        let temp = TempDir::new().expect("temporary directory");
        let mut run_store = FactBatchRunStore::create(
            temp.path().join("fact-runs"),
            FactBatchLimits {
                max_facts: 1,
                max_estimated_bytes: DEFAULT_FACT_BATCH_MAX_BYTES,
            },
            FactBatchRunLimits {
                max_batches: 2,
                max_estimated_bytes: DEFAULT_FACT_RUN_MAX_BYTES,
            },
        )
        .expect("create run store");
        let mut builder = FactBatchRunBuilder::new(FactBatchRunLimits {
            max_batches: 2,
            max_estimated_bytes: DEFAULT_FACT_RUN_MAX_BYTES,
        })
        .expect("valid run limits");
        for (source, extraction) in [first, second].into_iter().enumerate() {
            for batch in FactBatch::split_extraction(
                u64::try_from(source).expect("source ordinal"),
                extraction,
                FactBatchLimits {
                    max_facts: 1,
                    max_estimated_bytes: DEFAULT_FACT_BATCH_MAX_BYTES,
                },
            )
            .expect("split bounded extraction")
            {
                if let Some(run) = builder.push(batch).expect("admit batch") {
                    run_store.append_run(run).expect("persist spilled run");
                }
            }
        }
        if let Some(run) = builder.finish().expect("finish runs") {
            run_store.append_run(run).expect("persist final run");
        }
        let staged = StagedGraphOutput::from_run_store(
            &mut run_store,
            BuildOptions::default(),
            FactBatchMergeLimits { max_open_runs: 2 },
        )
        .expect("stage from persisted runs");
        assert_eq!(
            serde_json::to_vec(staged.graph()).expect("stage bytes"),
            serde_json::to_vec(&expected).expect("compatibility bytes")
        );
    }

    #[test]
    fn clustering_limit_rejects_before_graph_clustering() {
        let graph = KnowledgeGraph {
            nodes: vec![
                batch(FactBatchKey::new(0, 0), "one").extraction().nodes[0].clone(),
                batch(FactBatchKey::new(1, 0), "two").extraction().nodes[0].clone(),
            ],
            ..KnowledgeGraph::default()
        };
        assert_eq!(
            ClusterResourceLimits {
                max_nodes: 1,
                max_edges: 1,
            }
            .check(&graph),
            Err(ClusterResourceError::Exceeded {
                nodes: 2,
                edges: 0,
                max_nodes: 1,
                max_edges: 1,
            })
        );
    }
}
