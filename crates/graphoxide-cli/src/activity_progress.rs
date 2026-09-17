//! Opt-in, authenticated stderr progress for explicit LLM and wiki operations.
//!
//! Only fixed enums, the run nonce, and bounded aggregate counters cross this
//! channel. Source text, model output, paths, and errors never enter it.

use crate::build_progress::{resolve_run_nonce, BuildProgressMode, BUILD_PROGRESS_MAX_VALUE};
use serde::Serialize;
use std::{
    io::Write as _,
    sync::Mutex,
    time::{Duration, Instant},
};

pub const ACTIVITY_PROGRESS_PREFIX: &str = "[graphoxide-activity] ";
const COUNTER_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityOperation {
    Label,
    Wiki,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPhase {
    Waiting,
    Preparing,
    Admitting,
    Authoring,
    Reviewing,
    Labeling,
    Retrying,
    Publishing,
    Serving,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum EventType {
    Started,
    Phase,
    Completed,
    Failed,
}

#[derive(Serialize)]
struct Event<'a> {
    schema_version: u8,
    run_nonce: &'a str,
    #[serde(rename = "type")]
    kind: EventType,
    operation: ActivityOperation,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<ActivityPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    processed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<u64>,
}

#[derive(Default)]
struct PhaseState {
    phase: Option<ActivityPhase>,
    counts: Option<(u64, u64)>,
    last_emit: Option<Instant>,
}

/// Starts immediately, completes only explicitly, and fails on an early return.
/// A mutex serializes parallel LLM batch callbacks and bounds repeated updates.
pub struct ActivityProgressReporter {
    operation: ActivityOperation,
    run_nonce: Option<String>,
    state: Mutex<PhaseState>,
    finished: bool,
}

impl ActivityProgressReporter {
    pub fn new(operation: ActivityOperation, mode: BuildProgressMode) -> anyhow::Result<Self> {
        let reporter = Self {
            operation,
            run_nonce: if mode == BuildProgressMode::Json {
                Some(resolve_run_nonce()?)
            } else {
                None
            },
            state: Mutex::new(PhaseState::default()),
            finished: false,
        };
        reporter.emit(EventType::Started, None, None);
        Ok(reporter)
    }

    pub fn phase(&self, phase: ActivityPhase) {
        self.phase_inner(phase, None);
    }

    pub fn phase_progress(&self, phase: ActivityPhase, processed: usize, total: usize) {
        let total = bounded(total);
        self.phase_inner(phase, Some((bounded(processed).min(total), total)));
    }

    fn phase_inner(&self, phase: ActivityPhase, counts: Option<(u64, u64)>) {
        if self.run_nonce.is_none() || self.finished {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.phase == Some(phase) {
            if state.counts == counts {
                return;
            }
            if let (Some((previous, previous_total)), Some((processed, total))) =
                (state.counts, counts)
            {
                if previous_total != total || processed < previous {
                    return;
                }
                if processed != total
                    && state
                        .last_emit
                        .is_some_and(|last| last.elapsed() < COUNTER_INTERVAL)
                {
                    return;
                }
            }
        }
        self.emit(EventType::Phase, Some(phase), counts);
        state.phase = Some(phase);
        state.counts = counts;
        state.last_emit = Some(Instant::now());
    }

    pub fn complete(&mut self) {
        if self.finished {
            return;
        }
        self.emit(EventType::Completed, None, None);
        self.finished = true;
    }

    fn emit(&self, kind: EventType, phase: Option<ActivityPhase>, counts: Option<(u64, u64)>) {
        let Some(run_nonce) = &self.run_nonce else {
            return;
        };
        let event = Event {
            schema_version: 1,
            run_nonce,
            kind,
            operation: self.operation,
            phase,
            processed: counts.map(|value| value.0),
            total: counts.map(|value| value.1),
        };
        if let Ok(payload) = serde_json::to_string(&event) {
            let _ = writeln!(
                std::io::stderr().lock(),
                "{ACTIVITY_PROGRESS_PREFIX}{payload}"
            );
        }
    }
}

impl Drop for ActivityProgressReporter {
    fn drop(&mut self) {
        if !self.finished {
            self.emit(EventType::Failed, None, None);
        }
    }
}

fn bounded(value: usize) -> u64 {
    u64::try_from(value)
        .unwrap_or(u64::MAX)
        .min(BUILD_PROGRESS_MAX_VALUE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_envelopes_are_bounded_and_source_free() {
        for kind in [
            EventType::Started,
            EventType::Phase,
            EventType::Completed,
            EventType::Failed,
        ] {
            let event = Event {
                schema_version: 1,
                run_nonce: "0123456789abcdef0123456789abcdef",
                kind,
                operation: ActivityOperation::Label,
                phase: matches!(kind, EventType::Phase).then_some(ActivityPhase::Labeling),
                processed: None,
                total: None,
            };
            let encoded = serde_json::to_string(&event).unwrap();
            assert!(encoded.len() < 512);
            let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            assert_eq!(value["schema_version"], 1);
            assert!(value.get("mode").is_none());
            assert!(value.get("files").is_none());
            assert!(value.get("processed").is_none());
        }
        assert!(bounded(usize::MAX) <= BUILD_PROGRESS_MAX_VALUE);
    }
}
