//! Bounded, source-safe progress reporting for graph build operations.
//!
//! Progress is an explicit stderr-only contract. It is deliberately separate
//! from [`crate::build_telemetry::BuildTelemetry`], whose stdout schema is
//! stable, and never includes source-derived strings.

use crate::build_telemetry::{BuildMode, BuildOperation, BuildStatus, BuildTelemetry};
use rand::TryRng as _;
use serde::Serialize;
use std::{
    io::{IsTerminal as _, Write as _},
    time::{Duration, Instant},
};

pub const BUILD_PROGRESS_NONCE_ENV: &str = "GRAPHOXIDE_PROGRESS_NONCE";
pub const BUILD_PROGRESS_PREFIX: &str = "[graphoxide-progress] ";
pub const BUILD_PROGRESS_SCHEMA_VERSION: u8 = 1;
pub const BUILD_PROGRESS_NONCE_HEX_LEN: usize = 32;
pub const BUILD_PROGRESS_MAX_VALUE: u64 = 9_007_199_254_740_991;
const COUNTER_EMIT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BuildProgressMode {
    #[default]
    Auto,
    Never,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildProgressPhase {
    Waiting,
    Auditing,
    Scanning,
    Extracting,
    Building,
    Clustering,
    Publishing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BuildProgressRunMode {
    Full,
    Incremental,
    /// The effective mode is selected only after lock-protected baseline work.
    Adaptive,
}

impl From<BuildMode> for BuildProgressRunMode {
    fn from(value: BuildMode) -> Self {
        match value {
            BuildMode::Full => Self::Full,
            BuildMode::Incremental => Self::Incremental,
        }
    }
}

impl BuildProgressRunMode {
    const fn known(self) -> Option<BuildMode> {
        match self {
            Self::Full => Some(BuildMode::Full),
            Self::Incremental => Some(BuildMode::Incremental),
            Self::Adaptive => None,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BuildProgressEvent<'a> {
    Started {
        schema_version: u8,
        run_nonce: &'a str,
        operation: BuildOperation,
        mode: BuildProgressRunMode,
    },
    Phase {
        schema_version: u8,
        run_nonce: &'a str,
        operation: BuildOperation,
        phase: BuildProgressPhase,
        #[serde(skip_serializing_if = "Option::is_none")]
        processed: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
    },
    Completed {
        schema_version: u8,
        run_nonce: &'a str,
        operation: BuildOperation,
        mode: BuildMode,
        status: BuildStatus,
        elapsed_ms: u64,
        stages_ms: CompletedStageDurations,
        files: CompletedFileStats,
        #[serde(skip_serializing_if = "Option::is_none")]
        source_bytes: Option<u64>,
    },
    NotCompleted {
        schema_version: u8,
        run_nonce: &'a str,
        operation: BuildOperation,
        mode: BuildMode,
        reason: BuildProgressTerminalReason,
    },
    Failed {
        schema_version: u8,
        run_nonce: &'a str,
        operation: BuildOperation,
        mode: BuildProgressRunMode,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BuildProgressTerminalReason {
    Queued,
    RefusedShrink,
}

#[derive(Serialize)]
struct CompletedStageDurations {
    scan_extract: u64,
    detect: u64,
    extract: u64,
    build: u64,
    cluster: u64,
    write: u64,
}

impl From<&crate::build_telemetry::BuildStageDurations> for CompletedStageDurations {
    fn from(value: &crate::build_telemetry::BuildStageDurations) -> Self {
        Self {
            scan_extract: bounded(value.scan_extract),
            detect: bounded(value.detect),
            extract: bounded(value.extract),
            build: bounded(value.build),
            cluster: bounded(value.cluster),
            write: bounded(value.write),
        }
    }
}

#[derive(Serialize)]
struct CompletedFileStats {
    indexed: u64,
    changed: u64,
    deleted: u64,
}

/// Emits explicit JSONL or lightweight TTY-only human phase updates.
///
/// A reporter that leaves scope without [`Self::complete`] emits a source-safe
/// `failed` envelope. Abrupt process termination may omit it; consumers must
/// still treat process close as authoritative and never persist partial data.
pub struct BuildProgressReporter {
    operation: BuildOperation,
    mode: BuildProgressRunMode,
    progress: BuildProgressMode,
    json: bool,
    human: bool,
    run_nonce: String,
    started: bool,
    started_at: Option<Instant>,
    finished: bool,
    last_phase: Option<BuildProgressPhase>,
    last_processed: Option<u64>,
    last_total: Option<u64>,
    last_counter_emit: Option<Instant>,
    indexed_inputs: Option<u64>,
}

/// Prepared progress configuration for long-lived commands. A JSON factory
/// owns one validated channel nonce so an entropy failure cannot surface only
/// after a watcher has announced readiness or accepted a mutation.
#[derive(Clone)]
pub struct BuildProgressFactory {
    progress: BuildProgressMode,
    run_nonce: String,
}

impl BuildProgressFactory {
    pub fn new(progress: BuildProgressMode) -> anyhow::Result<Self> {
        let run_nonce = if progress == BuildProgressMode::Json {
            resolve_run_nonce()?
        } else {
            String::new()
        };
        Ok(Self {
            progress,
            run_nonce,
        })
    }

    #[must_use]
    pub fn reporter(&self, operation: BuildOperation, mode: BuildMode) -> BuildProgressReporter {
        BuildProgressReporter::with_prepared_mode(
            operation,
            mode.into(),
            self.progress,
            std::io::stderr().is_terminal(),
            self.run_nonce.clone(),
        )
    }

    #[must_use]
    pub fn adaptive_reporter(&self, operation: BuildOperation) -> BuildProgressReporter {
        BuildProgressReporter::with_prepared_mode(
            operation,
            BuildProgressRunMode::Adaptive,
            self.progress,
            std::io::stderr().is_terminal(),
            self.run_nonce.clone(),
        )
    }
}

impl BuildProgressReporter {
    pub fn new(
        operation: BuildOperation,
        mode: BuildMode,
        progress: BuildProgressMode,
    ) -> anyhow::Result<Self> {
        Self::with_mode(
            operation,
            mode.into(),
            progress,
            std::io::stderr().is_terminal(),
        )
    }

    /// Report a run whose effective full/incremental mode is selected only
    /// after lock-protected baseline inspection, without a duplicate scan.
    pub fn new_adaptive(
        operation: BuildOperation,
        progress: BuildProgressMode,
    ) -> anyhow::Result<Self> {
        Self::with_mode(
            operation,
            BuildProgressRunMode::Adaptive,
            progress,
            std::io::stderr().is_terminal(),
        )
    }

    fn with_mode(
        operation: BuildOperation,
        mode: BuildProgressRunMode,
        progress: BuildProgressMode,
        stderr_is_terminal: bool,
    ) -> anyhow::Result<Self> {
        let factory = BuildProgressFactory::new(progress)?;
        Ok(Self::with_prepared_mode(
            operation,
            mode,
            progress,
            stderr_is_terminal,
            factory.run_nonce,
        ))
    }

    fn with_prepared_mode(
        operation: BuildOperation,
        mode: BuildProgressRunMode,
        progress: BuildProgressMode,
        stderr_is_terminal: bool,
        run_nonce: String,
    ) -> Self {
        Self {
            operation,
            mode,
            progress,
            json: progress == BuildProgressMode::Json,
            human: progress == BuildProgressMode::Auto && stderr_is_terminal,
            run_nonce,
            started: false,
            started_at: None,
            finished: false,
            last_phase: None,
            last_processed: None,
            last_total: None,
            last_counter_emit: None,
            indexed_inputs: None,
        }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.json || self.human
    }

    /// Keep the established path-bearing wait diagnostic only for default
    /// auto mode when stderr is not a terminal. TTY auto and JSON already
    /// render a source-safe Waiting phase; explicit Never remains silent.
    #[must_use]
    pub const fn emits_legacy_wait_diagnostic(&self) -> bool {
        matches!(self.progress, BuildProgressMode::Auto) && !self.human
    }

    pub fn start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.started_at = Some(Instant::now());
        self.emit(&BuildProgressEvent::Started {
            schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
            run_nonce: &self.run_nonce,
            operation: self.operation,
            mode: self.mode,
        });
        if self.human {
            let mode = mode_label(self.mode);
            let _ = if mode.is_empty() {
                writeln!(
                    std::io::stderr().lock(),
                    "[graphoxide] {} build started",
                    operation_label(self.operation),
                )
            } else {
                writeln!(
                    std::io::stderr().lock(),
                    "[graphoxide] {} {mode} build started",
                    operation_label(self.operation),
                )
            };
        }
    }

    pub fn phase(&mut self, phase: BuildProgressPhase) {
        self.phase_inner(phase, None);
    }

    /// Bind the successful corpus/admitted-input count already produced by
    /// extraction. This is intentionally distinct from BuildTelemetry's
    /// changed-input `files.processed` field on incremental project builds.
    pub fn set_indexed_inputs(&mut self, indexed: usize) {
        self.indexed_inputs = Some(bounded_usize(indexed));
    }

    /// Report phase-local work only when the denominator is already known.
    /// Repeated counter observations are emitted at most ten times per second,
    /// except that the first and final observations are always visible.
    pub fn phase_progress(&mut self, phase: BuildProgressPhase, processed: usize, total: usize) {
        let total = bounded_usize(total);
        let processed = bounded_usize(processed).min(total);
        self.phase_inner(phase, Some((processed, total)));
    }

    /// Create a source-safe emitter for a central extraction progress monitor.
    /// Worker threads update counters only; the single monitor owns calls to
    /// this closure and therefore owns all stderr writes.
    pub fn counter_emitter(
        &self,
        phase: BuildProgressPhase,
    ) -> Option<std::sync::Arc<dyn Fn(usize, usize) + Send + Sync + 'static>> {
        if !self.enabled() {
            return None;
        }
        let json = self.json;
        let human = self.human;
        let operation = self.operation;
        let run_nonce = self.run_nonce.clone();
        Some(std::sync::Arc::new(move |processed, total| {
            let total = bounded_usize(total);
            let processed = bounded_usize(processed).min(total);
            emit_json(
                json,
                &BuildProgressEvent::Phase {
                    schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
                    run_nonce: &run_nonce,
                    operation,
                    phase,
                    processed: Some(processed),
                    total: Some(total),
                },
            );
            if human {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "[graphoxide] {} ({processed}/{total})…",
                    phase_label(phase),
                );
            }
        }))
    }

    fn phase_inner(&mut self, phase: BuildProgressPhase, progress: Option<(u64, u64)>) {
        if !self.started || self.finished || !self.enabled() {
            return;
        }
        let changed_phase = self.last_phase != Some(phase);
        if !changed_phase {
            let Some((processed, total)) = progress else {
                return;
            };
            if self.last_total != Some(total)
                || self
                    .last_processed
                    .is_some_and(|previous| processed < previous)
            {
                return;
            }
            let final_observation = processed == total && self.last_processed != Some(processed);
            let interval_elapsed = self
                .last_counter_emit
                .is_none_or(|last| last.elapsed() >= COUNTER_EMIT_INTERVAL);
            if !final_observation && !interval_elapsed {
                self.last_processed = Some(processed);
                return;
            }
        }
        let (processed, total) = progress.map_or((None, None), |(processed, total)| {
            (Some(processed), Some(total))
        });
        self.emit(&BuildProgressEvent::Phase {
            schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
            run_nonce: &self.run_nonce,
            operation: self.operation,
            phase,
            processed,
            total,
        });
        if self.human {
            let suffix = processed
                .zip(total)
                .map_or_else(String::new, |(processed, total)| {
                    format!(" ({processed}/{total})")
                });
            let _ = writeln!(
                std::io::stderr().lock(),
                "[graphoxide] {}{suffix}…",
                phase_label(phase),
            );
        }
        self.last_phase = Some(phase);
        self.last_processed = processed;
        self.last_total = total;
        self.last_counter_emit = progress.map(|_| Instant::now());
    }

    pub fn complete(&mut self, report: &BuildTelemetry, source_bytes: Option<u64>) {
        debug_assert_eq!(report.operation, self.operation);
        debug_assert!(self.mode.known().is_none_or(|mode| mode == report.mode));
        let elapsed_ms = self.started_at.map_or(report.elapsed_ms, |started| {
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        match report.status {
            BuildStatus::Rebuilt | BuildStatus::Unchanged | BuildStatus::NoTrackedChanges => {
                self.emit(&BuildProgressEvent::Completed {
                    schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
                    run_nonce: &self.run_nonce,
                    operation: self.operation,
                    mode: report.mode,
                    status: report.status,
                    elapsed_ms: bounded(elapsed_ms),
                    stages_ms: (&report.stages_ms).into(),
                    files: CompletedFileStats {
                        indexed: self
                            .indexed_inputs
                            .unwrap_or_else(|| bounded_usize(report.files.detected)),
                        changed: bounded_usize(report.files.changed),
                        deleted: bounded_usize(report.files.deleted),
                    },
                    source_bytes: source_bytes.map(bounded),
                });
            }
            BuildStatus::Queued | BuildStatus::RefusedShrink => {
                let reason = match report.status {
                    BuildStatus::Queued => BuildProgressTerminalReason::Queued,
                    BuildStatus::RefusedShrink => BuildProgressTerminalReason::RefusedShrink,
                    _ => unreachable!("matched non-success build status"),
                };
                self.emit(&BuildProgressEvent::NotCompleted {
                    schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
                    run_nonce: &self.run_nonce,
                    operation: self.operation,
                    mode: report.mode,
                    reason,
                });
            }
        }
        self.finished = true;
        if self.human {
            let message = match report.status {
                BuildStatus::Rebuilt | BuildStatus::Unchanged | BuildStatus::NoTrackedChanges => {
                    format!(
                        "build complete in {}",
                        crate::build_telemetry::format_elapsed(elapsed_ms),
                    )
                }
                BuildStatus::Queued => "build not completed; changes were queued".to_owned(),
                BuildStatus::RefusedShrink => {
                    "build not completed; graph shrink was refused".to_owned()
                }
            };
            let _ = writeln!(std::io::stderr().lock(), "[graphoxide] {message}");
        }
    }

    fn emit(&self, event: &BuildProgressEvent<'_>) {
        emit_json(self.json, event);
    }
}

fn emit_json(enabled: bool, event: &BuildProgressEvent<'_>) {
    if !enabled {
        return;
    }
    // Every V1 payload contains only fixed field names, enums, bounded numeric
    // aggregates, and one fixed-format non-source-derived nonce.
    if let Ok(payload) = serde_json::to_string(event) {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{BUILD_PROGRESS_PREFIX}{payload}");
    }
}

impl Drop for BuildProgressReporter {
    fn drop(&mut self) {
        if !self.started || self.finished {
            return;
        }
        self.emit(&BuildProgressEvent::Failed {
            schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
            run_nonce: &self.run_nonce,
            operation: self.operation,
            mode: self.mode,
        });
    }
}

fn resolve_run_nonce() -> anyhow::Result<String> {
    match std::env::var(BUILD_PROGRESS_NONCE_ENV) {
        Ok(value) => {
            anyhow::ensure!(
                valid_run_nonce(&value),
                "{BUILD_PROGRESS_NONCE_ENV} must contain exactly 32 lowercase hexadecimal characters"
            );
            Ok(value)
        }
        Err(std::env::VarError::NotPresent) => generate_run_nonce().ok_or_else(|| {
            anyhow::anyhow!("operating-system entropy is unavailable for --progress=json")
        }),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!(
            "{BUILD_PROGRESS_NONCE_ENV} must contain exactly 32 lowercase hexadecimal characters"
        ),
    }
}

fn valid_run_nonce(value: &str) -> bool {
    value.len() == BUILD_PROGRESS_NONCE_HEX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn generate_run_nonce() -> Option<String> {
    let mut bytes = [0_u8; BUILD_PROGRESS_NONCE_HEX_LEN / 2];
    let mut rng = rand::rngs::SysRng;
    rng.try_fill_bytes(&mut bytes).ok()?;
    Some(hex::encode(bytes))
}

const fn bounded(value: u64) -> u64 {
    if value > BUILD_PROGRESS_MAX_VALUE {
        BUILD_PROGRESS_MAX_VALUE
    } else {
        value
    }
}

fn bounded_usize(value: usize) -> u64 {
    bounded(u64::try_from(value).unwrap_or(u64::MAX))
}

const fn operation_label(operation: BuildOperation) -> &'static str {
    match operation {
        BuildOperation::Extract => "extract",
        BuildOperation::Index => "index",
        BuildOperation::Update => "update",
    }
}

const fn mode_label(mode: BuildProgressRunMode) -> &'static str {
    match mode {
        BuildProgressRunMode::Full => "full",
        BuildProgressRunMode::Incremental => "incremental",
        BuildProgressRunMode::Adaptive => "",
    }
}

const fn phase_label(phase: BuildProgressPhase) -> &'static str {
    match phase {
        BuildProgressPhase::Waiting => "Waiting for build lock",
        BuildProgressPhase::Auditing => "Auditing index coverage",
        BuildProgressPhase::Scanning => "Scanning inputs",
        BuildProgressPhase::Extracting => "Extracting inputs",
        BuildProgressPhase::Building => "Building graph",
        BuildProgressPhase::Clustering => "Clustering communities",
        BuildProgressPhase::Publishing => "Publishing graph",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn completed_event_contains_only_bounded_aggregate_fields() {
        let mut report = BuildTelemetry::new(
            BuildOperation::Index,
            BuildMode::Full,
            BuildStatus::Rebuilt,
            "/private/source/graph.json".into(),
        );
        report.elapsed_ms = u64::MAX;
        report.files.detected = 7;
        report.files.processed = 5;
        report.files.changed = 3;
        report.files.deleted = 1;
        report.warnings.push("secret source warning".into());
        let encoded = serde_json::to_string(&BuildProgressEvent::Completed {
            schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
            run_nonce: NONCE,
            operation: report.operation,
            mode: report.mode,
            status: report.status,
            elapsed_ms: bounded(report.elapsed_ms),
            stages_ms: (&report.stages_ms).into(),
            files: CompletedFileStats {
                indexed: bounded_usize(report.files.detected),
                changed: bounded_usize(report.files.changed),
                deleted: bounded_usize(report.files.deleted),
            },
            source_bytes: Some(bounded(u64::MAX)),
        })
        .unwrap();

        assert!(encoded.len() <= 4096);
        assert!(!encoded.contains("private"));
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("output_path"));
        assert!(!encoded.contains("warnings"));
        assert!(!encoded.contains("nodes"));
        assert!(encoded.contains(&format!("\"source_bytes\":{BUILD_PROGRESS_MAX_VALUE}")));
        assert!(encoded.contains(&format!("\"elapsed_ms\":{BUILD_PROGRESS_MAX_VALUE}")));
    }

    #[test]
    fn phase_wire_has_optional_paired_bounded_counters() {
        assert_eq!(BUILD_PROGRESS_PREFIX, "[graphoxide-progress] ");
        let without_counter = serde_json::to_string(&BuildProgressEvent::Phase {
            schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
            run_nonce: NONCE,
            operation: BuildOperation::Update,
            phase: BuildProgressPhase::Waiting,
            processed: None,
            total: None,
        })
        .unwrap();
        assert_eq!(
            without_counter,
            r#"{"type":"phase","schema_version":1,"run_nonce":"0123456789abcdef0123456789abcdef","operation":"update","phase":"waiting"}"#,
        );
        let with_counter = serde_json::to_string(&BuildProgressEvent::Phase {
            schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
            run_nonce: NONCE,
            operation: BuildOperation::Index,
            phase: BuildProgressPhase::Extracting,
            processed: Some(4),
            total: Some(9),
        })
        .unwrap();
        assert!(with_counter.contains("\"processed\":4,\"total\":9"));
    }

    #[test]
    fn explicit_modes_are_independent_from_stdout_json() {
        let auto = BuildProgressReporter::with_mode(
            BuildOperation::Extract,
            BuildProgressRunMode::Full,
            BuildProgressMode::Auto,
            true,
        )
        .unwrap();
        assert!(auto.human);
        assert!(!auto.json);
        assert!(!auto.emits_legacy_wait_diagnostic());
        let piped_auto = BuildProgressReporter::with_mode(
            BuildOperation::Extract,
            BuildProgressRunMode::Full,
            BuildProgressMode::Auto,
            false,
        )
        .unwrap();
        assert!(piped_auto.emits_legacy_wait_diagnostic());
        let never = BuildProgressReporter::with_mode(
            BuildOperation::Extract,
            BuildProgressRunMode::Full,
            BuildProgressMode::Never,
            true,
        )
        .unwrap();
        assert!(!never.enabled());
        assert!(!never.emits_legacy_wait_diagnostic());
        let json = BuildProgressReporter::with_mode(
            BuildOperation::Extract,
            BuildProgressRunMode::Full,
            BuildProgressMode::Json,
            false,
        )
        .unwrap();
        assert!(!json.emits_legacy_wait_diagnostic());
    }

    #[test]
    fn run_nonce_is_fixed_lowercase_hex() {
        assert!(valid_run_nonce(NONCE));
        assert!(!valid_run_nonce("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!valid_run_nonce("short"));
        assert!(!valid_run_nonce("0123456789abcdef/source-derived!!"));
    }

    #[test]
    fn adaptive_is_limited_to_start_and_failure_events() {
        let started = serde_json::to_string(&BuildProgressEvent::Started {
            schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
            run_nonce: NONCE,
            operation: BuildOperation::Update,
            mode: BuildProgressRunMode::Adaptive,
        })
        .unwrap();
        let failed = serde_json::to_string(&BuildProgressEvent::Failed {
            schema_version: BUILD_PROGRESS_SCHEMA_VERSION,
            run_nonce: NONCE,
            operation: BuildOperation::Update,
            mode: BuildProgressRunMode::Adaptive,
        })
        .unwrap();
        assert!(started.contains("\"mode\":\"adaptive\""));
        assert!(failed.contains("\"mode\":\"adaptive\""));
    }
}
