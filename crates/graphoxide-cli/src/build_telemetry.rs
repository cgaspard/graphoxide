//! Stable, script-friendly reports for graph build operations.
//!
//! Wall-clock measurements deliberately live outside graph artifacts so adding
//! telemetry cannot change deterministic graph bytes or cache identities.

use serde::Serialize;
use std::{path::PathBuf, time::Instant};

pub const BUILD_TELEMETRY_SCHEMA_VERSION: u8 = 1;
/// Schema for the opt-in index-runtime telemetry sidecar.
///
/// This is deliberately independent from [`BuildTelemetry`]. `BuildTelemetry`
/// is the stable stdout contract for `extract --json` and `update --json`; new
/// runtime fields must be added here instead of extending that contract.
pub const INDEX_RUNTIME_TELEMETRY_SCHEMA_VERSION: u8 = 1;
/// Schema for the additive runtime telemetry sidecar emitted by current CLI
/// commands. V1 remains available as a source- and wire-compatible API.
pub const INDEX_RUNTIME_TELEMETRY_V2_SCHEMA_VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildOperation {
    Extract,
    Index,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildMode {
    Full,
    Incremental,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    Rebuilt,
    Unchanged,
    NoTrackedChanges,
    Queued,
    RefusedShrink,
}

/// Durations of mutually useful build phases, in integer milliseconds.
///
/// `scan_extract` is used by the project extraction API, where discovery and
/// cached extraction currently share one call boundary. The update service has
/// separate `detect` and `extract` boundaries. Fields that do not apply remain
/// zero so consumers can rely on a stable object shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BuildStageDurations {
    pub scan_extract: u64,
    pub detect: u64,
    pub extract: u64,
    pub build: u64,
    pub cluster: u64,
    pub write: u64,
}

/// Sub-stage durations within the build phase, in integer milliseconds.
///
/// These fields are a refinement of [`BuildStageDurations::build`]. Consumers
/// that read `stages_ms.build` still get the aggregate total; these fields
/// identify which sub-step dominates on large corpora. Fields that do not
/// apply (e.g. `reconcile_ms` on a full build) remain zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BuildSubStageDurations {
    /// Incremental baseline merge (reconcile prunes, tiering, clone).
    pub reconcile_ms: u64,
    /// Fact staging, node merge, edge/hyperedge resolution.
    pub merge_ms: u64,
    /// Semantic fuzzy deduplication (the hotspot on large repos).
    pub dedup_ms: u64,
    /// Same-topology comparison (watch service fast-path).
    pub topology_ms: u64,
}

impl BuildSubStageDurations {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BuildFileStats {
    pub detected: usize,
    pub processed: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub deleted: usize,
    pub skipped: usize,
    pub unclassified: usize,
    pub sensitive: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BuildGraphStats {
    pub nodes: usize,
    pub edges: usize,
    pub clustered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildTelemetry {
    pub schema_version: u8,
    pub operation: BuildOperation,
    pub mode: BuildMode,
    pub status: BuildStatus,
    pub output_path: String,
    pub elapsed_ms: u64,
    pub stages_ms: BuildStageDurations,
    #[serde(skip_serializing_if = "BuildSubStageDurations::is_default")]
    pub build_substages_ms: BuildSubStageDurations,
    pub files: BuildFileStats,
    pub graph: BuildGraphStats,
    pub passes: usize,
    pub warnings: Vec<String>,
}

impl BuildTelemetry {
    pub fn new(
        operation: BuildOperation,
        mode: BuildMode,
        status: BuildStatus,
        output_path: PathBuf,
    ) -> Self {
        Self {
            schema_version: BUILD_TELEMETRY_SCHEMA_VERSION,
            operation,
            mode,
            status,
            output_path: output_path.to_string_lossy().into_owned(),
            elapsed_ms: 0,
            stages_ms: BuildStageDurations::default(),
            build_substages_ms: BuildSubStageDurations::default(),
            files: BuildFileStats::default(),
            graph: BuildGraphStats::default(),
            passes: 1,
            warnings: Vec::new(),
        }
    }
}

/// The execution architecture used for a build.
///
/// `Legacy` describes the pre-runtime path and is emitted until an index
/// runtime supplies an isolated configuration. Keeping this explicit prevents
/// reports from implying that a dedicated I/O plane was active when it was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeExecutionModel {
    Legacy,
    Isolated,
}

/// I/O implementation selected by the index runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeIoBackend {
    Legacy,
    Threaded,
    IoUring,
}

/// I/O backend requested by the caller before runtime fallback resolution.
///
/// This is separate from [`RuntimeIoBackend`], which records the backend that
/// actually ran.  Keeping both values in the opt-in sidecar prevents a report
/// from claiming that an unavailable request (such as `io_uring` in a portable
/// build) was honored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeIoBackendRequest {
    Auto,
    Threaded,
    IoUring,
}

