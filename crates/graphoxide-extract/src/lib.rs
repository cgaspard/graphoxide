//! File detection and per-file extraction.
//!
//! Port of upstream `detect.py`, `extract.py`, `extractors/*`, `cache.py`,
//! `manifest.py`. The pipeline stage contract is unchanged:
//!
//! ```text
//! collect_files(root) -> Vec<PathBuf>
//! extract(path)       -> Extraction { nodes, edges }
//! ```
//!
//! Legacy compatibility wrappers retain their existing path-based behavior.
//! The additive runtime API below admits source files through fixed I/O owners
//! and invokes byte-only extractors on fixed CPU owners.

use anyhow::Context as _;

mod bash;
mod bytes;
pub mod cache;
pub mod cargo_introspect;
mod compat;
pub mod containers;
pub mod coverage;
mod csharp;
mod dart;
pub mod detect;
mod diagrams;
mod dot;
mod dotnet;
pub mod engine;
mod engineering;
pub mod extractor_registry;
mod fallback;
mod format_adapter;
pub mod format_registry;
mod java;
mod js_resolution;
mod json_config;
pub mod languages;
pub mod llm;
pub mod manifest_ingest;
mod native;
mod office;
mod parser_budget;
mod pascal;
mod pdf;
pub mod pg_introspect;
mod php;
mod project_path;
mod protocols;
pub mod resolution;
pub mod resolver_registry;
mod ruby;
pub mod scip_ingest;
pub mod semantic_pipeline;
mod sfc;
mod simulation;
mod sql;
pub mod stale;
mod structured;
mod swift;
pub mod terraform;
pub mod vision;

pub use detect::collect_files;
pub use engine::extract;
pub use js_resolution::resolve_js_module_path;
pub use protocols::{
    extract_binary_protocol_with_binding_or_inventory, extract_bound_binary_protocol_bytes,
    BinaryProtocolKind, SchemaBindingError, VerifiedBinarySchemaBinding,
};
pub use sfc::mask_vue_non_script;
pub use terraform::extract_terraform;

/// Result of the byte-only I/O/CPU extraction substrate.
///
/// This intentionally stops before project-wide resolution, cache persistence,
/// and graph publication. Those stages still use compatibility adapters while
/// their path-based sibling probes are moved behind I/O-owned snapshots. Each
/// contained extraction was nevertheless produced without a source filesystem
/// call from a CPU extractor.
#[derive(Debug)]
pub struct RuntimeProjectExtraction {
    /// Per-file facts in deterministic normalized-path order.
    pub extractions: Vec<graphoxide_core::Extraction>,
    /// Discovery result used to create the I/O tickets.
    pub detection: detect::DetectResult,
    /// Stable diagnostics for sources rejected before CPU extraction.
    pub read_failures: Vec<graphoxide_index_runtime::FileReadFailure>,
}

const MAX_ISOLATED_PARSER_ALLOWANCE_BYTES: usize = 16 * 1024 * 1024;

/// Choose one per-file parser policy from the memory budget alone.
///
/// Worker count and corpus size are scheduling inputs, not semantic inputs:
/// changing either must not alter bounded facts or cache keys for an unchanged
/// source. A shared [`RuntimeParserAdmission`] below gates cold parses so the
/// sum of their fixed allowances never exceeds this parser pool. The deferred
/// scan reserves half of the CPU partition for source bytes retained by the
/// project resolver.
fn isolated_parser_layout(
    config: graphoxide_index_runtime::IndexRuntimeConfig,
    reserve_resolver_snapshot: bool,
) -> (usize, usize) {
    let cpu_arenas_bytes = config.memory_budget().cpu_arenas_bytes;
    let resolver_snapshot_bytes = if reserve_resolver_snapshot {
        cpu_arenas_bytes / 2
    } else {
        0
    };
    let parser_pool_bytes = cpu_arenas_bytes.saturating_sub(resolver_snapshot_bytes);
    let parser_allowance_bytes = parser_pool_bytes.clamp(1, MAX_ISOLATED_PARSER_ALLOWANCE_BYTES);
    (parser_allowance_bytes, resolver_snapshot_bytes)
}

/// Run the byte-only extraction substrate with dedicated I/O and CPU owners.
///
/// The supplied configuration is validated before any input is opened. Inputs
/// are sorted before admission and results are restored to that same order;
/// worker-count changes therefore do not perturb per-file fact order.
pub fn extract_project_with_runtime(
    root: &std::path::Path,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
) -> anyhow::Result<RuntimeProjectExtraction> {
    use graphoxide_index_runtime::{read_files_concurrently, FileReadRequest, InputIdentity};
    use std::{collections::BTreeMap, sync::Arc};

    // Office containers must enter the same bounded byte-admission path as
    // every other isolated source; legacy sidecar conversion remains
    // available through the explicit non-isolated entrypoint.
    let detect_options = detect::DetectOptions {
        convert_office_sidecars: false,
        ..detect::DetectOptions::default()
    };
    let detection = detect::detect(root, &detect_options)?;
    let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut files = detection
        .files
        .values()
        .flatten()
        .map(std::path::PathBuf::from)
        .filter(|path| detection.is_supported_source(path))
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();

    let mut contexts = BTreeMap::new();
    for path in files {
        let physical_path = detection.physical_source(&path);
        let relative = path
            .strip_prefix(&resolved_root)
            .or_else(|_| path.strip_prefix(root))
            .map_or_else(
                |_| normalized_project_key(&path, &resolved_root, root),
                |relative| normalized_project_key(relative, &resolved_root, root),
            );
        if let Some((existing, _)) =
            contexts.insert(relative.clone(), (path.clone(), physical_path))
        {
            anyhow::bail!(
                "distinct source paths {} and {} normalize to the same runtime identity {relative:?}",
                existing.display(),
                path.display()
            );
        }
    }
    let requests = contexts
        .iter()
        .enumerate()
        .map(|(ordinal, (relative, (_, physical_path)))| {
            FileReadRequest::new_verified_under(
                InputIdentity::new(relative.clone(), ordinal as u64),
                physical_path.clone(),
                &resolved_root,
            )
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let contexts = Arc::new(contexts);
    let (parser_allowance_bytes, _) = isolated_parser_layout(config, false);
    let parser_pool_bytes = config.memory_budget().cpu_arenas_bytes;
    let parser_admission = Arc::new(RuntimeParserAdmission::new(parser_pool_bytes));
    let output_budget = config.memory_budget().cache_and_runs_bytes;
    let output_admission = Arc::new(RuntimeOutputAdmission::new(output_budget));
    let completed = read_files_concurrently(config, requests, move |input| {
        let relative = input.identity.normalized_path.as_ref();
        let (path, _) = contexts
            .get(relative)
            .expect("runtime ticket context must exist");
        let _parser_permit = parser_admission
            .acquire_with_cancellation(parser_allowance_bytes, None)
            .expect("validated runtime config must admit its canonical parser allowance");
        let extraction = engine::extract_as_bytes_with_parser_allowance(
            path,
            relative,
            input.bytes(),
            parser_allowance_bytes,
        )
        .with_context(|| format!("extract {relative}"))?;
        let retained_bytes = extraction_retained_bytes(&extraction)?;
        anyhow::ensure!(
            output_admission.try_reserve(retained_bytes),
            "isolated retained extraction output exceeds its {output_budget}-byte budget at {relative}"
        );
        Ok(extraction)
    })
    .map_err(|error| anyhow::anyhow!("isolated extraction runtime failed: {error:?}"))?;

    let mut extractions = Vec::with_capacity(completed.completed.len());
    for completed in completed.completed {
        extractions.push(completed.value?);
    }
    Ok(RuntimeProjectExtraction {
        extractions,
        detection,
        read_failures: completed.failures,
    })
}

/// Collect and extract a project in parallel, storing repo-relative paths.
pub fn extract_project(root: &std::path::Path) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    extract_project_with_options(root, false)
}

/// Extract a project, optionally bypassing the AST cache for a true full scan.
pub fn extract_project_with_options(
    root: &std::path::Path,
    force: bool,
) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    extract_project_with_options_and_output(root, force, &root.join("graphoxide-out"))
}

/// Extract a project while storing the incremental manifest and AST cache in
/// an explicit managed output directory.
///
/// This keeps scans side-effect free inside `root` when callers direct output
/// elsewhere, while the existing wrappers retain `root/graphoxide-out`.
pub fn extract_project_with_options_and_output(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    extract_project_with_options_and_output_filtered(root, force, managed_output_dir, false)
}

/// Extract a project with an explicit code-only boundary. The filtered mode
/// excludes document, paper, image, and video tiers before cache lookup, so a
/// `--code-only` build cannot accidentally retain locally parsed document
/// nodes or create semantic-cache artifacts for skipped inputs.
pub fn extract_project_with_options_and_output_filtered(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    Ok(extract_project_with_scan_options(
        root,
        force,
        managed_output_dir,
        code_only,
        &detect::DetectOptions::default(),
    )?
    .extractions)
}

#[derive(Debug, Clone)]
pub struct ProjectExtractionResult {
    pub extractions: Vec<graphoxide_core::Extraction>,
    pub detection: detect::DetectResult,
    /// One entry per file that could not be read or extracted.
    pub warnings: Vec<String>,
}

/// Completion evidence for a project extraction attempt.
///
/// A filesystem walk error represents work that could not be enumerated, so it
/// contributes one unsuccessful unit even though there is no path to extract.
/// A file that could not be read or extracted contributes one unsuccessful unit
/// too: it is skipped with a warning rather than aborting the scan, so a single
/// malformed input cannot cost a whole repository its graph. Every remaining
/// dispatched file contributes one completed unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectExtractionProgress {
    pub total: usize,
    pub succeeded: usize,
}

impl ProjectExtractionProgress {
    pub const fn is_complete(self) -> bool {
        self.succeeded == self.total
    }
}

/// A scan manifest prepared in memory but not yet made visible on disk.
///
/// Callers that also write a graph should commit the graph first, then consume
/// this value. If the graph is refused or its write fails, dropping this value
/// leaves the previous manifest untouched.
#[derive(Debug)]
#[must_use = "commit this manifest only after the corresponding graph write succeeds"]
pub struct PendingProjectManifest {
    output_directory: std::path::PathBuf,
    entries: cache::Manifest,
}

impl PendingProjectManifest {
    pub fn path(&self) -> std::path::PathBuf {
        self.output_directory.join("manifest.json")
    }

    pub fn commit(self) -> anyhow::Result<()> {
        cache::save_manifest_to_output(&self.output_directory, &self.entries)
    }

    /// Publish the prepared manifest only when the destination can be replaced
    /// atomically. Unlike the legacy extract-only commit, this never falls
    /// back to an in-place copy after a rename failure.
    pub fn commit_strict(self) -> anyhow::Result<()> {
        graphoxide_core::write_json_atomic_strict(self.path(), &self.entries, true)
    }
}

/// Project extraction whose manifest is intentionally deferred until the graph
/// artifact has been durably accepted.
#[derive(Debug)]
#[must_use = "the pending manifest must be committed or deliberately discarded"]
pub struct DeferredProjectExtractionResult {
    pub extractions: Vec<graphoxide_core::Extraction>,
    /// Conservative retained-byte estimate for the returned extraction facts.
    ///
    /// The isolated executor admits this total against the runtime's
    /// cache-and-run partition before all per-file results can accumulate.
    /// Legacy execution reports the same estimate without claiming admission.
    pub retained_output_bytes: usize,
    pub detection: detect::DetectResult,
    pub progress: ProjectExtractionProgress,
    /// One entry per file that could not be read or extracted.
    pub warnings: Vec<String>,
    /// Canonical physical identities whose current extraction completed.
    ///
    /// Incremental graph replacement must use this authoritative scan
    /// evidence rather than inferring ownership from fact provenance. Failed
    /// reads/extractions are deliberately absent so their committed facts can
    /// survive a retry.
    pub rebuilt_sources: Vec<std::path::PathBuf>,
    /// Sources whose bytes differed from the previously committed manifest.
    ///
    /// The isolated runtime reads and hashes every candidate through its I/O
    /// owners, but only schedules a parser for these sources. Callers merge
    /// this delta with the committed graph to retain unchanged facts.
    pub changed_sources: usize,
    /// Sources whose content hash matched the previously committed manifest.
    pub unchanged_sources: usize,
    /// Previously-manifested sources that are no longer part of this scan.
    pub deleted_sources: usize,
    /// Deterministic cache decisions for this scan. This is separate from
    /// `unchanged_sources`: manifest/baseline reuse after a payload hash match
    /// is not misreported as a runtime artifact hit.
    pub runtime_cache: cache::RuntimeCacheTelemetry,
    /// Fail-open runtime-v1 cache persistence diagnostics. Cache failures do
    /// not invalidate a completed extraction or alter graph publication.
    pub runtime_cache_diagnostics: Vec<String>,
    /// Fail-open diagnostics from optional resolver metadata reads. Indexed
    /// source failures remain fatal; missing metadata simply leaves the
    /// isolated resolver with the same unresolved evidence it would have when
    /// legacy metadata reads fail.
    pub resolution_snapshot_diagnostics: Vec<String>,
    pub pending_manifest: PendingProjectManifest,
}

/// One file's contribution to a project scan: its extraction plus the manifest
/// evidence that dates it.
///
/// Returned as an error only for faults specific to this file, so the caller
/// can record the failure and continue with the rest of the corpus.
type ProjectExtractionRow = (String, graphoxide_core::Extraction, f64, String);

fn extract_one_project_file(
    path: &std::path::Path,
    relative: &str,
    force: bool,
    managed_output_dir: &std::path::Path,
) -> anyhow::Result<ProjectExtractionRow> {
    use md5::Digest as _;
    let bytes = std::fs::read(path)?;
    let cached = (!force)
        .then(|| cache::ast_cache_get_from_output(managed_output_dir, relative, &bytes))
        .flatten();
    let extraction = if let Some(cached) = cached {
        cached
    } else {
        // The caller names the file in the context it attaches to this error.
        let extracted = engine::extract_as(path, relative)?;
        // The cache is an optimization: a failed write costs the next scan
        // some time, not this scan its result.
        if let Err(error) =
            cache::ast_cache_put_to_output(managed_output_dir, relative, &bytes, &extracted)
        {
            tracing::warn!("{relative}: caching the extraction failed: {error:#}");
        }
        extracted
    };
    let metadata = std::fs::metadata(path)?;
    let mtime = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let hash = format!("{:x}", md5::Md5::digest(&bytes));
    Ok((relative.to_owned(), extraction, mtime, hash))
}

#[derive(Debug, Clone)]
struct RuntimeFileContext {
    /// Logical path retained as graph provenance and format identity.
    path: std::path::PathBuf,
    /// Once-canonicalized regular target used only by runtime I/O.
    physical_path: std::path::PathBuf,
    indexed: bool,
}

#[derive(Debug)]
struct RuntimeScanRow {
    relative: String,
    extraction: Option<graphoxide_core::Extraction>,
    mtime: f64,
    hash: String,
    changed: bool,
    indexed: bool,
    snapshot_source: Option<Vec<u8>>,
    warning: Option<String>,
    runtime_manifest: Option<cache::RuntimeAstManifestEvidence>,
    runtime_cache: cache::RuntimeCacheTelemetry,
    runtime_cache_diagnostics: Vec<String>,
}

#[derive(Debug)]
enum RuntimeCacheHitUseError {
    Rejected(cache::RuntimeAstCacheRejection),
    ExceedsOutputAdmission,
}

// `serde_json` can expand deeply nested singleton objects far beyond their
// compact wire representation. Cache artifacts live in Graphoxide's managed
// output, but still receive a deliberately high pre-decode scratch charge so
// copied, stale, or malformed local state fails open before fact allocation.
const RUNTIME_CACHE_DECODE_EXPANSION_MULTIPLIER: usize = 256;

fn decode_admitted_runtime_cache_hit(
    hit: graphoxide_index_runtime::cache::RuntimeCacheHit,
    evidence: &cache::RuntimeAstCacheEvidence,
    output_admission: &RuntimeOutputAdmission,
    cancellation: &graphoxide_index_runtime::RuntimeCancellation,
) -> Result<graphoxide_core::Extraction, RuntimeCacheHitUseError> {
    // The cache service separately retains shared transfer credit for the raw
    // payload until `hit` is dropped. Validate the integrity-checked runtime
    // preamble and fact-affecting header before admitting conservative serde
    // scratch space or allocating extraction facts.
    cache::validate_runtime_ast_cache_payload_header(hit.source, &hit.payload, evidence)
        .map_err(RuntimeCacheHitUseError::Rejected)?;
    let decode_charge = hit
        .payload
        .len()
        .checked_mul(RUNTIME_CACHE_DECODE_EXPANSION_MULTIPLIER)
        .ok_or(RuntimeCacheHitUseError::ExceedsOutputAdmission)?;
    let reservation = output_admission
        .try_reserve_temporary_with_cancellation(decode_charge, Some(cancellation))
        .ok_or(RuntimeCacheHitUseError::ExceedsOutputAdmission)?;
    let extraction = cache::decode_runtime_ast_cache_payload(hit.source, &hit.payload, evidence)
        .map_err(RuntimeCacheHitUseError::Rejected)?;
    let retained_bytes = extraction_retained_bytes(&extraction)
        .map_err(|_| RuntimeCacheHitUseError::ExceedsOutputAdmission)?;
    if !reservation.commit(retained_bytes) {
        return Err(RuntimeCacheHitUseError::ExceedsOutputAdmission);
    }
    Ok(extraction)
}

fn validate_admitted_runtime_cache_hit(
    hit: graphoxide_index_runtime::cache::RuntimeCacheHit,
    evidence: &cache::RuntimeAstCacheEvidence,
) -> Result<(), RuntimeCacheHitUseError> {
    cache::validate_runtime_ast_cache_payload_header(hit.source, &hit.payload, evidence)
        .map(|_| ())
        .map_err(RuntimeCacheHitUseError::Rejected)
}

fn persist_runtime_cache_extraction(
    client: &graphoxide_index_runtime::cache::RuntimeCacheIoClient,
    evidence: &cache::RuntimeAstCacheEvidence,
    extraction: &graphoxide_core::Extraction,
    replace_existing: bool,
    cancellation: &graphoxide_index_runtime::RuntimeCancellation,
) -> Result<
    graphoxide_index_runtime::cache::RuntimeCacheIoPersistOutcome,
    graphoxide_index_runtime::cache::RuntimeCacheIoServiceError,
> {
    let encoded_bytes = cache::runtime_ast_cache_payload_len(evidence, extraction)
        .map_err(graphoxide_index_runtime::cache::RuntimeCacheIoServiceError::Cache)?;
    client.persist_encoded_with_cancellation(
        evidence.key,
        encoded_bytes,
        replace_existing,
        cancellation,
        |output| cache::encode_runtime_ast_cache_payload_into(output, evidence, extraction),
    )
}

fn runtime_manifest_byte_limit(cache_and_runs_bytes: usize, admitted_files: usize) -> usize {
    const MANIFEST_BASE_BYTES: usize = 4 * 1024;
    const MANIFEST_BYTES_PER_FILE: usize = 8 * 1024;
    const MAX_RUNTIME_MANIFEST_BYTES: usize = 32 * 1024 * 1024;

    MANIFEST_BASE_BYTES
        .saturating_add(admitted_files.saturating_mul(MANIFEST_BYTES_PER_FILE))
        .min(cache_and_runs_bytes / 64)
        .min(MAX_RUNTIME_MANIFEST_BYTES)
}

fn runtime_manifest_reservation(cache_and_runs_bytes: usize, admitted_files: usize) -> usize {
    runtime_manifest_byte_limit(cache_and_runs_bytes, admitted_files)
        .saturating_mul(32)
        .min(cache_and_runs_bytes / 2)
}

#[derive(Debug)]
struct RuntimeParserAdmission {
    byte_limit: usize,
    active_bytes: std::sync::Mutex<usize>,
    changed: std::sync::Condvar,
}

