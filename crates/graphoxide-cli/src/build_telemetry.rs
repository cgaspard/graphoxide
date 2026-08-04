//! Stable, script-friendly reports for graph build operations.
//!
//! Wall-clock measurements deliberately live outside graph artifacts so adding
//! telemetry cannot change deterministic graph bytes or cache identities.

use serde::Serialize;
use std::{path::PathBuf, time::Instant};

pub const BUILD_TELEMETRY_SCHEMA_VERSION: u8 = 1;

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
}
