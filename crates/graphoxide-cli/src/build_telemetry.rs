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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildOperation {
    Extract,
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

/// Additive, opt-in runtime telemetry written outside graph artifacts.
///
/// The embedded build report lets benchmark tools correlate the stable build
/// result with runtime configuration without changing the existing stdout JSON
/// schema. Stage durations can overlap in a future isolated pipeline and must
/// not be summed by consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexRuntimeTelemetryV1 {
    pub schema_version: u8,
    pub build: BuildTelemetry,
    pub runtime: IndexRuntimeConfiguration,
    pub simd: RuntimeSimdTelemetry,
}

impl IndexRuntimeTelemetryV1 {
    pub fn legacy(build: BuildTelemetry) -> Self {
        Self {
            schema_version: INDEX_RUNTIME_TELEMETRY_SCHEMA_VERSION,
            build,
            runtime: IndexRuntimeConfiguration::legacy(),
            simd: RuntimeSimdTelemetry::detect(),
        }
    }

    #[must_use]
    pub fn isolated(build: BuildTelemetry, runtime: IndexRuntimeConfiguration) -> Self {
        Self {
            schema_version: INDEX_RUNTIME_TELEMETRY_SCHEMA_VERSION,
            build,
            runtime,
            simd: RuntimeSimdTelemetry::detect(),
        }
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

pub fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
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

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["build"]["schema_version"], 1);
        assert_eq!(value["build"]["operation"], "extract");
        assert_eq!(value["runtime"]["execution_model"], "legacy");
        assert_eq!(value["runtime"]["io_workers"], serde_json::Value::Null);
        assert_eq!(value["runtime"]["admission"], serde_json::Value::Null);
        assert!(value["simd"]["detected_features"].is_array());
        assert!(value["simd"]["enabled_kernels"]
            .as_array()
            .expect("portable kernel list")
            .iter()
            .any(|kernel| kernel == "memchr-runtime-dispatch"));
        assert_eq!(serde_json::to_value(build).unwrap()["schema_version"], 1);
    }

    #[test]
    fn isolated_sidecar_records_its_resolved_runtime() {
        let sidecar = IndexRuntimeTelemetryV1::isolated(
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
        );
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
    }
}