impl RuntimeParserAdmission {
    fn new(byte_limit: usize) -> Self {
        Self {
            byte_limit,
            active_bytes: std::sync::Mutex::new(0),
            changed: std::sync::Condvar::new(),
        }
    }

    fn acquire_with_cancellation(
        &self,
        bytes: usize,
        cancellation: Option<&graphoxide_index_runtime::RuntimeCancellation>,
    ) -> Option<RuntimeParserPermit<'_>> {
        let mut active = self
            .active_bytes
            .lock()
            .expect("parser admission mutex poisoned");
        loop {
            if cancellation.is_some_and(|token| token.is_cancelled()) || bytes > self.byte_limit {
                return None;
            }
            if active
                .checked_add(bytes)
                .is_some_and(|next| next <= self.byte_limit)
            {
                *active += bytes;
                return Some(RuntimeParserPermit {
                    admission: self,
                    reserved: bytes,
                });
            }
            let (next, _) = self
                .changed
                .wait_timeout(active, std::time::Duration::from_millis(10))
                .expect("parser admission mutex poisoned while waiting");
            active = next;
        }
    }

    #[cfg(test)]
    fn active_bytes(&self) -> usize {
        *self
            .active_bytes
            .lock()
            .expect("parser admission mutex poisoned")
    }
}

struct RuntimeParserPermit<'a> {
    admission: &'a RuntimeParserAdmission,
    reserved: usize,
}

impl Drop for RuntimeParserPermit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .admission
            .active_bytes
            .lock()
            .expect("parser admission mutex poisoned");
        *active = active
            .checked_sub(self.reserved)
            .expect("parser admission accounting underflow");
        self.admission.changed.notify_all();
    }
}

#[derive(Debug)]
struct RuntimeOutputAdmission {
    byte_limit: usize,
    state: std::sync::Mutex<RuntimeOutputAdmissionState>,
    changed: std::sync::Condvar,
}

#[derive(Debug, Default)]
struct RuntimeOutputAdmissionState {
    retained_bytes: usize,
    temporary_bytes: usize,
}

impl RuntimeOutputAdmission {
    fn new(byte_limit: usize) -> Self {
        Self {
            byte_limit,
            state: std::sync::Mutex::new(RuntimeOutputAdmissionState::default()),
            changed: std::sync::Condvar::new(),
        }
    }

    fn try_reserve(&self, bytes: usize) -> bool {
        self.try_reserve_with_cancellation(bytes, None)
    }

    fn try_reserve_with_cancellation(
        &self,
        bytes: usize,
        cancellation: Option<&graphoxide_index_runtime::RuntimeCancellation>,
    ) -> bool {
        let mut state = self.state.lock().expect("output admission mutex poisoned");
        loop {
            if cancellation.is_some_and(|token| token.is_cancelled())
                || state
                    .retained_bytes
                    .checked_add(bytes)
                    .is_none_or(|eventual| eventual > self.byte_limit)
            {
                return false;
            }
            if state
                .retained_bytes
                .checked_add(state.temporary_bytes)
                .and_then(|current| current.checked_add(bytes))
                .is_some_and(|current| current <= self.byte_limit)
            {
                state.retained_bytes += bytes;
                return true;
            }
            let (next, _) = self
                .changed
                .wait_timeout(state, std::time::Duration::from_millis(10))
                .expect("output admission mutex poisoned while waiting");
            state = next;
        }
    }

    fn retained_bytes(&self) -> usize {
        self.state
            .lock()
            .expect("output admission mutex poisoned")
            .retained_bytes
    }

    fn try_reserve_temporary_with_cancellation(
        &self,
        bytes: usize,
        cancellation: Option<&graphoxide_index_runtime::RuntimeCancellation>,
    ) -> Option<RuntimeOutputReservation<'_>> {
        let mut state = self.state.lock().expect("output admission mutex poisoned");
        loop {
            if cancellation.is_some_and(|token| token.is_cancelled())
                || state
                    .retained_bytes
                    .checked_add(bytes)
                    .is_none_or(|eventual| eventual > self.byte_limit)
            {
                return None;
            }
            if state
                .retained_bytes
                .checked_add(state.temporary_bytes)
                .and_then(|current| current.checked_add(bytes))
                .is_some_and(|current| current <= self.byte_limit)
            {
                state.temporary_bytes += bytes;
                break;
            }
            let (next, _) = self
                .changed
                .wait_timeout(state, std::time::Duration::from_millis(10))
                .expect("output admission mutex poisoned while waiting");
            state = next;
        }
        Some(RuntimeOutputReservation {
            admission: self,
            reserved: bytes,
            active: true,
        })
    }
}

struct RuntimeOutputReservation<'a> {
    admission: &'a RuntimeOutputAdmission,
    reserved: usize,
    active: bool,
}

impl RuntimeOutputReservation<'_> {
    fn commit(mut self, actual_bytes: usize) -> bool {
        let mut state = self
            .admission
            .state
            .lock()
            .expect("output admission mutex poisoned");
        state.temporary_bytes = state
            .temporary_bytes
            .checked_sub(self.reserved)
            .expect("temporary output admission accounting underflow");
        if actual_bytes > self.reserved {
            // A cache decoder must acquire enough credit before serde is
            // allowed to allocate. Growing the reservation after decoding
            // would make the admission boundary observational rather than a
            // bound, so an underestimated expansion is a safe cache miss.
            self.active = false;
            self.admission.changed.notify_all();
            return false;
        }
        state.retained_bytes = state
            .retained_bytes
            .checked_add(actual_bytes)
            .expect("retained output admission accounting overflow");
        debug_assert!(
            state.retained_bytes.saturating_add(state.temporary_bytes) <= self.admission.byte_limit
        );
        self.active = false;
        self.admission.changed.notify_all();
        true
    }
}

impl Drop for RuntimeOutputReservation<'_> {
    fn drop(&mut self) {
        if self.active {
            let mut state = self
                .admission
                .state
                .lock()
                .expect("output admission mutex poisoned");
            state.temporary_bytes = state
                .temporary_bytes
                .checked_sub(self.reserved)
                .expect("temporary output admission accounting underflow");
            self.active = false;
            self.admission.changed.notify_all();
        }
    }
}

fn extraction_retained_bytes(extraction: &graphoxide_core::Extraction) -> anyhow::Result<usize> {
    use graphoxide_core::{Edge, Node};
    use std::mem::size_of;

    let mut retained = size_of::<graphoxide_core::Extraction>();
    retained = retained
        .saturating_add(
            extraction
                .nodes
                .capacity()
                .saturating_mul(size_of::<Node>()),
        )
        .saturating_add(
            extraction
                .edges
                .capacity()
                .saturating_mul(size_of::<Edge>()),
        )
        .saturating_add(
            extraction
                .hyperedges
                .capacity()
                .saturating_mul(size_of::<serde_json::Value>()),
        );
    for node in &extraction.nodes {
        retained = retained.saturating_add(node_dynamic_retained_bytes(node));
    }
    for edge in &extraction.edges {
        retained = retained.saturating_add(edge_dynamic_retained_bytes(edge));
    }
    for hyperedge in &extraction.hyperedges {
        retained = retained.saturating_add(json_value_retained_bytes(hyperedge));
    }

    // Serialized size supplies conservative headroom for allocator metadata,
    // B-tree nodes, and extractor-owned capacity that is not visible through
    // public schema fields. Count without allocating a duplicate payload.
    let mut serialized = RetainedCountingWriter::default();
    serde_json::to_writer(&mut serialized, extraction)?;
    Ok(retained.saturating_add(serialized.bytes))
}

/// Conservatively estimate the retained memory owned by extraction facts.
///
/// Callers that append post-scan facts should measure the final extraction
/// vector before reserving memory for a graph baseline or materialization.
pub fn extractions_retained_bytes(
    extractions: &[graphoxide_core::Extraction],
) -> anyhow::Result<usize> {
    extractions.iter().try_fold(0usize, |retained, extraction| {
        extraction_retained_bytes(extraction).map(|bytes| retained.saturating_add(bytes))
    })
}

fn node_dynamic_retained_bytes(node: &graphoxide_core::Node) -> usize {
    node.id
        .capacity()
        .saturating_add(node.label.capacity())
        .saturating_add(node.file_type.capacity())
        .saturating_add(node.source_file.capacity())
        .saturating_add(node.source_location.as_ref().map_or(0, String::capacity))
        .saturating_add(json_map_retained_bytes(&node.extra))
}

fn edge_dynamic_retained_bytes(edge: &graphoxide_core::Edge) -> usize {
    edge.source
        .capacity()
        .saturating_add(edge.target.capacity())
        .saturating_add(edge.relation.capacity())
        .saturating_add(edge.source_file.capacity())
        .saturating_add(json_map_retained_bytes(&edge.extra))
}

/// Conservative charge for one node appended by a corpus resolver. The
/// serialized contribution supplies allocator/map headroom in the same way as
/// the whole-extraction admission calculation.
pub(crate) fn resolver_node_admission_bytes(node: &graphoxide_core::Node) -> usize {
    use std::mem::size_of;

    size_of::<graphoxide_core::Node>()
        .saturating_add(node_dynamic_retained_bytes(node))
        .saturating_add(serialized_retained_bytes(node))
}

/// Conservative charge for one edge appended by a corpus resolver.
pub(crate) fn resolver_edge_admission_bytes(edge: &graphoxide_core::Edge) -> usize {
    use std::mem::size_of;

    size_of::<graphoxide_core::Edge>()
        .saturating_add(edge_dynamic_retained_bytes(edge))
        .saturating_add(serialized_retained_bytes(edge))
}

fn serialized_retained_bytes(value: &impl serde::Serialize) -> usize {
    let mut serialized = RetainedCountingWriter::default();
    serde_json::to_writer(&mut serialized, value).map_or(usize::MAX, |()| serialized.bytes)
}

fn json_map_retained_bytes(map: &std::collections::BTreeMap<String, serde_json::Value>) -> usize {
    use std::mem::size_of;

    map.iter().fold(
        map.len().saturating_mul(
            size_of::<String>()
                .saturating_add(size_of::<serde_json::Value>())
                .saturating_add(3 * size_of::<usize>()),
        ),
        |retained, (key, value)| {
            retained
                .saturating_add(key.capacity())
                .saturating_add(json_value_retained_bytes(value))
        },
    )
}

fn json_value_retained_bytes(value: &serde_json::Value) -> usize {
    use std::mem::size_of;

    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
        serde_json::Value::String(value) => value.capacity(),
        serde_json::Value::Array(values) => values
            .capacity()
            .saturating_mul(size_of::<serde_json::Value>())
            .saturating_add(values.iter().fold(0usize, |retained, value| {
                retained.saturating_add(json_value_retained_bytes(value))
            })),
        serde_json::Value::Object(values) => values.iter().fold(
            values.len().saturating_mul(
                size_of::<String>()
                    .saturating_add(size_of::<serde_json::Value>())
                    .saturating_add(3 * size_of::<usize>()),
            ),
            |retained, (key, value)| {
                retained
                    .saturating_add(key.capacity())
                    .saturating_add(json_value_retained_bytes(value))
            },
        ),
    }
}

#[derive(Default)]
struct RetainedCountingWriter {
    bytes: usize,
}