/// Memory and ownership layout selected for one admitted batch.
///
/// This is additive runtime-sidecar evidence.  It deliberately lives outside
/// [`BuildTelemetry`] so the stable `extract --json` and `update --json`
/// contracts remain byte-for-byte unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeAdmissionTelemetry {
    pub admitted_requests: usize,
    pub effective_io_workers: usize,
    pub effective_compute_workers: usize,
    pub effective_read_batch_bytes: usize,
    pub io_pool_bytes_per_worker: usize,
    pub io_buffers_bytes: usize,
    pub ready_inputs_bytes: usize,
    pub cpu_arenas_bytes: usize,
    pub cache_and_runs_bytes: usize,
    pub query_reserve_bytes: usize,
    pub emergency_reserve_bytes: usize,
}

/// Configuration that affects index throughput and bounded-resource behavior.
///
/// `None` means that the legacy path did not configure or expose that value.
/// This is intentionally not serialized with `skip_serializing_if`: consumers
/// can rely on a stable sidecar shape while distinguishing unknown values from
/// an explicit zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexRuntimeConfiguration {
    pub execution_model: RuntimeExecutionModel,
    pub io_backend: RuntimeIoBackend,
    pub io_backend_request: Option<RuntimeIoBackendRequest>,
    pub io_backend_fallback: Option<String>,
    pub memory_budget_bytes: Option<usize>,
    pub io_workers: Option<usize>,
    pub compute_workers: Option<usize>,
    pub read_batch_bytes: Option<usize>,
    pub cache_partitions: Option<usize>,
    /// Effective per-batch runtime layout. `None` is reserved for the legacy
    /// executor, which has no isolated-admission plane to observe.
    pub admission: Option<RuntimeAdmissionTelemetry>,
}

impl IndexRuntimeConfiguration {
    /// Honest configuration for the existing indexing path.
    pub fn legacy() -> Self {
        Self {
            execution_model: RuntimeExecutionModel::Legacy,
            io_backend: RuntimeIoBackend::Legacy,
            io_backend_request: None,
            io_backend_fallback: None,
            memory_budget_bytes: None,
            io_workers: None,
            compute_workers: None,
            read_batch_bytes: None,
            cache_partitions: None,
            admission: None,
        }
    }
}

/// Runtime-detected CPU capabilities. A detected capability does not promise a
/// particular crate used it; `enabled_kernels` records the kernels explicitly
/// selected by the runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RuntimeSimdTelemetry {
    pub architecture: String,
    pub detected_features: Vec<String>,
    pub enabled_kernels: Vec<String>,
}

/// Truthful per-run evidence for the isolated extraction cache.
///
/// These counters describe completed control-plane decisions rather than
/// speculative probes. They remain outside graph artifacts so observing the
/// cache can never change deterministic graph bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RuntimeCacheTelemetry {
    pub enabled: bool,
    pub metadata_hits: u64,
    pub runtime_hits: u64,
    pub legacy_hits: u64,
    pub misses: u64,
    pub bypasses: u64,
    pub stale_or_corrupt: u64,
    pub probe_failures: u64,
    pub payload_reads_avoided: u64,
    pub parses_avoided: u64,
    pub stores: u64,
    pub already_present: u64,
    pub store_failures: u64,
}

impl From<graphoxide_extract::cache::RuntimeCacheTelemetry> for RuntimeCacheTelemetry {
    fn from(cache: graphoxide_extract::cache::RuntimeCacheTelemetry) -> Self {
        Self {
            enabled: cache.enabled,
            metadata_hits: cache.metadata_hits,
            runtime_hits: cache.runtime_hits,
            legacy_hits: cache.legacy_hits,
            misses: cache.misses,
            bypasses: cache.bypasses,
            stale_or_corrupt: cache.stale_or_corrupt,
            probe_failures: cache.probe_failures,
            payload_reads_avoided: cache.payload_reads_avoided,
            parses_avoided: cache.parses_avoided,
            stores: cache.stores,
            already_present: cache.already_present,
            store_failures: cache.store_failures,
        }
    }
}

/// V2 cache evidence. The original [`RuntimeCacheTelemetry`] DTO remains
/// unchanged for V1 source and wire compatibility.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RuntimeCacheTelemetryV2 {
    pub enabled: bool,
    pub metadata_hits: u64,
    pub runtime_hits: u64,
    pub legacy_hits: u64,
    pub misses: u64,
    pub bypasses: u64,
    pub stale_or_corrupt: u64,
    pub probe_failures: u64,
    pub payload_reads_avoided: u64,
    pub parses_avoided: u64,
    pub stores: u64,
    pub already_present: u64,
    pub store_failures: u64,
    pub payload_bytes_read: u64,
    pub payload_bytes_written: u64,
    pub artifact_bytes_read: u64,
    pub artifact_bytes_written: u64,
    pub peak_in_flight_transfer_bytes: u64,
}

