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

/// Decode and unescape an XML attribute without changing its literal
/// whitespace. `quick-xml` 0.40 made its replacement attribute helpers apply
/// XML attribute normalization; retaining the prior unescape-only contract
/// keeps existing graph labels and identities stable across the security
/// upgrade.
fn decode_xml_attribute(value: &[u8]) -> quick_xml::Result<std::borrow::Cow<'_, str>> {
    let value =
        std::str::from_utf8(value).map_err(|error| quick_xml::Error::Encoding(error.into()))?;
    quick_xml::escape::unescape(value).map_err(quick_xml::Error::Escape)
}

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

/// Aggregate parser work for an isolated extraction run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeWorkTelemetry {
    /// Parser invocations attempted after cache decisions.
    pub parses: u64,
}

/// Additive measurements returned by telemetry-aware extraction entry points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeExtractionTelemetry {
    pub io: graphoxide_index_runtime::RuntimeIoTelemetry,
    pub work: RuntimeWorkTelemetry,
    pub cache_io: graphoxide_index_runtime::cache::RuntimeCacheIoTelemetry,
}

fn collect_runtime_cache_io_telemetry<F>(
    require_telemetry: bool,
    snapshot: F,
) -> anyhow::Result<graphoxide_index_runtime::cache::RuntimeCacheIoTelemetry>
where
    F: FnOnce() -> Result<
        graphoxide_index_runtime::cache::RuntimeCacheIoTelemetry,
        graphoxide_index_runtime::cache::RuntimeCacheIoServiceError,
    >,
{
    if !require_telemetry {
        return Ok(graphoxide_index_runtime::cache::RuntimeCacheIoTelemetry::default());
    }
    snapshot().map_err(|error| anyhow::anyhow!("runtime cache telemetry barrier failed: {error}"))
}

/// Byte-only extraction plus additive runtime measurements.
#[derive(Debug)]
pub struct RuntimeProjectExtractionWithTelemetry {
    pub result: RuntimeProjectExtraction,
    pub telemetry: RuntimeExtractionTelemetry,
}

impl std::ops::Deref for RuntimeProjectExtractionWithTelemetry {
    type Target = RuntimeProjectExtraction;

    fn deref(&self) -> &Self::Target {
        &self.result
    }
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
    extract_project_with_runtime_with_telemetry(root, config).map(|extraction| extraction.result)
}

/// Run the byte-only extraction substrate and return additive runtime
/// measurements without changing [`RuntimeProjectExtraction`].
pub fn extract_project_with_runtime_with_telemetry(
    root: &std::path::Path,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
) -> anyhow::Result<RuntimeProjectExtractionWithTelemetry> {
    use graphoxide_index_runtime::{
        read_files_concurrently_with_telemetry, FileReadRequest, InputIdentity,
    };
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
    let completed = read_files_concurrently_with_telemetry(config, requests, move |input| {
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
        if !output_admission.try_reserve(retained_bytes) {
            return Err(retained_output_budget_error(
                config.memory_budget_bytes,
                output_budget,
            ));
        }
        Ok(extraction)
    })
    .map_err(|error| anyhow::anyhow!("isolated extraction runtime failed: {error:?}"))?;

    let runtime_io = completed.telemetry;
    let completed = completed.result;
    let runtime_work = RuntimeWorkTelemetry {
        parses: u64::try_from(completed.completed.len()).unwrap_or(u64::MAX),
    };
    let mut extractions = Vec::with_capacity(completed.completed.len());
    for completed in completed.completed {
        extractions.push(completed.value?);
    }
    Ok(RuntimeProjectExtractionWithTelemetry {
        result: RuntimeProjectExtraction {
            extractions,
            detection,
            read_failures: completed.failures,
        },
        telemetry: RuntimeExtractionTelemetry {
            io: runtime_io,
            work: runtime_work,
            cache_io: graphoxide_index_runtime::cache::RuntimeCacheIoTelemetry::default(),
        },
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

/// A whole-pass manifest ownership limit was exhausted before a pending map
/// could be admitted. Callers must not downgrade this to a per-file skip,
/// because no subset manifest can truthfully authorize the accepted graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestRetainedLimitError {
    limit: usize,
    pending: bool,
}

impl std::fmt::Display for ManifestRetainedLimitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let phase = if self.pending {
            "explicit pending manifest retained ownership would exceed"
        } else {
            "explicit committed manifest retained ownership exceeds"
        };
        write!(
            formatter,
            "{phase} the effective {}-byte manifest cap; retry with a larger graph byte limit or request a Full Rebuild",
            self.limit
        )
    }
}

impl std::error::Error for ManifestRetainedLimitError {}

impl PendingProjectManifest {
    pub fn path(&self) -> std::path::PathBuf {
        self.output_directory.join("manifest.json")
    }

    /// Exact held-generation entries prepared by extraction.
    ///
    /// Graph coordinators may clone a bounded subset into a full-corpus
    /// pending manifest, but must never recompute these rows from source paths
    /// after accepting the corresponding graph facts.
    pub fn entries(&self) -> &cache::Manifest {
        &self.entries
    }

    /// Conservative retained-memory charge for this unpublished map.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        pending_manifest_retained_charge(&self.entries)
    }

    /// Construct an unpublished manifest from already-admitted entries.
    pub fn from_entries(output_directory: std::path::PathBuf, entries: cache::Manifest) -> Self {
        Self {
            output_directory,
            entries,
        }
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
    /// Conservative retained ownership charge for the deferred manifest map.
    ///
    /// This map remains live beside the returned extractions until graph
    /// publication succeeds, so graph-stage callers must reserve both values
    /// from the shared cache/run partition.
    pub pending_manifest_retained_bytes: usize,
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
    /// Successfully generation-verified ambiguous representation sources.
    ///
    /// A caller with a committed graph may use this non-destructive evidence
    /// to authorize repair of an actual structural Code/MPEG conflict, or to
    /// prove that a code-only policy exclusion was checked. Verification alone
    /// is not permission to erase a byte-identical media inventory or its
    /// semantic enrichment.
    pub verified_representation_sources: Vec<std::path::PathBuf>,
    /// Authoritative cross-tier ownership resets for the current generation.
    ///
    /// These are narrower than `verified_representation_sources`: they cover
    /// proven source-kind transitions and changed media only when the scan
    /// produced replacement inventory. Callers must pass them through the
    /// unsuppressed baseline ownership-reset channel, never ordinary deletion
    /// prunes. The reset applies only to carried baseline facts, so fresh facts
    /// for the same source remain intact.
    pub ownership_prune_sources: Vec<std::path::PathBuf>,
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

/// Deferred extraction plus additive runtime measurements.
#[derive(Debug)]
#[must_use = "the pending manifest must be committed or deliberately discarded"]
pub struct DeferredProjectExtractionWithTelemetry {
    pub result: DeferredProjectExtractionResult,
    pub telemetry: RuntimeExtractionTelemetry,
}

/// Progress-aware extraction evidence kept separate from the established
/// telemetry DTO. The indexed byte count covers only dispatched indexed
/// inputs, excluding resolver-only metadata contexts.
#[derive(Debug)]
#[must_use = "the pending manifest must be committed or deliberately discarded"]
pub struct DeferredProjectExtractionWithProgress {
    pub result: DeferredProjectExtractionResult,
    pub telemetry: RuntimeExtractionTelemetry,
    pub indexed_source_bytes: u64,
    /// Whether the committed graph and admitted manifest can authorize an
    /// incremental graph delta for this pass.
    pub incremental_baseline_eligible: bool,
}

#[derive(Debug)]
struct DeferredProjectExtractionInternal {
    extraction: DeferredProjectExtractionWithTelemetry,
    indexed_source_bytes: u64,
    incremental_baseline_eligible: bool,
}

impl DeferredProjectExtractionInternal {
    fn into_progress(self) -> DeferredProjectExtractionWithProgress {
        DeferredProjectExtractionWithProgress {
            result: self.extraction.result,
            telemetry: self.extraction.telemetry,
            indexed_source_bytes: self.indexed_source_bytes,
            incremental_baseline_eligible: self.incremental_baseline_eligible,
        }
    }
}

impl std::ops::Deref for DeferredProjectExtractionWithTelemetry {
    type Target = DeferredProjectExtractionResult;

    fn deref(&self) -> &Self::Target {
        &self.result
    }
}

/// One file's contribution to a project scan: its extraction plus the manifest
/// evidence that dates it.
///
/// Returned as an error only for faults specific to this file, so the caller
/// can record the failure and continue with the rest of the corpus.
type ProjectExtractionRow = (String, graphoxide_core::Extraction, f64, String);
type DetectionTestHook<'a> = &'a mut dyn FnMut(&detect::DetectResult) -> anyhow::Result<()>;

#[derive(Default)]
struct LegacyExtractionHooks<'a> {
    before_extraction: Option<DetectionTestHook<'a>>,
    after_extraction: Option<DetectionTestHook<'a>>,
    progress: Option<ProjectExtractionProgressObserver>,
}

/// Source-safe aggregate progress produced from the already-dispatched indexed
/// inputs. Worker threads update atomics only; a single bounded monitor invokes
/// this callback at most ten times per second plus the initial/final state.
pub type ProjectExtractionProgressObserver =
    std::sync::Arc<dyn Fn(usize, usize) + Send + Sync + 'static>;

struct ProjectExtractionProgressState {
    processed: std::sync::atomic::AtomicUsize,
    done: std::sync::atomic::AtomicBool,
    wake: std::sync::Condvar,
    gate: std::sync::Mutex<()>,
    total: usize,
}

impl ProjectExtractionProgressState {
    fn complete_one(&self) {
        let _ = self.processed.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |current| Some(current.saturating_add(1).min(self.total)),
        );
    }

    fn complete_many(&self, count: usize) {
        let _ = self.processed.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |current| Some(current.saturating_add(count).min(self.total)),
        );
    }
}

struct ProjectExtractionCompletion {
    state: Option<std::sync::Arc<ProjectExtractionProgressState>>,
}

impl ProjectExtractionCompletion {
    fn new(state: Option<std::sync::Arc<ProjectExtractionProgressState>>, indexed: bool) -> Self {
        Self {
            state: indexed.then_some(state).flatten(),
        }
    }
}

impl Drop for ProjectExtractionCompletion {
    fn drop(&mut self) {
        if let Some(state) = self.state.as_ref() {
            state.complete_one();
        }
    }
}