impl std::io::Write for RetainedCountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn normalized_project_key(
    path: &std::path::Path,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> String {
    use unicode_normalization::UnicodeNormalization;

    path.strip_prefix(resolved_root)
        .or_else(|_| path.strip_prefix(original_root))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
        .nfc()
        .collect()
}

fn normalized_manifest_key(
    stored: &str,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> String {
    use unicode_normalization::UnicodeNormalization;

    let path = std::path::Path::new(stored);
    if path.is_absolute() {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        normalized_project_key(&resolved, resolved_root, original_root)
    } else {
        stored.replace('\\', "/").nfc().collect()
    }
}

fn normalized_previous_manifest(
    manifest: &cache::Manifest,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> cache::Manifest {
    let mut normalized = cache::Manifest::new();
    // Load legacy absolute spellings first. Portable relative rows are newer
    // and authoritative when both forms address the same source.
    for absolute in [true, false] {
        for (stored, entry) in manifest {
            if std::path::Path::new(stored).is_absolute() == absolute {
                normalized.insert(
                    normalized_manifest_key(stored, resolved_root, original_root),
                    entry.clone(),
                );
            }
        }
    }
    normalized
}

fn normalized_previous_manifest_owned(
    manifest: cache::Manifest,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> cache::Manifest {
    fn manifest_key_is_absolute(stored: &str) -> bool {
        let portable = stored.replace('\\', "/");
        std::path::Path::new(stored).is_absolute()
            || portable.starts_with("//")
            || portable.as_bytes().get(1).is_some_and(|byte| *byte == b':')
    }

    fn normalize_relative(stored: &str) -> Option<String> {
        use unicode_normalization::UnicodeNormalization as _;

        if stored.contains('\0') {
            return None;
        }
        let portable = stored.replace('\\', "/").nfc().collect::<String>();
        let mut components = Vec::new();
        for component in portable.split('/') {
            match component {
                "" | "." => {}
                ".." => return None,
                component => components.push(component),
            }
        }
        (!components.is_empty()).then(|| components.join("/"))
    }

    let mut staged = std::collections::BTreeMap::<String, (bool, cache::ManifestEntry)>::new();
    // Preserve the compatibility precedence of the borrowed helper without
    // cloning or partitioning an attacker-sized manifest. The small boolean
    // records whether the winning row was relative so one input pass can give
    // portable relative rows precedence over legacy absolute spellings.
    // Absolute compatibility rows are handled lexically: manifest data must
    // never trigger a host/UNC filesystem probe, and paths outside the two
    // already-known roots vanish.
    for (stored, entry) in manifest {
        let is_absolute = manifest_key_is_absolute(&stored);
        let path = std::path::Path::new(&stored);
        let key = if is_absolute {
            if !path.is_absolute() {
                None
            } else {
                path.strip_prefix(resolved_root)
                    .or_else(|_| path.strip_prefix(original_root))
                    .ok()
                    .and_then(|relative| normalize_relative(&relative.to_string_lossy()))
            }
        } else {
            normalize_relative(&stored)
        };
        if let Some(key) = key {
            let is_relative = !is_absolute;
            match staged.entry(key) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert((is_relative, entry));
                }
                std::collections::btree_map::Entry::Occupied(mut slot)
                    if is_relative >= slot.get().0 =>
                {
                    slot.insert((is_relative, entry));
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }
    }
    staged
        .into_iter()
        .map(|(key, (_, entry))| (key, entry))
        .collect()
}

const RESOLUTION_BASELINE_WORKING_SET_MULTIPLIER: usize = 8;

#[derive(Debug)]
struct ResolverBaselineContext {
    extractions: Vec<graphoxide_core::Extraction>,
    retained_bytes: usize,
    working_set_charge: usize,
}

fn eligible_resolver_owner_keys(
    contexts: &std::collections::BTreeMap<String, RuntimeFileContext>,
    changed_owner_keys: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<String> {
    contexts
        .iter()
        .filter(|(source, context)| {
            context.indexed && !changed_owner_keys.contains(source.as_str())
        })
        .map(|(source, _)| source.clone())
        .collect()
}

fn baseline_owner_key(
    source_file: &str,
    container_source: Option<&str>,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> Option<String> {
    let owner = container_source.unwrap_or(source_file);
    (!owner.is_empty()).then(|| normalized_manifest_key(owner, resolved_root, original_root))
}

fn baseline_group_key(
    source_file: &str,
    owner_key: &str,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> String {
    if source_file.is_empty() {
        owner_key.to_owned()
    } else {
        normalized_manifest_key(source_file, resolved_root, original_root)
    }
}

/// Move eligible JavaScript-family nodes from one capped committed graph into
/// deterministic per-logical-source resolver chunks. Ownership eligibility
/// follows the physical outer source (`_container_source` when present);
/// partitioning uses each node's logical source so multi-module containers
/// never collapse into a chunk that exposes only its first file node. Edges,
/// hyperedges, and non-JS nodes are lookup-dead for JS module resolution and
/// are dropped with the consumed scan-local graph.
fn load_resolver_baseline_context(
    graph_path: &std::path::Path,
    graph_byte_cap: u64,
    eligible_owners: &std::collections::BTreeSet<String>,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> anyhow::Result<ResolverBaselineContext> {
    use graphoxide_core::{CappedGraphRead, CONTAINER_SOURCE_ATTRIBUTE};
    use std::collections::BTreeMap;

    anyhow::ensure!(
        graph_byte_cap > 0,
        "isolated resolver baseline has no remaining graph-read budget"
    );
    let CappedGraphRead {
        graph,
        admitted_bytes,
        ..
    } = graphoxide_core::read_graph_capped(graph_path, graph_byte_cap)?;
    let working_set_charge = admitted_bytes
        .checked_mul(RESOLUTION_BASELINE_WORKING_SET_MULTIPLIER)
        .ok_or_else(|| anyhow::anyhow!("resolver baseline working-set charge exceeds usize"))?;
    let mut groups = BTreeMap::<String, graphoxide_core::Extraction>::new();

    for mut node in graph.nodes {
        if !crate::js_resolution::is_javascript_source(&node.source_file) {
            continue;
        }
        let container_source = node
            .extra
            .get(CONTAINER_SOURCE_ATTRIBUTE)
            .and_then(serde_json::Value::as_str)
            .filter(|source| !source.is_empty());
        let Some(owner_key) = baseline_owner_key(
            &node.source_file,
            container_source,
            resolved_root,
            original_root,
        ) else {
            continue;
        };
        if eligible_owners.contains(&owner_key) {
            let group_key =
                baseline_group_key(&node.source_file, &owner_key, resolved_root, original_root);
            node.source_file.clone_from(&group_key);
            groups.entry(group_key).or_default().nodes.push(node);
        }
    }

    let extractions = groups.into_values().collect::<Vec<_>>();
    let retained_bytes = extractions_retained_bytes(&extractions)?;
    anyhow::ensure!(
        retained_bytes <= working_set_charge,
        "resolver baseline context retains {retained_bytes} bytes, exceeding its {working_set_charge}-byte graph working-set charge"
    );
    Ok(ResolverBaselineContext {
        extractions,
        retained_bytes,
        working_set_charge,
    })
}

impl DeferredProjectExtractionResult {
    /// Preserve the legacy extract-only behavior by publishing the manifest and
    /// returning the ordinary result shape.
    pub fn commit_manifest(self) -> anyhow::Result<ProjectExtractionResult> {
        let Self {
            extractions,
            detection,
            warnings,
            pending_manifest,
            ..
        } = self;
        pending_manifest.commit()?;
        Ok(ProjectExtractionResult {
            extractions,
            detection,
            warnings,
        })
    }
}

/// Extract through the dedicated I/O/CPU runtime and defer manifest publication
/// until the caller has committed the matching graph.
///
/// This is the production indexing entrypoint. Discovery and manifest I/O stay
/// in the control/I/O plane; every source is materialized once by an I/O owner
/// and is then passed to CPU extraction as a byte lease. The legacy AST cache
/// is deliberately not consulted here because it is path-I/O based.
pub fn extract_project_with_runtime_scan_options_deferred_manifest(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
) -> anyhow::Result<DeferredProjectExtractionResult> {
    extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
        config,
        graphoxide_index_runtime::RuntimeCancellation::new(),
    )
}

/// Extract through the dedicated I/O/CPU runtime with cooperative
/// cancellation, deferring manifest publication until the caller commits the
/// matching graph.
#[allow(clippy::too_many_arguments)]
pub fn extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
    cancellation: graphoxide_index_runtime::RuntimeCancellation,
) -> anyhow::Result<DeferredProjectExtractionResult> {
    use graphoxide_index_runtime::{
        read_files_concurrently_with_cancellation, FileReadRequest, InputIdentity,
    };
    use md5::Digest as _;
    use std::{collections::BTreeMap, sync::Arc};

    anyhow::ensure!(
        !cancellation.is_cancelled(),
        "isolated extraction cancelled"
    );
    let managed_output_dir = if managed_output_dir.is_absolute() {
        managed_output_dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(managed_output_dir)
    };
    let mut detect_options = detect_options.clone();
    detect_options.output_dir = Some(managed_output_dir.clone());
    detect_options.convert_office_sidecars = false;
    let detection = detect::detect(root, &detect_options)?;
    anyhow::ensure!(
        !cancellation.is_cancelled(),
        "isolated extraction cancelled"
    );
    let mut indexed_files = detection
        .files
        .iter()
        .filter(|(kind, _)| !code_only || kind.as_str() == detect::FileType::Code.as_str())
        .flat_map(|(_, paths)| paths)
        .map(std::path::PathBuf::from)
        .filter(|path| detection.is_supported_source(path))
        .collect::<Vec<_>>();
    indexed_files.sort();
    indexed_files.dedup();
    let indexed_paths = indexed_files
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let total_work = indexed_paths
        .len()
        .saturating_add(detection.walk_errors.len());
    let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    // Metadata needed by JS/TS/SFC resolution is admitted by I/O owners even
    // in `--code-only` mode. It is retained only in the bounded project
    // snapshot and never becomes a manifest/output row unless it was already
    // selected for indexing.
    let mut snapshot_paths = indexed_paths.clone();
    snapshot_paths.extend(
        detection
            .files
            .values()
            .flatten()
            .map(std::path::PathBuf::from)
            .filter(|path| detection.is_supported_source(path))
            .filter(|path| {
                crate::js_resolution::ProjectSnapshot::needs_file(path.to_string_lossy().as_ref())
            }),
    );
    let mut contexts = BTreeMap::<String, RuntimeFileContext>::new();
    for path in snapshot_paths {
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        let indexed = indexed_paths.contains(&path);
        let physical_path = detection.physical_source(&path);
        let relative = path
            .strip_prefix(&resolved_root)
            .or_else(|_| path.strip_prefix(root))
            .map_or_else(
                |_| normalized_project_key(&path, &resolved_root, root),
                |relative| normalized_project_key(relative, &resolved_root, root),
            );
        match contexts.entry(relative) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(RuntimeFileContext {
                    path,
                    physical_path,
                    indexed,
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get().path != path || entry.get().physical_path != physical_path {
                    anyhow::bail!(
                        "distinct source paths {} and {} normalize to the same runtime identity {:?}",
                        entry.get().path.display(),
                        path.display(),
                        entry.key()
                    );
                }
                entry.get_mut().indexed |= indexed;
            }
        }
    }
    let cache_and_runs_budget = config.memory_budget().cache_and_runs_bytes;
    let manifest_byte_limit = runtime_manifest_byte_limit(cache_and_runs_budget, contexts.len());
    // Manifest entries have a fixed schema, but decoding and lexical
    // normalization briefly overlap tree-node storage. Reserve a conservative
    // 32x expansion while the one-pass priority staging map is consumed.
    let manifest_reservation = runtime_manifest_reservation(cache_and_runs_budget, contexts.len());
    let bounded_manifest =
        cache::load_manifest_from_output_bounded(&managed_output_dir, manifest_byte_limit);
    let cache::RuntimeManifestLoad {
        manifest: loaded_manifest,
        status: manifest_status,
    } = bounded_manifest;
    let committed_manifest_exists = manifest_status == cache::RuntimeManifestLoadStatus::Loaded;
    let baseline_graph_path = managed_output_dir.join("graph.json");
    let committed_baseline_eligible = committed_manifest_exists && baseline_graph_path.is_file();
    // Keep cache authorization evidence even when graph.json is absent. It
    // cannot authorize an incremental graph delta, but a strong metadata-only
    // hit can still restore the extraction used for a clean graph rebuild.
    let cache_previous = Arc::new(normalized_previous_manifest_owned(
        loaded_manifest,
        &resolved_root,
        root,
    ));
    let previous = if committed_baseline_eligible {
        Arc::clone(&cache_previous)
    } else {
        Arc::new(cache::Manifest::new())
    };

    let mut runtime_cache = cache::RuntimeCacheTelemetry::default();
    let mut runtime_cache_diagnostics = Vec::new();
    match manifest_status {
        cache::RuntimeManifestLoadStatus::Loaded | cache::RuntimeManifestLoadStatus::Missing => {}
        cache::RuntimeManifestLoadStatus::Oversize => {
            runtime_cache.stale_or_corrupt = runtime_cache.stale_or_corrupt.saturating_add(1);
            runtime_cache_diagnostics.push(format!(
                "runtime manifest exceeded its {manifest_byte_limit}-byte safety limit; rebuilding without metadata cache authorization"
            ));
        }
        cache::RuntimeManifestLoadStatus::Corrupt => {
            runtime_cache.stale_or_corrupt = runtime_cache.stale_or_corrupt.saturating_add(1);
            runtime_cache_diagnostics.push(
                "runtime manifest was corrupt; rebuilding without metadata cache authorization"
                    .to_owned(),
            );
        }
        cache::RuntimeManifestLoadStatus::UnsafeOrUnreadable => {
            runtime_cache.probe_failures = runtime_cache.probe_failures.saturating_add(1);
            runtime_cache_diagnostics.push(
                "runtime manifest was unsafe or unreadable; rebuilding without metadata cache authorization"
                    .to_owned(),
            );
        }
    }

    let all_requests = contexts
        .iter()
        .enumerate()
        .map(|(ordinal, (relative, context))| {
            FileReadRequest::new_verified_under(
                InputIdentity::new(relative.clone(), ordinal as u64),
                context.physical_path.clone(),
                &resolved_root,
            )
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    // Resolver source bytes remain live while sibling workers may still parse.
    // Give each phase a disjoint portion of the shared CPU-arena partition.
    let (parser_allowance_bytes, snapshot_budget) = isolated_parser_layout(config, true);
    let parser_pool_bytes = config
        .memory_budget()
        .cpu_arenas_bytes
        .saturating_sub(snapshot_budget);
    let parser_admission = Arc::new(RuntimeParserAdmission::new(parser_pool_bytes));
    let runtime_cache_options = cache::RuntimeAstCacheOptions::isolated(
        u64::try_from(parser_allowance_bytes).unwrap_or(u64::MAX),
    );
    let budget_after_manifest = cache_and_runs_budget.saturating_sub(manifest_reservation);
    let cache_service_budget = budget_after_manifest / 2;
    let mut runtime_cache_service = if cache_service_budget == 0 {
        None
    } else {
        match graphoxide_index_runtime::cache::RuntimeCacheIoService::start_for_memory_budget(
            managed_output_dir.clone(),
            cache_service_budget,
        ) {
            Ok(service)
                if service.memory_accounting().max_resident_bytes <= budget_after_manifest =>
            {
                runtime_cache.enabled = true;
                Some(service)
            }
            Ok(service) => {
                let reserved = service.memory_accounting().max_resident_bytes;
                let _ = service.shutdown();
                runtime_cache.probe_failures = runtime_cache.probe_failures.saturating_add(1);
                runtime_cache_diagnostics.push(format!(
                    "runtime cache requires {reserved} managed bytes, exceeding its {budget_after_manifest}-byte remaining cache/run partition; continuing without it"
                ));
                None
            }
            Err(error) => {
                runtime_cache.probe_failures = runtime_cache.probe_failures.saturating_add(1);
                runtime_cache_diagnostics.push(format!(
                    "runtime cache could not start; continuing without it: {error}"
                ));
                None
            }
        }
    };
    let cache_client = runtime_cache_service
        .as_ref()
        .map(graphoxide_index_runtime::cache::RuntimeCacheIoService::client);
    let cache_reservation = runtime_cache_service
        .as_ref()
        .map_or(0, |service| service.memory_accounting().max_resident_bytes);
    let output_budget = budget_after_manifest.saturating_sub(cache_reservation);
    let output_admission = Arc::new(RuntimeOutputAdmission::new(output_budget));

    // Probe metadata-authorized entries in stable request order. Only sources
    // the project resolver does not need may avoid their payload read.
    let mut requests = Vec::with_capacity(all_requests.len());
    let mut metadata_rows = Vec::new();
    let mut preflight_skip_probe = BTreeMap::<String, bool>::new();
    for request in all_requests {
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        let relative = request.identity.normalized_path.to_string();
        let Some(context) = contexts.get(relative.as_str()) else {
            requests.push(request);
            continue;
        };
        let Some(prior_entry) = cache_previous.get(relative.as_str()) else {
            requests.push(request);
            continue;
        };
        let Some(prior_cache) = prior_entry.runtime_cache else {
            requests.push(request);
            continue;
        };
        let Some(evidence) = cache::runtime_ast_cache_evidence_from_digest(
            &relative,
            prior_cache.content_digest,
            runtime_cache_options,
        ) else {
            requests.push(request);
            continue;
        };
        if force
            || !context.indexed
            || !cache::runtime_ast_cache_is_eligible(&relative)
            || crate::js_resolution::ProjectSnapshot::needs_file(&relative)
            || prior_entry.ast_version != cache::AST_CACHE_VERSION
            || evidence.key.as_bytes() != prior_cache.artifact_key
        {
            requests.push(request);
            continue;
        }
        let Some(client) = cache_client.as_ref() else {
            requests.push(request);
            continue;
        };
        let metadata_request =
            graphoxide_index_runtime::cache::RuntimeCacheMetadataProbeRequest::new(
                evidence.key,
                request.clone(),
                graphoxide_index_runtime::SourceIdentityEvidence::from_digest(
                    prior_cache.source_identity_digest,
                ),
            );
        match client.probe_metadata_only_with_cancellation(metadata_request, &cancellation) {
            Ok(probe) => {
                if probe.runtime_rejected_before_legacy {
                    runtime_cache.stale_or_corrupt =
                        runtime_cache.stale_or_corrupt.saturating_add(1);
                }
                match probe.outcome {
                    graphoxide_index_runtime::cache::RuntimeCacheProbeOutcome::Hit(hit) => {
                        let decoded = if committed_baseline_eligible {
                            validate_admitted_runtime_cache_hit(hit, &evidence).map(|()| None)
                        } else {
                            decode_admitted_runtime_cache_hit(
                                hit,
                                &evidence,
                                &output_admission,
                                &cancellation,
                            )
                            .map(Some)
                        };
                        match decoded {
                            Ok(extraction) => {
                                let mut row_cache = cache::RuntimeCacheTelemetry::enabled();
                                row_cache.metadata_hits = 1;
                                row_cache.payload_reads_avoided = 1;
                                row_cache.parses_avoided = 1;
                                metadata_rows.push(RuntimeScanRow {
                                    relative,
                                    extraction,
                                    mtime: prior_entry.mtime,
                                    hash: prior_entry.ast_hash.clone(),
                                    changed: !committed_baseline_eligible,
                                    indexed: true,
                                    snapshot_source: None,
                                    warning: None,
                                    runtime_manifest: Some(prior_cache),
                                    runtime_cache: row_cache,
                                    runtime_cache_diagnostics: Vec::new(),
                                });
                            }
                            Err(RuntimeCacheHitUseError::Rejected(rejection)) => {
                                runtime_cache.stale_or_corrupt =
                                    runtime_cache.stale_or_corrupt.saturating_add(1);
                                runtime_cache.misses = runtime_cache.misses.saturating_add(1);
                                runtime_cache_diagnostics.push(format!(
                                    "runtime cache envelope for {relative} was rejected ({rejection:?}); reparsing"
                                ));
                                preflight_skip_probe.insert(relative, true);
                                requests.push(request);
                            }
                            Err(RuntimeCacheHitUseError::ExceedsOutputAdmission) => {
                                runtime_cache.misses = runtime_cache.misses.saturating_add(1);
                                runtime_cache_diagnostics.push(format!(
                                    "runtime cache payload for {relative} exceeded decoded-output admission; reparsing"
                                ));
                                preflight_skip_probe.insert(relative, false);
                                requests.push(request);
                            }
                        }
                    }
                    graphoxide_index_runtime::cache::RuntimeCacheProbeOutcome::Missing => {
                        runtime_cache.misses = runtime_cache.misses.saturating_add(1);
                        preflight_skip_probe.insert(relative, false);
                        requests.push(request);
                    }
                    graphoxide_index_runtime::cache::RuntimeCacheProbeOutcome::RejectedCorruptOrStale => {
                        runtime_cache.stale_or_corrupt =
                            runtime_cache.stale_or_corrupt.saturating_add(1);
                        runtime_cache.misses = runtime_cache.misses.saturating_add(1);
                        preflight_skip_probe.insert(relative, false);
                        requests.push(request);
                    }
                    graphoxide_index_runtime::cache::RuntimeCacheProbeOutcome::SourceChanged
                    | graphoxide_index_runtime::cache::RuntimeCacheProbeOutcome::MetadataOnlyUnsupported => {
                        requests.push(request);
                    }
                }
            }
            Err(error) => {
                runtime_cache.probe_failures = runtime_cache.probe_failures.saturating_add(1);
                runtime_cache_diagnostics.push(format!(
                    "runtime metadata cache probe for {relative} failed; reading the source: {error}"
                ));
                requests.push(request);
            }
        }
    }

    let contexts = Arc::new(contexts);
    let contexts_for_compute = Arc::clone(&contexts);
    let previous_for_compute = Arc::clone(&previous);
    let preflight_skip_probe = Arc::new(preflight_skip_probe);
    let snapshot_admission = Arc::new(crate::js_resolution::ProjectSnapshotAdmission::new(
        snapshot_budget,
    ));
    let output_admission_for_compute = Arc::clone(&output_admission);
    let parser_admission_for_compute = Arc::clone(&parser_admission);
    let cache_client_for_compute = cache_client.clone();
    let compute_cancellation = cancellation.clone();
    let completed = read_files_concurrently_with_cancellation(
        config,
        requests,
        cancellation.clone(),
        move |input| -> anyhow::Result<_> {
        anyhow::ensure!(
            !compute_cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        let relative = input.identity.normalized_path.to_string();
        let context = contexts_for_compute
            .get(relative.as_str())
            .expect("runtime ticket context must exist");
        let path = &context.path;
        let indexed = context.indexed;
        let mtime = input
            .file_identity
            .modified
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .unwrap_or_default()
            .as_secs_f64();
        let snapshot_required = crate::js_resolution::ProjectSnapshot::needs_file(&relative);
        if snapshot_required
            && !snapshot_admission.try_reserve(
                crate::js_resolution::ProjectSnapshot::admission_bytes(
                    &relative,
                    input.retained_capacity_bytes(),
                ),
            )
        {
            anyhow::bail!(
                "isolated project resolution snapshot exceeds its {snapshot_budget}-byte budget"
            );
        }
        let source_identity = input.source_identity_evidence();
        let cache_eligible = indexed && cache::runtime_ast_cache_is_eligible(&relative);
        let evidence = cache_eligible
            .then(|| {
                cache::runtime_ast_cache_evidence(
                    &relative,
                    input.bytes(),
                    runtime_cache_options,
                )
            })
            .flatten();
        let hash = indexed.then(|| format!("{:x}", md5::Md5::digest(input.bytes())));
        let previous_entry = previous_for_compute.get(relative.as_str());
        let cache_policy_changed = evidence.as_ref().is_some_and(|evidence| {
            previous_entry
                .and_then(|entry| entry.runtime_cache)
                .is_none_or(|stored| stored.artifact_key != evidence.key.as_bytes())
        });
        let preflight_requires_repair = preflight_skip_probe.contains_key(relative.as_str());
        let changed = indexed
            && (force
                || !committed_baseline_eligible
                || previous_entry.is_none_or(|entry| {
                    entry.ast_version != cache::AST_CACHE_VERSION
                        || entry.ast_hash != hash.as_deref().unwrap_or_default()
                })
                || cache_policy_changed
                || preflight_requires_repair);
        let runtime_manifest = evidence.as_ref().and_then(|evidence| {
            source_identity.map(|identity| cache::RuntimeAstManifestEvidence {
                content_digest: evidence.content_digest,
                source_identity_digest: identity.digest(),
                artifact_key: evidence.key.as_bytes(),
            })
        });
        let mut row_cache = if cache_client_for_compute.is_some() {
            cache::RuntimeCacheTelemetry::enabled()
        } else {
            cache::RuntimeCacheTelemetry::default()
        };
        let mut row_cache_diagnostics = Vec::new();
        let mut extraction = None;
        let mut warning = None;
        let mut should_persist = false;
        let mut replace_existing = false;

        if changed {
            if force || evidence.is_none() {
                if cache_client_for_compute.is_some() {
                    row_cache.bypasses = row_cache.bypasses.saturating_add(1);
                }
                should_persist = evidence.is_some() && cache_client_for_compute.is_some();
                // `--force` makes the fresh parser result authoritative. A
                // valid outer runtime frame with a forged or stale inner
                // envelope must not survive as AlreadyPresent.
                replace_existing = force && evidence.is_some();
            } else if cache_client_for_compute.is_none() {
                // Startup/protocol failure is recorded once at the control
                // plane. It is not a per-file policy bypass.
            } else if let Some(preflight_replace) =
                preflight_skip_probe.get(relative.as_str()).copied()
            {
                // The control-plane metadata probe already classified this
                // exact key. Avoid counting or transferring the same miss a
                // second time after the verified source read.
                replace_existing = preflight_replace;
                should_persist = true;
            } else if let (Some(evidence), Some(client)) =
                (evidence.as_ref(), cache_client_for_compute.as_ref())
            {
                match client.probe_with_cancellation(evidence.key, &compute_cancellation) {
                    Ok(probe) => match probe.outcome {
                        graphoxide_index_runtime::cache::RuntimeCacheProbeOutcome::Hit(hit) => {
                            let source = hit.source;
                            debug_assert_eq!(
                                source,
                                graphoxide_index_runtime::cache::RuntimeCacheSource::RuntimeV1
                            );
                            match decode_admitted_runtime_cache_hit(
                                hit,
                                evidence,
                                &output_admission_for_compute,
                                &compute_cancellation,
                            ) {
                                Ok(cached) => {
                                    row_cache.record_hit(source);
                                    extraction = Some(cached);
                                }
                                Err(RuntimeCacheHitUseError::Rejected(rejection)) => {
                                    row_cache.stale_or_corrupt =
                                        row_cache.stale_or_corrupt.saturating_add(1);
                                    row_cache.misses = row_cache.misses.saturating_add(1);
                                    replace_existing = true;
                                    should_persist = true;
                                    row_cache_diagnostics.push(format!(
                                        "runtime cache envelope for {relative} was rejected ({rejection:?}); reparsing"
                                    ));
                                }
                                Err(RuntimeCacheHitUseError::ExceedsOutputAdmission) => {
                                    row_cache.misses = row_cache.misses.saturating_add(1);
                                    should_persist = true;
                                    row_cache_diagnostics.push(format!(
                                        "runtime cache payload for {relative} exceeded decoded-output admission; reparsing"
                                    ));
                                }
                            }
                        }
                        graphoxide_index_runtime::cache::RuntimeCacheProbeOutcome::Missing => {
                            row_cache.misses = row_cache.misses.saturating_add(1);
                            should_persist = true;
                        }
                        graphoxide_index_runtime::cache::RuntimeCacheProbeOutcome::RejectedCorruptOrStale => {
                            row_cache.stale_or_corrupt =
                                row_cache.stale_or_corrupt.saturating_add(1);
                            row_cache.misses = row_cache.misses.saturating_add(1);
                            should_persist = true;
                        }
                        graphoxide_index_runtime::cache::RuntimeCacheProbeOutcome::SourceChanged
                        | graphoxide_index_runtime::cache::RuntimeCacheProbeOutcome::MetadataOnlyUnsupported => {
                            row_cache.misses = row_cache.misses.saturating_add(1);
                            should_persist = true;
                        }
                    },
                    Err(error) => {
                        row_cache.probe_failures = row_cache.probe_failures.saturating_add(1);
                        row_cache.misses = row_cache.misses.saturating_add(1);
                        should_persist = true;
                        row_cache_diagnostics.push(format!(
                            "runtime cache probe for {relative} failed; reparsing: {error}"
                        ));
                    }
                }
            }

            anyhow::ensure!(
                !compute_cancellation.is_cancelled(),
                "isolated extraction cancelled"
            );
            if extraction.is_none() {
                let Some(_parser_permit) = parser_admission_for_compute
                    .acquire_with_cancellation(
                        parser_allowance_bytes,
                        Some(&compute_cancellation),
                    )
                else {
                    anyhow::ensure!(
                        !compute_cancellation.is_cancelled(),
                        "isolated extraction cancelled"
                    );
                    anyhow::bail!(
                        "isolated parser allowance exceeds its {parser_pool_bytes}-byte pool"
                    );
                };
                match engine::extract_as_bytes_with_parser_allowance_and_cancellation(
                    path,
                    &relative,
                    input.bytes(),
                    parser_allowance_bytes,
                    &compute_cancellation,
                )
                    .with_context(|| format!("extract {relative}"))
                {
                    Ok(parsed) => {
                        let retained_bytes = extraction_retained_bytes(&parsed)?;
                        if !output_admission_for_compute.try_reserve_with_cancellation(
                            retained_bytes,
                            Some(&compute_cancellation),
                        ) {
                            anyhow::ensure!(
                                !compute_cancellation.is_cancelled(),
                                "isolated extraction cancelled"
                            );
                            anyhow::bail!(
                                "isolated retained extraction output exceeds its {output_budget}-byte budget at {relative}"
                            );
                        }
                        extraction = Some(parsed);
                    }
                    Err(error) => {
                        warning = Some(format!("skipped {relative}: {error:#}"));
                    }
                }
            }
            if should_persist
                && let (Some(client), Some(evidence), Some(extraction)) = (
                    cache_client_for_compute.as_ref(),
                    evidence.as_ref(),
                    extraction.as_ref(),
                )
                && !extraction.nodes.is_empty()
            {
                anyhow::ensure!(
                    !compute_cancellation.is_cancelled(),
                    "isolated extraction cancelled"
                );
                match persist_runtime_cache_extraction(
                    client,
                    evidence,
                    extraction,
                    replace_existing,
                    &compute_cancellation,
                ) {
                    Ok(outcome) => row_cache.record_persist(outcome),
                    Err(error) => {
                        row_cache.store_failures = row_cache.store_failures.saturating_add(1);
                        row_cache_diagnostics.push(format!(
                            "runtime cache persistence for {relative} failed: {error}"
                        ));
                    }
                }
            }
        }
        anyhow::ensure!(
            !compute_cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        let snapshot_source = snapshot_required.then(|| input.into_buffer().into_vec());
        Ok(RuntimeScanRow {
            relative,
            extraction,
            mtime,
            hash: hash.unwrap_or_default(),
            changed,
            indexed,
            snapshot_source,
            warning,
            runtime_manifest,
            runtime_cache: row_cache,
            runtime_cache_diagnostics: row_cache_diagnostics,
        })
    },
    )
    .map_err(|error| anyhow::anyhow!("isolated extraction runtime failed: {error:?}"))?;

    anyhow::ensure!(
        !cancellation.is_cancelled(),
        "isolated extraction cancelled"
    );
    let mut rows = metadata_rows;
    rows.reserve(completed.completed.len());
    for completed in completed.completed {
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        rows.push(completed.value?);
    }
    rows.sort_by(|left, right| left.relative.cmp(&right.relative));
    for row in &rows {
        runtime_cache.merge(row.runtime_cache);
        runtime_cache_diagnostics.extend(row.runtime_cache_diagnostics.iter().cloned());
    }
    runtime_cache_diagnostics.sort();
    runtime_cache_diagnostics.dedup();
    drop(cache_client);
    if let Some(service) = runtime_cache_service.take()
        && let Err(error) = service.shutdown()
    {
        runtime_cache.store_failures = runtime_cache.store_failures.saturating_add(1);
        runtime_cache_diagnostics.push(format!(
            "runtime cache I/O owner did not shut down cleanly: {error}"
        ));
    }
    let mut warnings = completed
        .failures
        .iter()
        .filter(|failure| {
            contexts
                .get(failure.identity.normalized_path.as_ref())
                .is_some_and(|context| context.indexed)
        })
        .map(|failure| {
            format!(
                "skipped {}: isolated I/O failed: {:?}",
                failure.identity.normalized_path, failure.kind
            )
        })
        .collect::<Vec<_>>();
    warnings.extend(rows.iter().filter_map(|row| row.warning.clone()));
    warnings.sort();
    for warning in &warnings {
        tracing::warn!("{warning}");
    }
    let succeeded = rows
        .iter()
        .filter(|row| row.indexed && row.warning.is_none())
        .count();
    if succeeded == 0
        && let Some(warning) = warnings.first()
    {
        anyhow::bail!("{warning}");
    }
    let resolution_snapshot_diagnostics = completed
        .failures
        .iter()
        .filter(|failure| {
            contexts
                .get(failure.identity.normalized_path.as_ref())
                .is_some_and(|context| !context.indexed)
        })
        .map(|failure| {
            format!(
                "resolver metadata unavailable for {}: {:?}",
                failure.identity.normalized_path, failure.kind
            )
        })
        .collect::<Vec<_>>();
    let mut project_snapshot =
        crate::js_resolution::ProjectSnapshot::with_byte_limit(snapshot_budget);
    for row in &mut rows {
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        let Some(source) = row.snapshot_source.take() else {
            continue;
        };
        project_snapshot
            .insert_owned(row.relative.clone(), source)
            .map_err(|error| match error {
                crate::js_resolution::ProjectSnapshotError::ExceedsBudget { byte_limit } => {
                    anyhow::anyhow!(
                        "isolated project resolution snapshot exceeds its {byte_limit}-byte budget"
                    )
                }
                crate::js_resolution::ProjectSnapshotError::InvalidPath(path) => {
                    anyhow::anyhow!("invalid project snapshot path: {path}")
                }
            })?;
    }
    let mut manifest = rows
        .iter()
        .filter(|row| row.indexed && row.warning.is_none())
        .map(|row| {
            let semantic_hash = previous
                .get(&row.relative)
                .filter(|entry| {
                    entry.ast_version == cache::AST_CACHE_VERSION && entry.ast_hash == row.hash
                })
                .map(|entry| entry.semantic_hash.clone())
                .unwrap_or_default();
            (
                row.relative.clone(),
                cache::ManifestEntry {
                    mtime: row.mtime,
                    ast_version: cache::AST_CACHE_VERSION,
                    ast_hash: row.hash.clone(),
                    semantic_hash,
                    runtime_cache: row.runtime_manifest,
                },
            )
        })
        .collect::<cache::Manifest>();
    if code_only {
        for paths in detection
            .files
            .iter()
            .filter(|(kind, _)| kind.as_str() != detect::FileType::Code.as_str())
            .map(|(_, paths)| paths)
        {
            for path in paths {
                let path = std::path::Path::new(path);
                let key = normalized_project_key(path, &resolved_root, root);
                if let Some(entry) = previous.get(&key) {
                    manifest.entry(key).or_insert_with(|| entry.clone());
                }
            }
        }
    }
    let changed_sources = rows
        .iter()
        .filter(|row| row.indexed && row.changed && row.warning.is_none())
        .count();
    let changed_resolver_owner_keys = rows
        .iter()
        .filter(|row| row.indexed && row.changed)
        .map(|row| row.relative.clone())
        .collect::<std::collections::BTreeSet<_>>();
    // Only byte-identical live sources are valid lookup context. A changed
    // source whose extraction failed keeps its committed graph facts for a
    // later retry, but combining those stale nodes with its current snapshot
    // text would create a hybrid module revision.
    let eligible_resolver_owners =
        eligible_resolver_owner_keys(&contexts, &changed_resolver_owner_keys);
    let mut rebuilt_sources = rows
        .iter()
        .filter(|row| row.indexed && row.changed && row.warning.is_none())
        .filter_map(|row| {
            contexts
                .get(row.relative.as_str())
                .map(|context| context.physical_path.clone())
        })
        .collect::<Vec<_>>();
    rebuilt_sources.sort();
    rebuilt_sources.dedup();
    let unchanged_sources = succeeded.saturating_sub(changed_sources);
    let deleted_sources = previous
        .keys()
        .filter(|source| {
            contexts
                .get(source.as_str())
                .is_none_or(|context| !context.indexed)
        })
        .count();
    let mut extractions = Vec::new();
    for row in rows {
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        if let Some(extraction) = row.extraction {
            extractions.push(extraction);
        }
    }
    let fresh_has_javascript = extractions.iter().any(|extraction| {
        extraction.nodes.iter().any(|node| {
            node.extra.get("type").and_then(serde_json::Value::as_str) == Some("file")
                && crate::js_resolution::is_javascript_source(&node.source_file)
        })
    });
    let fresh_before_resolution = extractions_retained_bytes(&extractions)?;
    let mut resolver_context = Vec::new();
    let mut baseline_working_set_charge = 0usize;
    let mut fresh_output_limit = output_budget;
    if !force
        && fresh_has_javascript
        && committed_baseline_eligible
        && !eligible_resolver_owners.is_empty()
    {
        let remaining = output_budget
            .checked_sub(fresh_before_resolution)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "isolated fresh extraction output exceeds its {output_budget}-byte cache/run budget before resolver baseline admission"
                )
            })?;
        let graph_byte_cap = remaining / RESOLUTION_BASELINE_WORKING_SET_MULTIPLIER;
        let context = load_resolver_baseline_context(
            &baseline_graph_path,
            u64::try_from(graph_byte_cap).unwrap_or(u64::MAX),
            &eligible_resolver_owners,
            &resolved_root,
            root,
        )?;
        if !context.extractions.is_empty() {
            anyhow::ensure!(
                context.working_set_charge <= remaining,
                "resolver baseline requires a {}-byte graph working set, exceeding {remaining} remaining cache/run bytes",
                context.working_set_charge
            );
            debug_assert_eq!(
                context.retained_bytes,
                extractions_retained_bytes(&context.extractions)?
            );
            baseline_working_set_charge = context.working_set_charge;
            fresh_output_limit = output_budget
                .checked_sub(baseline_working_set_charge)
                .expect("admitted baseline charge fits output budget");
            resolver_context = context.extractions;
        }
    }
    anyhow::ensure!(
        !cancellation.is_cancelled(),
        "isolated extraction cancelled"
    );
    resolution::resolve_with_snapshot_context_bounded(
        &mut extractions,
        resolver_context,
        &project_snapshot,
        fresh_output_limit,
        config.memory_budget().cpu_arenas_bytes,
    )?;
    anyhow::ensure!(
        !cancellation.is_cancelled(),
        "isolated extraction cancelled"
    );
    let retained_output_bytes = extractions_retained_bytes(&extractions)?;
    let cache_run_total = retained_output_bytes
        .checked_add(baseline_working_set_charge)
        .ok_or_else(|| anyhow::anyhow!("isolated resolver cache/run charge exceeds usize"))?;
    anyhow::ensure!(
        cache_run_total <= output_budget,
        "isolated resolver retains {retained_output_bytes} fresh bytes plus a {baseline_working_set_charge}-byte baseline working-set charge, exceeding its {output_budget}-byte cache/run budget"
    );
    debug_assert!(output_admission.retained_bytes() <= output_budget);
    Ok(DeferredProjectExtractionResult {
        extractions,
        retained_output_bytes,
        detection,
        progress: ProjectExtractionProgress {
            total: total_work,
            succeeded,
        },
        warnings,
        rebuilt_sources,
        changed_sources,
        unchanged_sources,
        deleted_sources,
        runtime_cache,
        runtime_cache_diagnostics,
        resolution_snapshot_diagnostics,
        pending_manifest: PendingProjectManifest {
            output_directory: managed_output_dir,
            entries: manifest,
        },
    })
}

/// Extract a project with caller-controlled ignore policy and retain the full
/// discovery diagnostics. CLI callers use this to persist `--exclude` and
/// `--no-gitignore` without performing a second, potentially divergent scan.
pub fn extract_project_with_scan_options(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
) -> anyhow::Result<ProjectExtractionResult> {
    extract_project_with_scan_options_deferred_manifest(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
    )?
    .commit_manifest()
}

/// Extract a project without publishing its next manifest. This is the safe
/// entry point for graph-building callers: write the graph using `progress`,
/// then call `pending_manifest.commit()` only when that write succeeds.
pub fn extract_project_with_scan_options_deferred_manifest(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
) -> anyhow::Result<DeferredProjectExtractionResult> {
    use rayon::prelude::*;
    let managed_output_dir = if managed_output_dir.is_absolute() {
        managed_output_dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(managed_output_dir)
    };
    let mut detect_options = detect_options.clone();
    // Discovery and cache persistence must agree on which generated directory
    // belongs to this build. A mismatched caller option could otherwise ingest
    // the real managed output back into the corpus.
    detect_options.output_dir = Some(managed_output_dir.clone());
    let detection = detect::detect(root, &detect_options)?;
    let mut files = detection
        .files
        .iter()
        .filter(|(kind, _)| !code_only || kind.as_str() == detect::FileType::Code.as_str())
        .flat_map(|(_, paths)| paths)
        .map(std::path::PathBuf::from)
        .filter(|path| detection.is_supported_source(path))
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    let total_work = files.len().saturating_add(detection.walk_errors.len());
    let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    // One unreadable or unextractable file must not abort the scan. Each
    // failure becomes a warning and one unsuccessful unit of progress, which
    // the build guard already interprets as an incomplete build.
    let outcomes: Vec<anyhow::Result<_>> = files
        .par_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&resolved_root)
                .or_else(|_| path.strip_prefix(root))
                .map_or_else(
                    |_| normalized_project_key(path, &resolved_root, root),
                    |relative| normalized_project_key(relative, &resolved_root, root),
                );
            extract_one_project_file(path, &relative, force, &managed_output_dir)
                .with_context(|| format!("skipped {relative}"))
        })
        .collect();
    let mut rows = Vec::with_capacity(outcomes.len());
    let mut warnings = Vec::new();
    let mut failures: Vec<anyhow::Error> = Vec::new();
    for outcome in outcomes {
        match outcome {
            Ok(row) => rows.push(row),
            Err(error) => {
                let warning = format!("{error:#}");
                tracing::warn!("{warning}");
                warnings.push(warning);
                failures.push(error);
            }
        }
    }
    // Individual bad files are tolerated; a corpus in which nothing at all
    // could be extracted is a broken backend, not an empty success.
    if rows.is_empty()
        && let Some(first) = failures.into_iter().next()
    {
        return Err(first);
    }
    let succeeded = rows.len();
    let mut rebuilt_sources = rows
        .iter()
        .map(|(relative, _, _, _)| {
            let logical = resolved_root.join(relative);
            detection.physical_source(&logical)
        })
        .collect::<Vec<_>>();
    rebuilt_sources.sort();
    rebuilt_sources.dedup();
    let previous = normalized_previous_manifest(
        &cache::load_manifest_from_output(&managed_output_dir),
        &resolved_root,
        root,
    );
    let mut manifest: cache::Manifest = rows
        .iter()
        .map(|(relative, _, mtime, hash)| {
            let semantic_hash = previous
                .get(relative)
                .filter(|entry| {
                    entry.ast_version == cache::AST_CACHE_VERSION && entry.ast_hash == *hash
                })
                .map(|entry| entry.semantic_hash.clone())
                .unwrap_or_default();
            (
                relative.clone(),
                cache::ManifestEntry {
                    mtime: *mtime,
                    ast_version: cache::AST_CACHE_VERSION,
                    ast_hash: hash.clone(),
                    semantic_hash,
                    runtime_cache: None,
                },
            )
        })
        .collect();
    if code_only {
        for paths in detection
            .files
            .iter()
            .filter(|(kind, _)| kind.as_str() != detect::FileType::Code.as_str())
            .map(|(_, paths)| paths)
        {
            for path in paths {
                let path = std::path::Path::new(path);
                let key = normalized_project_key(path, &resolved_root, root);
                if let Some(entry) = previous.get(&key) {
                    manifest.entry(key).or_insert_with(|| entry.clone());
                }
            }
        }
    }
    let mut extractions: Vec<_> = rows
        .into_iter()
        .map(|(_, extraction, _, _)| extraction)
        .collect();
    resolution::resolve_with_root(&mut extractions, &resolved_root);
    let retained_output_bytes = extractions_retained_bytes(&extractions)?;
    Ok(DeferredProjectExtractionResult {
        extractions,
        retained_output_bytes,
        detection,
        progress: ProjectExtractionProgress {
            total: total_work,
            succeeded,
        },
        warnings,
        rebuilt_sources,
        changed_sources: succeeded,
        unchanged_sources: 0,
        deleted_sources: 0,
        runtime_cache: cache::RuntimeCacheTelemetry::default(),
        runtime_cache_diagnostics: Vec::new(),
        resolution_snapshot_diagnostics: Vec::new(),
        pending_manifest: PendingProjectManifest {
            output_directory: managed_output_dir,
            entries: manifest,
        },
    })
}

#[derive(Debug, Clone)]
pub struct ExtractFilesResult {
    /// One extraction per input that was successfully processed, in input
    /// order. Skipped inputs produce no entry, so callers pairing extractions
    /// with their inputs must filter `skipped` out of the input list first.
    pub extractions: Vec<graphoxide_core::Extraction>,
    pub warnings: Vec<String>,
    /// Inputs that could not be read or extracted, exactly as they were passed.
    ///
    /// A caller holding a previous graph uses this to keep those files' records
    /// rather than treating them as rebuilt-to-nothing.
    pub skipped: Vec<std::path::PathBuf>,
    pub key_root: std::path::PathBuf,
    pub managed_output_dir: std::path::PathBuf,
}

/// Explicit-file extraction whose replacement manifest is not yet visible.
///
/// Graph-building callers must accept their graph artifact before committing
/// this manifest. Dropping the pending manifest keeps the prior scan state
/// intact, which makes graph-build failures and shrink refusals retryable.
#[derive(Debug)]
#[must_use = "commit this manifest only after the corresponding graph write succeeds"]
pub struct DeferredExtractFilesResult {
    pub result: ExtractFilesResult,
    pub pending_manifest: PendingProjectManifest,
}

impl DeferredExtractFilesResult {
    /// Publish the prepared manifest and return the legacy result shape.
    pub fn commit_manifest(self) -> anyhow::Result<ExtractFilesResult> {
        self.pending_manifest.commit()?;
        Ok(self.result)
    }

    /// Deliberately retain the previous manifest. Callers that publish a
    /// separately reconstructed full-corpus manifest after graph acceptance
    /// use this to consume the extraction without exposing the target subset.
    pub fn discard_manifest(self) -> ExtractFilesResult {
        self.result
    }
}

fn common_file_parent(files: &[std::path::PathBuf]) -> anyhow::Result<std::path::PathBuf> {
    anyhow::ensure!(!files.is_empty(), "at least one input file is required");
    let resolved = files
        .iter()
        .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect::<Vec<_>>();
    let mut root = resolved[0]
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("input file has no parent: {}", files[0].display()))?;
    while !resolved.iter().all(|path| path.starts_with(&root)) {
        root = root
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("input files have no common parent"))?;
    }
    Ok(root)
}

/// Extract an explicit file set. When `cache_root` contains every input it is
/// also the source-identity anchor, matching upstream's root fallback. An
/// out-of-tree cache root remains storage-only and source identity falls back
/// to the files' common corpus parent.
pub fn extract_files(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    force: bool,
) -> anyhow::Result<ExtractFilesResult> {
    extract_files_deferred_manifest(files, cache_root, force)?.commit_manifest()
}

/// Extract an explicit file set without publishing its replacement manifest.
pub fn extract_files_deferred_manifest(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    force: bool,
) -> anyhow::Result<DeferredExtractFilesResult> {
    extract_files_with_deferred_manifest(files, cache_root, force, |path, relative| {
        engine::extract_as(path, relative)
    })
}

/// Injectable variant used by backend adapters and failure-path tests.
pub fn extract_files_with<F>(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    force: bool,
    extractor: F,
) -> anyhow::Result<ExtractFilesResult>
where
    F: Fn(&std::path::Path, &str) -> anyhow::Result<graphoxide_core::Extraction>,
{
    extract_files_with_deferred_manifest(files, cache_root, force, extractor)?.commit_manifest()
}

/// Injectable deferred-manifest variant used by graph-building adapters and
/// failure-path tests.
pub fn extract_files_with_deferred_manifest<F>(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    force: bool,
    extractor: F,
) -> anyhow::Result<DeferredExtractFilesResult>
where
    F: Fn(&std::path::Path, &str) -> anyhow::Result<graphoxide_core::Extraction>,
{
    use md5::Digest as _;
    let common_root = common_file_parent(files)?;
    let key_root = cache_root
        .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()))
        .filter(|root| {
            files.iter().all(|path| {
                std::fs::canonicalize(path)
                    .unwrap_or_else(|_| path.clone())
                    .starts_with(root)
            })
        })
        .unwrap_or(common_root);
    let cache_base = cache_root.map(std::path::Path::to_path_buf).unwrap_or(
        std::env::current_dir()
            .map_err(|error| anyhow::anyhow!("resolve current directory: {error}"))?,
    );
    let managed_output_dir = cache_base.join("graphoxide-out");
    let previous = cache::load_manifest_from_output(&managed_output_dir);
    let mut rows = Vec::with_capacity(files.len());
    let mut warnings = Vec::new();
    // A per-file fault is tolerated, but a fault on every dispatched file is
    // evidence of a broken extraction backend rather than of bad inputs, and
    // must not be reported as an empty success.
    let mut failures: Vec<anyhow::Error> = Vec::new();
    let mut skipped: Vec<std::path::PathBuf> = Vec::new();
    let mut missing_extractors = std::collections::BTreeMap::<String, usize>::new();
    for original in files {
        let path = std::fs::canonicalize(original).unwrap_or_else(|_| original.clone());
        let relative = path
            .strip_prefix(&key_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        // A file-specific fault costs that file, not the run.
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                let warning = format!("skipped {relative}: {error}");
                tracing::warn!("{warning}");
                warnings.push(warning);
                failures.push(anyhow::Error::new(error).context(format!("read {relative}")));
                skipped.push(original.clone());
                continue;
            }
        };
        let cached = (!force)
            .then(|| cache::ast_cache_get_from_output(&managed_output_dir, &relative, &bytes))
            .flatten();
        let extraction = if let Some(cached) = cached {
            cached
        } else {
            let extracted =
                match extractor(&path, &relative).with_context(|| format!("extract {relative}")) {
                    Ok(extracted) => extracted,
                    Err(error) => {
                        let warning = format!("skipped {relative}: {error:#}");
                        tracing::warn!("{warning}");
                        warnings.push(warning);
                        failures.push(error);
                        skipped.push(original.clone());
                        continue;
                    }
                };
            if extracted.nodes.is_empty() {
                if detect::classify_file(&path) == Some(detect::FileType::Code)
                    && !engine::has_ast_extractor(&path)
                {
                    let suffix = path
                        .extension()
                        .and_then(|value| value.to_str())
                        .map(|value| format!(".{}", value.to_ascii_lowercase()))
                        .unwrap_or_else(|| "<extensionless>".into());
                    *missing_extractors.entry(suffix).or_default() += 1;
                } else {
                    warnings.push(format!(
                        "{} produced zero nodes; the anomalous result was not cached and will be retried",
                        relative
                    ));
                }
            } else if let Err(error) =
                cache::ast_cache_put_to_output(&managed_output_dir, &relative, &bytes, &extracted)
            {
                tracing::warn!("{relative}: caching the extraction failed: {error:#}");
            }
            extracted
        };
        let mtime = match std::fs::metadata(&path).and_then(|metadata| metadata.modified()) {
            Ok(modified) => modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            Err(error) => {
                let warning = format!("skipped {relative}: {error}");
                tracing::warn!("{warning}");
                warnings.push(warning);
                failures.push(anyhow::Error::new(error).context(format!("stat {relative}")));
                skipped.push(original.clone());
                continue;
            }
        };
        let hash = if extraction.nodes.is_empty() {
            String::new()
        } else {
            format!("{:x}", md5::Md5::digest(&bytes))
        };
        rows.push((relative, extraction, mtime, hash));
    }
    if rows.is_empty()
        && let Some(first) = failures.into_iter().next()
    {
        return Err(first);
    }
    if !missing_extractors.is_empty() {
        let summary = missing_extractors
            .into_iter()
            .map(|(suffix, count)| format!("{suffix} ({count})"))
            .collect::<Vec<_>>()
            .join(", ");
        warnings.push(format!(
            "code files have no AST extractor (#1689): {summary}"
        ));
    }
    let manifest = rows
        .iter()
        .map(|(relative, _, mtime, hash)| {
            let semantic_hash = previous
                .get(relative)
                .filter(|entry| {
                    entry.ast_version == cache::AST_CACHE_VERSION
                        && entry.ast_hash == *hash
                        && !hash.is_empty()
                })
                .map(|entry| entry.semantic_hash.clone())
                .unwrap_or_default();
            (
                relative.clone(),
                cache::ManifestEntry {
                    mtime: *mtime,
                    ast_version: cache::AST_CACHE_VERSION,
                    ast_hash: hash.clone(),
                    semantic_hash,
                    runtime_cache: None,
                },
            )
        })
        .collect();
    let mut extractions = rows
        .into_iter()
        .map(|(_, extraction, _, _)| extraction)
        .collect::<Vec<_>>();
    resolution::resolve_with_root(&mut extractions, &key_root);
    Ok(DeferredExtractFilesResult {
        result: ExtractFilesResult {
            extractions,
            warnings,
            skipped,
            key_root,
            managed_output_dir: managed_output_dir.clone(),
        },
        pending_manifest: PendingProjectManifest {
            output_directory: managed_output_dir,
            entries: manifest,
        },
    })
}