impl RuntimeCacheTelemetryV2 {
    /// Combine stable cache decisions with exact owner-observed byte evidence.
    #[must_use]
    pub fn from_runtime(
        cache: graphoxide_extract::cache::RuntimeCacheTelemetry,
        io: graphoxide_index_runtime::cache::RuntimeCacheIoTelemetry,
    ) -> Self {
        Self {
            enabled: cache.enabled,
            metadata_hits: cache.metadata_hits,
            runtime_hits: cache.runtime_hits,
            legacy_hits: cache.legacy_hits,
            misses: cache.misses,
            bypasses: cache.bypasses,
            stale_or_corrupt: cache.stale_or_corrupt,
            probe_failures: cache.probe_failures,
            payload_reads_avoided: cache.payload_reads_avoided,
            parses_avoided: cache.parses_avoided,
            stores: cache.stores,
            already_present: cache.already_present,
            store_failures: cache.store_failures,
            payload_bytes_read: io.payload_bytes_read,
            payload_bytes_written: io.payload_bytes_written,
            artifact_bytes_read: io.artifact_bytes_read,
            artifact_bytes_written: io.artifact_bytes_written,
            peak_in_flight_transfer_bytes: io.peak_in_flight_transfer_bytes,
        }
    }
}

impl From<RuntimeCacheTelemetry> for RuntimeCacheTelemetryV2 {
    fn from(cache: RuntimeCacheTelemetry) -> Self {
        Self {
            enabled: cache.enabled,
            metadata_hits: cache.metadata_hits,
            runtime_hits: cache.runtime_hits,
            legacy_hits: cache.legacy_hits,
            misses: cache.misses,
            bypasses: cache.bypasses,
            stale_or_corrupt: cache.stale_or_corrupt,
            probe_failures: cache.probe_failures,
            payload_reads_avoided: cache.payload_reads_avoided,
            parses_avoided: cache.parses_avoided,
            stores: cache.stores,
            already_present: cache.already_present,
            store_failures: cache.store_failures,
            ..Self::default()
        }
    }
}

/// Aggregate source I/O evidence from the isolated runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RuntimeIoTelemetry {
    pub sources_selected: u64,
    pub source_bytes_selected: u64,
    pub sources_read: u64,
    pub source_bytes_read: u64,
    pub sources_delivered: u64,
    pub source_bytes_delivered: u64,
    pub source_bytes_avoided: u64,
    pub read_failures: u64,
    /// Peak live ready-input admission credit, including pre-open tickets.
    pub peak_ready_bytes: u64,
    pub peak_ready_items: u64,
}

impl From<graphoxide_index_runtime::RuntimeIoTelemetry> for RuntimeIoTelemetry {
    fn from(io: graphoxide_index_runtime::RuntimeIoTelemetry) -> Self {
        Self {
            sources_selected: io.sources_selected,
            source_bytes_selected: io.source_bytes_selected,
            sources_read: io.sources_read,
            source_bytes_read: io.source_bytes_read,
            sources_delivered: io.sources_delivered,
            source_bytes_delivered: io.source_bytes_delivered,
            source_bytes_avoided: io.source_bytes_avoided,
            read_failures: io.read_failures,
            peak_ready_bytes: io.peak_ready_bytes,
            peak_ready_items: io.peak_ready_items,
        }
    }
}

/// Aggregate parser work after cache decisions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RuntimeWorkTelemetry {
    pub parses: u64,
}

impl From<graphoxide_extract::RuntimeWorkTelemetry> for RuntimeWorkTelemetry {
    fn from(work: graphoxide_extract::RuntimeWorkTelemetry) -> Self {
        Self {
            parses: work.parses,
        }
    }
}

/// Operating-system source used for the process high-water mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePeakRssSource {
    Unavailable,
    GetrusageMaxrssBytes,
    GetrusageMaxrssKib,
}

/// Process-wide peak resident set observed when the sidecar is finalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RuntimeProcessTelemetry {
    pub peak_rss_bytes: Option<u64>,
    pub peak_rss_source: RuntimePeakRssSource,
}

impl Default for RuntimeProcessTelemetry {
    fn default() -> Self {
        Self {
            peak_rss_bytes: None,
            peak_rss_source: RuntimePeakRssSource::Unavailable,
        }
    }
}

impl RuntimeProcessTelemetry {
    /// Detect the process-wide RSS high-water mark without adding sampling
    /// threads or changing the default execution path.
    #[must_use]
    pub fn detect() -> Self {
        #[cfg(any(
            target_os = "android",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "linux",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        {
            if let Some(value) = getrusage_maxrss() {
                return value
                    .checked_mul(1024)
                    .map_or_else(Self::default, |bytes| Self {
                        peak_rss_bytes: Some(bytes),
                        peak_rss_source: RuntimePeakRssSource::GetrusageMaxrssKib,
                    });
            }
        }

        #[cfg(target_os = "macos")]
        {
            if let Some(bytes) = getrusage_maxrss() {
                return Self {
                    peak_rss_bytes: Some(bytes),
                    peak_rss_source: RuntimePeakRssSource::GetrusageMaxrssBytes,
                };
            }
        }

        Self::default()
    }
}