struct ProjectExtractionProgressMonitor {
    state: Option<std::sync::Arc<ProjectExtractionProgressState>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ProjectExtractionProgressMonitor {
    fn start(total: usize, observer: Option<ProjectExtractionProgressObserver>) -> Self {
        let Some(observer) = observer else {
            return Self {
                state: None,
                thread: None,
            };
        };
        observer(0, total);
        if total == 0 {
            return Self {
                state: None,
                thread: None,
            };
        }
        let state = std::sync::Arc::new(ProjectExtractionProgressState {
            processed: std::sync::atomic::AtomicUsize::new(0),
            done: std::sync::atomic::AtomicBool::new(false),
            wake: std::sync::Condvar::new(),
            gate: std::sync::Mutex::new(()),
            total,
        });
        let monitor_state = std::sync::Arc::clone(&state);
        let thread = std::thread::spawn(move || {
            let mut last = 0;
            let interval = std::time::Duration::from_millis(100);
            let mut next_emit = std::time::Instant::now() + interval;
            loop {
                let guard = monitor_state
                    .gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let timeout = next_emit.saturating_duration_since(std::time::Instant::now());
                let _ = monitor_state
                    .wake
                    .wait_timeout(guard, timeout)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let processed = monitor_state
                    .processed
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .min(monitor_state.total);
                let done = monitor_state
                    .done
                    .load(std::sync::atomic::Ordering::Acquire);
                let now = std::time::Instant::now();
                if processed != last && (done || now >= next_emit) {
                    observer(processed, monitor_state.total);
                    last = processed;
                }
                if done {
                    break;
                }
                if now >= next_emit {
                    next_emit = now + interval;
                }
            }
        });
        Self {
            state: Some(state),
            thread: Some(thread),
        }
    }

    fn counter(&self) -> Option<std::sync::Arc<ProjectExtractionProgressState>> {
        self.state.as_ref().map(std::sync::Arc::clone)
    }

    fn finish(mut self) {
        if let Some(state) = self.state.as_ref() {
            debug_assert_eq!(
                state.processed.load(std::sync::atomic::Ordering::Relaxed),
                state.total,
                "every dispatched indexed input must reach a terminal extraction attempt"
            );
        }
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(state) = self.state.as_ref() {
            state.done.store(true, std::sync::atomic::Ordering::Release);
            state.wake.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ProjectExtractionProgressMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn extract_one_project_file(
    path: &std::path::Path,
    relative: &str,
    force: bool,
    managed_output_dir: &std::path::Path,
) -> anyhow::Result<ProjectExtractionRow> {
    use md5::Digest as _;
    if is_ambiguous_typescript_extension(relative)
        && let Some(evidence) =
            detect::checked_mpeg_transport_stream_evidence(std::path::Path::new(relative), path)?
    {
        return Ok((
            relative.to_owned(),
            engine::mpeg_transport_stream_inventory(path, relative, evidence.byte_length),
            evidence.mtime,
            evidence.ast_hash,
        ));
    }
    let bytes = std::fs::read(path)?;
    let cached = (!force)
        .then(|| cache::ast_cache_get_from_output(managed_output_dir, relative, &bytes))
        .flatten();
    let extraction = if let Some(cached) = cached {
        cached
    } else {
        // The caller names the file in the context it attaches to this error.
        let extracted = if is_ambiguous_typescript_extension(relative) {
            // `.ts` is extension-ambiguous. Keep classification, extraction,
            // and cache evidence bound to the same admitted allocation rather
            // than reopening a generation that may have changed meanwhile.
            engine::extract_as_admitted_bytes_with_path_probes(path, relative, &bytes)?
        } else {
            engine::extract_as(path, relative)?
        };
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
    /// Stable discovery bucket persisted as incremental ownership evidence.
    source_kind: String,
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
    parses: u64,
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
    (
        graphoxide_index_runtime::cache::RuntimeCacheIoPersistOutcome,
        usize,
    ),
    graphoxide_index_runtime::cache::RuntimeCacheIoServiceError,
> {
    let encoded_bytes = cache::runtime_ast_cache_payload_len(evidence, extraction)
        .map_err(graphoxide_index_runtime::cache::RuntimeCacheIoServiceError::Cache)?;
    client
        .persist_encoded_with_cancellation(
            evidence.key,
            encoded_bytes,
            replace_existing,
            cancellation,
            |output| cache::encode_runtime_ast_cache_payload_into(output, evidence, extraction),
        )
        .map(|outcome| (outcome, encoded_bytes))
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

/// Conservative wire-byte admission for a project manifest that will remain
/// live beside graph work. The one-sixty-fourth partition preserves the
/// runtime loader's historical 32x decode allowance while leaving at least
/// half of the caller's retained budget for the decoded map and pending rows.
#[must_use]
pub fn project_manifest_wire_byte_limit(
    retained_budget_bytes: usize,
    admitted_files: usize,
) -> usize {
    runtime_manifest_byte_limit(retained_budget_bytes, admitted_files)
}

fn runtime_manifest_retained_charge(
    cache_and_runs_bytes: usize,
    admitted_manifest_bytes: usize,
) -> usize {
    admitted_manifest_bytes
        .saturating_mul(32)
        .min(cache_and_runs_bytes / 2)
}

fn pending_manifest_retained_reservation(
    cache_and_runs_bytes: usize,
    prospective_entries: usize,
) -> usize {
    runtime_manifest_byte_limit(cache_and_runs_bytes, prospective_entries)
        .saturating_mul(32)
        .min(cache_and_runs_bytes / 2)
}

fn pending_manifest_entry_retained_charge(
    key: &str,
    ast_hash: &str,
    semantic_hash: &str,
    source_kind: Option<&str>,
) -> usize {
    // The key, both hashes, and optional source kind own separate allocations. The fixed charge
    // covers the BTree slot, inline strings/evidence, allocator metadata, and
    // node slack; doubling the complete total remains conservative across
    // allocator and BTree implementations without serializing another copy.
    const BTREE_AND_ALLOCATOR_SLACK_BYTES: usize = 64;
    std::mem::size_of::<(String, cache::ManifestEntry)>()
        .saturating_add(BTREE_AND_ALLOCATOR_SLACK_BYTES)
        .saturating_add(key.len())
        .saturating_add(ast_hash.len())
        .saturating_add(semantic_hash.len())
        .saturating_add(source_kind.map_or(0, str::len))
        .saturating_mul(2)
}

fn pending_manifest_retained_charge(manifest: &cache::Manifest) -> usize {
    manifest.iter().fold(0usize, |total, (key, entry)| {
        total.saturating_add(pending_manifest_entry_retained_charge(
            key,
            &entry.ast_hash,
            &entry.semantic_hash,
            entry.source_kind.as_deref(),
        ))
    })
}

/// Conservative retained-memory charge for a prepared project manifest.
#[must_use]
pub fn project_manifest_retained_bytes(manifest: &cache::Manifest) -> usize {
    pending_manifest_retained_charge(manifest)
}

fn pending_manifest_budget_error(
    memory_budget_bytes: usize,
    reservation_bytes: usize,
    required_bytes: usize,
) -> anyhow::Error {
    anyhow::anyhow!(
        "isolated pending manifest requires a {required_bytes}-byte retained ownership charge beyond its {reservation_bytes}-byte reservation within the effective {memory_budget_bytes}-byte managed-memory budget; retry with a larger --memory-budget-bytes value"
    )
}

fn retained_output_budget_error(
    memory_budget_bytes: usize,
    output_budget_bytes: usize,
) -> anyhow::Error {
    anyhow::anyhow!(
        "isolated retained extraction output exhausted its {output_budget_bytes}-byte output cap within the effective {memory_budget_bytes}-byte managed-memory budget; retry with a larger --memory-budget-bytes value"
    )
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

fn detected_source_kinds(
    detection: &detect::DetectResult,
    resolved_root: &std::path::Path,
    original_root: &std::path::Path,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let mut kinds = std::collections::BTreeMap::new();
    for (kind, paths) in &detection.files {
        for path in paths {
            let key =
                normalized_project_key(std::path::Path::new(path), resolved_root, original_root);
            if let Some(previous) = kinds.insert(key.clone(), kind.clone()) {
                anyhow::ensure!(
                    previous == *kind,
                    "source {key:?} was discovered in both {previous:?} and {kind:?} buckets"
                );
            }
        }
    }
    Ok(kinds)
}

/// Whether a live source's current discovery bucket proves that its committed
/// fact ownership changed. Missing legacy evidence is not enough to invalidate
/// unrelated policy-excluded formats, but an ambiguous `.ts` now positively
/// classified as video must never preserve facts formerly parsed as code.
fn source_kind_transition(
    entry: &cache::ManifestEntry,
    current_kind: &str,
    relative: &str,
) -> bool {
    match entry.source_kind.as_deref() {
        Some(previous_kind) => previous_kind != current_kind,
        None => {
            current_kind == detect::FileType::Video.as_str()
                && std::path::Path::new(relative)
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
        }
    }
}

fn source_kind_transition_affects_code(
    entry: &cache::ManifestEntry,
    current_kind: &str,
    relative: &str,
) -> bool {
    source_kind_transition(entry, current_kind, relative)
        && entry.source_kind.as_deref().is_none_or(|previous_kind| {
            previous_kind == detect::FileType::Code.as_str()
                || current_kind == detect::FileType::Code.as_str()
        })
}

const DIAGNOSTIC_SOURCE_PATH_MAX_BYTES: usize = 256;

fn bounded_diagnostic_source_path(path: &str) -> String {
    let mut end = path.len().min(DIAGNOSTIC_SOURCE_PATH_MAX_BYTES);
    while !path.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let sample = if end < path.len() {
        format!("{}…", &path[..end])
    } else {
        path.to_owned()
    };
    // Debug formatting escapes control characters so an untrusted path cannot
    // forge additional diagnostic lines. The input slice is fixed-size, so
    // escape expansion is bounded as well.
    format!("{sample:?}")
}

fn unverified_source_kind_transition_error(
    sources: &std::collections::BTreeSet<String>,
) -> anyhow::Error {
    let first = sources.first().map_or_else(
        || "<none>".to_owned(),
        |source| bounded_diagnostic_source_path(source),
    );
    anyhow::anyhow!(
        "source classification transition could not be verified from admitted bytes for {} source(s); first source: {first}; retry the extraction",
        sources.len()
    )
}

fn ensure_code_only_ambiguous_media_has_trusted_manifest(
    force: bool,
    code_only: bool,
    committed_graph_exists: bool,
    committed_manifest_is_trusted: bool,
    detected_kinds: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<()> {
    if force || !code_only || !committed_graph_exists || committed_manifest_is_trusted {
        return Ok(());
    }
    let ambiguous_sources = detected_kinds
        .iter()
        .filter(|(relative, current_kind)| {
            current_kind.as_str() == detect::FileType::Video.as_str()
                && std::path::Path::new(relative.as_str())
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
        })
        .map(|(relative, _)| relative.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(first) = ambiguous_sources.first() {
        anyhow::bail!(
            "cannot safely perform a --code-only incremental extraction: the committed graph has no trustworthy manifest for {} extension-ambiguous MPEG transport-stream source(s); first source: {}; rerun without --code-only or use --force for a full rebuild",
            ambiguous_sources.len(),
            bounded_diagnostic_source_path(first)
        );
    }
    Ok(())
}

fn extraction_confirms_mpeg_transport_stream(
    extraction: &graphoxide_core::Extraction,
    relative: &str,
) -> bool {
    extraction.nodes.iter().any(|node| {
        node.source_file == relative
            && node.extra.get("format").and_then(serde_json::Value::as_str)
                == Some("mpeg_transport_stream")
    })
}

fn is_ambiguous_typescript_extension(relative: &str) -> bool {
    std::path::Path::new(relative)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
}

fn admitted_mpeg_classification_matches_extraction(
    relative: &str,
    source_kind: Option<&str>,
    extraction: &graphoxide_core::Extraction,
) -> bool {
    if !is_ambiguous_typescript_extension(relative) {
        return true;
    }
    let admitted_as_mpeg = source_kind == Some(detect::FileType::Video.as_str());
    let extracted_as_mpeg = extraction_confirms_mpeg_transport_stream(extraction, relative);
    admitted_as_mpeg == extracted_as_mpeg
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

/// Telemetry-aware production indexing entry point.
pub fn extract_project_with_runtime_scan_options_deferred_manifest_with_telemetry(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
) -> anyhow::Result<DeferredProjectExtractionWithTelemetry> {
    extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_telemetry(
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
    extract_project_with_runtime_scan_options_deferred_manifest_impl(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
        config,
        cancellation,
        false,
        None,
        None,
    )
    .map(|extraction| extraction.extraction.result)
}

/// Cancellation-aware extraction with build-mode and indexed-byte evidence,
/// but without starting a progress monitor or requiring strict telemetry.
#[allow(clippy::too_many_arguments)]
pub fn extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_build_evidence(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
    cancellation: graphoxide_index_runtime::RuntimeCancellation,
) -> anyhow::Result<DeferredProjectExtractionWithProgress> {
    extract_project_with_runtime_scan_options_deferred_manifest_impl(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
        config,
        cancellation,
        false,
        None,
        None,
    )
    .map(DeferredProjectExtractionInternal::into_progress)
}

/// Cancellation-aware extraction with best-effort aggregate runtime evidence
/// and source-safe phase observations. Unlike the runtime-report entry point,
/// this does not make cache telemetry barriers strict merely because progress
/// is enabled.
#[allow(clippy::too_many_arguments)]
pub fn extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_progress(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
    cancellation: graphoxide_index_runtime::RuntimeCancellation,
    progress: ProjectExtractionProgressObserver,
) -> anyhow::Result<DeferredProjectExtractionWithProgress> {
    extract_project_with_runtime_scan_options_deferred_manifest_impl(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
        config,
        cancellation,
        false,
        None,
        Some(progress),
    )
    .map(DeferredProjectExtractionInternal::into_progress)
}

/// Cancellation-aware production indexing entry point with additive runtime
/// measurements.
#[allow(clippy::too_many_arguments)]
pub fn extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_telemetry(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
    cancellation: graphoxide_index_runtime::RuntimeCancellation,
) -> anyhow::Result<DeferredProjectExtractionWithTelemetry> {
    extract_project_with_runtime_scan_options_deferred_manifest_impl(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
        config,
        cancellation,
        true,
        None,
        None,
    )
    .map(|extraction| extraction.extraction)
}

/// Strict telemetry-aware extraction with build-mode and indexed-byte
/// evidence, but without starting a progress monitor.
#[allow(clippy::too_many_arguments)]
pub fn extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_telemetry_and_build_evidence(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
    cancellation: graphoxide_index_runtime::RuntimeCancellation,
) -> anyhow::Result<DeferredProjectExtractionWithProgress> {
    extract_project_with_runtime_scan_options_deferred_manifest_impl(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
        config,
        cancellation,
        true,
        None,
        None,
    )
    .map(DeferredProjectExtractionInternal::into_progress)
}

/// Strict telemetry-aware extraction with the same source-safe phase seam.
#[allow(clippy::too_many_arguments)]
pub fn extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_telemetry_and_progress(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
    cancellation: graphoxide_index_runtime::RuntimeCancellation,
    progress: ProjectExtractionProgressObserver,
) -> anyhow::Result<DeferredProjectExtractionWithProgress> {
    extract_project_with_runtime_scan_options_deferred_manifest_impl(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
        config,
        cancellation,
        true,
        None,
        Some(progress),
    )
    .map(DeferredProjectExtractionInternal::into_progress)
}

#[allow(clippy::too_many_arguments)]
fn extract_project_with_runtime_scan_options_deferred_manifest_impl(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    config: graphoxide_index_runtime::IndexRuntimeConfig,
    cancellation: graphoxide_index_runtime::RuntimeCancellation,
    require_telemetry: bool,
    mut post_request_test_hook: Option<DetectionTestHook<'_>>,
    progress_observer: Option<ProjectExtractionProgressObserver>,
) -> anyhow::Result<DeferredProjectExtractionInternal> {
    use graphoxide_index_runtime::{
        read_files_concurrently_with_cancellation_and_telemetry, FileReadRequest, InputIdentity,
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
    let extraction_progress =
        ProjectExtractionProgressMonitor::start(indexed_paths.len(), progress_observer);
    let extraction_progress_counter = extraction_progress.counter();
    let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let detected_kinds = detected_source_kinds(&detection, &resolved_root, root)?;
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
        let source_kind = detected_kinds.get(&relative).cloned().ok_or_else(|| {
            anyhow::anyhow!("discovered source {relative:?} has no stable classification")
        })?;
        match contexts.entry(relative) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(RuntimeFileContext {
                    path,
                    physical_path,
                    source_kind,
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
                anyhow::ensure!(
                    entry.get().source_kind == source_kind,
                    "source {:?} changed discovery bucket while preparing runtime input",
                    entry.key()
                );
                entry.get_mut().indexed |= indexed;
            }
        }
    }
    let cache_and_runs_budget = config.memory_budget().cache_and_runs_bytes;
    // Code-only scans still preserve committed non-code ownership rows. Size
    // the bounded wire admission from the complete discovered corpus rather
    // than only the paths selected for runtime reads.
    let manifest_byte_limit =
        runtime_manifest_byte_limit(cache_and_runs_budget, detected_kinds.len());
    let bounded_manifest =
        cache::load_manifest_from_output_bounded(&managed_output_dir, manifest_byte_limit);
    let cache::RuntimeManifestLoad {
        manifest: loaded_manifest,
        status: manifest_status,
        admitted_bytes: admitted_manifest_bytes,
    } = bounded_manifest;
    // The bounded loader caps the transient raw read before decode. Once it
    // returns, charge only the exact successfully decoded wire length whose
    // normalized tree remains live. Missing or rejected inputs report zero.
    let loaded_manifest_reservation =
        runtime_manifest_retained_charge(cache_and_runs_budget, admitted_manifest_bytes);
    // The loaded and next manifests coexist until the deferred result is
    // returned. Reserve the historical 32x bounded-normalization allowance for
    // the pending map independently of the exact post-load charge above.
    let pending_manifest_reservation = pending_manifest_retained_reservation(
        cache_and_runs_budget,
        contexts.len().saturating_add(loaded_manifest.len()),
    );
    let manifest_reservation = loaded_manifest_reservation
        .checked_add(pending_manifest_reservation)
        .expect("two half-partition manifest charges cannot overflow");
    debug_assert!(manifest_reservation <= cache_and_runs_budget);
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
    ensure_code_only_ambiguous_media_has_trusted_manifest(
        force,
        code_only,
        baseline_graph_path.is_file(),
        committed_manifest_exists,
        &detected_kinds,
    )?;
    let source_kind_transitions = detected_kinds
        .iter()
        .filter_map(|(relative, current_kind)| {
            previous.get(relative).and_then(|entry| {
                source_kind_transition_affects_code(entry, current_kind, relative)
                    .then(|| relative.clone())
            })
        })
        .collect::<std::collections::BTreeSet<_>>();
    // A syntactically valid manifest can still predate the committed graph
    // because graph publication precedes manifest publication. Reverify every
    // extension-ambiguous media path so callers can prove a policy exclusion or
    // repair an actual structural representation conflict. Verification alone
    // is deliberately not authority to erase stable media ownership.
    let ambiguous_media_sources = detected_kinds
        .iter()
        .filter(|(relative, current_kind)| {
            current_kind.as_str() == detect::FileType::Video.as_str()
                && is_ambiguous_typescript_extension(relative)
        })
        .map(|(relative, _)| relative.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let resolver_dependencies_invalidated =
        !source_kind_transitions.is_empty() || !ambiguous_media_sources.is_empty();
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
        .filter_map(|(ordinal, (relative, context))| {
            let streaming_media = context.source_kind == detect::FileType::Video.as_str()
                && is_ambiguous_typescript_extension(relative);
            (!streaming_media).then(|| {
                FileReadRequest::new_verified_under(
                    InputIdentity::new(relative.clone(), ordinal as u64),
                    context.physical_path.clone(),
                    &resolved_root,
                )
            })
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    if let Some(hook) = post_request_test_hook.as_mut() {
        hook(&detection)?;
    }
    let selected_sources = u64::try_from(contexts.len()).unwrap_or(u64::MAX);
    let mut source_bytes_selected = all_requests.iter().fold(0_u64, |total, request| {
        total.saturating_add(request.selected_source_bytes().unwrap_or(0))
    });
    let mut indexed_source_bytes = all_requests
        .iter()
        .filter(|request| {
            contexts
                .get(request.identity.normalized_path.as_ref())
                .is_some_and(|context| context.indexed)
        })
        .fold(0_u64, |total, request| {
            total.saturating_add(request.selected_source_bytes().unwrap_or(0))
        });
    let mut source_bytes_avoided = 0_u64;
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
    let cache_service_budget = if force { 0 } else { budget_after_manifest / 2 };
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
    // Confirmed MPEG `.ts` inputs are inventory-only. Stream their digest and
    // length through one fixed buffer instead of admitting the complete media
    // payload into the runtime source arena. The checked helper binds the
    // discriminator, digest, and current path to one no-follow generation.
    let mut streaming_media_rows = Vec::new();
    let mut streaming_media_sources_read = 0_u64;
    let mut streaming_media_source_bytes_read = 0_u64;
    for (relative, context) in &contexts {
        if context.source_kind != detect::FileType::Video.as_str()
            || !is_ambiguous_typescript_extension(relative)
        {
            continue;
        }
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        let evidence = detect::checked_mpeg_transport_stream_evidence_with_cancellation(
            std::path::Path::new(relative),
            &context.physical_path,
            &cancellation,
        )
        .map_err(|error| {
            if cancellation.is_cancelled() {
                anyhow::anyhow!("isolated extraction cancelled")
            } else {
                anyhow::Error::from(error).context(
                    unverified_source_kind_transition_error(&std::collections::BTreeSet::from([
                        relative.clone(),
                    ]))
                    .to_string(),
                )
            }
        })?
        .ok_or_else(|| {
            unverified_source_kind_transition_error(&std::collections::BTreeSet::from([
                relative.clone()
            ]))
        })?;
        source_bytes_selected = source_bytes_selected.saturating_add(evidence.byte_length);
        if context.indexed {
            indexed_source_bytes = indexed_source_bytes.saturating_add(evidence.byte_length);
        }
        streaming_media_sources_read = streaming_media_sources_read.saturating_add(1);
        streaming_media_source_bytes_read =
            streaming_media_source_bytes_read.saturating_add(evidence.byte_length);
        let extraction = if context.indexed {
            let extraction = engine::mpeg_transport_stream_inventory(
                &context.path,
                relative,
                evidence.byte_length,
            );
            let retained_bytes = extraction_retained_bytes(&extraction)?;
            if !output_admission.try_reserve_with_cancellation(retained_bytes, Some(&cancellation))
            {
                anyhow::ensure!(
                    !cancellation.is_cancelled(),
                    "isolated extraction cancelled"
                );
                return Err(retained_output_budget_error(
                    config.memory_budget_bytes,
                    output_budget,
                ));
            }
            Some(extraction)
        } else {
            None
        };
        streaming_media_rows.push(RuntimeScanRow {
            relative: relative.clone(),
            extraction,
            mtime: evidence.mtime,
            hash: evidence.ast_hash,
            changed: context.indexed,
            indexed: context.indexed,
            snapshot_source: None,
            warning: None,
            runtime_manifest: None,
            runtime_cache: cache::RuntimeCacheTelemetry::default(),
            runtime_cache_diagnostics: Vec::new(),
            parses: 0,
        });
        if context.indexed
            && let Some(progress) = extraction_progress_counter.as_ref()
        {
            progress.complete_one();
        }
    }

    // Probe metadata-authorized entries in stable request order. Only sources
    // the project resolver does not need may avoid their payload read.
    let mut requests = Vec::with_capacity(all_requests.len());
    let mut metadata_rows = streaming_media_rows;
    let mut preflight_skip_probe = BTreeMap::<String, bool>::new();
    for request in all_requests {
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        let relative = request.identity.normalized_path.to_string();
        let request_source_bytes = request.selected_source_bytes().unwrap_or(0);
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
            || (resolver_dependencies_invalidated
                && crate::js_resolution::is_javascript_source(&relative))
            || prior_entry.ast_version != cache::AST_CACHE_VERSION
            || prior_entry.source_kind.as_deref() != Some(context.source_kind.as_str())
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
                                source_bytes_avoided =
                                    source_bytes_avoided.saturating_add(request_source_bytes);
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
                                    parses: 0,
                                });
                                if let Some(progress) = extraction_progress_counter.as_ref() {
                                    progress.complete_one();
                                }
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
    let cache_previous_for_compute = Arc::clone(&cache_previous);
    let preflight_skip_probe = Arc::new(preflight_skip_probe);
    let snapshot_admission = Arc::new(crate::js_resolution::ProjectSnapshotAdmission::new(
        snapshot_budget,
    ));
    let output_admission_for_compute = Arc::clone(&output_admission);
    let parser_admission_for_compute = Arc::clone(&parser_admission);
    let cache_client_for_compute = cache_client.clone();
    let compute_cancellation = cancellation.clone();
    let extraction_progress_for_compute = extraction_progress_counter.clone();
    let completed = read_files_concurrently_with_cancellation_and_telemetry(
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
        let _progress_completion = ProjectExtractionCompletion::new(
            extraction_progress_for_compute.clone(),
            context.indexed,
        );
        let path = &context.path;
        let indexed = context.indexed;
        anyhow::ensure!(
            detect::classify_admitted_source(std::path::Path::new(&relative), input.bytes())
                .is_some_and(|kind| kind.as_str() == context.source_kind.as_str()),
            "source classification changed after discovery for {}; retry the extraction",
            bounded_diagnostic_source_path(&relative)
        );
        let mtime = input
            .file_identity
            .modified
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .unwrap_or_default()
            .as_secs_f64();
        let snapshot_required = crate::js_resolution::ProjectSnapshot::needs_admitted_file(
            &relative,
            input.bytes(),
        );
        if snapshot_required
            && !snapshot_admission.try_reserve(
                crate::js_resolution::ProjectSnapshot::admission_bytes(
                    &relative,
                    input.retained_capacity_bytes(),
                ),
            )
        {
            anyhow::bail!(
                "isolated project resolution snapshot exhausted its {snapshot_budget}-byte cap within the effective {}-byte managed-memory budget; retry with a larger --memory-budget-bytes value",
                config.memory_budget_bytes
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
        let cache_previous_entry = cache_previous_for_compute.get(relative.as_str());
        let cache_policy_changed = evidence.as_ref().is_some_and(|evidence| {
            cache_previous_entry
                .and_then(|entry| entry.runtime_cache)
                .is_none_or(|stored| stored.artifact_key != evidence.key.as_bytes())
        });
        let preflight_requires_repair = preflight_skip_probe.contains_key(relative.as_str());
        let changed = indexed
            && (force
                || (context.source_kind == detect::FileType::Video.as_str()
                    && is_ambiguous_typescript_extension(&relative))
                || !committed_baseline_eligible
                || previous_entry.is_none_or(|entry| {
                    entry.ast_version != cache::AST_CACHE_VERSION
                        || entry.ast_hash != hash.as_deref().unwrap_or_default()
                        || entry.source_kind.as_deref() != Some(context.source_kind.as_str())
                })
                || (resolver_dependencies_invalidated
                    && crate::js_resolution::is_javascript_source(&relative))
                || cache_policy_changed
                || preflight_requires_repair);
        let candidate_runtime_manifest = (!force).then(|| {
            evidence.as_ref().and_then(|evidence| {
                source_identity.map(|identity| cache::RuntimeAstManifestEvidence {
                    content_digest: evidence.content_digest,
                    source_identity_digest: identity.digest(),
                    artifact_key: evidence.key.as_bytes(),
                })
            })
        }).flatten();
        // Authorization is committed only when it was already present on an
        // unchanged row, a cache hit is validated below, or fresh persistence
        // succeeds. Merely computing the deterministic key must never bless a
        // stale artifact after startup or store failure.
        let mut runtime_manifest = if !force
            && !changed
            && cache_previous_entry
                .and_then(|entry| entry.runtime_cache)
                .is_some()
        {
            candidate_runtime_manifest
        } else {
            None
        };
        let mut row_cache = if cache_client_for_compute.is_some() {
            cache::RuntimeCacheTelemetry::enabled()
        } else {
            cache::RuntimeCacheTelemetry::default()
        };
        let mut row_cache_diagnostics = Vec::new();
        let mut extraction = None;
        let mut warning = None;
        let mut parses = 0_u64;
        let mut should_persist = false;
        let mut replace_existing = false;

        if changed {
            if force || evidence.is_none() {
                if force && cache_eligible {
                    // Force disables the cache service entirely, but an
                    // eligible source was still bypassed by explicit policy.
                    // Report that decision even though there was no client
                    // from which to attempt a read.
                    row_cache.bypasses = row_cache.bypasses.saturating_add(1);
                } else if cache_client_for_compute.is_some() {
                    row_cache.bypasses = row_cache.bypasses.saturating_add(1);
                }
                should_persist = evidence.is_some() && cache_client_for_compute.is_some();
            } else if cache_client_for_compute.is_none() {
                // Startup/protocol failure is recorded once at the control
                // plane. It is not a per-file policy bypass.
            } else if cache_previous_entry.is_some_and(|entry| {
                entry.runtime_cache.is_none()
                    || entry.source_kind.as_deref() != Some(context.source_kind.as_str())
            }) {
                // A committed manifest without cache authorization requires a
                // fresh parse. Do not probe a same-key artifact left by an
                // interrupted or forced run; replace it only after parsing.
                row_cache.misses = row_cache.misses.saturating_add(1);
                replace_existing = true;
                should_persist = true;
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
                                    runtime_manifest = candidate_runtime_manifest;
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
                        "isolated parser requires a {parser_allowance_bytes}-byte allowance beyond its {parser_pool_bytes}-byte pool cap within the effective {}-byte managed-memory budget; retry with a larger --memory-budget-bytes value",
                        config.memory_budget_bytes
                    );
                };
                parses = parses.saturating_add(1);
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
                            return Err(retained_output_budget_error(
                                config.memory_budget_bytes,
                                output_budget,
                            ));
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
                    Ok((outcome, _payload_bytes)) => {
                        row_cache.record_persist(outcome);
                        runtime_manifest = candidate_runtime_manifest;
                    }
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
            parses,
        })
    },
    )
    .map_err(|error| anyhow::anyhow!("isolated extraction runtime failed: {error:?}"))?;

    let mut runtime_io = completed.telemetry;
    let completed = completed.result;
    if let Some(progress) = extraction_progress_counter.as_ref() {
        let indexed_failures = completed
            .failures
            .iter()
            .filter(|failure| {
                contexts
                    .get(failure.identity.normalized_path.as_ref())
                    .is_some_and(|context| context.indexed)
            })
            .count();
        progress.complete_many(indexed_failures);
    }
    runtime_io.sources_selected = selected_sources;
    runtime_io.source_bytes_selected = source_bytes_selected;
    runtime_io.sources_read = runtime_io
        .sources_read
        .saturating_add(streaming_media_sources_read);
    runtime_io.source_bytes_read = runtime_io
        .source_bytes_read
        .saturating_add(streaming_media_source_bytes_read);
    runtime_io.source_bytes_avoided = source_bytes_avoided;
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
    let mut runtime_media_generation_mismatches = std::collections::BTreeSet::new();
    for relative in &ambiguous_media_sources {
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        let Ok(row_index) =
            rows.binary_search_by(|row| row.relative.as_str().cmp(relative.as_str()))
        else {
            runtime_media_generation_mismatches.insert(relative.clone());
            continue;
        };
        let row = &rows[row_index];
        let logical = resolved_root.join(relative);
        let physical = detection.physical_source(&logical);
        let verified = detect::checked_mpeg_transport_stream_evidence_with_cancellation(
            &logical,
            &physical,
            &cancellation,
        )
        .is_ok_and(|evidence| {
            evidence.is_some_and(|evidence| {
                evidence.mtime == row.mtime && evidence.ast_hash == row.hash
            })
        });
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "isolated extraction cancelled"
        );
        if !verified {
            runtime_media_generation_mismatches.insert(relative.clone());
        }
    }
    let successfully_admitted_sources = rows
        .iter()
        .filter(|row| row.warning.is_none())
        .map(|row| row.relative.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let ownership_prune_verification_candidates = source_kind_transitions
        .iter()
        .chain(ambiguous_media_sources.iter())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let unverified_source_kind_transitions = ownership_prune_verification_candidates
        .iter()
        .filter(|relative| {
            !successfully_admitted_sources.contains(relative.as_str())
                || runtime_media_generation_mismatches.contains(*relative)
        })
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let verified_representation_keys = ownership_prune_verification_candidates
        .iter()
        .filter(|relative| successfully_admitted_sources.contains(relative.as_str()))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut changed_media_keys = std::collections::BTreeSet::new();
    if !code_only {
        for relative in &ambiguous_media_sources {
            anyhow::ensure!(
                !cancellation.is_cancelled(),
                "isolated extraction cancelled"
            );
            let Ok(row_index) =
                rows.binary_search_by(|row| row.relative.as_str().cmp(relative.as_str()))
            else {
                continue;
            };
            let row = &rows[row_index];
            if previous.get(relative).is_none_or(|entry| {
                entry.ast_version != cache::AST_CACHE_VERSION
                    || entry.ast_hash != row.hash
                    || entry.source_kind.as_deref() != Some(detect::FileType::Video.as_str())
            }) {
                changed_media_keys.insert(relative.clone());
            }
        }
    }
    let authoritative_ownership_prune_keys = source_kind_transitions
        .iter()
        .chain(changed_media_keys.iter())
        .filter(|relative| verified_representation_keys.contains(*relative))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    // Verification and destructive authority are intentionally distinct. A
    // byte-identical media row is valid evidence for baseline conflict repair,
    // but must not erase stable inventory or semantic enrichment by itself.
    let mut verified_representation_sources = verified_representation_keys
        .iter()
        .map(|relative| detection.physical_source(&resolved_root.join(relative)))
        .collect::<Vec<_>>();
    verified_representation_sources.sort();
    verified_representation_sources.dedup();
    let mut ownership_prune_sources = authoritative_ownership_prune_keys
        .iter()
        .map(|relative| detection.physical_source(&resolved_root.join(relative)))
        .collect::<Vec<_>>();
    ownership_prune_sources.sort();
    ownership_prune_sources.dedup();
    for row in &rows {
        runtime_cache.merge(row.runtime_cache);
        runtime_cache_diagnostics.extend(row.runtime_cache_diagnostics.iter().cloned());
    }
    let runtime_cache_io = if let Some(client) = cache_client.as_ref() {
        collect_runtime_cache_io_telemetry(require_telemetry, || {
            client.telemetry_snapshot_with_cancellation(&cancellation)
        })?
    } else {
        graphoxide_index_runtime::cache::RuntimeCacheIoTelemetry::default()
    };
    let runtime_work = RuntimeWorkTelemetry {
        parses: rows
            .iter()
            .fold(0_u64, |total, row| total.saturating_add(row.parses)),
    };
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
    if !unverified_source_kind_transitions.is_empty() {
        return Err(unverified_source_kind_transition_error(
            &unverified_source_kind_transitions,
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
                        "isolated project resolution snapshot exhausted its {byte_limit}-byte cap within the effective {}-byte managed-memory budget; retry with a larger --memory-budget-bytes value",
                        config.memory_budget_bytes
                    )
                }
                crate::js_resolution::ProjectSnapshotError::InvalidPath(path) => {
                    anyhow::anyhow!("invalid project snapshot path: {path}")
                }
            })?;
    }
    let mut pending_manifest_charge = rows
        .iter()
        .filter(|row| row.indexed && row.warning.is_none())
        .fold(0usize, |total, row| {
            let source_kind = contexts
                .get(row.relative.as_str())
                .map(|context| context.source_kind.as_str());
            let semantic_hash = previous
                .get(&row.relative)
                .filter(|entry| {
                    entry.ast_version == cache::AST_CACHE_VERSION
                        && entry.ast_hash == row.hash
                        && entry.source_kind.as_deref() == source_kind
                })
                .map_or("", |entry| entry.semantic_hash.as_str());
            total.saturating_add(pending_manifest_entry_retained_charge(
                &row.relative,
                &row.hash,
                semantic_hash,
                source_kind,
            ))
        });
    if code_only {
        for paths in detection
            .files
            .iter()
            .filter(|(kind, _)| kind.as_str() != detect::FileType::Code.as_str())
            .map(|(_, paths)| paths)
        {
            for path in paths {
                let key = normalized_project_key(std::path::Path::new(path), &resolved_root, root);
                if let Some(entry) = previous
                    .get(&key)
                    .filter(|_| !authoritative_ownership_prune_keys.contains(&key))
                {
                    // Over-counting a normalization collision is intentional:
                    // this is a pre-allocation proof, not a usage statistic.
                    pending_manifest_charge = pending_manifest_charge.saturating_add(
                        pending_manifest_entry_retained_charge(
                            &key,
                            &entry.ast_hash,
                            &entry.semantic_hash,
                            entry.source_kind.as_deref(),
                        ),
                    );
                }
            }
        }
    }
    if pending_manifest_charge > pending_manifest_reservation {
        return Err(pending_manifest_budget_error(
            config.memory_budget_bytes,
            pending_manifest_reservation,
            pending_manifest_charge,
        ));
    }

    let mut manifest = rows
        .iter()
        .filter(|row| row.indexed && row.warning.is_none())
        .map(|row| {
            let source_kind = contexts
                .get(row.relative.as_str())
                .map(|context| context.source_kind.clone())
                .expect("indexed runtime row has a detected source kind");
            let semantic_hash = previous
                .get(&row.relative)
                .filter(|entry| {
                    entry.ast_version == cache::AST_CACHE_VERSION
                        && entry.ast_hash == row.hash
                        && entry.source_kind.as_deref() == Some(source_kind.as_str())
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
                    source_kind: Some(source_kind),
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
                if let Some(entry) = previous
                    .get(&key)
                    .filter(|_| !authoritative_ownership_prune_keys.contains(&key))
                {
                    manifest.entry(key).or_insert_with(|| {
                        let mut carried = entry.clone();
                        if force {
                            // A forced scan is a project-wide trust reset even
                            // when code-only policy carries non-code ownership.
                            // Do not let a later full scan replay an artifact
                            // that this build deliberately did not validate.
                            carried.runtime_cache = None;
                        }
                        carried
                    });
                }
            }
        }
    }
    let pending_manifest_retained_bytes = pending_manifest_retained_charge(&manifest);
    debug_assert!(pending_manifest_retained_bytes <= pending_manifest_reservation);
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
        .filter(|source| !detected_kinds.contains_key(source.as_str()))
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
                    "isolated fresh extraction output exhausted its {output_budget}-byte output cap within the effective {}-byte managed-memory budget before resolver baseline admission; retry with a larger --memory-budget-bytes value",
                    config.memory_budget_bytes
                )
            })?;
        let graph_byte_cap = remaining / RESOLUTION_BASELINE_WORKING_SET_MULTIPLIER;
        let context = load_resolver_baseline_context(
            &baseline_graph_path,
            u64::try_from(graph_byte_cap).unwrap_or(u64::MAX),
            &eligible_resolver_owners,
            &resolved_root,
            root,
        )
        .with_context(|| {
            format!(
                "admit the resolver baseline within the {output_budget}-byte output cap and effective {}-byte managed-memory budget; retry with a larger --memory-budget-bytes value",
                config.memory_budget_bytes
            )
        })?;
        if !context.extractions.is_empty() {
            anyhow::ensure!(
                context.working_set_charge <= remaining,
                "resolver baseline requires a {}-byte graph working set, exceeding {remaining} remaining bytes of the {output_budget}-byte output cap within the effective {}-byte managed-memory budget; retry with a larger --memory-budget-bytes value",
                context.working_set_charge,
                config.memory_budget_bytes
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
        "isolated resolver retains {retained_output_bytes} fresh bytes plus a {baseline_working_set_charge}-byte baseline working-set charge, exceeding its {output_budget}-byte output cap within the effective {}-byte managed-memory budget; retry with a larger --memory-budget-bytes value",
        config.memory_budget_bytes
    );
    debug_assert!(output_admission.retained_bytes() <= output_budget);
    extraction_progress.finish();
    Ok(DeferredProjectExtractionInternal {
        extraction: DeferredProjectExtractionWithTelemetry {
            result: DeferredProjectExtractionResult {
                extractions,
                retained_output_bytes,
                pending_manifest_retained_bytes,
                detection,
                progress: ProjectExtractionProgress {
                    total: total_work,
                    succeeded,
                },
                warnings,
                rebuilt_sources,
                verified_representation_sources,
                ownership_prune_sources,
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
            },
            telemetry: RuntimeExtractionTelemetry {
                io: runtime_io,
                work: runtime_work,
                cache_io: runtime_cache_io,
            },
        },
        indexed_source_bytes,
        incremental_baseline_eligible: committed_baseline_eligible,
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
    extract_project_with_scan_options_deferred_manifest_impl(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
        None,
    )
}

/// Legacy extraction with source-safe aggregate phase observations.
pub fn extract_project_with_scan_options_deferred_manifest_with_progress(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    progress: ProjectExtractionProgressObserver,
) -> anyhow::Result<DeferredProjectExtractionResult> {
    extract_project_with_scan_options_deferred_manifest_impl_with_hooks(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
        LegacyExtractionHooks {
            progress: Some(progress),
            ..LegacyExtractionHooks::default()
        },
    )
}

fn extract_project_with_scan_options_deferred_manifest_impl(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    after_extraction_hook: Option<DetectionTestHook<'_>>,
) -> anyhow::Result<DeferredProjectExtractionResult> {
    extract_project_with_scan_options_deferred_manifest_impl_with_hooks(
        root,
        force,
        managed_output_dir,
        code_only,
        detect_options,
        LegacyExtractionHooks {
            after_extraction: after_extraction_hook,
            ..LegacyExtractionHooks::default()
        },
    )
}

fn extract_project_with_scan_options_deferred_manifest_impl_with_hooks(
    root: &std::path::Path,
    force: bool,
    managed_output_dir: &std::path::Path,
    code_only: bool,
    detect_options: &detect::DetectOptions,
    hooks: LegacyExtractionHooks<'_>,
) -> anyhow::Result<DeferredProjectExtractionResult> {
    use rayon::prelude::*;
    let LegacyExtractionHooks {
        mut before_extraction,
        mut after_extraction,
        progress: progress_observer,
    } = hooks;
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
    if let Some(observer) = progress_observer.as_ref() {
        observer(0, files.len());
    }
    let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let detected_kinds = detected_source_kinds(&detection, &resolved_root, root)?;
    if let Some(hook) = before_extraction.as_mut() {
        hook(&detection)?;
    }
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
    let unverified_row_classifications = rows
        .iter()
        .filter(|(relative, extraction, _, _)| {
            !admitted_mpeg_classification_matches_extraction(
                relative,
                detected_kinds.get(relative).map(String::as_str),
                extraction,
            )
        })
        .map(|(relative, _, _, _)| relative.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if !unverified_row_classifications.is_empty() {
        return Err(unverified_source_kind_transition_error(
            &unverified_row_classifications,
        ));
    }
    let succeeded = rows.len();
    let mut rebuilt_sources = rows
        .iter()
        .map(|(relative, _, _, _)| {
            let logical = resolved_root.join(relative);
            detection.physical_source(&logical)
        })
        .collect::<Vec<_>>();
    let (loaded_previous, committed_manifest_is_trusted) =
        cache::load_manifest_from_output_with_trust(&managed_output_dir);
    let previous = normalized_previous_manifest(&loaded_previous, &resolved_root, root);
    ensure_code_only_ambiguous_media_has_trusted_manifest(
        force,
        code_only,
        managed_output_dir.join("graph.json").is_file(),
        committed_manifest_is_trusted,
        &detected_kinds,
    )?;
    let source_kind_transition_keys = detected_kinds
        .iter()
        .filter_map(|(relative, current_kind)| {
            previous.get(relative).and_then(|entry| {
                source_kind_transition_affects_code(entry, current_kind, relative)
                    .then(|| relative.clone())
            })
        })
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(hook) = after_extraction.as_mut() {
        hook(&detection)?;
    }
    let current_mpeg_keys = detected_kinds
        .iter()
        .filter(|(relative, kind)| {
            kind.as_str() == detect::FileType::Video.as_str()
                && is_ambiguous_typescript_extension(relative)
        })
        .map(|(relative, _)| relative.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let final_verification_keys = current_mpeg_keys
        .union(&source_kind_transition_keys)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut verified_mpeg_keys = std::collections::BTreeSet::new();
    let mut verified_transition_keys = std::collections::BTreeSet::new();
    let mut verified_ambiguous_evidence = std::collections::BTreeMap::new();
    let mut unverified_media_generations = std::collections::BTreeSet::new();
    let extracted_row_evidence = rows
        .iter()
        .map(|(relative, _, mtime, hash)| (relative.as_str(), (*mtime, hash.as_str())))
        .collect::<std::collections::BTreeMap<_, _>>();
    for key in &final_verification_keys {
        let logical = resolved_root.join(key);
        let physical = detection.physical_source(&logical);
        let current_kind = detected_kinds
            .get(key)
            .expect("final classification candidate has a detected kind");
        let row_evidence = extracted_row_evidence.get(key.as_str());
        // A full project scan may authorize a representation transition only
        // from a successfully extracted row bound to this exact generation.
        // Code-only policy intentionally has no row for excluded media, so a
        // checked final reopen remains its explicit verification path.
        let row_is_verified =
            row_evidence.is_some() || code_only && current_kind != detect::FileType::Code.as_str();
        let ambiguous_evidence = is_ambiguous_typescript_extension(key)
            .then(|| detect::checked_ambiguous_source_evidence(&logical, &physical).ok())
            .flatten();
        let verified = if is_ambiguous_typescript_extension(key) {
            ambiguous_evidence.as_ref().is_some_and(|evidence| {
                evidence.kind.as_str() == current_kind
                    && row_is_verified
                    && row_evidence.is_none_or(|(mtime, hash)| {
                        evidence.mtime == *mtime && evidence.ast_hash == *hash
                    })
            })
        } else {
            row_is_verified
                && detect::classify_file_at(&logical, &physical)
                    .is_some_and(|kind| kind.as_str() == current_kind)
        };
        if !verified {
            unverified_media_generations.insert(key.clone());
            continue;
        }
        if let Some(evidence) = ambiguous_evidence {
            verified_ambiguous_evidence.insert(key.clone(), evidence);
        }
        if current_mpeg_keys.contains(key) {
            verified_mpeg_keys.insert(key.clone());
        }
        if source_kind_transition_keys.contains(key) {
            verified_transition_keys.insert(key.clone());
        }
    }
    if !unverified_media_generations.is_empty() {
        return Err(unverified_source_kind_transition_error(
            &unverified_media_generations,
        ));
    }
    let mut manifest: cache::Manifest = rows
        .iter()
        .map(|(relative, _, mtime, hash)| {
            let source_kind = detected_kinds
                .get(relative)
                .expect("extracted source has a detected kind");
            let semantic_hash = previous
                .get(relative)
                .filter(|entry| {
                    entry.ast_version == cache::AST_CACHE_VERSION
                        && entry.ast_hash == *hash
                        && entry.source_kind.as_deref() == Some(source_kind.as_str())
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
                    source_kind: Some(source_kind.clone()),
                    runtime_cache: None,
                },
            )
        })
        .collect();
    let successfully_extracted_keys = rows
        .iter()
        .map(|(relative, _, _, _)| relative.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let changed_mpeg_keys = if code_only {
        std::collections::BTreeSet::new()
    } else {
        verified_mpeg_keys
            .iter()
            .filter(|key| {
                let evidence = verified_ambiguous_evidence
                    .get(*key)
                    .expect("verified MPEG key has exact evidence");
                previous.get(*key).is_none_or(|entry| {
                    entry.ast_version != cache::AST_CACHE_VERSION
                        || entry.ast_hash != evidence.ast_hash
                        || entry.source_kind.as_deref() != Some(detect::FileType::Video.as_str())
                })
            })
            .cloned()
            .collect()
    };
    let verified_representation_keys = verified_mpeg_keys
        .union(&verified_transition_keys)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let ownership_prune_keys = verified_transition_keys
        .union(&changed_mpeg_keys)
        .filter(|key| {
            code_only
                && detected_kinds
                    .get(*key)
                    .is_some_and(|kind| kind != detect::FileType::Code.as_str())
                || successfully_extracted_keys.contains(*key)
        })
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut verified_representation_sources = verified_representation_keys
        .iter()
        .map(|key| detection.physical_source(&resolved_root.join(key)))
        .collect::<Vec<_>>();
    verified_representation_sources.sort();
    verified_representation_sources.dedup();
    let mut ownership_prune_sources = ownership_prune_keys
        .iter()
        .map(|key| detection.physical_source(&resolved_root.join(key)))
        .collect::<Vec<_>>();
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
                if !ownership_prune_keys.contains(&key)
                    && let Some(entry) = previous.get(&key)
                {
                    manifest.entry(key).or_insert_with(|| {
                        let mut carried = entry.clone();
                        if force {
                            // Legacy force must publish the same cache trust
                            // boundary as the runtime executor.
                            carried.runtime_cache = None;
                        }
                        carried
                    });
                }
            }
        }
    }
    rebuilt_sources.sort();
    rebuilt_sources.dedup();
    ownership_prune_sources.sort();
    ownership_prune_sources.dedup();
    let mut resolver_invalidated_keys = ownership_prune_keys
        .iter()
        .filter(|key| {
            detected_kinds
                .get(*key)
                .is_some_and(|kind| kind != detect::FileType::Code.as_str())
        })
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    resolver_invalidated_keys.extend(
        rows.iter()
            .filter(|(relative, extraction, _, _)| {
                extraction_confirms_mpeg_transport_stream(extraction, relative)
            })
            .map(|(relative, _, _, _)| relative.clone()),
    );
    resolver_invalidated_keys.extend(verified_mpeg_keys);
    let mut extractions: Vec<_> = rows
        .into_iter()
        .map(|(_, extraction, _, _)| extraction)
        .collect();
    resolution::resolve_with_root(&mut extractions, &resolved_root);
    crate::js_resolution::invalidate_resolved_targets_for_sources(
        &mut extractions,
        &resolver_invalidated_keys,
    );
    let retained_output_bytes = extractions_retained_bytes(&extractions)?;
    let pending_manifest_retained_bytes = pending_manifest_retained_charge(&manifest);
    if let Some(observer) = progress_observer.as_ref() {
        observer(files.len(), files.len());
    }
    Ok(DeferredProjectExtractionResult {
        extractions,
        retained_output_bytes,
        pending_manifest_retained_bytes,
        detection,
        progress: ProjectExtractionProgress {
            total: total_work,
            succeeded,
        },
        warnings,
        rebuilt_sources,
        verified_representation_sources,
        ownership_prune_sources,
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
    /// Detector buckets bound to the same admitted bytes as each successful
    /// extraction, keyed by canonical explicit source identity.
    pub admitted_source_kinds: std::collections::BTreeMap<std::path::PathBuf, String>,
    /// Successfully byte-verified candidates whose carried baseline ownership
    /// may need a reset across structural and semantic tiers.
    ///
    /// This may overlap the caller's rebuilt-source set. Baseline-merging
    /// callers with a committed graph must first gate it on a structural
    /// representation mismatch, then pass confirmed resets through an
    /// unsuppressed ownership-reset channel, never fold them into ordinary
    /// deletion pruning. Current MPEG transport streams and proven
    /// code-affecting classification transitions are the only explicit-file
    /// rows nominated here.
    pub ownership_reset_sources: Vec<std::path::PathBuf>,
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

    /// Consume the deferred wrapper while retaining its exact held-generation
    /// entries for a caller-owned full-corpus manifest merge.
    pub fn into_uncommitted_parts(self) -> (ExtractFilesResult, cache::Manifest) {
        (self.result, self.pending_manifest.entries)
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
    let cache_base = cache_root.map(std::path::Path::to_path_buf).unwrap_or(
        std::env::current_dir()
            .map_err(|error| anyhow::anyhow!("resolve current directory: {error}"))?,
    );
    let managed_output_dir = cache_base.join("graphoxide-out");
    extract_files_with_deferred_manifest_and_output_impl(
        files,
        cache_root,
        &managed_output_dir,
        force,
        ExplicitFileExtractionOptions {
            admitted_previous: None,
            manifest_retained_limit: None,
            bounded_builtin_mpeg_inventory: true,
        },
        |path, relative, bytes| {
            engine::extract_as_admitted_bytes_with_path_probes(path, relative, bytes)
        },
    )
}

/// Extract an explicit file set without publishing its replacement manifest,
/// storing cache and manifest state in an explicit managed output directory.
/// `cache_root` retains its existing source-identity-anchor/fallback semantics
/// and no longer selects the storage location.
pub fn extract_files_deferred_manifest_with_output(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    managed_output_dir: &std::path::Path,
    force: bool,
) -> anyhow::Result<DeferredExtractFilesResult> {
    extract_files_with_deferred_manifest_and_output_impl(
        files,
        cache_root,
        managed_output_dir,
        force,
        ExplicitFileExtractionOptions {
            admitted_previous: None,
            manifest_retained_limit: None,
            bounded_builtin_mpeg_inventory: true,
        },
        |path, relative, bytes| {
            engine::extract_as_admitted_bytes_with_path_probes(path, relative, bytes)
        },
    )
}

/// Extract an explicit file set using one already-admitted committed
/// manifest. Watch coordinators use this entrypoint so incremental selection,
/// cache ownership, and the prepared replacement manifest all share the same
/// bounded manifest generation without reopening `manifest.json`.
pub fn extract_files_deferred_manifest_with_output_and_previous(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    managed_output_dir: &std::path::Path,
    force: bool,
    previous: &cache::Manifest,
    manifest_retained_limit: usize,
) -> anyhow::Result<DeferredExtractFilesResult> {
    extract_files_with_deferred_manifest_and_output_impl(
        files,
        cache_root,
        managed_output_dir,
        force,
        ExplicitFileExtractionOptions {
            admitted_previous: Some(previous),
            manifest_retained_limit: Some(manifest_retained_limit),
            bounded_builtin_mpeg_inventory: true,
        },
        |path, relative, bytes| {
            engine::extract_as_admitted_bytes_with_path_probes(path, relative, bytes)
        },
    )
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
    let cache_base = cache_root.map(std::path::Path::to_path_buf).unwrap_or(
        std::env::current_dir()
            .map_err(|error| anyhow::anyhow!("resolve current directory: {error}"))?,
    );
    let managed_output_dir = cache_base.join("graphoxide-out");
    extract_files_with_deferred_manifest_and_output(
        files,
        cache_root,
        &managed_output_dir,
        force,
        extractor,
    )
}

/// Injectable deferred-manifest variant with independent source-identity and
/// managed-output roots.
pub fn extract_files_with_deferred_manifest_and_output<F>(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    managed_output_dir: &std::path::Path,
    force: bool,
    extractor: F,
) -> anyhow::Result<DeferredExtractFilesResult>
where
    F: Fn(&std::path::Path, &str) -> anyhow::Result<graphoxide_core::Extraction>,
{
    extract_files_with_deferred_manifest_and_output_impl(
        files,
        cache_root,
        managed_output_dir,
        force,
        ExplicitFileExtractionOptions {
            admitted_previous: None,
            manifest_retained_limit: None,
            bounded_builtin_mpeg_inventory: false,
        },
        |path, relative, _bytes| extractor(path, relative),
    )
}

struct ExplicitFileExtractionOptions<'a> {
    admitted_previous: Option<&'a cache::Manifest>,
    manifest_retained_limit: Option<usize>,
    bounded_builtin_mpeg_inventory: bool,
}

fn extract_files_with_deferred_manifest_and_output_impl<F>(
    files: &[std::path::PathBuf],
    cache_root: Option<&std::path::Path>,
    managed_output_dir: &std::path::Path,
    force: bool,
    options: ExplicitFileExtractionOptions<'_>,
    extractor: F,
) -> anyhow::Result<DeferredExtractFilesResult>
where
    F: Fn(&std::path::Path, &str, &[u8]) -> anyhow::Result<graphoxide_core::Extraction>,
{
    use md5::Digest as _;
    let ExplicitFileExtractionOptions {
        admitted_previous,
        manifest_retained_limit,
        bounded_builtin_mpeg_inventory,
    } = options;
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
    let loaded_previous;
    let previous = if let Some(previous) = admitted_previous {
        previous
    } else {
        loaded_previous = cache::load_manifest_from_output(managed_output_dir);
        &loaded_previous
    };
    let previous_manifest_retained = pending_manifest_retained_charge(previous);
    if let Some(limit) = manifest_retained_limit
        && previous_manifest_retained > limit
    {
        return Err(ManifestRetainedLimitError {
            limit,
            pending: false,
        }
        .into());
    }
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
        if bounded_builtin_mpeg_inventory {
            match detect::checked_mpeg_transport_stream_evidence(
                std::path::Path::new(&relative),
                &path,
            ) {
                Ok(Some(evidence)) => {
                    rows.push((
                        relative.clone(),
                        engine::mpeg_transport_stream_inventory(
                            &path,
                            &relative,
                            evidence.byte_length,
                        ),
                        evidence.mtime,
                        evidence.ast_hash,
                        Some(detect::FileType::Video.as_str().to_owned()),
                    ));
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    let warning = format!("skipped {relative}: {error}");
                    tracing::warn!("{warning}");
                    warnings.push(warning);
                    failures.push(
                        anyhow::Error::new(error)
                            .context(format!("inspect extension-ambiguous source {relative}")),
                    );
                    skipped.push(original.clone());
                    continue;
                }
            }
        }
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
        let source_kind = detect::classify_admitted_source(std::path::Path::new(&relative), &bytes)
            .map(|kind| kind.as_str().to_owned());
        let cached = (!force)
            .then(|| cache::ast_cache_get_from_output(managed_output_dir, &relative, &bytes))
            .flatten();
        let extraction = if let Some(cached) = cached {
            cached
        } else {
            let extracted = match extractor(&path, &relative, &bytes)
                .with_context(|| format!("extract {relative}"))
            {
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
                if source_kind.as_deref() == Some(detect::FileType::Code.as_str())
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
                cache::ast_cache_put_to_output(managed_output_dir, &relative, &bytes, &extracted)
            {
                tracing::warn!("{relative}: caching the extraction failed: {error:#}");
            }
            extracted
        };
        if !admitted_mpeg_classification_matches_extraction(
            &relative,
            source_kind.as_deref(),
            &extraction,
        ) {
            return Err(unverified_source_kind_transition_error(
                &std::collections::BTreeSet::from([relative]),
            ));
        }
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
        rows.push((relative, extraction, mtime, hash, source_kind));
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
    let unverified_ambiguous_rows = rows
        .iter()
        .filter_map(|(relative, _, mtime, hash, source_kind)| {
            let expected = source_kind.as_deref()?;
            if !is_ambiguous_typescript_extension(relative) {
                return None;
            }
            let physical = key_root.join(relative);
            (!detect::checked_ambiguous_source_evidence(std::path::Path::new(relative), &physical)
                .is_ok_and(|actual| {
                    actual.kind.as_str() == expected
                        && (hash.is_empty() || actual.ast_hash == *hash)
                        && actual.mtime == *mtime
                }))
            .then(|| relative.clone())
        })
        .collect::<std::collections::BTreeSet<_>>();
    if !unverified_ambiguous_rows.is_empty() {
        return Err(unverified_source_kind_transition_error(
            &unverified_ambiguous_rows,
        ));
    }
    let mut manifest = cache::Manifest::new();
    let mut exact_manifest_retained = 0usize;
    for (relative, _, mtime, hash, source_kind) in &rows {
        let semantic_hash = previous
            .get(relative)
            .filter(|entry| {
                entry.ast_version == cache::AST_CACHE_VERSION
                    && entry.ast_hash == *hash
                    && !hash.is_empty()
                    && entry.source_kind.as_deref() == source_kind.as_deref()
            })
            .map(|entry| entry.semantic_hash.as_str())
            .unwrap_or_default();
        let entry_charge = pending_manifest_entry_retained_charge(
            relative,
            hash,
            semantic_hash,
            source_kind.as_deref(),
        );
        let required = previous_manifest_retained
            .saturating_add(exact_manifest_retained)
            .saturating_add(entry_charge);
        if let Some(limit) = manifest_retained_limit
            && required > limit
        {
            return Err(ManifestRetainedLimitError {
                limit,
                pending: true,
            }
            .into());
        }
        exact_manifest_retained = exact_manifest_retained.saturating_add(entry_charge);
        manifest.insert(
            relative.clone(),
            cache::ManifestEntry {
                mtime: *mtime,
                ast_version: cache::AST_CACHE_VERSION,
                ast_hash: hash.clone(),
                semantic_hash: semantic_hash.to_owned(),
                source_kind: source_kind.clone(),
                runtime_cache: None,
            },
        );
    }
    let resolver_invalidated_keys = rows
        .iter()
        .filter(|(relative, extraction, _, _, source_kind)| {
            source_kind.as_deref() == Some(detect::FileType::Video.as_str())
                && extraction_confirms_mpeg_transport_stream(extraction, relative)
        })
        .map(|(relative, _, _, _, _)| relative.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let mut ownership_reset_sources = rows
        .iter()
        .filter(|(relative, extraction, _, _, source_kind)| {
            let current_mpeg = source_kind.as_deref() == Some(detect::FileType::Video.as_str())
                && extraction_confirms_mpeg_transport_stream(extraction, relative);
            let proven_transition = source_kind.as_deref().is_some_and(|current_kind| {
                previous.get(relative).is_some_and(|entry| {
                    source_kind_transition_affects_code(entry, current_kind, relative)
                })
            });
            current_mpeg || proven_transition
        })
        .map(|(relative, _, _, _, _)| key_root.join(relative))
        .collect::<Vec<_>>();
    ownership_reset_sources.sort();
    ownership_reset_sources.dedup();
    let admitted_source_kinds = rows
        .iter()
        .filter_map(|(relative, _, _, _, source_kind)| {
            source_kind
                .as_ref()
                .map(|kind| (key_root.join(relative), kind.clone()))
        })
        .collect();
    let mut extractions = rows
        .into_iter()
        .map(|(_, extraction, _, _, _)| extraction)
        .collect::<Vec<_>>();
    resolution::resolve_with_root(&mut extractions, &key_root);
    crate::js_resolution::invalidate_resolved_targets_for_sources(
        &mut extractions,
        &resolver_invalidated_keys,
    );
    Ok(DeferredExtractFilesResult {
        result: ExtractFilesResult {
            extractions,
            warnings,
            skipped,
            admitted_source_kinds,
            ownership_reset_sources,
            key_root,
            managed_output_dir: managed_output_dir.to_path_buf(),
        },
        pending_manifest: PendingProjectManifest {
            output_directory: managed_output_dir.to_path_buf(),
            entries: manifest,
        },
    })
}

#[cfg(test)]
mod tests {
    use anyhow::Context as _;
    use graphoxide_core::{make_id, Edge, Extraction};
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    #[test]
    fn xml_attribute_decode_preserves_whitespace_and_entities() {
        let decoded = super::decode_xml_attribute(b"literal\tline\ncarriage\r &amp; &#x2026;")
            .expect("decode safe XML attribute");
        assert_eq!(decoded, "literal\tline\ncarriage\r & …");
        assert!(matches!(
            super::decode_xml_attribute(b"&custom;"),
            Err(quick_xml::Error::Escape(
                quick_xml::escape::EscapeError::UnrecognizedEntity(_, _)
            ))
        ));
    }

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn central_progress_monitor_is_live_monotonic_and_rate_bounded() {
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_for_callback = std::sync::Arc::clone(&observed);
        let observer: super::ProjectExtractionProgressObserver =
            std::sync::Arc::new(move |processed, total| {
                observed_for_callback
                    .lock()
                    .expect("progress observations")
                    .push((std::time::Instant::now(), processed, total));
            });
        let monitor = super::ProjectExtractionProgressMonitor::start(12, Some(observer));
        let counter = monitor.counter().expect("live progress counter");
        for _ in 0..12 {
            counter.complete_one();
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        monitor.finish();

        let observed = observed.lock().expect("progress observations");
        assert_eq!(observed.first().map(|(_, value, _)| *value), Some(0));
        assert_eq!(observed.last().map(|(_, value, _)| *value), Some(12));
        assert!(observed.iter().any(|(_, value, _)| (1..12).contains(value)));
        assert!(observed.windows(2).all(|pair| pair[0].1 < pair[1].1));
        assert!(observed.iter().all(|(_, _, total)| *total == 12));
        assert!(
            observed.len() <= 5,
            "100 ms throttling emitted too many observations: {observed:?}"
        );
    }

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

    fn mpeg_transport_stream_fixture() -> Vec<u8> {
        let mut media = vec![0xff; 5 * 188];
        for packet in 0..5 {
            let offset = packet * 188;
            media[offset..offset + 4].copy_from_slice(&[
                0x47,
                0x40,
                packet as u8,
                0x10 | packet as u8,
            ]);
        }
        media
    }

    fn assert_mpeg_resolution_integrity(
        extractions: &[graphoxide_core::Extraction],
        source: &str,
        stem: &str,
    ) {
        let node_ids = extractions
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .map(|node| node.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let media = extractions
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .find(|node| {
                node.source_file == source
                    && node.extra.get("format").and_then(serde_json::Value::as_str)
                        == Some("mpeg_transport_stream")
            })
            .expect("truthful MPEG transport-stream inventory");
        let expected_media_id = make_id(&["format_inventory", "mpeg_transport_stream", stem]);
        assert_eq!(media.id, expected_media_id);
        let former_code_anchor = make_id(&[stem]);
        assert_ne!(media.id, former_code_anchor);
        let unresolved = make_id(&["ref", stem]);
        let module_edge = extractions
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .find(|edge| edge.source_file == "main.ts" && edge.relation == "imports_from")
            .expect("module import evidence");
        assert_eq!(module_edge.true_target(), unresolved);
        assert!(!module_edge.extra.contains_key("target_file"));
        for edge in extractions.iter().flat_map(|extraction| &extraction.edges) {
            assert_ne!(edge.true_target(), media.id, "media inventory is not code");
            assert_ne!(
                edge.true_target(),
                former_code_anchor,
                "former TypeScript anchor must not remain resolved"
            );
            assert_ne!(
                edge.extra
                    .get("target_file")
                    .and_then(serde_json::Value::as_str),
                Some(source),
                "symbol bindings into media must be removed"
            );
            if edge.true_target() != unresolved {
                assert!(
                    node_ids.contains(edge.true_target()),
                    "non-reference edge target {} must exist; edge={edge:?}",
                    edge.true_target()
                );
            }
        }
    }

    fn seed_forged_python_runtime_artifact(
        fixture: &Fixture,
        output: &Path,
        runtime: graphoxide_index_runtime::IndexRuntimeConfig,
    ) {
        let relative = "main.py";
        let source = b"def answer():\n    return 42\n";
        let source_path = fixture.root.join(relative);
        fs::write(&source_path, source).expect("write Python source");
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
        forged
            .nodes
            .iter_mut()
            .find(|node| node.label == "answer()")
            .expect("answer function node")
            .label = "forged()".into();
        let forged_payload = super::cache::encode_runtime_ast_cache_payload(&evidence, &forged)
            .expect("encode forged inner envelope");
        let mut artifact_store = graphoxide_index_runtime::cache::RuntimeCache::open(output)
            .expect("open runtime artifact store");
        artifact_store
            .put(evidence.key, &forged_payload)
            .expect("seed valid outer frame with forged facts");
    }

    fn assert_fresh_answer_without_forgery(result: &super::DeferredProjectExtractionResult) {
        assert!(result
            .extractions
            .iter()
            .any(|extraction| { extraction.nodes.iter().any(|node| node.label == "answer()") }));
        assert!(result
            .extractions
            .iter()
            .all(|extraction| { extraction.nodes.iter().all(|node| node.label != "forged()") }));
    }

    #[cfg(unix)]
    fn set_runtime_cache_data_files_read_only(output: &Path) -> Vec<(PathBuf, u32)> {
        use std::os::unix::fs::PermissionsExt as _;

        fn collect(directory: &Path, files: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(directory).expect("read runtime cache directory") {
                let entry = entry.expect("runtime cache entry");
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path).expect("runtime cache metadata");
                if metadata.is_dir() {
                    collect(&path, files);
                } else if metadata.is_file()
                    && path.file_name().and_then(|name| name.to_str()) != Some("owner.lock")
                {
                    files.push(path);
                }
            }
        }

        let mut files = Vec::new();
        collect(&output.join("cache/runtime-v2"), &mut files);
        files.sort();
        assert!(!files.is_empty(), "forged cache created data files");
        files
            .into_iter()
            .map(|path| {
                let mode = fs::metadata(&path)
                    .expect("cache data metadata")
                    .permissions()
                    .mode();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o400))
                    .expect("make cache data read-only");
                (path, mode)
            })
            .collect()
    }

    #[cfg(unix)]
    fn restore_runtime_cache_data_permissions(files: &[(PathBuf, u32)]) {
        use std::os::unix::fs::PermissionsExt as _;

        for (path, mode) in files {
            fs::set_permissions(path, fs::Permissions::from_mode(*mode))
                .expect("restore cache data permissions");
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
        assert!(!serial.runtime_cache.enabled);
        assert!(!parallel.runtime_cache.enabled);
        assert_eq!(serial.runtime_cache.stores, 0);
        assert_eq!(parallel.runtime_cache.stores, 0);
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
    fn legacy_missing_source_kind_is_ambiguous_only_for_transport_stream_ts() {
        let legacy = super::cache::ManifestEntry::default();
        assert!(super::source_kind_transition(
            &legacy,
            "video",
            "segment.ts"
        ));
        assert!(super::source_kind_transition(
            &legacy,
            "video",
            "SEGMENT.TS"
        ));
        assert!(!super::source_kind_transition(&legacy, "video", "clip.mp4"));
        assert!(!super::source_kind_transition(&legacy, "video", "clip.mov"));

        let known_code = super::cache::ManifestEntry {
            source_kind: Some("code".into()),
            ..super::cache::ManifestEntry::default()
        };
        assert!(super::source_kind_transition(
            &known_code,
            "video",
            "clip.mp4"
        ));
    }

    #[test]
    fn runtime_manifest_charge_uses_exact_loaded_bytes_after_bounded_admission() {
        // Production reproduction: the 512 MiB automatic runtime assigns
        // 128 MiB to cache/runs and discovered 3,696 inputs. The committed
        // SafeEVAC manifest is 610,448 bytes, far below the 2 MiB read cap.
        let cache_and_runs_budget = 134_217_728;
        let byte_limit = super::runtime_manifest_byte_limit(cache_and_runs_budget, 3_696);
        let retained_charge =
            super::runtime_manifest_retained_charge(cache_and_runs_budget, 610_448);
        assert_eq!(byte_limit, 2_097_152);
        assert_eq!(retained_charge, 19_534_336);
        assert_eq!(cache_and_runs_budget - retained_charge, 114_683_392);

        let pending_reservation =
            super::pending_manifest_retained_reservation(cache_and_runs_budget, 3_696);
        assert_eq!(pending_reservation, 67_108_864);
        assert_eq!(
            cache_and_runs_budget - retained_charge - pending_reservation,
            47_574_528,
            "loaded and pending manifest ownership are simultaneously reserved"
        );

        let mut pending = super::cache::Manifest::new();
        for index in 0..3_696 {
            pending.insert(
                format!("backend-controller/src/generated/unit_{index:04}.graphql"),
                super::cache::ManifestEntry {
                    mtime: index as f64,
                    ast_version: super::cache::AST_CACHE_VERSION,
                    ast_hash: format!("{index:032x}"),
                    semantic_hash: format!("{index:032x}"),
                    source_kind: Some("code".into()),
                    runtime_cache: None,
                },
            );
        }
        let owned_pending_charge = super::pending_manifest_retained_charge(&pending);
        assert!(owned_pending_charge > 0);
        assert!(owned_pending_charge <= pending_reservation);

        assert_eq!(
            super::runtime_manifest_retained_charge(cache_and_runs_budget, 0),
            0,
            "missing and rejected manifests retain no post-load charge"
        );
        assert_eq!(
            super::runtime_manifest_retained_charge(cache_and_runs_budget, usize::MAX),
            cache_and_runs_budget / 2,
            "the exact-byte charge remains conservatively capped"
        );
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
        let result =
            super::extract_project_with_runtime_scan_options_deferred_manifest_with_telemetry(
                &fixture.root,
                false,
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
        assert!(
            incremental.ownership_prune_sources.is_empty(),
            "an ordinary same-kind TypeScript edit preserves the semantic tier"
        );
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
                    source_kind: "code".into(),
                    indexed: true,
                },
            ),
            (
                "unchanged.ts".to_owned(),
                super::RuntimeFileContext {
                    path: unchanged_path.clone(),
                    physical_path: unchanged_path,
                    source_kind: "code".into(),
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
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("resolver baseline"), "{diagnostic}");
        assert!(
            diagnostic.contains("effective 8388608-byte"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("--memory-budget-bytes"), "{diagnostic}");
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
    fn build_evidence_counts_only_indexed_source_bytes_and_baseline_eligibility() {
        let fixture = Fixture::new();
        let main = "import { shared } from './shared'; export const run = shared;\n";
        let shared = "export const shared = 1;\n";
        let resolver_metadata = "packages:\n  - 'packages/*'\n";
        fixture.write("main.ts", main);
        fixture.write("shared.ts", shared);
        fixture.write("pnpm-workspace.yaml", resolver_metadata);
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);

        let first = super::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_build_evidence(
            &fixture.root,
            true,
            &output,
            true,
            &super::detect::DetectOptions::default(),
            runtime,
            graphoxide_index_runtime::RuntimeCancellation::new(),
        )
        .expect("progress build evidence");
        assert_eq!(
            first.indexed_source_bytes,
            (main.len() + shared.len()) as u64,
            "resolver-only workspace metadata must not be labeled indexed source size"
        );
        assert!(
            first.telemetry.io.source_bytes_selected > first.indexed_source_bytes,
            "runtime I/O telemetry should retain its broader metadata semantics"
        );
        assert!(!first.incremental_baseline_eligible);
        commit_runtime_baseline(first.result, &fixture.root, &output);

        let second = super::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_build_evidence(
            &fixture.root,
            false,
            &output,
            true,
            &super::detect::DetectOptions::default(),
            runtime,
            graphoxide_index_runtime::RuntimeCancellation::new(),
        )
        .expect("warm build evidence");
        assert!(second.incremental_baseline_eligible);
        assert_eq!(
            second.indexed_source_bytes,
            (main.len() + shared.len()) as u64
        );
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
        let source = "pub fn indexed() {}\n";
        fixture.write("lib.rs", source);
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let result =
            super::extract_project_with_runtime_scan_options_deferred_manifest_with_telemetry(
                &fixture.root,
                false,
                &output,
                false,
                &super::detect::DetectOptions::default(),
                runtime,
            )
            .expect("isolated runtime scan");
        assert!(result.runtime_cache_diagnostics.is_empty());
        assert_eq!(result.changed_sources, 1);
        assert!(result.runtime_cache.enabled);
        assert_eq!(result.runtime_cache.bypasses, 0);
        assert_eq!(result.runtime_cache.misses, 1);
        assert_eq!(result.runtime_cache.stores, 1);
        assert_eq!(result.telemetry.io.sources_selected, 1);
        assert_eq!(
            result.telemetry.io.source_bytes_selected,
            source.len() as u64
        );
        assert_eq!(result.telemetry.io.sources_read, 1);
        assert_eq!(result.telemetry.io.sources_delivered, 1);
        assert_eq!(result.telemetry.io.source_bytes_read, source.len() as u64);
        assert_eq!(
            result.telemetry.io.source_bytes_delivered,
            source.len() as u64
        );
        assert_eq!(result.telemetry.io.source_bytes_avoided, 0);
        assert_eq!(result.telemetry.work.parses, 1);
        assert!(result.telemetry.cache_io.payload_bytes_written > 0);
        assert_eq!(
            result.telemetry.cache_io.artifact_bytes_written,
            graphoxide_index_runtime::cache::runtime_cache_artifact_bytes(
                usize::try_from(result.telemetry.cache_io.payload_bytes_written)
                    .expect("test payload length fits usize"),
            ),
            "the extraction wrapper snapshots only after the completed publish command"
        );
        assert!(result.telemetry.cache_io.peak_in_flight_transfer_bytes > 0);
        assert!(
            output.join("cache/runtime-v2").exists(),
            "successful extraction must be available to the runtime read-through path"
        );
        let retained_output_bytes = result.retained_output_bytes;
        commit_runtime_baseline(result.result, &fixture.root, &output);

        let warm =
            super::extract_project_with_runtime_scan_options_deferred_manifest_with_telemetry(
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
        assert_eq!(warm.telemetry.io.sources_selected, 1);
        assert_eq!(warm.telemetry.io.source_bytes_selected, source.len() as u64);
        assert_eq!(warm.telemetry.io.sources_read, 0);
        assert_eq!(warm.telemetry.io.sources_delivered, 0);
        assert_eq!(warm.telemetry.io.source_bytes_read, 0);
        assert_eq!(warm.telemetry.io.source_bytes_delivered, 0);
        assert_eq!(warm.telemetry.io.source_bytes_avoided, source.len() as u64);
        assert_eq!(warm.telemetry.io.read_failures, 0);
        assert_eq!(warm.telemetry.work.parses, 0);
        assert!(warm.telemetry.cache_io.payload_bytes_read > 0);
        assert_eq!(
            warm.telemetry.cache_io.artifact_bytes_read,
            graphoxide_index_runtime::cache::runtime_cache_artifact_bytes(
                usize::try_from(warm.telemetry.cache_io.payload_bytes_read)
                    .expect("test payload length fits usize"),
            ),
            "the warm wrapper snapshots only after its completed owner read"
        );
        assert!(warm.telemetry.cache_io.peak_in_flight_transfer_bytes > 0);

        let forced =
            super::extract_project_with_runtime_scan_options_deferred_manifest_with_telemetry(
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
        assert!(!forced.runtime_cache.enabled);
        assert_eq!(forced.runtime_cache.bypasses, 1);
        assert_eq!(forced.runtime_cache.metadata_hits, 0);
        assert_eq!(forced.runtime_cache.runtime_hits, 0);
        assert_eq!(forced.runtime_cache.stores, 0);
        assert_eq!(forced.runtime_cache.already_present, 0);
        assert_eq!(
            forced.telemetry.cache_io,
            graphoxide_index_runtime::cache::RuntimeCacheIoTelemetry::default()
        );
        assert_eq!(forced.telemetry.io.sources_read, 1);
        assert_eq!(forced.telemetry.io.sources_delivered, 1);
        assert_eq!(forced.telemetry.work.parses, 1);
    }

    #[test]
    fn cache_telemetry_barrier_is_strict_only_for_opt_in_callers() {
        let barrier_called = std::cell::Cell::new(false);
        let fallback = super::collect_runtime_cache_io_telemetry(false, || {
            barrier_called.set(true);
            Err(graphoxide_index_runtime::cache::RuntimeCacheIoServiceError::WorkerUnavailable)
        })
        .expect("default extraction does not require cache telemetry");
        assert_eq!(
            fallback,
            graphoxide_index_runtime::cache::RuntimeCacheIoTelemetry::default()
        );
        assert!(!barrier_called.get(), "default execution skips the barrier");

        let error = super::collect_runtime_cache_io_telemetry(true, || {
            Err(graphoxide_index_runtime::cache::RuntimeCacheIoServiceError::WorkerUnavailable)
        })
        .expect_err("opt-in telemetry cannot publish fabricated zero evidence");
        assert!(error.to_string().contains("telemetry barrier failed"));
    }

    #[test]
    fn isolated_runtime_force_never_authorizes_a_preexisting_runtime_artifact() {
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
        .expect("force scan ignores forged runtime artifact");
        assert!(!forced.runtime_cache.enabled);
        assert_eq!(forced.runtime_cache.bypasses, 1);
        assert_eq!(forced.runtime_cache.stores, 0);
        assert_eq!(forced.runtime_cache.already_present, 0);
        assert!(forced
            .pending_manifest
            .entries
            .values()
            .all(|entry| entry.runtime_cache.is_none()));
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
        .expect("fresh repair after force deliberately omitted cache authorization");
        assert_eq!(rebuilt.runtime_cache.metadata_hits, 0);
        assert_eq!(rebuilt.runtime_cache.runtime_hits, 0);
        assert_eq!(rebuilt.runtime_cache.parses_avoided, 0);
        assert_eq!(rebuilt.runtime_cache.stores, 1);
        assert_eq!(rebuilt.changed_sources, 1);
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
    fn force_code_only_clears_carried_non_code_runtime_authorization() {
        for legacy_executor in [false, true] {
            let fixture = Fixture::new();
            fixture.write("main.rs", "pub fn answer() -> u32 { 42 }\n");
            fixture.write("notes.md", "# Notes\n\nRetained documentation.\n");
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
            .expect("cold full runtime scan");
            assert!(
                cold.pending_manifest.entries["notes.md"]
                    .runtime_cache
                    .is_some(),
                "the excluded row must begin with real cache authorization"
            );
            commit_runtime_baseline(cold, &fixture.root, &output);

            let forced = if legacy_executor {
                super::extract_project_with_scan_options_deferred_manifest(
                    &fixture.root,
                    true,
                    &output,
                    true,
                    &super::detect::DetectOptions::default(),
                )
            } else {
                super::extract_project_with_runtime_scan_options_deferred_manifest(
                    &fixture.root,
                    true,
                    &output,
                    true,
                    &super::detect::DetectOptions::default(),
                    runtime,
                )
            }
            .expect("forced code-only trust reset");
            assert!(
                forced.pending_manifest.entries["notes.md"]
                    .runtime_cache
                    .is_none(),
                "policy-preserved non-code ownership cannot preserve cache trust on force"
            );
            forced
                .pending_manifest
                .commit()
                .expect("commit forced trust-reset manifest");

            let repair =
                super::extract_project_with_runtime_scan_options_deferred_manifest_with_telemetry(
                    &fixture.root,
                    false,
                    &output,
                    false,
                    &super::detect::DetectOptions::default(),
                    runtime,
                )
                .expect("normal full scan repairs authorization");
            assert_eq!(repair.runtime_cache.metadata_hits, 0);
            assert_eq!(repair.runtime_cache.runtime_hits, 0);
            assert_eq!(repair.runtime_cache.parses_avoided, 0);
            assert_eq!(repair.telemetry.work.parses, 2);
            assert!(repair
                .extractions
                .iter()
                .flat_map(|extraction| &extraction.nodes)
                .any(|node| node.source_file == "notes.md"));
            assert!(repair
                .pending_manifest
                .entries
                .values()
                .all(|entry| entry.runtime_cache.is_some()));
        }
    }

    #[test]
    fn cache_startup_failure_after_force_does_not_reauthorize_forged_artifact() {
        let fixture = Fixture::new();
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        seed_forged_python_runtime_artifact(&fixture, &output, runtime);

        let forced = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("force trust reset");
        assert!(forced
            .pending_manifest
            .entries
            .values()
            .all(|entry| entry.runtime_cache.is_none()));
        commit_runtime_baseline(forced, &fixture.root, &output);
        fs::remove_file(output.join("graph.json")).expect("force fresh fact rebuild");

        let owner =
            graphoxide_index_runtime::cache::RuntimeCacheIoService::start(output.clone(), 1)
                .expect("hold runtime cache owner");
        let startup_failed = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("fresh extraction survives cache startup failure");
        assert!(!startup_failed.runtime_cache.enabled);
        assert!(startup_failed
            .runtime_cache_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("runtime cache could not start")));
        assert!(startup_failed
            .pending_manifest
            .entries
            .values()
            .all(|entry| entry.runtime_cache.is_none()));
        assert_fresh_answer_without_forgery(&startup_failed);
        commit_runtime_baseline(startup_failed, &fixture.root, &output);
        owner.shutdown().expect("release runtime cache owner");

        fs::remove_file(output.join("graph.json")).expect("force later cache decision");
        let later = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("later scan repairs rather than replaying forged artifact");
        assert_eq!(later.runtime_cache.runtime_hits, 0);
        assert_eq!(later.runtime_cache.parses_avoided, 0);
        assert_eq!(later.runtime_cache.stores, 1);
        assert_fresh_answer_without_forgery(&later);
        assert!(later
            .pending_manifest
            .entries
            .values()
            .all(|entry| entry.runtime_cache.is_some()));
    }

    #[test]
    fn unchanged_runtime_manifest_refreshes_current_source_identity() {
        let fixture = Fixture::new();
        let source = b"def answer():\n    return 42\n";
        let source_path = fixture.root.join("main.py");
        fs::write(&source_path, source).expect("write initial source");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial cache-authorized scan");
        let initial_evidence = initial.pending_manifest.entries["main.py"]
            .runtime_cache
            .expect("initial runtime authorization");
        commit_runtime_baseline(initial, &fixture.root, &output);

        let replacement = fixture.root.join("replacement.tmp");
        fs::write(&replacement, source).expect("write byte-identical replacement");
        fs::remove_file(&source_path).expect("remove original identity");
        fs::rename(&replacement, &source_path).expect("install replacement identity");
        let unchanged = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("byte-identical scan with a new physical identity");
        assert_eq!(unchanged.changed_sources, 0);
        let current_evidence = unchanged.pending_manifest.entries["main.py"]
            .runtime_cache
            .expect("refreshed runtime authorization");
        assert_eq!(
            current_evidence.content_digest,
            initial_evidence.content_digest
        );
        assert_eq!(current_evidence.artifact_key, initial_evidence.artifact_key);
        assert_ne!(
            current_evidence.source_identity_digest, initial_evidence.source_identity_digest,
            "byte verification should refresh metadata authorization evidence"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_store_failure_after_force_does_not_reauthorize_forged_artifact() {
        let fixture = Fixture::new();
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        seed_forged_python_runtime_artifact(&fixture, &output, runtime);

        let forced = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("force trust reset");
        commit_runtime_baseline(forced, &fixture.root, &output);
        fs::remove_file(output.join("graph.json")).expect("force fresh fact rebuild");

        let cache_files = set_runtime_cache_data_files_read_only(&output);
        let store_failed = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("fresh extraction survives cache store failure");
        restore_runtime_cache_data_permissions(&cache_files);
        assert!(store_failed.runtime_cache.enabled);
        assert_eq!(store_failed.runtime_cache.store_failures, 1);
        assert_eq!(store_failed.runtime_cache.stores, 0);
        assert!(store_failed
            .runtime_cache_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("runtime cache persistence")));
        assert!(store_failed
            .pending_manifest
            .entries
            .values()
            .all(|entry| entry.runtime_cache.is_none()));
        assert_fresh_answer_without_forgery(&store_failed);
        commit_runtime_baseline(store_failed, &fixture.root, &output);

        fs::remove_file(output.join("graph.json")).expect("force later cache decision");
        let later = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("later scan repairs rather than replaying forged artifact");
        assert_eq!(later.runtime_cache.runtime_hits, 0);
        assert_eq!(later.runtime_cache.parses_avoided, 0);
        assert_eq!(later.runtime_cache.stores, 1);
        assert_fresh_answer_without_forgery(&later);
        assert!(later
            .pending_manifest
            .entries
            .values()
            .all(|entry| entry.runtime_cache.is_some()));
    }

    #[test]
    fn isolated_runtime_cache_keeps_javascript_family_on_contextual_bypass() {
        let fixture = Fixture::new();
        fixture.write("main.ts", "export const answer: number = 42;\n");
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
        fs::remove_dir_all(output.join("cache/runtime-v2")).expect("remove runtime artifact store");

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
        let mut diagnostics = Vec::new();
        for workers in [1, 2, 4] {
            let runtime = graphoxide_index_runtime::IndexRuntimeConfig {
                memory_budget_bytes: 128 * 1024,
                io_workers: workers,
                compute_workers: workers,
                io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
                read_batch_bytes: 4 * 1024,
            };
            let output = fixture.root.join(format!("output-{workers}"));
            fs::create_dir_all(&output).expect("create committed output");
            let graph_bytes = br#"{"nodes":[],"edges":[],"sentinel":"last-good"}"#;
            let manifest_bytes = b"{}";
            fs::write(output.join("graph.json"), graph_bytes).expect("seed committed graph");
            fs::write(output.join("manifest.json"), manifest_bytes)
                .expect("seed committed manifest");
            let error = super::extract_project_with_runtime_scan_options_deferred_manifest(
                &fixture.root,
                true,
                &output,
                false,
                &super::detect::DetectOptions::default(),
                runtime,
            )
            .expect_err("high-fanout facts must exceed the retained-output partition");
            let diagnostic = error.to_string();
            assert_eq!(
                diagnostic,
                "isolated retained extraction output exhausted its 16320-byte output cap within the effective 131072-byte managed-memory budget; retry with a larger --memory-budget-bytes value"
            );
            assert!(!diagnostic.contains("fanout_"));
            assert_eq!(
                fs::read(output.join("graph.json")).expect("read last-good graph"),
                graph_bytes,
                "a rejected scan changed the committed graph"
            );
            assert_eq!(
                fs::read(output.join("manifest.json")).expect("read last-good manifest"),
                manifest_bytes,
                "a rejected scan changed the committed manifest"
            );
            let mut output_entries = fs::read_dir(&output)
                .expect("read committed output")
                .map(|entry| {
                    entry
                        .expect("output entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>();
            output_entries.sort();
            assert_eq!(output_entries, ["graph.json", "manifest.json"]);
            diagnostics.push(diagnostic);
        }
        assert!(diagnostics.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn isolated_incremental_typescript_to_mpeg_rebuilds_importers_without_endpoint_aliasing() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        fixture.write("segment.ts", "export const phantom = 42;\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial TypeScript baseline");
        assert_eq!(
            initial.pending_manifest.entries["segment.ts"]
                .source_kind
                .as_deref(),
            Some("code")
        );
        let baseline = commit_runtime_baseline(initial, &fixture.root, &output);
        let former_code_id = make_id(&["segment"]);
        assert!(baseline
            .links
            .iter()
            .any(|edge| { edge.source_file == "main.ts" && edge.true_target() == former_code_id }));

        fs::write(
            fixture.root.join("segment.ts"),
            mpeg_transport_stream_fixture(),
        )
        .expect("replace TypeScript with MPEG transport stream");
        let incremental = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("incremental TypeScript-to-MPEG scan");

        assert_eq!(incremental.changed_sources, 2);
        assert_eq!(incremental.ownership_prune_sources.len(), 1);
        assert_eq!(
            incremental.ownership_prune_sources[0],
            fs::canonicalize(fixture.root.join("segment.ts")).expect("canonical media source")
        );
        assert_eq!(
            incremental.pending_manifest.entries["segment.ts"]
                .source_kind
                .as_deref(),
            Some("video")
        );
        let media_node = incremental
            .extractions
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .find(|node| {
                node.source_file == "segment.ts"
                    && node.extra.get("format").and_then(serde_json::Value::as_str)
                        == Some("mpeg_transport_stream")
            })
            .expect("fresh MPEG inventory node");
        let media_id = media_node.id.clone();
        assert_ne!(media_id, former_code_id);
        let fresh_import = incremental
            .extractions
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .find(|edge| edge.source_file == "main.ts" && edge.relation == "imports_from")
            .expect("recomputed importer edge");
        assert_eq!(fresh_import.true_target(), make_id(&["ref", "segment"]));
        assert_ne!(fresh_import.true_target(), media_id);

        let fresh = graphoxide_graph::dedupe_raw_extractions(&incremental.extractions);
        let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_ownership_resets_and_materialization_limit(
            fresh,
            &baseline,
            &incremental.rebuilt_sources,
            &[],
            graphoxide_graph::incremental::IncrementalBaselinePrunes {
                deletion_sources: &[],
                ownership_reset_sources: &incremental.ownership_prune_sources,
            },
            Some(&fixture.root),
            64 * 1024 * 1024,
        )
        .expect("merge TypeScript-to-MPEG delta");
        assert!(merged.nodes.iter().all(|node| {
            node.source_file != "segment.ts"
                || node.extra.get("type").and_then(serde_json::Value::as_str) != Some("file")
        }));
        assert!(merged
            .edges
            .iter()
            .all(|edge| { edge.source_file != "main.ts" || edge.true_target() != media_id }));
        let clustered = graphoxide_graph::build_graph_with_options_and_root(
            std::slice::from_ref(&merged),
            &fixture.root,
            graphoxide_graph::BuildOptions::default(),
        )
        .expect("build clustered TypeScript-to-MPEG graph");
        assert!(clustered.nodes.iter().any(|node| node.id == media_id));
        assert!(clustered
            .links
            .iter()
            .all(|edge| { edge.source_file != "main.ts" || edge.true_target() != media_id }));
    }

    #[test]
    fn transition_diagnostics_are_deterministic_escaped_and_bounded() {
        let long = format!("a\n{}", "界".repeat(8_192));
        let mut forward = std::collections::BTreeSet::new();
        forward.insert("z.ts".to_owned());
        forward.insert(long.clone());
        forward.insert("middle.ts".to_owned());
        let mut reverse = std::collections::BTreeSet::new();
        reverse.insert("middle.ts".to_owned());
        reverse.insert(long);
        reverse.insert("z.ts".to_owned());

        let first = super::unverified_source_kind_transition_error(&forward).to_string();
        let second = super::unverified_source_kind_transition_error(&reverse).to_string();
        assert_eq!(
            first, second,
            "worker/insertion order must not affect diagnostics"
        );
        assert!(first.contains("3 source(s)"));
        assert!(
            first.contains("\\n"),
            "control characters must be escaped: {first}"
        );
        assert!(
            first.contains('…'),
            "long paths must be visibly truncated: {first}"
        );
        assert!(
            first.len() < 2_500,
            "diagnostic must remain bounded: {}",
            first.len()
        );
        assert!(super::bounded_diagnostic_source_path(&"界".repeat(1_024)).len() < 1_100);
    }

    #[test]
    fn code_only_ambiguous_media_requires_a_trusted_manifest_for_a_committed_graph() {
        for legacy_executor in [false, true] {
            for corrupt_manifest in [false, true] {
                let fixture = Fixture::new();
                fixture.write("main.ts", "export const main = true;\n");
                let segment = fixture.write("segment.ts", "export const phantom = 42;\n");
                let output = fixture.root.join("graphoxide-out");
                let runtime = runtime_config(32 * 1024 * 1024);
                let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
                    &fixture.root,
                    false,
                    &output,
                    false,
                    &super::detect::DetectOptions::default(),
                    runtime,
                )
                .expect("initial TypeScript baseline");
                commit_runtime_baseline(initial, &fixture.root, &output);
                fs::write(&segment, mpeg_transport_stream_fixture())
                    .expect("replace TypeScript with MPEG");
                let graph_before = fs::read(output.join("graph.json")).expect("baseline graph");
                let manifest_path = output.join("manifest.json");
                if corrupt_manifest {
                    fs::write(&manifest_path, b"{not-json").expect("corrupt committed manifest");
                } else {
                    fs::remove_file(&manifest_path).expect("remove committed manifest");
                }
                let manifest_before = fs::read(&manifest_path).ok();

                let error = if legacy_executor {
                    super::extract_project_with_scan_options_deferred_manifest(
                        &fixture.root,
                        false,
                        &output,
                        true,
                        &super::detect::DetectOptions::default(),
                    )
                } else {
                    super::extract_project_with_runtime_scan_options_deferred_manifest(
                        &fixture.root,
                        false,
                        &output,
                        true,
                        &super::detect::DetectOptions::default(),
                        runtime,
                    )
                }
                .expect_err("untrusted manifest cannot authorize ambiguous carry-forward");
                let error = error.to_string();
                assert!(
                    error.contains("cannot safely perform a --code-only"),
                    "{error}"
                );
                assert!(error.contains("full rebuild"), "{error}");
                assert_eq!(fs::read(output.join("graph.json")).unwrap(), graph_before);
                assert_eq!(fs::read(&manifest_path).ok(), manifest_before);
            }
        }
    }

    #[test]
    fn new_mpeg_segment_after_a_trusted_manifest_is_safely_code_only_excluded() {
        for legacy_executor in [false, true] {
            let fixture = Fixture::new();
            fixture.write("main.ts", "export const main = true;\n");
            let output = fixture.root.join("graphoxide-out");
            let runtime = runtime_config(32 * 1024 * 1024);
            let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
                &fixture.root,
                false,
                &output,
                false,
                &super::detect::DetectOptions::default(),
                runtime,
            )
            .expect("initial trusted baseline");
            commit_runtime_baseline(initial, &fixture.root, &output);
            fs::write(
                fixture.root.join("new-segment.ts"),
                mpeg_transport_stream_fixture(),
            )
            .expect("add rotating MPEG segment");

            let result = if legacy_executor {
                super::extract_project_with_scan_options_deferred_manifest(
                    &fixture.root,
                    false,
                    &output,
                    true,
                    &super::detect::DetectOptions::default(),
                )
            } else {
                super::extract_project_with_runtime_scan_options_deferred_manifest(
                    &fixture.root,
                    false,
                    &output,
                    true,
                    &super::detect::DetectOptions::default(),
                    runtime,
                )
            }
            .expect("verified media can authorize a safe code-only exclusion");
            assert!(result.ownership_prune_sources.is_empty());
            assert_eq!(result.verified_representation_sources.len(), 1);
            assert_eq!(
                result.verified_representation_sources[0],
                fs::canonicalize(fixture.root.join("new-segment.ts"))
                    .expect("canonical new media path")
            );
            assert!(!result
                .pending_manifest
                .entries
                .contains_key("new-segment.ts"));
        }
    }

    #[test]
    fn code_only_preserved_manifest_ownership_is_reported_as_live_memory() {
        let fixture = Fixture::new();
        fixture.write("main.ts", "export const main = true;\n");
        for index in 0..96 {
            fixture.write(
                &format!("docs/section-{index:03}/reference-{index:03}.md"),
                "# Reference\n\nPreserved non-code ownership.\n",
            );
        }
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(64 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial mixed baseline");
        commit_runtime_baseline(initial, &fixture.root, &output);

        let code_only = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            true,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("code-only scan preserves non-code ownership");
        assert_eq!(code_only.pending_manifest.entries.len(), 97);
        assert_eq!(
            code_only.pending_manifest_retained_bytes,
            super::pending_manifest_retained_charge(&code_only.pending_manifest.entries)
        );
        assert!(code_only.pending_manifest_retained_bytes > code_only.retained_output_bytes);
    }

    #[test]
    fn unverified_code_only_kind_transition_preserves_last_good_graph_and_manifest() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        let original_typescript = b"export const phantom = 42;\n";
        let segment_path = fixture.root.join("segment.ts");
        fs::write(&segment_path, original_typescript).expect("write TypeScript target");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial TypeScript baseline");
        commit_runtime_baseline(initial, &fixture.root, &output);
        let graph_before = fs::read(output.join("graph.json")).expect("last-good graph bytes");
        let manifest_before =
            fs::read(output.join("manifest.json")).expect("last-good manifest bytes");

        fs::write(&segment_path, mpeg_transport_stream_fixture())
            .expect("nominate TypeScript-to-MPEG transition");
        let mut replace_after_detection = |_detection: &super::detect::DetectResult| {
            fs::write(&segment_path, original_typescript)
                .context("replace media generation after discovery")?;
            Ok(())
        };
        let error = super::extract_project_with_runtime_scan_options_deferred_manifest_impl(
            &fixture.root,
            false,
            &output,
            true,
            &super::detect::DetectOptions::default(),
            runtime,
            graphoxide_index_runtime::RuntimeCancellation::new(),
            false,
            Some(&mut replace_after_detection),
            None,
        )
        .expect_err("unverified transition must abort before publication");
        assert!(error
            .to_string()
            .contains("source classification transition could not be verified"));
        assert!(error.to_string().contains("segment.ts"));
        assert_eq!(
            fs::read(output.join("graph.json")).expect("preserved graph bytes"),
            graph_before
        );
        assert_eq!(
            fs::read(output.join("manifest.json")).expect("preserved manifest bytes"),
            manifest_before
        );
        let retained_graph = graphoxide_core::read_graph(output.join("graph.json"))
            .expect("read retained last-good graph");
        assert!(retained_graph.nodes.iter().any(|node| {
            node.source_file == "segment.ts"
                && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("file")
        }));
        assert!(retained_graph.links.iter().any(|edge| {
            edge.source_file == "main.ts" && edge.true_target() == make_id(&["segment"])
        }));
        let retained_manifest: super::cache::Manifest =
            serde_json::from_slice(&manifest_before).expect("decode retained manifest");
        assert_eq!(
            retained_manifest["segment.ts"].source_kind.as_deref(),
            Some("code")
        );
    }

    #[test]
    fn isolated_code_only_typescript_to_mpeg_prunes_stale_code_ownership() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        fixture.write("segment.ts", "export const phantom = 42;\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial TypeScript baseline");
        let baseline = commit_runtime_baseline(initial, &fixture.root, &output);
        let segment_path = fixture.root.join("segment.ts");
        fs::write(&segment_path, mpeg_transport_stream_fixture())
            .expect("replace TypeScript with MPEG transport stream");

        let incremental = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            true,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("code-only TypeScript-to-MPEG scan");
        assert!(
            segment_path.is_file(),
            "the live media source remains on disk"
        );
        assert!(!incremental
            .pending_manifest
            .entries
            .contains_key("segment.ts"));
        assert_eq!(incremental.ownership_prune_sources.len(), 1);
        assert_eq!(
            incremental.ownership_prune_sources[0],
            fs::canonicalize(&segment_path).expect("canonical media source")
        );
        assert!(incremental.extractions.iter().all(|extraction| {
            extraction
                .nodes
                .iter()
                .all(|node| node.source_file != "segment.ts")
        }));
        let media_id = make_id(&["format_inventory", "mpeg_transport_stream", "segment"]);
        let fresh_import = incremental
            .extractions
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .find(|edge| edge.source_file == "main.ts" && edge.relation == "imports_from")
            .expect("code-only importer was dependency-invalidated");
        assert_eq!(fresh_import.true_target(), make_id(&["ref", "segment"]));

        let fresh = graphoxide_graph::dedupe_raw_extractions(&incremental.extractions);
        let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
            fresh,
            &baseline,
            &incremental.rebuilt_sources,
            &[],
            &incremental.ownership_prune_sources,
            Some(&fixture.root),
            64 * 1024 * 1024,
        )
        .expect("merge code-only ownership prune");
        assert!(merged
            .nodes
            .iter()
            .all(|node| node.source_file != "segment.ts"));
        assert!(merged
            .edges
            .iter()
            .all(|edge| edge.source_file != "segment.ts"));
        assert!(merged.edges.iter().all(|edge| {
            edge.source_file != "main.ts"
                || (edge.true_target() != make_id(&["segment"]) && edge.true_target() != media_id)
        }));
        let clustered = graphoxide_graph::build_graph_with_options_and_root(
            std::slice::from_ref(&merged),
            &fixture.root,
            graphoxide_graph::BuildOptions::default(),
        )
        .expect("build clustered code-only ownership prune");
        assert!(clustered
            .nodes
            .iter()
            .all(|node| node.source_file != "segment.ts"));
        assert!(clustered
            .links
            .iter()
            .all(|edge| { edge.source_file != "main.ts" || edge.true_target() != media_id }));
    }

    #[test]
    fn explicit_file_manifest_uses_canonical_kind_for_later_project_transition() {
        let fixture = Fixture::new();
        let main = fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        let segment = fixture.write("segment.ts", "export const phantom = 42;\n");
        let output = fixture.root.join("graphoxide-out");
        let explicit = super::extract_files_deferred_manifest_with_output(
            &[main, segment.clone()],
            Some(&fixture.root),
            &output,
            false,
        )
        .expect("explicit-file TypeScript baseline");
        assert_eq!(
            explicit.pending_manifest.entries["segment.ts"]
                .source_kind
                .as_deref(),
            Some("code"),
            "manifest evidence must use the detector bucket, not node.file_type"
        );
        fs::create_dir_all(&output).expect("create explicit managed output");
        let baseline = graphoxide_graph::build_graph_with_options_and_root(
            &explicit.result.extractions,
            &fixture.root,
            graphoxide_graph::BuildOptions::default(),
        )
        .expect("build explicit-file baseline");
        graphoxide_core::write_graph_atomic(output.join("graph.json"), &baseline, true)
            .expect("write explicit-file baseline");
        explicit
            .pending_manifest
            .commit()
            .expect("commit explicit-file manifest");

        fs::write(&segment, mpeg_transport_stream_fixture())
            .expect("replace explicit TypeScript with MPEG");
        let runtime = runtime_config(32 * 1024 * 1024);
        let incremental = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            true,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("project scan consumes explicit-file manifest");
        assert_eq!(incremental.ownership_prune_sources.len(), 1);
        assert!(!incremental
            .pending_manifest
            .entries
            .contains_key("segment.ts"));
        let fresh = graphoxide_graph::dedupe_raw_extractions(&incremental.extractions);
        let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
            fresh,
            &baseline,
            &incremental.rebuilt_sources,
            &[],
            &incremental.ownership_prune_sources,
            Some(&fixture.root),
            64 * 1024 * 1024,
        )
        .expect("merge cross-writer ownership transition");
        assert!(merged
            .nodes
            .iter()
            .all(|node| node.source_file != "segment.ts"));
        assert!(merged
            .edges
            .iter()
            .all(|edge| edge.source_file != "segment.ts"));
    }

    #[test]
    fn isolated_incremental_mpeg_to_typescript_replaces_inventory_and_resolves_importer() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        fs::write(
            fixture.root.join("segment.ts"),
            mpeg_transport_stream_fixture(),
        )
        .expect("write initial MPEG transport stream");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial MPEG baseline");
        let media_id = initial
            .extractions
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .find(|node| node.source_file == "segment.ts")
            .expect("initial media node")
            .id
            .clone();
        let baseline = commit_runtime_baseline(initial, &fixture.root, &output);
        fixture.write("segment.ts", "export const phantom = 42;\n");

        let incremental = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("incremental MPEG-to-TypeScript scan");
        assert_eq!(incremental.changed_sources, 2);
        assert_eq!(incremental.ownership_prune_sources.len(), 1);
        assert_eq!(
            incremental.ownership_prune_sources[0],
            fs::canonicalize(fixture.root.join("segment.ts")).expect("canonical code source")
        );
        assert_eq!(
            incremental.pending_manifest.entries["segment.ts"]
                .source_kind
                .as_deref(),
            Some("code")
        );
        let fresh_import = incremental
            .extractions
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .find(|edge| edge.source_file == "main.ts" && edge.relation == "imports_from")
            .expect("recomputed TypeScript import");
        assert_eq!(fresh_import.true_target(), make_id(&["segment"]));
        assert_ne!(fresh_import.true_target(), media_id);
        let fresh = graphoxide_graph::dedupe_raw_extractions(&incremental.extractions);
        let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_ownership_resets_and_materialization_limit(
            fresh,
            &baseline,
            &incremental.rebuilt_sources,
            &[],
            graphoxide_graph::incremental::IncrementalBaselinePrunes {
                deletion_sources: &[],
                ownership_reset_sources: &incremental.ownership_prune_sources,
            },
            Some(&fixture.root),
            64 * 1024 * 1024,
        )
        .expect("merge MPEG-to-TypeScript delta");
        assert!(merged.nodes.iter().all(|node| node.id != media_id));
        assert!(merged.nodes.iter().any(|node| {
            node.source_file == "segment.ts"
                && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("file")
        }));
    }

    #[test]
    fn isolated_runtime_never_resolves_mpeg_ts_media_as_typescript() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        let media = mpeg_transport_stream_fixture();
        fs::write(fixture.root.join("segment.ts"), &media).expect("write MPEG-TS fixture");

        let result = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &fixture.root.join("graphoxide-out"),
            false,
            &super::detect::DetectOptions::default(),
            runtime_config(16 * 1024 * 1024),
        )
        .expect("extract TypeScript beside MPEG transport-stream media");

        let media_node = result
            .extractions
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .find(|node| {
                node.source_file == "segment.ts"
                    && node.extra.get("format").and_then(serde_json::Value::as_str)
                        == Some("mpeg_transport_stream")
            })
            .expect("truthful MPEG transport-stream inventory");
        assert_eq!(media_node.file_type, "document");
        let media_node_id = media_node.id.clone();
        assert!(result.extractions.iter().all(|extraction| {
            extraction.nodes.iter().all(|node| {
                node.source_file != "segment.ts"
                    || node.extra.get("type").and_then(serde_json::Value::as_str) != Some("file")
            })
        }));
        let import = result
            .extractions
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .find(|edge| edge.source_file == "main.ts" && edge.relation == "imports_from")
            .expect("unresolved TypeScript import edge");
        assert_ne!(import.true_target(), media_node_id);
        assert_eq!(
            import.true_target(),
            graphoxide_core::make_id(&["ref", "segment"])
        );
        assert!(result.pending_manifest.entries.contains_key("main.ts"));
        assert!(result.pending_manifest.entries.contains_key("segment.ts"));
    }

    #[test]
    fn isolated_runtime_streams_large_mpeg_inventory_outside_source_arena() {
        use std::io::Write as _;

        let fixture = Fixture::new();
        fixture.write("main.ts", "export const main = true;\n");
        let segment = fixture.root.join("segment.ts");
        let packet_count = 50_000_u64;
        let mut packet = [0xff; 188];
        packet[..4].copy_from_slice(&[0x47, 0x40, 0x00, 0x10]);
        let file = fs::File::create(&segment).expect("create large MPEG stream");
        let mut writer = std::io::BufWriter::new(file);
        for _ in 0..packet_count {
            writer.write_all(&packet).expect("stream MPEG packet");
        }
        writer.flush().expect("flush MPEG stream");
        let expected_bytes = packet_count * 188;
        let output = fixture.root.join("graphoxide-out");

        let result =
            super::extract_project_with_runtime_scan_options_deferred_manifest_with_telemetry(
                &fixture.root,
                true,
                &output,
                false,
                &super::detect::DetectOptions::default(),
                runtime_config(4 * 1024 * 1024),
            )
            .expect("large media remains inventory-only under a smaller source arena");
        let media = result
            .extractions
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .find(|node| {
                node.extra.get("format").and_then(serde_json::Value::as_str)
                    == Some("mpeg_transport_stream")
            })
            .expect("streamed MPEG inventory");
        assert_eq!(
            media
                .extra
                .get("byte_length")
                .and_then(serde_json::Value::as_u64),
            Some(expected_bytes)
        );
        assert_eq!(
            result.pending_manifest.entries["segment.ts"].ast_hash,
            super::detect::md5_file(&segment)
        );
        assert_eq!(result.telemetry.io.sources_selected, 2);
        assert!(result.telemetry.io.source_bytes_selected >= expected_bytes);
        assert_eq!(result.telemetry.io.sources_read, 2);
        assert!(result.telemetry.io.source_bytes_read >= expected_bytes);
        assert!(result.telemetry.io.source_bytes_delivered < expected_bytes);
        assert!(result.telemetry.io.peak_ready_bytes < expected_bytes);
        assert_eq!(result.telemetry.work.parses, 1);
    }

    #[test]
    fn legacy_full_and_unchanged_scans_never_resolve_mpeg_as_typescript() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        fs::write(
            fixture.root.join("segment.ts"),
            mpeg_transport_stream_fixture(),
        )
        .expect("write MPEG transport stream");
        let output = fixture.root.join("graphoxide-out");

        let first = super::extract_project_with_scan_options_deferred_manifest(
            &fixture.root,
            true,
            &output,
            false,
            &super::detect::DetectOptions::default(),
        )
        .expect("fresh legacy extraction");
        assert_mpeg_resolution_integrity(&first.extractions, "segment.ts", "segment");
        first
            .pending_manifest
            .commit()
            .expect("commit fresh legacy manifest");

        let unchanged = super::extract_project_with_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
        )
        .expect("unchanged legacy extraction");
        assert_mpeg_resolution_integrity(&unchanged.extractions, "segment.ts", "segment");
    }

    #[test]
    fn explicit_file_scans_never_resolve_mpeg_as_typescript() {
        let fixture = Fixture::new();
        let main = fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        let segment = fixture.root.join("segment.ts");
        fs::write(&segment, mpeg_transport_stream_fixture()).expect("write MPEG transport stream");
        let output = fixture.root.join("graphoxide-out");

        let first = super::extract_files_deferred_manifest_with_output(
            &[main.clone(), segment.clone()],
            Some(&fixture.root),
            &output,
            true,
        )
        .expect("fresh explicit-file extraction");
        assert_mpeg_resolution_integrity(&first.result.extractions, "segment.ts", "segment");
        first
            .pending_manifest
            .commit()
            .expect("commit explicit-file manifest");

        let cached = super::extract_files_deferred_manifest_with_output(
            &[main, segment],
            Some(&fixture.root),
            &output,
            false,
        )
        .expect("cached explicit-file extraction");
        assert_mpeg_resolution_integrity(&cached.result.extractions, "segment.ts", "segment");
    }

    #[test]
    fn explicit_builtin_admitted_bytes_preserve_legacy_path_probe_semantics() {
        let fixture = Fixture::new();
        let main = fixture.write(
            "main.py",
            "from helper import helper\n\ndef caller():\n    return helper()\n",
        );
        fixture.write("helper.py", "def helper():\n    return 42\n");
        let mut expected =
            vec![super::engine::extract_as(&main, "main.py").expect("legacy path extraction")];
        super::resolution::resolve_with_root(&mut expected, &fixture.root);

        let output = fixture.root.join("graphoxide-out");
        let actual = super::extract_files_deferred_manifest_with_output(
            std::slice::from_ref(&main),
            Some(&fixture.root),
            &output,
            true,
        )
        .expect("admitted-byte explicit extraction");
        assert_eq!(
            serde_json::to_value(&actual.result.extractions).expect("serialize actual"),
            serde_json::to_value(expected).expect("serialize expected")
        );
    }

    #[test]
    fn explicit_pending_manifest_preflights_previous_plus_exact_ownership() {
        use md5::Digest as _;

        let fixture = Fixture::new();
        let source_bytes = b"export const answer = 42;\n";
        let source = fixture.root.join("main.ts");
        fs::write(&source, source_bytes).expect("write source");
        let output = fixture.root.join("graphoxide-out");
        fs::create_dir_all(&output).expect("create output");
        fs::write(output.join("graph.json"), b"last-good-graph").expect("seed graph");
        fs::write(output.join("manifest.json"), b"{\"sentinel\":true}").expect("seed manifest");
        let graph_before = fs::read(output.join("graph.json")).expect("graph sentinel");
        let manifest_before = fs::read(output.join("manifest.json")).expect("manifest sentinel");
        let mut previous = super::cache::Manifest::new();
        previous.insert(
            "main.ts".into(),
            super::cache::ManifestEntry {
                mtime: 0.0,
                ast_version: super::cache::AST_CACHE_VERSION,
                ast_hash: format!("{:x}", md5::Md5::digest(source_bytes)),
                semantic_hash: "s".repeat(64 * 1024),
                source_kind: Some("code".into()),
                runtime_cache: None,
            },
        );
        let previous_charge = super::project_manifest_retained_bytes(&previous);
        let admitted = super::extract_files_deferred_manifest_with_output_and_previous(
            std::slice::from_ref(&source),
            Some(&fixture.root),
            &output,
            true,
            &previous,
            usize::MAX,
        )
        .expect("measure one exact row");
        let exact_charge = admitted.pending_manifest.retained_bytes();
        drop(admitted);
        let limit = previous_charge
            .checked_add(exact_charge)
            .expect("bounded fixture charge")
            .saturating_sub(1);

        let error = super::extract_files_deferred_manifest_with_output_and_previous(
            std::slice::from_ref(&source),
            Some(&fixture.root),
            &output,
            true,
            &previous,
            limit,
        )
        .expect_err("candidate row must be rejected before manifest insertion");
        assert!(
            error
                .to_string()
                .contains("explicit pending manifest retained ownership would exceed"),
            "{error:#}"
        );
        assert_eq!(fs::read(output.join("graph.json")).unwrap(), graph_before);
        assert_eq!(
            fs::read(output.join("manifest.json")).unwrap(),
            manifest_before
        );
    }

    #[test]
    fn explicit_final_recheck_rejects_path_mutation_after_matching_extraction() {
        let fixture = Fixture::new();
        let segment = fixture.root.join("segment.ts");
        let media = mpeg_transport_stream_fixture();
        fs::write(&segment, &media).expect("write admitted media");
        let output = fixture.root.join("graphoxide-out");
        fs::create_dir_all(&output).expect("create managed output");
        fs::write(output.join("graph.json"), b"last-good-graph").expect("seed graph sentinel");
        fs::write(output.join("manifest.json"), b"{}").expect("seed manifest sentinel");
        let graph_before = fs::read(output.join("graph.json")).expect("graph sentinel");
        let manifest_before = fs::read(output.join("manifest.json")).expect("manifest sentinel");

        let error = super::extract_files_with_deferred_manifest_and_output(
            std::slice::from_ref(&segment),
            Some(&fixture.root),
            &output,
            true,
            |path, relative| {
                fs::write(path, b"export const replacement = true;\n")
                    .context("replace path after admitted media")?;
                Ok(super::engine::mpeg_transport_stream_inventory(
                    path,
                    relative,
                    media.len() as u64,
                ))
            },
        )
        .expect_err("final path generation must agree with admitted/extracted media");
        assert!(
            error
                .to_string()
                .contains("source classification transition could not be verified"),
            "{error:#}"
        );
        assert_eq!(fs::read(output.join("graph.json")).unwrap(), graph_before);
        assert_eq!(
            fs::read(output.join("manifest.json")).unwrap(),
            manifest_before
        );
    }

    #[test]
    fn explicit_generic_extractor_fails_closed_on_both_mpeg_classification_races() {
        for admitted_media in [false, true] {
            let fixture = Fixture::new();
            let segment = fixture.root.join("segment.ts");
            if admitted_media {
                fs::write(&segment, mpeg_transport_stream_fixture()).expect("write admitted media");
            } else {
                fs::write(&segment, b"export const phantom = 42;\n")
                    .expect("write admitted TypeScript");
            }
            let output = fixture.root.join("graphoxide-out");
            fs::create_dir_all(&output).expect("create managed output");
            fs::write(output.join("graph.json"), b"last-good-graph").expect("seed graph sentinel");
            fs::write(output.join("manifest.json"), b"{}").expect("seed manifest sentinel");
            let graph_before = fs::read(output.join("graph.json")).expect("graph sentinel");
            let manifest_before =
                fs::read(output.join("manifest.json")).expect("manifest sentinel");

            let error = super::extract_files_with_deferred_manifest_and_output(
                std::slice::from_ref(&segment),
                Some(&fixture.root),
                &output,
                true,
                |path, relative| {
                    if admitted_media {
                        fs::write(path, b"export const phantom = 42;\n")
                            .context("replace media with TypeScript")?;
                    } else {
                        fs::write(path, mpeg_transport_stream_fixture())
                            .context("replace TypeScript with media")?;
                    }
                    super::engine::extract_as(path, relative)
                },
            )
            .expect_err("classification/extraction disagreement must abort");
            assert!(
                error
                    .to_string()
                    .contains("source classification transition could not be verified"),
                "{error:#}"
            );
            assert_eq!(fs::read(output.join("graph.json")).unwrap(), graph_before);
            assert_eq!(
                fs::read(output.join("manifest.json")).unwrap(),
                manifest_before
            );
        }
    }

    #[test]
    fn legacy_code_only_reclassification_race_aborts_before_publication() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        let segment = fixture.write("segment.ts", "export const phantom = 42;\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial TypeScript baseline");
        commit_runtime_baseline(initial, &fixture.root, &output);
        let graph_before = fs::read(output.join("graph.json")).expect("baseline graph");
        let manifest_before = fs::read(output.join("manifest.json")).expect("baseline manifest");
        fs::write(&segment, mpeg_transport_stream_fixture()).expect("nominate ambiguous media");
        let mut replace_after_extraction = |_detection: &super::detect::DetectResult| {
            fs::write(&segment, b"export const replacement = true;\n")
                .context("replace media after legacy extraction")?;
            Ok(())
        };

        let error = super::extract_project_with_scan_options_deferred_manifest_impl(
            &fixture.root,
            false,
            &output,
            true,
            &super::detect::DetectOptions::default(),
            Some(&mut replace_after_extraction),
        )
        .expect_err("final ambiguous-media reclassification must be fatal");
        assert!(
            error
                .to_string()
                .contains("source classification transition could not be verified"),
            "{error:#}"
        );
        assert_eq!(fs::read(output.join("graph.json")).unwrap(), graph_before);
        assert_eq!(
            fs::read(output.join("manifest.json")).unwrap(),
            manifest_before
        );
    }

    #[test]
    fn legacy_mpeg_to_typescript_missing_after_extraction_preserves_last_good_artifacts() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        let segment = fixture.root.join("segment.ts");
        fs::write(&segment, mpeg_transport_stream_fixture())
            .expect("write initial MPEG transport stream");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial MPEG baseline");
        commit_runtime_baseline(initial, &fixture.root, &output);
        let graph_before = fs::read(output.join("graph.json")).expect("baseline graph");
        let manifest_before = fs::read(output.join("manifest.json")).expect("baseline manifest");
        fixture.write("segment.ts", "export const phantom = 42;\n");
        let mut remove_after_extraction = |_detection: &super::detect::DetectResult| {
            fs::remove_file(&segment).context("remove TypeScript after legacy extraction")?;
            Ok(())
        };

        let error = super::extract_project_with_scan_options_deferred_manifest_impl(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            Some(&mut remove_after_extraction),
        )
        .expect_err("missing reverse-transition generation must abort");
        assert!(
            error
                .to_string()
                .contains("source classification transition could not be verified"),
            "{error:#}"
        );
        assert_eq!(fs::read(output.join("graph.json")).unwrap(), graph_before);
        assert_eq!(
            fs::read(output.join("manifest.json")).unwrap(),
            manifest_before
        );
    }

    #[test]
    fn legacy_failed_mpeg_transition_row_aborts_before_hybrid_publication() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        let segment = fixture.write("segment.ts", "export const phantom = 42;\n");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial TypeScript baseline");
        commit_runtime_baseline(initial, &fixture.root, &output);
        let graph_before = fs::read(output.join("graph.json")).expect("baseline graph");
        let manifest_before = fs::read(output.join("manifest.json")).expect("baseline manifest");
        fs::write(&segment, mpeg_transport_stream_fixture()).expect("nominate MPEG transition");
        let mut remove_before_extraction = |_detection: &super::detect::DetectResult| {
            fs::remove_file(&segment).context("remove MPEG before legacy extraction")?;
            Ok(())
        };
        let mut restore_after_extraction = |_detection: &super::detect::DetectResult| {
            fs::write(&segment, mpeg_transport_stream_fixture())
                .context("restore MPEG before final classification")?;
            Ok(())
        };

        let error = super::extract_project_with_scan_options_deferred_manifest_impl_with_hooks(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            super::LegacyExtractionHooks {
                before_extraction: Some(&mut remove_before_extraction),
                after_extraction: Some(&mut restore_after_extraction),
                ..super::LegacyExtractionHooks::default()
            },
        )
        .expect_err("a missing transition row must abort the entire deferred build");
        assert!(
            error
                .to_string()
                .contains("source classification transition could not be verified"),
            "{error:#}"
        );
        assert_eq!(fs::read(output.join("graph.json")).unwrap(), graph_before);
        assert_eq!(
            fs::read(output.join("manifest.json")).unwrap(),
            manifest_before
        );
    }

    #[test]
    fn full_runtime_mpeg_read_failure_aborts_before_publication() {
        let fixture = Fixture::new();
        fixture.write(
            "main.ts",
            "import { phantom } from './segment';\nexport const main = phantom;\n",
        );
        let segment = fixture.root.join("segment.ts");
        fs::write(&segment, mpeg_transport_stream_fixture())
            .expect("write initial MPEG transport stream");
        let output = fixture.root.join("graphoxide-out");
        let runtime = runtime_config(32 * 1024 * 1024);
        let initial = super::extract_project_with_runtime_scan_options_deferred_manifest(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
        )
        .expect("initial MPEG baseline");
        commit_runtime_baseline(initial, &fixture.root, &output);
        let graph_before = fs::read(output.join("graph.json")).expect("baseline graph");
        let manifest_before = fs::read(output.join("manifest.json")).expect("baseline manifest");
        let mut remove_after_requests = |_detection: &super::detect::DetectResult| {
            fs::remove_file(&segment).context("remove MPEG after verified request creation")?;
            Ok(())
        };

        let error = super::extract_project_with_runtime_scan_options_deferred_manifest_impl(
            &fixture.root,
            false,
            &output,
            false,
            &super::detect::DetectOptions::default(),
            runtime,
            graphoxide_index_runtime::RuntimeCancellation::new(),
            false,
            Some(&mut remove_after_requests),
            None,
        )
        .expect_err("every nominated MPEG source must be verified before publication");
        assert!(
            error
                .to_string()
                .contains("source classification transition could not be verified"),
            "{error:#}"
        );
        assert_eq!(fs::read(output.join("graph.json")).unwrap(), graph_before);
        assert_eq!(
            fs::read(output.join("manifest.json")).unwrap(),
            manifest_before
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