#[cfg(test)]
mod tests {
    use graphoxide_core::{make_id, Edge, Extraction};
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "graphoxide-injected-calls-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).expect("create extraction fixture");
            Self { root }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.root.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create extraction fixture parent");
            }
            fs::write(&path, contents).expect("write extraction fixture");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).expect("remove extraction fixture");
        }
    }

    fn extract(path: &Path, source_file: &str) -> graphoxide_core::Extraction {
        super::engine::extract_as(path, source_file).expect("extract fixture file")
    }

    fn runtime_config(memory_budget_bytes: usize) -> graphoxide_index_runtime::IndexRuntimeConfig {
        graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes,
            io_workers: 2,
            compute_workers: 2,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        }
    }

    fn commit_runtime_baseline(
        result: super::DeferredProjectExtractionResult,
        root: &Path,
        output: &Path,
    ) -> graphoxide_core::KnowledgeGraph {
        fs::create_dir_all(output).expect("create managed output");
        let graph = graphoxide_graph::build_graph_with_options_and_root(
            &result.extractions,
            root,
            graphoxide_graph::BuildOptions::default(),
        )
        .expect("build committed baseline");
        graphoxide_core::write_graph_atomic(output.join("graph.json"), &graph, true)
            .expect("write committed graph");
        result
            .pending_manifest
            .commit()
            .expect("commit matching manifest");
        graph
    }

    fn js_edge_topology(
        graph: &graphoxide_core::KnowledgeGraph,
        source_file: &str,
    ) -> std::collections::BTreeSet<(String, String, String, Option<String>)> {
        graph
            .links
            .iter()
            .filter(|edge| {
                edge.source_file == source_file
                    && matches!(
                        edge.relation.as_str(),
                        "imports" | "imports_from" | "re_exports"
                    )
            })
            .map(|edge| {
                (
                    edge.true_source().to_owned(),
                    edge.true_target().to_owned(),
                    edge.relation.clone(),
                    edge.extra
                        .get("target_file")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                )
            })
            .collect()
    }

    fn byond_include_edge(extraction: &Extraction) -> &Edge {
        extraction
            .edges
            .iter()
            .find(|edge| {
                edge.extra
                    .get("context")
                    .and_then(serde_json::Value::as_str)
                    == Some("import")
            })
            .expect("BYOND include edge")
    }

    #[test]
    fn byte_engine_extracts_without_source_path_access() {
        let missing = Path::new("/graphoxide-byte-only-does-not-exist.rs");
        let extraction = super::engine::extract_as_bytes(
            missing,
            "byte_only.rs",
            b"pub fn admitted_source() {}\n",
        )
        .expect("extract admitted source bytes");
        assert!(
            extraction
                .nodes
                .iter()
                .any(|node| node.label == "admitted_source()"),
            "the byte entrypoint must not reopen its source path"
        );
    }

    #[test]
    fn byond_byte_engine_never_probes_include_siblings() {
        let source = "#include \"helpers.dm\"\n/proc/RunTest()\n\treturn\n";
        for extension in ["dm", "dme"] {
            let with_sibling = Fixture::new();
            let with_sibling_path = with_sibling.write(&format!("main.{extension}"), source);
            with_sibling.write("helpers.dm", "/proc/Helper()\n\treturn\n");

            let without_sibling = Fixture::new();
            let without_sibling_path = without_sibling.write(&format!("main.{extension}"), source);
            let source_file = format!("project/main.{extension}");

            let existing = super::engine::extract_as_bytes(
                &with_sibling_path,
                &source_file,
                source.as_bytes(),
            )
            .expect("extract BYOND bytes beside an existing include");
            let missing = super::engine::extract_as_bytes(
                &without_sibling_path,
                &source_file,
                source.as_bytes(),
            )
            .expect("extract BYOND bytes beside a missing include");

            assert_eq!(
                serde_json::to_value(&existing).expect("serialize existing-sibling extraction"),
                serde_json::to_value(&missing).expect("serialize missing-sibling extraction"),
                ".{extension} byte extraction must be independent of sibling existence",
            );
            let include = byond_include_edge(&existing);
            assert_eq!(include.relation, "imports");
            assert_eq!(include.true_target(), make_id(&["project/helpers"]));
            assert_eq!(
                include
                    .extra
                    .get("target_file")
                    .and_then(serde_json::Value::as_str),
                Some("project/helpers.dm"),
            );
            assert_eq!(
                include
                    .extra
                    .get("external")
                    .and_then(serde_json::Value::as_bool),
                Some(true),
                ".{extension} byte extraction must fail closed to an unresolved include",
            );
        }
    }

    #[test]
    fn legacy_byond_path_engine_preserves_include_resolution() {
        let source = "#include \"helpers.dm\"\n/proc/RunTest()\n\treturn\n";
        for extension in ["dm", "dme"] {
            let with_sibling = Fixture::new();
            let with_sibling_path = with_sibling.write(&format!("main.{extension}"), source);
            let sibling_path = with_sibling.write("helpers.dm", "/proc/Helper()\n\treturn\n");
            let source_file = format!("project/main.{extension}");
            let resolved = extract(&with_sibling_path, &source_file);
            let resolved_include = byond_include_edge(&resolved);

            assert_eq!(resolved_include.relation, "imports_from");
            assert_eq!(
                resolved_include.true_target(),
                make_id(&["project/helpers"])
            );
            assert_eq!(
                resolved_include
                    .extra
                    .get("target_file")
                    .and_then(serde_json::Value::as_str),
                Some("project/helpers.dm"),
            );
            assert!(!resolved_include.extra.contains_key("external"));
            let expected_sibling_source = sibling_path.to_string_lossy().replace('\\', "/");
            assert!(resolved.nodes.iter().any(|node| {
                node.id == resolved_include.true_target()
                    && node.source_file == expected_sibling_source
            }));

            let without_sibling = Fixture::new();
            let without_sibling_path = without_sibling.write(&format!("main.{extension}"), source);
            let unresolved = extract(&without_sibling_path, &source_file);
            let unresolved_include = byond_include_edge(&unresolved);

            assert_eq!(unresolved_include.relation, "imports");
            assert_eq!(
                unresolved_include.true_target(),
                make_id(&["project/helpers"])
            );
            assert_eq!(
                unresolved_include
                    .extra
                    .get("target_file")
                    .and_then(serde_json::Value::as_str),
                Some("project/helpers.dm"),
            );
            assert_eq!(
                unresolved_include
                    .extra
                    .get("external")
                    .and_then(serde_json::Value::as_bool),
                Some(true),
            );
        }
    }

    #[test]
    fn c_byte_engine_normalizes_only_contained_quoted_includes() {
        let fixture = Fixture::new();
        fixture.write("include/worker.h", "int root_worker(void);\n");
        fixture.write("src/include/worker.h", "int nested_worker(void);\n");

        let parent_source = "#include \"../include/worker.h\"\nint main(void) { return 0; }\n";
        let parent_path = fixture.write("src/main.c", parent_source);
        let parent =
            super::engine::extract_as_bytes(&parent_path, "src/main.c", parent_source.as_bytes())
                .expect("extract parent-relative C include bytes");
        let parent_targets = parent
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports")
            .map(Edge::true_target)
            .collect::<Vec<_>>();
        assert_eq!(parent_targets, [make_id(&["include/worker"])]);
        assert!(!parent_targets.contains(&make_id(&["src/include/worker"]).as_str()));

        let static_source =
            "#include \"./include/./worker.h\"\nint static_main(void) { return 0; }\n";
        let static_path = fixture.write("src/static.c", static_source);
        let static_extraction =
            super::engine::extract_as_bytes(&static_path, "src/static.c", static_source.as_bytes())
                .expect("extract contained C include bytes");
        assert!(static_extraction.edges.iter().any(|edge| {
            edge.relation == "imports" && edge.true_target() == make_id(&["src/include/worker"])
        }));

        let unsafe_source = concat!(
            "#include \"/src/include/worker.h\"\n",
            "#include \"C:/src/include/worker.h\"\n",
            "#include \"C:src/include/worker.h\"\n",
            "#include \"\\\\server\\share\\src\\include\\worker.h\"\n",
            "#include \"../../src/include/worker.h\"\n",
        );
        let unsafe_path = fixture.write("src/unsafe.c", unsafe_source);
        let unsafe_extraction =
            super::engine::extract_as_bytes(&unsafe_path, "src/unsafe.c", unsafe_source.as_bytes())
                .expect("extract unsafe C include bytes");
        assert!(unsafe_extraction
            .edges
            .iter()
            .all(|edge| edge.relation != "imports"));
    }

    #[test]
    fn byte_engine_keeps_sfc_and_pascal_path_access_in_the_io_plane() {
        let missing_vue = Path::new("/graphoxide-byte-only-does-not-exist/component.vue");
        let vue = super::engine::extract_as_bytes(
            missing_vue,
            "src/component.vue",
            b"<template><main /></template>\n<script lang=\"ts\">export const admitted = 1;</script>\n",
        )
        .expect("extract admitted SFC source bytes");
        assert!(
            vue.nodes.iter().any(|node| node.label == "component.vue"),
            "SFC byte extraction must not reopen the physical source path"
        );

        let missing_pascal = Path::new("/graphoxide-byte-only-does-not-exist/main.pas");
        let pascal = super::engine::extract_as_bytes(
            missing_pascal,
            "src/main.pas",
            b"unit Main; uses Sibling; interface implementation end.",
        )
        .expect("extract admitted Pascal source bytes");
        assert!(
            pascal.edges.iter().any(|edge| {
                edge.relation == "imports" && edge.target == graphoxide_core::make_id(&["Sibling"])
            }),
            "Pascal byte extraction must use a stable unresolved unit identity instead of probing siblings"
        );
    }

    #[test]
    fn isolated_runtime_restores_deterministic_input_order() {
        let fixture = Fixture::new();
        fixture.write("z.rs", "pub fn zed() {}\n");
        fixture.write("a.rs", "pub fn alpha() {}\n");
        let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes: 4 * 1024 * 1024,
            io_workers: 2,
            compute_workers: 2,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        };
        let result = super::extract_project_with_runtime(&fixture.root, runtime)
            .expect("isolated runtime extraction");
        assert!(result.read_failures.is_empty());
        let source_files = result
            .extractions
            .iter()
            .filter_map(|extraction| extraction.nodes.first())
            .map(|node| node.source_file.as_str())
            .collect::<Vec<_>>();
        assert_eq!(source_files, ["a.rs", "z.rs"]);
    }

    #[test]
    fn isolated_runtime_admits_office_container_before_any_sidecar_conversion() {
        let fixture = Fixture::new();
        let office = fixture.root.join("report.docx");
        fs::write(&office, vec![b'x'; 256 * 1024]).expect("write oversized Office fixture");
        let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes: 256 * 1024,
            io_workers: 1,
            compute_workers: 1,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        };

        let result = super::extract_project_with_runtime(&fixture.root, runtime)
            .expect("isolated runtime reports bounded source rejection");
        assert!(result.detection.files["document"]
            .iter()
            .any(|path| path.ends_with("report.docx")));
        assert!(result.extractions.is_empty());
        assert_eq!(result.read_failures.len(), 1);
        assert!(matches!(
            result.read_failures[0].kind,
            graphoxide_index_runtime::FileReadFailureKind::ExceedsReadyBudget { .. }
        ));
        assert!(
            !fixture.root.join("graphoxide-out/converted").exists(),
            "isolated discovery must not materialize Office sidecars before runtime admission"
        );
    }

    #[test]
    fn isolated_parser_allowance_is_worker_independent_and_aggregate_bounded() {
        let serial = graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes: 512 * 1024 * 1024,
            io_workers: 1,
            compute_workers: 1,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        };
        let parallel = graphoxide_index_runtime::IndexRuntimeConfig {
            compute_workers: 8,
            ..serial
        };
        let serial_evidence = serial.execution_evidence(8);
        let parallel_evidence = parallel.execution_evidence(8);
        assert_eq!(serial_evidence.effective_compute_workers, 1);
        assert_eq!(parallel_evidence.effective_compute_workers, 8);

        let (extract_only, no_snapshot) = super::isolated_parser_layout(serial, false);
        let (parallel_extract_only, parallel_no_snapshot) =
            super::isolated_parser_layout(parallel, false);
        assert_eq!(no_snapshot, 0);
        assert_eq!(parallel_no_snapshot, 0);
        assert_eq!(extract_only, parallel_extract_only);
        assert_eq!(extract_only, super::MAX_ISOLATED_PARSER_ALLOWANCE_BYTES);

        let (scan_parser, snapshot) = super::isolated_parser_layout(serial, true);
        let (parallel_scan_parser, parallel_snapshot) =
            super::isolated_parser_layout(parallel, true);
        assert_eq!(scan_parser, parallel_scan_parser);
        assert_eq!(snapshot, parallel_snapshot);
        assert_eq!(scan_parser, super::MAX_ISOLATED_PARSER_ALLOWANCE_BYTES);
        assert_eq!(snapshot, parallel_evidence.cpu_arenas_bytes / 2);
        let parser_pool = parallel_evidence.cpu_arenas_bytes.saturating_sub(snapshot);
        assert!(parser_pool / scan_parser >= 3);
    }

    #[test]
    fn isolated_parser_admission_wait_is_cancellation_aware() {
        let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes: 4 * 1024 * 1024,
            io_workers: 1,
            compute_workers: 8,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        };
        let (allowance, snapshot) = super::isolated_parser_layout(runtime, true);
        let parser_pool = runtime
            .memory_budget()
            .cpu_arenas_bytes
            .saturating_sub(snapshot);
        assert_eq!(allowance, parser_pool);
        let admission = std::sync::Arc::new(super::RuntimeParserAdmission::new(parser_pool));
        let held = admission
            .acquire_with_cancellation(allowance, None)
            .expect("first parser permit");
        assert_eq!(admission.active_bytes(), allowance);

        let cancellation = graphoxide_index_runtime::RuntimeCancellation::new();
        let worker_admission = std::sync::Arc::clone(&admission);
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let admitted = worker_admission
                .acquire_with_cancellation(allowance, Some(&worker_cancellation))
                .is_some();
            sender.send(admitted).expect("return parser admission");
        });
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_millis(25)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        cancellation.cancel();
        assert!(!receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("cancelled parser waiter wakes"));
        drop(held);
        worker.join().expect("parser admission worker");
        assert_eq!(admission.active_bytes(), 0);
    }

    #[test]
    fn near_limit_parser_facts_and_manifest_keys_are_worker_independent() {
        let fixture = Fixture::new();
        let serial_output = tempfile::tempdir().expect("serial output root");
        let parallel_output = tempfile::tempdir().expect("parallel output root");
        let serial_runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes: 512 * 1024 * 1024,
            io_workers: 1,
            compute_workers: 1,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 64 * 1024,
        };
        let parallel_runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            io_workers: 2,
            compute_workers: 4,
            ..serial_runtime
        };
        let (allowance, _) = super::isolated_parser_layout(serial_runtime, true);
        assert_eq!(allowance, 16 * 1024 * 1024);
        assert!(super::parser_budget::ParserPlan::for_source(allowance, 1022 * 1024).is_some());
        assert!(
            super::parser_budget::ParserPlan::for_source(allowance, 1024 * 1024).is_none(),
            "the fixed policy must reject a registered source whose 16x scratch plus fixed overhead exceeds 16 MiB"
        );

        for index in 0..4 {
            let declaration = format!("message NearLimit{index} {{ string value = 1; }}\n");
            let target_len: usize = 1022 * 1024;
            let mut source = "// bounded parser padding\n".repeat(
                target_len
                    .saturating_sub(declaration.len())
                    .div_ceil("// bounded parser padding\n".len()),
            );
            source.truncate(target_len.saturating_sub(declaration.len()));
            source.push_str(&declaration);
            fixture.write(&format!("near_{index}.proto"), &source);
        }

        let serial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &serial_output.path().join("graphoxide-out"),
            false,
            &super::detect::DetectOptions::default(),
            serial_runtime,
        )
        .expect("serial near-limit extraction");
        let parallel = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &parallel_output.path().join("graphoxide-out"),
            false,
            &super::detect::DetectOptions::default(),
            parallel_runtime,
        )
        .expect("parallel near-limit extraction");
        assert_eq!(serial.runtime_cache.stores, 4);
        assert_eq!(parallel.runtime_cache.stores, 4);
        assert_eq!(
            serde_json::to_vec(&serial.extractions).expect("serial extraction JSON"),
            serde_json::to_vec(&parallel.extractions).expect("parallel extraction JSON"),
            "near-limit bounded facts must not depend on worker count"
        );
        assert_eq!(
            serde_json::to_vec_pretty(&serial.pending_manifest.entries)
                .expect("serial manifest JSON"),
            serde_json::to_vec_pretty(&parallel.pending_manifest.entries)
                .expect("parallel manifest JSON"),
            "cache keys and strong source identity must be scheduler independent"
        );
    }

    #[test]
    fn bounded_manifest_normalization_drops_hostile_absolute_keys_lexically() {
        let fixture = Fixture::new();
        let inside = fixture.root.join("inside.py");
        let mut manifest = super::cache::Manifest::new();
        manifest.insert(
            inside.to_string_lossy().into_owned(),
            super::cache::ManifestEntry {
                ast_hash: "inside".into(),
                ..super::cache::ManifestEntry::default()
            },
        );
        manifest.insert(
            "/definitely-outside-graphoxide/secret.py".into(),
            super::cache::ManifestEntry {
                ast_hash: "outside".into(),
                ..super::cache::ManifestEntry::default()
            },
        );
        manifest.insert(
            r"C:\\host\share\secret.py".into(),
            super::cache::ManifestEntry {
                ast_hash: "windows-hostile".into(),
                ..super::cache::ManifestEntry::default()
            },
        );
        manifest.insert(
            "relative.py".into(),
            super::cache::ManifestEntry {
                ast_hash: "relative".into(),
                ..super::cache::ManifestEntry::default()
            },
        );

        let normalized =
            super::normalized_previous_manifest_owned(manifest, &fixture.root, &fixture.root);
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized["inside.py"].ast_hash, "inside");
        assert_eq!(normalized["relative.py"].ast_hash, "relative");
    }

    #[test]
    fn runtime_manifest_reservation_keeps_the_full_normalization_expansion() {
        let budget = 64 * 1024 * 1024;
        let byte_limit = super::runtime_manifest_byte_limit(budget, usize::MAX);
        let reservation = super::runtime_manifest_reservation(budget, usize::MAX);
        assert_eq!(byte_limit, budget / 64);
        assert_eq!(reservation, byte_limit * 32);
        assert_eq!(reservation, budget / 2);
    }

    #[test]
    fn runtime_cache_dense_extra_maps_are_precharged_before_decode() {
        let relative = "dense.py";
        let source = b"def dense(): pass\n";
        let evidence = super::cache::runtime_ast_cache_evidence(
            relative,
            source,
            super::cache::RuntimeAstCacheOptions::isolated(1024 * 1024),
        )
        .expect("runtime cache evidence");
        let mut extra = std::collections::BTreeMap::new();
        for index in 0..4_096 {
            extra.insert(
                format!("k{index:04}"),
                serde_json::Value::String("v".into()),
            );
        }
        let extraction = graphoxide_core::Extraction {
            nodes: vec![graphoxide_core::Node {
                id: "dense".into(),
                label: "dense()".into(),
                file_type: "code".into(),
                source_file: relative.into(),
                source_location: Some("L1".into()),
                community: None,
                extra,
            }],
            ..graphoxide_core::Extraction::default()
        };
        let payload = super::cache::encode_runtime_ast_cache_payload(&evidence, &extraction)
            .expect("encode dense envelope");
        let retained = super::extraction_retained_bytes(&extraction).expect("retained estimate");
        super::cache::validate_runtime_ast_cache_payload_header(
            graphoxide_index_runtime::cache::RuntimeCacheSource::RuntimeV1,
            &payload,
            &evidence,
        )
        .expect("validated runtime cache header");
        let decode_charge = payload
            .len()
            .checked_mul(super::RUNTIME_CACHE_DECODE_EXPANSION_MULTIPLIER)
            .expect("bounded decode charge");
        assert!(
            decode_charge >= retained,
            "conservative decode charge {decode_charge} must cover {retained} retained bytes"
        );
        let replay = super::cache::decode_runtime_ast_cache_payload(
            graphoxide_index_runtime::cache::RuntimeCacheSource::RuntimeV1,
            &payload,
            &evidence,
        )
        .expect("dense envelope direct round trip");
        assert_eq!(replay.nodes.len(), extraction.nodes.len());

        let temp = tempfile::tempdir().expect("temporary runtime cache");
        let mut runtime = graphoxide_index_runtime::cache::RuntimeCache::open(temp.path())
            .expect("open runtime cache");
        runtime
            .put(evidence.key, &payload)
            .expect("persist dense envelope");
        let hit = runtime.get(evidence.key).expect("dense runtime hit");
        let admission = super::RuntimeOutputAdmission::new(decode_charge.saturating_sub(1));
        assert!(matches!(
            super::decode_admitted_runtime_cache_hit(
                hit,
                &evidence,
                &admission,
                &graphoxide_index_runtime::RuntimeCancellation::new(),
            ),
            Err(super::RuntimeCacheHitUseError::ExceedsOutputAdmission)
        ));
        assert_eq!(
            admission.retained_bytes(),
            0,
            "a pre-decode admission miss must release all temporary credit"
        );

        let mut wrong_header = payload.clone();
        wrong_header[0] ^= 0x5a;
        let wrong_header_temp = tempfile::tempdir().expect("wrong-header runtime cache");
        let mut wrong_header_runtime =
            graphoxide_index_runtime::cache::RuntimeCache::open(wrong_header_temp.path())
                .expect("open wrong-header runtime cache");
        wrong_header_runtime
            .put(evidence.key, &wrong_header)
            .expect("persist wrong-header envelope");
        let hit = wrong_header_runtime
            .get(evidence.key)
            .expect("wrong-header runtime hit");
        let admission = super::RuntimeOutputAdmission::new(retained.saturating_mul(2));
        assert!(matches!(
            super::decode_admitted_runtime_cache_hit(
                hit,
                &evidence,
                &admission,
                &graphoxide_index_runtime::RuntimeCancellation::new(),
            ),
            Err(super::RuntimeCacheHitUseError::Rejected(
                super::cache::RuntimeAstCacheRejection::Preamble
            ))
        ));
        assert_eq!(
            admission.retained_bytes(),
            0,
            "a wrong header must be rejected before output admission"
        );
    }

    #[test]
    fn runtime_cache_nested_singletons_are_admitted_before_full_decode() {
        let relative = "nested.py";
        let source = b"def nested(): pass\n";
        let evidence = super::cache::runtime_ast_cache_evidence(
            relative,
            source,
            super::cache::RuntimeAstCacheOptions::isolated(1024 * 1024),
        )
        .expect("runtime cache evidence");
        let mut nested = serde_json::Value::String("leaf".into());
        for depth in (0..96).rev() {
            let mut singleton = serde_json::Map::new();
            singleton.insert(format!("k{depth}"), nested);
            nested = serde_json::Value::Object(singleton);
        }
        let extraction = graphoxide_core::Extraction {
            nodes: vec![graphoxide_core::Node {
                id: "nested".into(),
                label: "nested()".into(),
                file_type: "code".into(),
                source_file: relative.into(),
                source_location: Some("L1".into()),
                community: None,
                extra: std::collections::BTreeMap::from([("nested".into(), nested)]),
            }],
            ..graphoxide_core::Extraction::default()
        };
        let mut payload = super::cache::encode_runtime_ast_cache_payload(&evidence, &extraction)
            .expect("encode nested envelope");
        let retained = super::extraction_retained_bytes(&extraction).expect("retained estimate");
        let decode_charge = payload
            .len()
            .checked_mul(super::RUNTIME_CACHE_DECODE_EXPANSION_MULTIPLIER)
            .expect("bounded decode charge");
        assert!(
            decode_charge >= retained,
            "nested singleton charge {decode_charge} must cover {retained} retained bytes"
        );

        let needle = b"\"source_file\":\"nested.py\"";
        let replacement = b"\"source_file\":\"forged.py\"";
        let offset = payload
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("serialized source provenance");
        payload[offset..offset + needle.len()].copy_from_slice(replacement);

        let temp = tempfile::tempdir().expect("temporary nested runtime cache");
        let mut runtime = graphoxide_index_runtime::cache::RuntimeCache::open(temp.path())
            .expect("open nested runtime cache");
        runtime
            .put(evidence.key, &payload)
            .expect("persist nested envelope");
        let hit = runtime.get(evidence.key).expect("nested runtime hit");
        let admission = super::RuntimeOutputAdmission::new(decode_charge.saturating_sub(1));
        assert!(matches!(
            super::decode_admitted_runtime_cache_hit(
                hit,
                &evidence,
                &admission,
                &graphoxide_index_runtime::RuntimeCancellation::new(),
            ),
            Err(super::RuntimeCacheHitUseError::ExceedsOutputAdmission)
        ));
        assert_eq!(admission.retained_bytes(), 0);

        let hit = runtime
            .get(evidence.key)
            .expect("second nested runtime hit");
        let admission = super::RuntimeOutputAdmission::new(decode_charge);
        assert!(matches!(
            super::decode_admitted_runtime_cache_hit(
                hit,
                &evidence,
                &admission,
                &graphoxide_index_runtime::RuntimeCancellation::new(),
            ),
            Err(super::RuntimeCacheHitUseError::Rejected(
                super::cache::RuntimeAstCacheRejection::Provenance
            ))
        ));
        assert_eq!(admission.retained_bytes(), 0);
    }

    #[test]
    fn runtime_output_admission_coordinates_temporary_and_final_worker_charges() {
        let admission = std::sync::Arc::new(super::RuntimeOutputAdmission::new(100));
        let temporary = admission
            .try_reserve_temporary_with_cancellation(80, None)
            .expect("temporary decode reservation");
        let worker_admission = std::sync::Arc::clone(&admission);
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender
                .send(worker_admission.try_reserve(30))
                .expect("return final reservation result");
        });
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_millis(25)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(temporary.commit(20));
        assert!(receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("final reservation wakes after temporary commit"));
        worker.join().expect("reservation worker");
        assert_eq!(admission.retained_bytes(), 50);

        let cancellation = graphoxide_index_runtime::RuntimeCancellation::new();
        let blocked = std::sync::Arc::new(super::RuntimeOutputAdmission::new(100));
        let held = blocked
            .try_reserve_temporary_with_cancellation(80, None)
            .expect("held temporary reservation");
        let blocked_worker = std::sync::Arc::clone(&blocked);
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let admitted = blocked_worker
                .try_reserve_temporary_with_cancellation(30, Some(&worker_cancellation))
                .is_some();
            sender.send(admitted).expect("return cancelled admission");
        });
        cancellation.cancel();
        assert!(!receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("cancelled waiter wakes"));
        drop(held);
        worker.join().expect("cancelled reservation worker");
    }

    #[test]
    fn runtime_cache_mixed_warm_and_cold_outcomes_are_worker_deterministic() {
        let mut expected = None;
        for workers in [1, 2, 8] {
            let fixture = Fixture::new();
            fixture.write("a.py", "def alpha():\n    return 1\n");
            fixture.write("b.py", "def beta():\n    return 2\n");
            fixture.write("c.py", "def gamma():\n    return 3\n");
            let output = fixture.root.join("graphoxide-out");
            let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
                memory_budget_bytes: 32 * 1024 * 1024,
                io_workers: workers,
                compute_workers: workers,
                io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
                read_batch_bytes: 4 * 1024,
            };
            let cold = super::extract_project_with_runtime_scan_options_deferred_manifest(
                &fixture.root,
                false,
                &output,
                false,
                &super::detect::DetectOptions::default(),
                runtime,
            )
            .expect("cold worker-determinism scan");
            assert_eq!(cold.runtime_cache.stores, 3);
            commit_runtime_baseline(cold, &fixture.root, &output);

            fixture.write("c.py", "def gamma():\n    return 4\n");
            let mixed = super::extract_project_with_runtime_scan_options_deferred_manifest(
                &fixture.root,
                false,
                &output,
                false,
                &super::detect::DetectOptions::default(),
                runtime,
            )
            .expect("mixed warm/cold worker-determinism scan");
            let observation = (
                mixed.runtime_cache.metadata_hits,
                mixed.runtime_cache.runtime_hits,
                mixed.runtime_cache.misses,
                mixed.runtime_cache.parses_avoided,
                mixed.runtime_cache.stores,
                mixed.changed_sources,
                serde_json::to_value(&mixed.extractions).expect("mixed extraction JSON"),
            );
            assert_eq!(observation.0, 2, "metadata hits with {workers} workers");
            assert_eq!(observation.1, 0, "runtime hits with {workers} workers");
            assert_eq!(observation.2, 1, "cache misses with {workers} workers");
            assert_eq!(observation.3, 2, "parses avoided with {workers} workers");
            assert_eq!(observation.4, 1, "stores with {workers} workers");
            assert_eq!(observation.5, 1, "changed sources with {workers} workers");
            if let Some(expected) = &expected {
                assert_eq!(
                    &observation, expected,
                    "cache telemetry or facts changed with {workers} workers"
                );
            } else {
                expected = Some(observation);
            }
        }
    }

    #[test]
    fn isolated_runtime_facts_are_byte_identical_across_worker_counts() {
        let fixture = Fixture::new();
        fixture.write("z.rs", "pub fn zed() {}\n");
        fixture.write("a.rs", "pub fn alpha() { zed(); }\n");
        fixture.write("config.json", r#"{"services":{"api":{"port":8080}}}"#);
        fixture.write("design.dot", "digraph { api -> database; }\n");

        let mut expected = None;
        for workers in [1, 2, 3, 4, 8] {
            let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
                memory_budget_bytes: 4 * 1024 * 1024,
                io_workers: workers,
                compute_workers: workers,
                io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
                read_batch_bytes: 4 * 1024,
            };
            let result = super::extract_project_with_runtime(&fixture.root, runtime)
                .expect("isolated runtime extraction");
            assert!(
                result.read_failures.is_empty(),
                "worker count {workers} produced read failures: {:?}",
                result.read_failures
            );
            let bytes = serde_json::to_vec(&result.extractions)
                .expect("serialize deterministic extraction facts");
            if let Some(expected) = &expected {
                assert_eq!(
                    &bytes, expected,
                    "worker count {workers} changed deterministic extraction facts"
                );
            } else {
                expected = Some(bytes);
            }
        }
    }

    #[test]
    fn isolated_incremental_reextracts_legacy_dot_manifest_once() {
        let fixture = Fixture::new();
        fixture.write(
            "design.dot",
            "digraph architecture { api -> database [label=queries]; }\n",
        );
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial DOT scan");
        assert_eq!(initial.changed_sources, 1);
        let expected_extraction =
            serde_json::to_vec(&initial.extractions).expect("serialize initial DOT facts");
        commit_runtime_baseline(initial, &fixture.root, &output);

        let manifest_path = output.join("manifest.json");
        let mut legacy_manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read current manifest"))
                .expect("decode current manifest");
        let legacy_entry = legacy_manifest["design.dot"]
            .as_object_mut()
            .expect("DOT manifest entry");
        legacy_entry.remove("ast_version");
        legacy_entry.insert(
            "semantic_hash".into(),
            serde_json::Value::String("stale-semantic".into()),
        );
        graphoxide_core::write_json_atomic(&manifest_path, &legacy_manifest, true)
            .expect("write legacy manifest fixture");

        let rebuilt = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("schema-invalidated DOT scan");
        assert_eq!(rebuilt.changed_sources, 1);
        assert_eq!(rebuilt.unchanged_sources, 0);
        assert_eq!(
            serde_json::to_vec(&rebuilt.extractions).expect("serialize rebuilt DOT facts"),
            expected_extraction,
            "schema invalidation must reproduce deterministic DOT facts"
        );
        let next_entry = rebuilt
            .pending_manifest
            .entries
            .get("design.dot")
            .expect("rebuilt DOT manifest entry");
        assert_eq!(next_entry.ast_version, super::cache::AST_CACHE_VERSION);
        assert!(
            next_entry.semantic_hash.is_empty(),
            "semantic hashes from an older AST schema must not carry forward"
        );
        commit_runtime_baseline(rebuilt, &fixture.root, &output);

        let graph_before = fs::read(output.join("graph.json")).expect("read rebuilt graph");
        let manifest_before = fs::read(&manifest_path).expect("read rebuilt manifest");
        let unchanged = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("current-schema unchanged DOT scan");
        assert_eq!(unchanged.changed_sources, 0);
        assert_eq!(unchanged.unchanged_sources, 1);
        assert!(unchanged.extractions.is_empty());
        unchanged
            .pending_manifest
            .commit()
            .expect("commit unchanged current manifest");
        assert_eq!(
            fs::read(&manifest_path).expect("reread current manifest"),
            manifest_before,
            "a current-version rerun must preserve manifest bytes"
        );
        assert_eq!(
            fs::read(output.join("graph.json")).expect("reread rebuilt graph"),
            graph_before,
            "an unchanged rerun must leave graph bytes untouched"
        );
    }

    #[test]
    fn isolated_runtime_rejects_large_semantic_parsers_before_they_allocate() {
        let fixture = Fixture::new();
        fixture.write("large.dot", &"node_a -> node_b;\n".repeat(4_096));
        fixture.write(
            "large.proto",
            &"message Item { string value = 1; }\n".repeat(2_048),
        );
        fixture.write(
            "large.gltf",
            &format!("{{\"nodes\":[{}]}}", "{},".repeat(24_000)),
        );
        let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes: 4 * 1024 * 1024,
            io_workers: 3,
            compute_workers: 3,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        };
        let result = super::extract_project_with_runtime(&fixture.root, runtime)
            .expect("arena-rejected inputs retain stable inventory");
        assert!(result.read_failures.is_empty());
        assert_eq!(result.extractions.len(), 3);
        for extraction in result.extractions {
            assert_eq!(extraction.nodes.len(), 1);
            assert!(extraction.edges.is_empty());
            let root = extraction.nodes.first().expect("inventory root");
            assert_eq!(
                root.extra
                    .get("parse_status")
                    .and_then(serde_json::Value::as_str),
                Some("rejected")
            );
            assert_eq!(
                root.extra
                    .get("diagnostic")
                    .and_then(serde_json::Value::as_str),
                Some("parser_arena_budget")
            );
        }
    }

    #[test]
    fn cancelled_runtime_scan_preserves_the_committed_manifest() {
        let fixture = Fixture::new();
        fixture.write("main.rs", "fn main() {}\n");
        let output = fixture.root.join("graphoxide-out");
        let manifest = fixture.write("graphoxide-out/manifest.json", "committed-manifest\n");
        let cancellation = graphoxide_index_runtime::RuntimeCancellation::new();
        cancellation.cancel();

        let error =
            super::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation(
                &fixture.root,
                true,
                &output,
                false,
                &super::detect::DetectOptions::default(),
                runtime_config(4 * 1024 * 1024),
                cancellation,
            )
            .expect_err("pre-cancelled isolated scan");

        assert!(error.to_string().contains("isolated extraction cancelled"));
        assert_eq!(
            fs::read_to_string(manifest).expect("read committed manifest"),
            "committed-manifest\n"
        );
    }

    #[test]
    fn isolated_runtime_scan_uses_the_preloaded_project_snapshot_for_resolution() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { utility } from './utility'; export const run = utility;\n",
        );
        fixture.write("utility.ts", "export const utility = 1;\n");
        let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes: 4 * 1024 * 1024,
            io_workers: 2,
            compute_workers: 2,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        };
        let result = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &fixture.root.join("graphoxide-out"),
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("isolated runtime scan");
        assert!(
            result.resolution_snapshot_diagnostics.is_empty(),
            "fixture has no unavailable resolver metadata: {:?}",
            result.resolution_snapshot_diagnostics
        );
        let main = result
            .extractions
            .iter()
            .find(|extraction| {
                extraction
                    .nodes
                    .first()
                    .is_some_and(|node| node.source_file == "main.ts")
            })
            .expect("main extraction");
        assert!(main.edges.iter().any(|edge| {
            edge.relation == "imports_from"
                && edge.true_source() == make_id(&["main"])
                && edge.true_target() == make_id(&["utility"])
        }));
    }

    #[test]
    fn isolated_incremental_js_context_matches_full_named_barrel_resolution() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { utility } from './barrel'; export const run = utility;\n",
        );
        fixture.write("barrel.ts", "export { utility } from './utility';\n");
        fixture.write("utility.ts", "export const utility = 1;\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial isolated scan");
        let baseline = commit_runtime_baseline(initial, &fixture.root, &output);
        let mut legacy_resolver_baseline = baseline.clone();
        for node in &mut legacy_resolver_baseline.nodes {
            if matches!(node.source_file.as_str(), "barrel.ts" | "utility.ts") {
                node.source_file = fixture
                    .root
                    .join(&node.source_file)
                    .to_string_lossy()
                    .into_owned();
            }
        }
        graphoxide_core::write_graph_atomic(
            output.join("graph.json"),
            &legacy_resolver_baseline,
            true,
        )
        .expect("write legacy absolute-path resolver baseline");

        fixture.write(
            "main.ts",
            "import { utility } from './barrel'; export const run = utility; export const changed = true;\n",
        );
        let incremental = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("incremental isolated scan");
        assert_eq!(incremental.changed_sources, 1);
        assert_eq!(incremental.unchanged_sources, 2);
        let fresh = graphoxide_graph::dedupe_raw_extractions(&incremental.extractions);
        let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
            fresh,
            &baseline,
            &incremental.rebuilt_sources,
            &[],
            &[],
            Some(&fixture.root),
            64 * 1024 * 1024,
        )
        .expect("merge incremental JS delta");
        let incremental_graph = graphoxide_graph::build_graph_with_options_and_root(
            &[merged],
            &fixture.root,
            graphoxide_graph::BuildOptions::default(),
        )
        .expect("build incremental graph");

        let full = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("forced full comparison scan");
        let full_graph = graphoxide_graph::build_graph_with_options_and_root(
            &full.extractions,
            &fixture.root,
            graphoxide_graph::BuildOptions::default(),
        )
        .expect("build full comparison graph");

        let incremental_edges = js_edge_topology(&incremental_graph, "main.ts");
        let full_edges = js_edge_topology(&full_graph, "main.ts");
        assert_eq!(incremental_edges, full_edges);
        assert!(incremental_edges
            .iter()
            .any(|(_, _, relation, _)| relation == "imports"));
    }

    #[test]
    fn changed_failed_owner_is_excluded_from_resolver_baseline_context() {
        let fixture = Fixture::new();
        let changed_path = fixture.write("changed.ts", "export const stale = 1;\n");
        let unchanged_path = fixture.write("unchanged.ts", "export const stable = 1;\n");
        let graph = graphoxide_graph::build_graph_with_options_and_root(
            &[
                extract(&changed_path, "changed.ts"),
                extract(&unchanged_path, "unchanged.ts"),
            ],
            &fixture.root,
            graphoxide_graph::BuildOptions::default(),
        )
        .expect("build resolver eligibility graph");
        let graph_path = fixture.root.join("baseline.json");
        graphoxide_core::write_graph_atomic(&graph_path, &graph, true)
            .expect("write resolver eligibility graph");
        let contexts = [
            (
                "changed.ts".to_owned(),
                super::RuntimeFileContext {
                    path: changed_path.clone(),
                    physical_path: changed_path,
                    indexed: true,
                },
            ),
            (
                "unchanged.ts".to_owned(),
                super::RuntimeFileContext {
                    path: unchanged_path.clone(),
                    physical_path: unchanged_path,
                    indexed: true,
                },
            ),
        ]
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
        let changed = ["changed.ts".to_owned()]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let eligible = super::eligible_resolver_owner_keys(&contexts, &changed);
        assert!(!eligible.contains("changed.ts"));
        assert!(eligible.contains("unchanged.ts"));

        let resolved_root = fs::canonicalize(&fixture.root).expect("canonical fixture root");
        let baseline = super::load_resolver_baseline_context(
            &graph_path,
            u64::MAX,
            &eligible,
            &resolved_root,
            &fixture.root,
        )
        .expect("load only byte-identical resolver owners");
        let sources = baseline
            .extractions
            .iter()
            .flat_map(|extraction| extraction.nodes.iter())
            .map(|node| node.source_file.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(sources, std::collections::BTreeSet::from(["unchanged.ts"]));
    }

    #[test]
    fn isolated_incremental_sfc_context_matches_full_named_import_resolution() {
        let fixture = Fixture::new();
        fixture.write(
            "component.vue",
            "<template><main /></template>\n<script lang=\"ts\">import { utility } from './utility'; export const run = utility;</script>\n",
        );
        fixture.write("utility.ts", "export const utility = 1;\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial SFC scan");
        let baseline = commit_runtime_baseline(initial, &fixture.root, &output);

        fixture.write(
            "component.vue",
            "<template><main class=\"changed\" /></template>\n<script lang=\"ts\">import { utility } from './utility'; export const run = utility;</script>\n",
        );
        let incremental = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("incremental SFC scan");
        let fresh = graphoxide_graph::dedupe_raw_extractions(&incremental.extractions);
        let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
            fresh,
            &baseline,
            &incremental.rebuilt_sources,
            &[],
            &[],
            Some(&fixture.root),
            64 * 1024 * 1024,
        )
        .expect("merge SFC delta");
        let incremental_graph = graphoxide_graph::build_graph_with_options_and_root(
            &[merged],
            &fixture.root,
            graphoxide_graph::BuildOptions::default(),
        )
        .expect("build incremental SFC graph");
        let full = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("full SFC comparison scan");
        let full_graph = graphoxide_graph::build_graph_with_options_and_root(
            &full.extractions,
            &fixture.root,
            graphoxide_graph::BuildOptions::default(),
        )
        .expect("build full SFC graph");

        let incremental_edges = js_edge_topology(&incremental_graph, "component.vue");
        assert_eq!(
            incremental_edges,
            js_edge_topology(&full_graph, "component.vue")
        );
        assert!(incremental_edges
            .iter()
            .any(|(_, _, relation, _)| relation == "imports"));
    }

    #[test]
    fn isolated_incremental_deleted_js_target_is_not_loaded_as_baseline_context() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { utility } from './utility'; export const run = utility;\n",
        );
        let utility = fixture.write("utility.ts", "export const utility = 1;\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial deleted-target baseline");
        commit_runtime_baseline(initial, &fixture.root, &output);

        fs::remove_file(utility).expect("delete target fixture");
        fixture.write(
            "main.ts",
            "import { utility } from './utility'; export const changed = utility;\n",
        );
        let incremental = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("scan with deleted JS target");

        assert_eq!(incremental.changed_sources, 1);
        assert_eq!(incremental.deleted_sources, 1);
        let main = incremental
            .extractions
            .iter()
            .find(|extraction| {
                extraction
                    .nodes
                    .iter()
                    .any(|node| node.source_file == "main.ts")
            })
            .expect("changed main extraction");
        assert!(main.edges.iter().all(|edge| {
            edge.extra
                .get("target_file")
                .and_then(serde_json::Value::as_str)
                != Some("utility.ts")
        }));
    }

    #[test]
    fn isolated_manifest_without_graph_forces_every_indexed_source_fresh() {
        let fixture = Fixture::new();
        fixture.write("main.ts", "export const main = 1;\n");
        fixture.write("utility.ts", "export const utility = 1;\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial manifest-only scan");
        let indexed = initial.progress.succeeded;
        initial
            .pending_manifest
            .commit()
            .expect("commit manifest without a graph");
        assert!(!output.join("graph.json").exists());

        let rebuilt = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("manifest without graph triggers full rebuild");
        assert_eq!(rebuilt.changed_sources, indexed);
        assert_eq!(rebuilt.unchanged_sources, 0);
        assert_eq!(rebuilt.extractions.len(), indexed);
    }

    #[test]
    fn isolated_no_change_and_deletion_only_scans_skip_baseline_graph_reads() {
        let fixture = Fixture::new();
        fixture.write("main.ts", "export const main = 1;\n");
        let deleted = fixture.write("deleted.ts", "export const deleted = 1;\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial skip-read baseline");
        commit_runtime_baseline(initial, &fixture.root, &output);
        fs::write(output.join("graph.json"), b"not valid graph JSON")
            .expect("poison committed graph read");

        let unchanged = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("no-change scan skips graph read");
        assert_eq!(unchanged.changed_sources, 0);
        assert!(unchanged.extractions.is_empty());

        fs::remove_file(deleted).expect("delete unchanged fixture source");
        let deletion_only = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("deletion-only scan skips graph read");
        assert_eq!(deletion_only.changed_sources, 0);
        assert_eq!(deletion_only.deleted_sources, 1);
        assert!(deletion_only.extractions.is_empty());
    }

    #[test]
    fn isolated_low_baseline_budget_fails_without_mutating_committed_artifacts() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { utility } from './utility'; export const run = utility;\n",
        );
        fixture.write("utility.ts", "export const utility = 1;\n");
        fixture.write("notes.md", "baseline padding owner\n");
        let output = fixture.root.join("graphoxide-out");
        let initial_runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            initial_runtime,
        )
        .expect("initial large baseline scan");
        let mut padded_graph = commit_runtime_baseline(initial, &fixture.root, &output);
        for index in 0..1_000 {
            padded_graph.nodes.push(graphoxide_core::Node {
                id: format!("budget_padding_{index}"),
                label: "x".repeat(512),
                file_type: "text".into(),
                source_file: "notes.md".into(),
                source_location: None,
                community: None,
                extra: std::collections::BTreeMap::new(),
            });
        }
        let graph_path = output.join("graph.json");
        let manifest_path = output.join("manifest.json");
        graphoxide_core::write_graph_atomic(&graph_path, &padded_graph, true)
            .expect("write padded committed graph");
        let graph_before = fs::read(&graph_path).expect("read committed graph");
        let manifest_before = fs::read(&manifest_path).expect("read committed manifest");
        let low_runtime = runtime_config(8 * 1024 * 1024);
        assert!(
            graph_before.len()
                > low_runtime.memory_budget().cache_and_runs_bytes
                    / super::RESOLUTION_BASELINE_WORKING_SET_MULTIPLIER
        );

        fixture.write(
            "main.ts",
            "import { utility } from './utility'; export const changed = utility;\n",
        );
        let error = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            low_runtime,
        )
        .expect_err("oversized baseline must fail closed");
        assert!(error.to_string().contains("graph"));
        assert_eq!(fs::read(&graph_path).expect("reread graph"), graph_before);
        assert_eq!(
            fs::read(&manifest_path).expect("reread manifest"),
            manifest_before
        );
    }

    #[test]
    fn isolated_runtime_code_only_preloads_tsconfig_metadata_for_resolution() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { shared } from '@shared'; export const run = shared;\n",
        );
        fixture.write("shared.ts", "export const shared = 1;\n");
        fixture.write(
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@shared":["shared"]}}}"#,
        );
        let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            // Unicode-safe derived-ID normalization now has an explicit peak
            // charge; keep this a small-runtime test while admitting that
            // proven resolver scratch alongside tsconfig metadata.
            memory_budget_bytes: 5 * 1024 * 1024,
            io_workers: 2,
            compute_workers: 2,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        };
        let result = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &fixture.root.join("graphoxide-out"),
            true,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("isolated code-only runtime scan");
        let main = result
            .extractions
            .iter()
            .find(|extraction| {
                extraction
                    .nodes
                    .first()
                    .is_some_and(|node| node.source_file == "main.ts")
            })
            .expect("main extraction");
        assert!(main.edges.iter().any(|edge| {
            edge.relation == "imports_from"
                && edge.true_source() == make_id(&["main"])
                && edge.true_target() == make_id(&["shared"])
        }));
    }

    #[test]
    fn isolated_runtime_skips_one_bad_source_with_a_warning() {
        let fixture = Fixture::new();
        fixture.write("app.py", "def app():\n    return 1\n");
        fixture.write("tsconfig.json", "{not valid json");
        let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes: 4 * 1024 * 1024,
            io_workers: 2,
            compute_workers: 2,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        };

        let result = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &fixture.root.join("graphoxide-out"),
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("one malformed source must not abort the isolated scan");

        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert!(result.warnings[0].contains("tsconfig.json"));
        assert!(!result.progress.is_complete());
        assert_eq!(result.progress.succeeded, result.progress.total - 1);
        assert_eq!(result.changed_sources, 1);
        assert!(result
            .extractions
            .iter()
            .any(|extraction| extraction.nodes.iter().any(|node| node.label == "app()")));
        assert!(!result
            .pending_manifest
            .entries
            .contains_key("tsconfig.json"));
    }

    #[test]
    fn isolated_runtime_rejects_a_corpus_when_every_source_fails() {
        let fixture = Fixture::new();
        fixture.write("tsconfig.json", "{not valid json");
        let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes: 4 * 1024 * 1024,
            io_workers: 1,
            compute_workers: 1,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        };

        let error = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &fixture.root.join("graphoxide-out"),
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect_err("an entirely failed isolated scan must remain an error");
        assert!(
            error.to_string().contains("tsconfig.json"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn isolated_runtime_cache_cold_warm_and_force_paths_are_truthful() {
        let fixture = Fixture::new();
        fixture.write("lib.rs", "pub fn indexed() {}\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let result = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("isolated runtime scan");
        assert!(result.runtime_cache_diagnostics.is_empty());
        assert_eq!(result.changed_sources, 1);
        assert!(result.runtime_cache.enabled);
        assert_eq!(result.runtime_cache.bypasses, 1);
        assert_eq!(result.runtime_cache.stores, 1);
        assert!(
            output.join("cache/runtime-v1").exists(),
            "successful extraction must be available to the runtime read-through path"
        );
        let retained_output_bytes = result.retained_output_bytes;
        commit_runtime_baseline(result, &fixture.root, &output);

        let warm = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("warm isolated runtime scan");
        assert_eq!(warm.changed_sources, 0);
        assert_eq!(warm.unchanged_sources, 1);
        assert!(warm.extractions.is_empty());
        assert_eq!(warm.runtime_cache.metadata_hits, 1);
        assert_eq!(warm.runtime_cache.payload_reads_avoided, 1);
        assert_eq!(warm.runtime_cache.parses_avoided, 1);

        let forced = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("forced isolated runtime rescan");
        assert_eq!(
            forced.retained_output_bytes, retained_output_bytes,
            "force bypass must not perturb deterministic output admission"
        );
        assert_eq!(forced.runtime_cache.bypasses, 1);
        assert_eq!(forced.runtime_cache.metadata_hits, 0);
        assert_eq!(forced.runtime_cache.runtime_hits, 0);
        assert_eq!(forced.runtime_cache.stores, 1);
        assert_eq!(forced.runtime_cache.already_present, 0);
    }

    #[test]
    fn isolated_runtime_force_replaces_a_valid_outer_wrong_inner_artifact() {
        let fixture = Fixture::new();
        let relative = "main.py";
        let source = b"def answer():\n    return 42\n";
        let source_path = fixture.root.join(relative);
        fs::write(&source_path, source).expect("write Python source");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let (parser_allowance, _) = super::isolated_parser_layout(runtime, true);
        let evidence = super::cache::runtime_ast_cache_evidence(
            relative,
            source,
            super::cache::RuntimeAstCacheOptions::isolated(
                u64::try_from(parser_allowance).expect("parser allowance fits u64"),
            ),
        )
        .expect("runtime cache evidence");
        let mut forged = super::engine::extract_as_bytes_with_parser_allowance(
            &source_path,
            relative,
            source,
            parser_allowance,
        )
        .expect("fresh Python extraction");
        let forged_function = forged
            .nodes
            .iter_mut()
            .find(|node| node.label == "answer()")
            .expect("answer function node");
        forged_function.label = "forged()".into();
        let forged_payload = super::cache::encode_runtime_ast_cache_payload(&evidence, &forged)
            .expect("encode forged inner envelope");
        let mut artifact_store = graphoxide_index_runtime::cache::RuntimeCache::open(&output)
            .expect("open runtime artifact store");
        artifact_store
            .put(evidence.key, &forged_payload)
            .expect("seed valid outer frame with forged facts");
        drop(artifact_store);

        let forced = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("force scan replaces forged runtime artifact");
        assert_eq!(forced.runtime_cache.bypasses, 1);
        assert_eq!(forced.runtime_cache.stores, 1);
        assert_eq!(forced.runtime_cache.already_present, 0);
        commit_runtime_baseline(forced, &fixture.root, &output);

        fs::remove_file(output.join("graph.json")).expect("remove graph to require fact replay");
        let rebuilt = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("warm replay after force replacement");
        assert_eq!(rebuilt.runtime_cache.metadata_hits, 1);
        assert_eq!(rebuilt.runtime_cache.parses_avoided, 1);
        assert!(rebuilt
            .extractions
            .iter()
            .any(|extraction| { extraction.nodes.iter().any(|node| node.label == "answer()") }));
        assert!(rebuilt
            .extractions
            .iter()
            .all(|extraction| { extraction.nodes.iter().all(|node| node.label != "forged()") }));
    }

    #[test]
    fn isolated_runtime_cache_keeps_javascript_family_on_contextual_bypass() {
        let fixture = Fixture::new();
        fixture.write("main.ts", "export const answer: number = 42;\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let cold = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("cold TypeScript scan");
        assert_eq!(cold.runtime_cache.bypasses, 1);
        assert_eq!(cold.runtime_cache.stores, 0);
        assert!(cold
            .pending_manifest
            .entries
            .get("main.ts")
            .expect("TypeScript manifest row")
            .runtime_cache
            .is_none());
        commit_runtime_baseline(cold, &fixture.root, &output);

        let warm = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("warm TypeScript scan");
        assert_eq!(warm.changed_sources, 0);
        assert!(warm.extractions.is_empty());
        assert_eq!(warm.runtime_cache.metadata_hits, 0);
        assert_eq!(warm.runtime_cache.runtime_hits, 0);
        assert_eq!(warm.runtime_cache.legacy_hits, 0);
        assert_eq!(warm.runtime_cache.stores, 0);
    }

    #[test]
    fn isolated_runtime_cache_does_not_replay_path_aware_legacy_artifacts() {
        let fixture = Fixture::new();
        let source = b"def answer():\n    return 42\n";
        let source_path = fixture.root.join("main.py");
        fs::write(&source_path, source).expect("write Python source");
        let output = fixture.root.join("graphoxide-out");
        let path_aware = super::engine::extract_as(&source_path, "main.py")
            .expect("legacy path-aware extraction");
        super::cache::ast_cache_put_to_output(&output, "main.py", source, &path_aware)
            .expect("legacy AST artifact");

        let result = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime_config(32 * 1024 * 1024),
        )
        .expect("isolated scan beside a legacy artifact");
        assert_eq!(result.runtime_cache.legacy_hits, 0);
        assert_eq!(result.runtime_cache.runtime_hits, 0);
        assert_eq!(result.runtime_cache.misses, 1);
        assert_eq!(result.runtime_cache.parses_avoided, 0);
        assert_eq!(result.runtime_cache.stores, 1);
    }

    #[test]
    fn isolated_runtime_cache_repairs_a_missing_artifact_with_graph_present() {
        let fixture = Fixture::new();
        fixture.write("main.py", "def answer():\n    return 42\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let cold = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("cold Python scan");
        assert_eq!(cold.runtime_cache.stores, 1);
        commit_runtime_baseline(cold, &fixture.root, &output);
        fs::remove_dir_all(output.join("cache/runtime-v1")).expect("remove runtime artifact store");

        let repaired = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("repair missing runtime artifact");
        assert_eq!(repaired.changed_sources, 1);
        assert_eq!(repaired.runtime_cache.misses, 1);
        assert_eq!(repaired.runtime_cache.stores, 1);
        assert_eq!(repaired.runtime_cache.parses_avoided, 0);
        commit_runtime_baseline(repaired, &fixture.root, &output);

        let warm = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("warm scan after repair");
        assert_eq!(warm.runtime_cache.metadata_hits, 1);
        assert_eq!(warm.runtime_cache.parses_avoided, 1);
    }

    #[test]
    fn isolated_runtime_rejects_high_fanout_before_all_outputs_accumulate() {
        let fixture = Fixture::new();
        for file in 0..8 {
            let mut source = String::new();
            for function in 0..64 {
                source.push_str(&format!("pub fn f_{file}_{function}() {{}}\n"));
            }
            fixture.write(&format!("fanout_{file}.rs"), &source);
        }
        let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
            memory_budget_bytes: 128 * 1024,
            io_workers: 2,
            compute_workers: 2,
            io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
            read_batch_bytes: 4 * 1024,
        };
        let error = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &fixture.root.join("graphoxide-out"),
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect_err("high-fanout facts must exceed the retained-output partition");
        assert!(
            error
                .to_string()
                .contains("isolated retained extraction output exceeds"),
            "unexpected admission error: {error:#}"
        );
    }

    fn definition_labels(extraction: &graphoxide_core::Extraction) -> Vec<&str> {
        let mut labels: Vec<_> = extraction
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.extra.get("type").and_then(|value| value.as_str()),
                    Some("class" | "function")
                )
            })
            .map(|node| node.label.as_str())
            .collect();
        labels.sort_unstable();
        labels
    }

    fn assert_definition(extraction: &graphoxide_core::Extraction, id: &str, kind: &str) {
        let node = extraction
            .nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("missing {kind} node {id}"));
        assert_eq!(
            node.extra.get("type").and_then(|value| value.as_str()),
            Some(kind),
            "node {id} should be a {kind}"
        );
    }

    fn assert_export_status(extraction: &graphoxide_core::Extraction, id: &str, exported: bool) {
        let node = extraction
            .nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("missing node {id}"));
        assert_eq!(
            node.extra
                .get("exported")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            exported,
            "unexpected export status for node {id}"
        );
    }

    fn assert_single_edge(
        extraction: &graphoxide_core::Extraction,
        source: &str,
        target: &str,
        relation: &str,
    ) {
        let count = extraction
            .edges
            .iter()
            .filter(|edge| {
                edge.relation == relation
                    && edge.true_source() == source
                    && edge.true_target() == target
            })
            .count();
        assert_eq!(
            count, 1,
            "expected one {relation} edge from {source} to {target}"
        );
    }

    fn resolved_call_targets<'a>(
        extraction: &'a graphoxide_core::Extraction,
        source: &str,
    ) -> Vec<&'a str> {
        extraction
            .edges
            .iter()
            .filter(|edge| {
                edge.relation == "calls"
                    && edge.true_source() == source
                    && !edge
                        .extra
                        .get("unresolved_call")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false)
            })
            .map(|edge| edge.true_target())
            .collect()
    }

    #[test]
    fn detected_markdown_suffixes_extract_links() {
        let fixture = Fixture::new();
        for (filename, target) in [
            ("guide.md", "reference.md"),
            ("handbook.markdown", "reference.markdown"),
        ] {
            let markdown = fixture.write(filename, &format!("[Reference]({target})\n"));
            assert!(super::detect::is_supported_path(&markdown));

            let extraction = extract(&markdown, filename);
            let file_id = make_id(&[Path::new(filename)
                .with_extension("")
                .to_string_lossy()
                .as_ref()]);
            let target_id = make_id(&[Path::new(target)
                .with_extension("")
                .to_string_lossy()
                .as_ref()]);
            assert!(
                extraction.nodes.iter().all(|node| node.id != target_id),
                "a raw local link must not fabricate a target node for {target}"
            );
            assert_single_edge(&extraction, &file_id, &target_id, "references");
        }
    }

    #[test]
    fn javascript_extracts_exported_and_variable_bound_declarations() {
        let fixture = Fixture::new();
        let javascript = fixture.write(
            "demo.js",
            r#"
function bareFn() {}
async function bareAsyncFn() {}
const bareArrow = () => {};
const bareAsyncArrow = async () => {};
const bareFnExpr = function () {};
class BareClass { bareMethod() {} }

export function expFn() {}
export async function expAsyncFn() {}
export const expArrow = () => {};
export class ExpClass { expMethod() {} }
export default function defFn() {}
"#,
        );
        let extraction = extract(&javascript, "demo.js");

        let definitions = definition_labels(&extraction);
        assert_eq!(definitions.len(), 13);
        for label in [
            "BareClass",
            "ExpClass",
            "bareArrow()",
            "bareAsyncArrow()",
            "bareAsyncFn()",
            "bareFn()",
            "bareFnExpr()",
            "defFn()",
            "expArrow()",
            "expAsyncFn()",
            "expFn()",
        ] {
            assert!(definitions.contains(&label), "missing definition {label}");
        }

        let file = make_id(&["demo"]);
        for (name, kind) in [
            ("bareFn", "function"),
            ("bareAsyncFn", "function"),
            ("bareArrow", "function"),
            ("bareAsyncArrow", "function"),
            ("bareFnExpr", "function"),
            ("BareClass", "class"),
            ("expFn", "function"),
            ("expAsyncFn", "function"),
            ("expArrow", "function"),
            ("ExpClass", "class"),
            ("defFn", "function"),
        ] {
            let id = make_id(&["demo", name]);
            assert_definition(&extraction, &id, kind);
            assert_single_edge(&extraction, &file, &id, "contains");
        }

        for id in [
            make_id(&["demo", "expFn"]),
            make_id(&["demo", "expAsyncFn"]),
            make_id(&["demo", "expArrow"]),
            make_id(&["demo", "ExpClass"]),
            make_id(&["demo", "defFn"]),
        ] {
            assert_export_status(&extraction, &id, true);
        }
        assert_export_status(&extraction, &make_id(&["demo", "bareFn"]), false);
        assert_export_status(&extraction, &make_id(&["demo", "bareArrow"]), false);

        for (class, method) in [("BareClass", "bareMethod"), ("ExpClass", "expMethod")] {
            let class = make_id(&["demo", class]);
            let method = make_id(&[&class, method]);
            assert_definition(&extraction, &method, "function");
            assert_single_edge(&extraction, &class, &method, "method");
        }
    }

    #[test]
    fn javascript_variable_binding_names_own_their_calls() {
        let fixture = Fixture::new();
        let javascript = fixture.write(
            "calls.js",
            r#"
function helper() {}
export const publicName = function internalName() { helper(); };
"#,
        );
        let extraction = extract(&javascript, "calls.js");

        assert_eq!(
            definition_labels(&extraction),
            vec!["helper()", "publicName()"]
        );
        let public_name = make_id(&["calls", "publicName"]);
        assert_export_status(&extraction, &public_name, true);
        assert!(extraction
            .nodes
            .iter()
            .all(|node| node.id != make_id(&["calls", "internalName"])));
        assert_single_edge(
            &extraction,
            &public_name,
            &make_id(&["calls", "helper"]),
            "calls",
        );
    }

    #[test]
    fn typescript_extracts_exported_variable_bound_functions() {
        let fixture = Fixture::new();
        let typescript = fixture.write(
            "demo.ts",
            r#"
function helper(): void {}
export const typedArrow = async (): Promise<void> => { helper(); };
export const typedFnExpr = function (): void { helper(); };
export class Service {}
"#,
        );
        let extraction = extract(&typescript, "demo.ts");

        assert_eq!(
            definition_labels(&extraction),
            vec!["Service", "helper()", "typedArrow()", "typedFnExpr()"]
        );
        for id in [
            make_id(&["demo", "typedArrow"]),
            make_id(&["demo", "typedFnExpr"]),
            make_id(&["demo", "Service"]),
        ] {
            assert_export_status(&extraction, &id, true);
        }
        assert_export_status(&extraction, &make_id(&["demo", "helper"]), false);

        let helper = make_id(&["demo", "helper"]);
        for caller in ["typedArrow", "typedFnExpr"] {
            assert_single_edge(&extraction, &make_id(&["demo", caller]), &helper, "calls");
        }
    }

    #[test]
    fn javascript_cross_file_direct_calls_resolve_through_imports() {
        let fixture = Fixture::new();
        let library = fixture.write(
            "library.js",
            "export function helper() {}\nexport function open() {}\n",
        );
        let caller = fixture.write(
            "caller.js",
            r#"
import { helper, open } from "./library";
export function run() { helper(); open(); }
"#,
        );
        let mut extractions = vec![
            extract(&library, "library.js"),
            extract(&caller, "caller.js"),
        ];

        super::resolution::resolve(&mut extractions);

        assert_single_edge(
            &extractions[1],
            &make_id(&["caller", "run"]),
            &make_id(&["library", "helper"]),
            "calls",
        );
        assert_single_edge(
            &extractions[1],
            &make_id(&["caller", "run"]),
            &make_id(&["library", "open"]),
            "calls",
        );
    }

    #[test]
    fn go_cross_file_direct_calls_resolve_within_the_package() {
        let fixture = Fixture::new();
        let library = fixture.write("library.go", "package demo\nfunc helper() {}\n");
        let caller = fixture.write("caller.go", "package demo\nfunc run() { helper() }\n");
        let mut extractions = vec![
            extract(&library, "demo/library.go"),
            extract(&caller, "demo/caller.go"),
        ];

        super::resolution::resolve(&mut extractions);

        assert_single_edge(
            &extractions[1],
            &make_id(&["demo/caller", "run"]),
            &make_id(&["demo/library", "helper"]),
            "calls",
        );
    }

    #[test]
    fn compiled_languages_retain_unresolved_direct_call_facts() {
        let fixture = Fixture::new();
        for (filename, source) in [
            ("direct.py", "def caller():\n    missing()\n"),
            ("direct.js", "function caller() { missing(); }\n"),
            ("direct.ts", "function caller(): void { missing(); }\n"),
            ("direct.tsx", "function caller(): void { missing(); }\n"),
            ("direct.go", "package demo\nfunc caller() { missing() }\n"),
            ("direct.rs", "fn caller() { missing(); }\n"),
            (
                "Direct.java",
                "class Direct { void caller() { missing(); } }\n",
            ),
            ("direct.c", "void caller(void) { missing(); }\n"),
            ("direct.cpp", "void caller() { missing(); }\n"),
            ("direct.rb", "def caller\n  missing()\nend\n"),
            (
                "Direct.cs",
                "class Direct { void Caller() { Missing(); } }\n",
            ),
        ] {
            let path = fixture.write(filename, source);
            let extraction = extract(&path, filename);
            let fact = extraction
                .edges
                .iter()
                .find(|edge| {
                    edge.extra
                        .get("unresolved_call")
                        .and_then(|value| value.as_bool())
                        == Some(true)
                })
                .unwrap_or_else(|| panic!("{filename} dropped its unresolved direct call"));
            assert_eq!(
                fact.extra
                    .get("callee")
                    .and_then(|value| value.as_str())
                    .map(str::to_lowercase)
                    .as_deref(),
                Some("missing"),
                "{filename} retained the wrong callee"
            );
            assert_eq!(
                fact.extra
                    .get("member_call")
                    .and_then(|value| value.as_bool()),
                Some(false),
                "{filename} misclassified a direct call as a member call"
            );
        }
    }

    #[test]
    fn ast_parse_recovery_is_visible_on_the_file_anchor() {
        let fixture = Fixture::new();
        let path = fixture.write("broken.js", "function broken( {\n");

        let extraction = extract(&path, "broken.js");
        let file = extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("type").and_then(|value| value.as_str()) == Some("file"))
            .expect("file anchor");

        assert_eq!(
            file.extra
                .get("parser_has_error")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        let diagnostic_nodes = ["parse_error_count", "missing_node_count"]
            .iter()
            .filter_map(|key| file.extra.get(*key).and_then(|value| value.as_u64()))
            .sum::<u64>();
        assert!(diagnostic_nodes > 0, "parser recovery must be quantified");
        assert!(file
            .extra
            .get("parse_error_spans")
            .and_then(|value| value.as_array())
            .is_some_and(|spans| !spans.is_empty()));
    }

    #[test]
    fn rust_2021_raw_references_are_grammar_warnings_not_parse_errors() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "raw_reference.rs",
            "fn consume(_: &str) {}\nfn demo(raw: &str) { consume(&raw); }\n",
        );

        let extraction = extract(&path, "raw_reference.rs");
        let file = extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("type").and_then(|value| value.as_str()) == Some("file"))
            .expect("file anchor");

        assert_eq!(
            file.extra
                .get("parse_error_count")
                .and_then(|value| value.as_u64()),
            Some(0)
        );
        assert_eq!(
            file.extra
                .get("parser_compatibility_count")
                .and_then(|value| value.as_u64()),
            Some(1)
        );
    }

    #[test]
    fn javascript_member_calls_do_not_change_with_same_name_decoys() {
        for (case, extra_decoy) in [("one", ""), ("two", "class Backup { save() {} }")] {
            let fixture = Fixture::new();
            let library = fixture.write("library.js", "export function save() {}\n");
            let path = fixture.write(
                "member.js",
                &format!(
                    "import {{ save }} from './library';\nclass Repo {{ save() {{}} }}\n{extra_decoy}\nfunction caller(other) {{ other.save(); save(); }}\n"
                ),
            );
            let mut extractions =
                vec![extract(&library, "library.js"), extract(&path, "member.js")];

            super::resolution::resolve(&mut extractions);

            let caller = make_id(&["member", "caller"]);
            assert_eq!(
                resolved_call_targets(&extractions[1], &caller),
                vec![make_id(&["library", "save"])],
                "member-call decoys changed direct-call resolution in {case} case"
            );
            let unresolved: Vec<_> = extractions[1]
                .edges
                .iter()
                .filter(|edge| {
                    edge.true_source() == caller
                        && edge
                            .extra
                            .get("unresolved_call")
                            .and_then(|value| value.as_bool())
                            == Some(true)
                })
                .collect();
            assert_eq!(
                unresolved.len(),
                1,
                "expected the unsafe member call to remain auditable in {case} case"
            );
            assert_eq!(
                unresolved[0]
                    .extra
                    .get("member_call")
                    .and_then(|value| value.as_bool()),
                Some(true)
            );
        }
    }

    #[test]
    fn go_member_calls_do_not_change_with_same_name_decoys() {
        for extra_decoy in ["", "type Backup struct{}\nfunc (Backup) Save() {}\n"] {
            let fixture = Fixture::new();
            let library = fixture.write("library.go", "package demo\nfunc Save() {}\n");
            let path = fixture.write(
                "member.go",
                &format!(
                    "package demo\ntype Repo struct{{}}\nfunc (Repo) Save() {{}}\n{extra_decoy}func caller(other any) {{ other.Save(); Save() }}\n"
                ),
            );
            let mut extractions = vec![
                extract(&library, "demo/library.go"),
                extract(&path, "demo/member.go"),
            ];

            super::resolution::resolve(&mut extractions);

            let caller = make_id(&["demo/member", "caller"]);
            assert_eq!(
                resolved_call_targets(&extractions[1], &caller),
                vec![make_id(&["demo/library", "Save"])],
                "member-call decoys changed direct-call resolution"
            );
            assert_eq!(
                extractions[1]
                    .edges
                    .iter()
                    .filter(|edge| {
                        edge.true_source() == caller
                            && edge
                                .extra
                                .get("unresolved_call")
                                .and_then(|value| value.as_bool())
                                == Some(true)
                            && edge
                                .extra
                                .get("member_call")
                                .and_then(|value| value.as_bool())
                                == Some(true)
                    })
                    .count(),
                1,
                "unsafe Go member call should remain an unresolved audit fact"
            );
        }
    }

    #[test]
    fn python_injected_fields_resolve_to_their_typed_methods() {
        let fixture = Fixture::new();
        let ports = fixture.write(
            "ports.py",
            r#"
class InventoryRepository:
    def reserve(self, items): ...
    def release(self, items): ...

class PaymentGateway:
    def charge(self, order_id): ...

class DemoPaymentGateway:
    def charge(self, order_id): ...

class OrderRepository:
    def save(self, order): ...

class InMemoryOrderRepository:
    def save(self, order): ...

class NotificationService:
    def send_confirmation(self, order): ...
"#,
        );
        let checkout_file = fixture.write(
            "checkout.py",
            r#"
from ports import InventoryRepository, NotificationService, OrderRepository, PaymentGateway

class CheckoutService:
    def __init__(
        self,
        inventory: InventoryRepository,
        payments: PaymentGateway,
        orders: OrderRepository,
        notifications: NotificationService,
    ):
        self.inventory = inventory
        self.payments = payments
        self.orders = orders
        self.notifications = notifications

    def checkout(self, order):
        self.inventory.reserve(order.items)
        self.payments.charge(order.order_id)
        self.inventory.release(order.items)
        self.orders.save(order)
        self.notifications.send_confirmation(order)
"#,
        );
        let mut extractions = vec![
            extract(&ports, "ports.py"),
            extract(&checkout_file, "checkout.py"),
        ];
        super::resolution::resolve(&mut extractions);

        let checkout = make_id(&["checkout", "CheckoutService", "checkout"]);
        let expected = [
            (
                make_id(&["ports", "InventoryRepository", "reserve"]),
                "InventoryRepository",
            ),
            (
                make_id(&["ports", "InventoryRepository", "release"]),
                "InventoryRepository",
            ),
            (
                make_id(&["ports", "PaymentGateway", "charge"]),
                "PaymentGateway",
            ),
            (
                make_id(&["ports", "OrderRepository", "save"]),
                "OrderRepository",
            ),
            (
                make_id(&["ports", "NotificationService", "send_confirmation"]),
                "NotificationService",
            ),
        ];

        for (target, receiver_type) in expected {
            let edge = extractions
                .iter()
                .flat_map(|extraction| &extraction.edges)
                .find(|edge| {
                    edge.relation == "calls"
                        && edge.true_source() == checkout
                        && edge.true_target() == target
                })
                .unwrap_or_else(|| panic!("missing injected call from {checkout} to {target}"));
            assert_eq!(
                edge.extra
                    .get("receiver_type")
                    .and_then(|value| value.as_str()),
                Some(receiver_type)
            );
        }
    }
}