#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn getrusage_maxrss() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable storage for `rusage`, and it is read
    // only after getrusage reports success for the current process.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if status != 0 {
        return None;
    }
    // SAFETY: a successful getrusage initialized the complete output object.
    let usage = unsafe { usage.assume_init() };
    u64::try_from(usage.ru_maxrss).ok()
}

impl RuntimeSimdTelemetry {
    pub fn detect() -> Self {
        let mut detected_features = Vec::new();

        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            if std::is_x86_feature_detected!("sse2") {
                detected_features.push("sse2".to_owned());
            }
            if std::is_x86_feature_detected!("sse4.2") {
                detected_features.push("sse4_2".to_owned());
            }
            if std::is_x86_feature_detected!("avx") {
                detected_features.push("avx".to_owned());
            }
            if std::is_x86_feature_detected!("avx2") {
                detected_features.push("avx2".to_owned());
            }
            if std::is_x86_feature_detected!("avx512f") {
                detected_features.push("avx512f".to_owned());
            }
        }

        #[cfg(target_arch = "aarch64")]
        {
            if std::arch::is_aarch64_feature_detected!("neon") {
                detected_features.push("neon".to_owned());
            }
        }

        Self {
            architecture: std::env::consts::ARCH.to_owned(),
            detected_features,
            // These are byte-plane kernels routed through the shared portable
            // facade. Each dependency owns its CPU feature dispatch, so this
            // reports the enabled implementation rather than asserting a
            // particular ISA path on every invocation.
            enabled_kernels: vec![
                "memchr-runtime-dispatch".to_owned(),
                "simdutf8-runtime-dispatch".to_owned(),
                "blake3-runtime-dispatch".to_owned(),
                "crc32fast-runtime-dispatch".to_owned(),
            ],
        }
    }
}

/// Original opt-in runtime telemetry contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexRuntimeTelemetryV1 {
    pub schema_version: u8,
    pub build: BuildTelemetry,
    pub runtime: IndexRuntimeConfiguration,
    pub cache: RuntimeCacheTelemetry,
    pub simd: RuntimeSimdTelemetry,
}

impl IndexRuntimeTelemetryV1 {
    pub fn legacy(build: BuildTelemetry) -> Self {
        Self {
            schema_version: INDEX_RUNTIME_TELEMETRY_SCHEMA_VERSION,
            build,
            runtime: IndexRuntimeConfiguration::legacy(),
            cache: RuntimeCacheTelemetry::default(),
            simd: RuntimeSimdTelemetry::detect(),
        }
    }

    #[must_use]
    pub fn isolated(build: BuildTelemetry, runtime: IndexRuntimeConfiguration) -> Self {
        Self {
            schema_version: INDEX_RUNTIME_TELEMETRY_SCHEMA_VERSION,
            build,
            runtime,
            cache: RuntimeCacheTelemetry::default(),
            simd: RuntimeSimdTelemetry::detect(),
        }
    }

    #[must_use]
    pub const fn with_cache(mut self, cache: RuntimeCacheTelemetry) -> Self {
        self.cache = cache;
        self
    }
}

/// Additive V2 opt-in runtime telemetry written outside graph artifacts.
///
/// The embedded build report lets benchmark tools correlate the stable build
/// result with runtime configuration without changing the existing stdout JSON
/// schema. Stage durations can overlap in a future isolated pipeline and must
/// not be summed by consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexRuntimeTelemetryV2 {
    pub schema_version: u8,
    pub build: BuildTelemetry,
    pub runtime: IndexRuntimeConfiguration,
    pub io: RuntimeIoTelemetry,
    pub work: RuntimeWorkTelemetry,
    pub cache: RuntimeCacheTelemetryV2,
    pub process: RuntimeProcessTelemetry,
    pub simd: RuntimeSimdTelemetry,
}

impl IndexRuntimeTelemetryV2 {
    pub fn legacy(build: BuildTelemetry) -> Self {
        Self {
            schema_version: INDEX_RUNTIME_TELEMETRY_V2_SCHEMA_VERSION,
            build,
            runtime: IndexRuntimeConfiguration::legacy(),
            io: RuntimeIoTelemetry::default(),
            work: RuntimeWorkTelemetry::default(),
            cache: RuntimeCacheTelemetryV2::default(),
            process: RuntimeProcessTelemetry::detect(),
            simd: RuntimeSimdTelemetry::detect(),
        }
    }

    #[must_use]
    pub fn isolated(build: BuildTelemetry, runtime: IndexRuntimeConfiguration) -> Self {
        Self {
            schema_version: INDEX_RUNTIME_TELEMETRY_V2_SCHEMA_VERSION,
            build,
            runtime,
            io: RuntimeIoTelemetry::default(),
            work: RuntimeWorkTelemetry::default(),
            cache: RuntimeCacheTelemetryV2::default(),
            process: RuntimeProcessTelemetry::detect(),
            simd: RuntimeSimdTelemetry::detect(),
        }
    }

    /// Attach the cache evidence collected by the isolated extraction run.
    #[must_use]
    pub const fn with_cache(mut self, cache: RuntimeCacheTelemetryV2) -> Self {
        self.cache = cache;
        self
    }

    /// Attach aggregate source-I/O evidence collected by extraction.
    #[must_use]
    pub const fn with_io(mut self, io: RuntimeIoTelemetry) -> Self {
        self.io = io;
        self
    }

    /// Attach aggregate parser work collected by extraction.
    #[must_use]
    pub const fn with_work(mut self, work: RuntimeWorkTelemetry) -> Self {
        self.work = work;
        self
    }
}

/// Atomically write the optional runtime sidecar. This intentionally reuses
/// Graphoxide's destination-safe writer so a partial telemetry file can never
/// replace the previous completed report.
pub fn write_runtime_report(
    path: impl AsRef<std::path::Path>,
    report: &IndexRuntimeTelemetryV1,
) -> anyhow::Result<()> {
    graphoxide_core::write_json_atomic(path, report, true)
}

/// Atomically write an additive V2 runtime sidecar.
pub fn write_runtime_report_v2(
    path: impl AsRef<std::path::Path>,
    report: &IndexRuntimeTelemetryV2,
) -> anyhow::Result<()> {
    graphoxide_core::write_json_atomic(path, report, true)
}

pub fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Records wall-clock durations for build sub-stages using the
/// [`BuildSubStage`](graphoxide_graph::BuildSubStage) transition callback.
///
/// The timer is single-threaded (the build pipeline is sequential) and uses
/// `Instant` so it is unaffected by system clock adjustments.
pub struct SubStageTimer {
    last_stage: Option<graphoxide_graph::BuildSubStage>,
    last_instant: Instant,
    reconcile_ms: u64,
    merge_ms: u64,
    dedup_ms: u64,
    topology_ms: u64,
}

impl SubStageTimer {
    pub fn new() -> Self {
        Self {
            last_stage: None,
            last_instant: Instant::now(),
            reconcile_ms: 0,
            merge_ms: 0,
            dedup_ms: 0,
            topology_ms: 0,
        }
    }

    /// Create a callback suitable for the build sub-stage channel.
    pub fn callback(&self) -> impl Fn(graphoxide_graph::BuildSubStage) + '_ {
        move |stage: graphoxide_graph::BuildSubStage| {
            // The timer is mutated through &mut in the owning context; this
            // method is only used to document the expected signature.
            let _ = stage;
        }
    }

    /// Record a sub-stage transition and accumulate the elapsed time into the
    /// appropriate bucket. Call this from the sub-stage callback.
    pub fn tick(&mut self, stage: graphoxide_graph::BuildSubStage) {
        if self.last_stage == Some(stage) {
            return;
        }
        let elapsed = self.last_instant.elapsed().as_millis() as u64;
        match self.last_stage {
            Some(graphoxide_graph::BuildSubStage::MergingNodes)
            | Some(graphoxide_graph::BuildSubStage::ResolvingEdges)
            | Some(graphoxide_graph::BuildSubStage::ResolvingSemanticIds)
            | Some(graphoxide_graph::BuildSubStage::Normalizing)
            | Some(graphoxide_graph::BuildSubStage::ResolvingTwins)
            | Some(graphoxide_graph::BuildSubStage::IndexingAliases)
            | Some(graphoxide_graph::BuildSubStage::ResolvingHyperedges) => {
                self.merge_ms = self.merge_ms.saturating_add(elapsed);
            }
            Some(graphoxide_graph::BuildSubStage::Deduplicating) => {
                self.dedup_ms = self.dedup_ms.saturating_add(elapsed);
            }
            _ => {}
        }
        self.last_stage = Some(stage);
        self.last_instant = Instant::now();
    }

    /// Record the final stage (build complete) and return the accumulated
    /// sub-stage durations.
    pub fn finish(mut self) -> BuildSubStageDurations {
        let elapsed = self.last_instant.elapsed().as_millis() as u64;
        match self.last_stage {
            Some(graphoxide_graph::BuildSubStage::MergingNodes)
            | Some(graphoxide_graph::BuildSubStage::ResolvingEdges)
            | Some(graphoxide_graph::BuildSubStage::ResolvingSemanticIds)
            | Some(graphoxide_graph::BuildSubStage::Normalizing)
            | Some(graphoxide_graph::BuildSubStage::ResolvingTwins)
            | Some(graphoxide_graph::BuildSubStage::IndexingAliases)
            | Some(graphoxide_graph::BuildSubStage::ResolvingHyperedges)
            | Some(graphoxide_graph::BuildSubStage::DisambiguatingLabels) => {
                self.merge_ms = self.merge_ms.saturating_add(elapsed);
            }
            Some(graphoxide_graph::BuildSubStage::Deduplicating) => {
                self.dedup_ms = self.dedup_ms.saturating_add(elapsed);
            }
            None => {}
        }
        BuildSubStageDurations {
            reconcile_ms: self.reconcile_ms,
            merge_ms: self.merge_ms,
            dedup_ms: self.dedup_ms,
            topology_ms: self.topology_ms,
        }
    }

    /// Record a reconcile-phase duration (set externally since reconcile
    /// happens before the build sub-stage callback is active).
    pub fn set_reconcile_ms(&mut self, ms: u64) {
        self.reconcile_ms = ms;
    }

    /// Record a topology-comparison duration (set externally since the
    /// same-topology check happens after the build).
    pub fn set_topology_ms(&mut self, ms: u64) {
        self.topology_ms = ms;
    }
}

impl Default for SubStageTimer {
    fn default() -> Self {
        Self::new()
    }
}

pub fn format_elapsed(milliseconds: u64) -> String {
    format!("{:.3}s", milliseconds as f64 / 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_has_a_stable_machine_readable_shape() {
        let mut report = BuildTelemetry::new(
            BuildOperation::Extract,
            BuildMode::Full,
            BuildStatus::Rebuilt,
            PathBuf::from("graphoxide-out/graph.json"),
        );
        report.elapsed_ms = 12;
        report.stages_ms.scan_extract = 7;
        report.files.detected = 2;
        report.files.processed = 2;
        report.graph.nodes = 3;
        report.graph.edges = 2;

        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["operation"], "extract");
        assert_eq!(value["mode"], "full");
        assert_eq!(value["status"], "rebuilt");
        assert_eq!(value["stages_ms"]["scan_extract"], 7);
        assert_eq!(value["stages_ms"]["detect"], 0);
        assert_eq!(value["files"]["processed"], 2);
        assert_eq!(value["files"]["unclassified"], 0);
        assert_eq!(value["files"]["sensitive"], 0);
        assert_eq!(value["graph"]["nodes"], 3);
        // build_substages_ms is omitted when all-zero (skip_serializing_if)
        assert!(value.get("build_substages_ms").is_none());
    }

    #[test]
    fn build_substages_appear_in_json_when_populated() {
        let mut report = BuildTelemetry::new(
            BuildOperation::Extract,
            BuildMode::Full,
            BuildStatus::Rebuilt,
            PathBuf::from("graphoxide-out/graph.json"),
        );
        report.stages_ms.build = 200;
        report.build_substages_ms = BuildSubStageDurations {
            reconcile_ms: 0,
            merge_ms: 120,
            dedup_ms: 75,
            topology_ms: 5,
        };

        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["build_substages_ms"]["reconcile_ms"], 0);
        assert_eq!(value["build_substages_ms"]["merge_ms"], 120);
        assert_eq!(value["build_substages_ms"]["dedup_ms"], 75);
        assert_eq!(value["build_substages_ms"]["topology_ms"], 5);
        // The aggregate build stage is still present and unchanged.
        assert_eq!(value["stages_ms"]["build"], 200);
    }

    #[test]
    fn sub_stage_timer_accumulates_merge_and_dedup() {
        use graphoxide_graph::BuildSubStage;
        let mut timer = SubStageTimer::new();
        timer.tick(BuildSubStage::MergingNodes);
        std::thread::sleep(std::time::Duration::from_millis(5));
        timer.tick(BuildSubStage::ResolvingEdges);
        std::thread::sleep(std::time::Duration::from_millis(3));
        timer.tick(BuildSubStage::Deduplicating);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let durations = timer.finish();
        assert!(durations.merge_ms >= 5, "merge_ms should be >= 5ms, got {}", durations.merge_ms);
        assert!(durations.dedup_ms >= 2, "dedup_ms should be >= 2ms, got {}", durations.dedup_ms);
        assert_eq!(durations.reconcile_ms, 0);
        assert_eq!(durations.topology_ms, 0);
    }

    #[test]
    fn elapsed_format_is_fixed_precision_seconds() {
        assert_eq!(format_elapsed(12_345), "12.345s");
    }

    #[test]
    fn runtime_sidecar_keeps_build_contract_separate() {
        let build = BuildTelemetry::new(
            BuildOperation::Extract,
            BuildMode::Full,
            BuildStatus::Rebuilt,
            PathBuf::from("graphoxide-out/graph.json"),
        );
        let sidecar = IndexRuntimeTelemetryV1::legacy(build.clone());
        let value = serde_json::to_value(&sidecar).unwrap();

        assert_eq!(INDEX_RUNTIME_TELEMETRY_SCHEMA_VERSION, 1);
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["build"]["schema_version"], 1);
        assert_eq!(value["build"]["operation"], "extract");
        assert_eq!(value["runtime"]["execution_model"], "legacy");
        assert_eq!(value["runtime"]["io_workers"], serde_json::Value::Null);
        assert_eq!(value["runtime"]["admission"], serde_json::Value::Null);
        assert_eq!(value["cache"]["enabled"], false);
        assert_eq!(value["cache"]["runtime_hits"], 0);
        assert!(value.get("io").is_none());
        assert!(value.get("work").is_none());
        assert!(value.get("process").is_none());
        assert!(value["simd"]["detected_features"].is_array());
        assert!(value["simd"]["enabled_kernels"]
            .as_array()
            .expect("portable kernel list")
            .iter()
            .any(|kernel| kernel == "memchr-runtime-dispatch"));
        assert_eq!(serde_json::to_value(build).unwrap()["schema_version"], 1);
    }

    #[test]
    fn runtime_sidecar_v2_is_separate_and_additive() {
        let sidecar = IndexRuntimeTelemetryV2::legacy(BuildTelemetry::new(
            BuildOperation::Extract,
            BuildMode::Full,
            BuildStatus::Rebuilt,
            PathBuf::from("graphoxide-out/graph.json"),
        ));
        let value = serde_json::to_value(sidecar).unwrap();

        assert_eq!(INDEX_RUNTIME_TELEMETRY_V2_SCHEMA_VERSION, 2);
        assert_eq!(value["schema_version"], 2);
        assert_eq!(value["build"]["schema_version"], 1);
        assert_eq!(value["io"]["sources_selected"], 0);
        assert_eq!(value["work"]["parses"], 0);
        assert!(value["process"]["peak_rss_source"].is_string());
    }

    #[test]
    fn runtime_sidecar_v1_cache_dto_remains_wire_exact() {
        let cache = RuntimeCacheTelemetry {
            enabled: true,
            metadata_hits: 1,
            runtime_hits: 2,
            legacy_hits: 3,
            misses: 4,
            bypasses: 5,
            stale_or_corrupt: 6,
            probe_failures: 7,
            payload_reads_avoided: 8,
            parses_avoided: 9,
            stores: 10,
            already_present: 11,
            store_failures: 12,
        };
        let value = serde_json::to_value(
            IndexRuntimeTelemetryV1::legacy(BuildTelemetry::new(
                BuildOperation::Index,
                BuildMode::Full,
                BuildStatus::Rebuilt,
                PathBuf::from("graphoxide-out/graph.json"),
            ))
            .with_cache(cache),
        )
        .unwrap();

        let mut top_level = value
            .as_object()
            .expect("V1 sidecar object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        top_level.sort_unstable();
        assert_eq!(
            top_level,
            ["build", "cache", "runtime", "schema_version", "simd"]
        );
        let mut cache_fields = value["cache"]
            .as_object()
            .expect("V1 cache object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        cache_fields.sort_unstable();
        assert_eq!(
            cache_fields,
            [
                "already_present",
                "bypasses",
                "enabled",
                "legacy_hits",
                "metadata_hits",
                "misses",
                "parses_avoided",
                "payload_reads_avoided",
                "probe_failures",
                "runtime_hits",
                "stale_or_corrupt",
                "store_failures",
                "stores",
            ]
        );
    }

    #[test]
    fn isolated_sidecar_records_its_resolved_runtime() {
        let sidecar = IndexRuntimeTelemetryV2::isolated(
            BuildTelemetry::new(
                BuildOperation::Extract,
                BuildMode::Full,
                BuildStatus::Rebuilt,
                PathBuf::from("graphoxide-out/graph.json"),
            ),
            IndexRuntimeConfiguration {
                execution_model: RuntimeExecutionModel::Isolated,
                io_backend: RuntimeIoBackend::Threaded,
                io_backend_request: Some(RuntimeIoBackendRequest::Auto),
                io_backend_fallback: None,
                memory_budget_bytes: Some(512 * 1024 * 1024),
                io_workers: Some(2),
                compute_workers: Some(4),
                read_batch_bytes: Some(256 * 1024),
                cache_partitions: Some(graphoxide_index_runtime::cache::RUNTIME_CACHE_SHARDS),
                admission: Some(RuntimeAdmissionTelemetry {
                    admitted_requests: 2,
                    effective_io_workers: 2,
                    effective_compute_workers: 2,
                    effective_read_batch_bytes: 256 * 1024,
                    io_pool_bytes_per_worker: 52 * 1024 * 1024,
                    io_buffers_bytes: 104 * 1024 * 1024,
                    ready_inputs_bytes: 104 * 1024 * 1024,
                    cpu_arenas_bytes: 104 * 1024 * 1024,
                    cache_and_runs_bytes: 128 * 1024 * 1024,
                    query_reserve_bytes: 25 * 1024 * 1024,
                    emergency_reserve_bytes: 47 * 1024 * 1024,
                }),
            },
        )
        .with_io(RuntimeIoTelemetry {
            sources_selected: 3,
            source_bytes_selected: 30,
            sources_read: 2,
            source_bytes_read: 20,
            sources_delivered: 2,
            source_bytes_delivered: 20,
            source_bytes_avoided: 10,
            read_failures: 0,
            peak_ready_bytes: 8192,
            peak_ready_items: 2,
        })
        .with_work(RuntimeWorkTelemetry { parses: 2 });
        let value = serde_json::to_value(sidecar).unwrap();
        assert_eq!(value["runtime"]["execution_model"], "isolated");
        assert_eq!(value["runtime"]["io_backend"], "threaded");
        assert_eq!(value["runtime"]["compute_workers"], 4);
        assert_eq!(value["runtime"]["cache_partitions"], 64);
        assert_eq!(value["runtime"]["io_backend_request"], "auto");
        assert_eq!(
            value["runtime"]["admission"]["effective_compute_workers"],
            2
        );
        assert_eq!(value["cache"]["enabled"], false);
        assert_eq!(value["io"]["sources_selected"], 3);
        assert_eq!(value["io"]["source_bytes_avoided"], 10);
        assert_eq!(value["io"]["peak_ready_bytes"], 8192);
        assert_eq!(value["work"]["parses"], 2);
        assert_eq!(value["build"]["schema_version"], 1);
    }

    #[test]
    fn isolated_sidecar_records_truthful_cache_counters() {
        let report = IndexRuntimeTelemetryV2::isolated(
            BuildTelemetry::new(
                BuildOperation::Index,
                BuildMode::Incremental,
                BuildStatus::Rebuilt,
                PathBuf::from("graphoxide-out/graph.json"),
            ),
            IndexRuntimeConfiguration::legacy(),
        )
        .with_cache(RuntimeCacheTelemetryV2 {
            enabled: true,
            metadata_hits: 2,
            runtime_hits: 3,
            legacy_hits: 1,
            misses: 4,
            bypasses: 5,
            stale_or_corrupt: 6,
            probe_failures: 7,
            payload_reads_avoided: 2,
            parses_avoided: 6,
            stores: 8,
            already_present: 9,
            store_failures: 10,
            payload_bytes_read: 11,
            payload_bytes_written: 12,
            artifact_bytes_read: 13,
            artifact_bytes_written: 14,
            peak_in_flight_transfer_bytes: 15,
        });
        let value = serde_json::to_value(report).unwrap();

        assert_eq!(value["cache"]["enabled"], true);
        assert_eq!(value["cache"]["metadata_hits"], 2);
        assert_eq!(value["cache"]["runtime_hits"], 3);
        assert_eq!(value["cache"]["legacy_hits"], 1);
        assert_eq!(value["cache"]["misses"], 4);
        assert_eq!(value["cache"]["bypasses"], 5);
        assert_eq!(value["cache"]["stale_or_corrupt"], 6);
        assert_eq!(value["cache"]["probe_failures"], 7);
        assert_eq!(value["cache"]["payload_reads_avoided"], 2);
        assert_eq!(value["cache"]["parses_avoided"], 6);
        assert_eq!(value["cache"]["stores"], 8);
        assert_eq!(value["cache"]["already_present"], 9);
        assert_eq!(value["cache"]["store_failures"], 10);
        assert_eq!(value["cache"]["payload_bytes_read"], 11);
        assert_eq!(value["cache"]["payload_bytes_written"], 12);
        assert_eq!(value["cache"]["artifact_bytes_read"], 13);
        assert_eq!(value["cache"]["artifact_bytes_written"], 14);
        assert_eq!(value["cache"]["peak_in_flight_transfer_bytes"], 15);
    }

    #[test]
    fn runtime_sidecar_is_written_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.json");
        let report = IndexRuntimeTelemetryV1::legacy(BuildTelemetry::new(
            BuildOperation::Update,
            BuildMode::Incremental,
            BuildStatus::Unchanged,
            PathBuf::from("graphoxide-out/graph.json"),
        ));

        write_runtime_report(&path, &report).unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(written["schema_version"], 1);
        assert_eq!(written["build"]["status"], "unchanged");

        let v2_path = directory.path().join("runtime-v2.json");
        let report = IndexRuntimeTelemetryV2::legacy(BuildTelemetry::new(
            BuildOperation::Update,
            BuildMode::Incremental,
            BuildStatus::Unchanged,
            PathBuf::from("graphoxide-out/graph.json"),
        ));
        write_runtime_report_v2(&v2_path, &report).unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(v2_path).unwrap()).unwrap();
        assert_eq!(written["schema_version"], 2);
    }

    #[test]
    fn process_peak_rss_reports_value_and_units_together() {
        let process = RuntimeProcessTelemetry::detect();
        match process.peak_rss_source {
            RuntimePeakRssSource::Unavailable => assert_eq!(process.peak_rss_bytes, None),
            RuntimePeakRssSource::GetrusageMaxrssBytes
            | RuntimePeakRssSource::GetrusageMaxrssKib => {
                assert!(process.peak_rss_bytes.is_some());
            }
        }
    }
}
