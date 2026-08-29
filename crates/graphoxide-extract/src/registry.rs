//! Git-tracked, metadata-only source registry.
//!
//! This is deliberately separate from the project-local `catalog.json` format.
//! A registry names logical origins and immutable captures; local filesystem
//! bindings and scan state belong in disposable local state, never in Git.

use anyhow::{anyhow, ensure, Context as _};
use serde::{
    de::{Error as _, MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fmt, fs,
    io::Read as _,
    path::{Component, Path, PathBuf},
};

const REGISTRY_FILE: &str = "registry.json";
const REGISTRY_VERSION: u64 = 1;
const MAX_RECORD_BYTES: u64 = 256 * 1024;
const MAX_RECORDS: usize = 1_000_000;
const MAX_ID_BYTES: usize = 256;
const MAX_METADATA_BYTES: usize = 4_096;

/// The stable two-hex-character shard for one source identifier.
pub fn shard_for_source_id(source_id: &str) -> String {
    hex::encode(Sha256::digest(source_id.as_bytes()))[..2].to_owned()
}

/// A validated snapshot of one registry tree.
#[derive(Debug)]
pub struct RegistrySnapshot {
    catalog_id: String,
    tree_sha256: String,
    origins: BTreeMap<String, RegistryOrigin>,
    sources: BTreeMap<String, RegistrySource>,
    captures: BTreeMap<String, RegistryCapture>,
    runs: Vec<RegistryRun>,
    reviews: Vec<RegistryReview>,
    freshness_policy: Option<FreshnessPolicy>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryOrigin {
    pub version: u64,
    pub origin_id: String,
    pub kind: String,
    pub logical_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrySource {
    pub version: u64,
    pub source_id: String,
    pub origin_id: String,
    pub relative_path: String,
    pub state: RegistrySourceState,
    pub active_capture_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegistrySourceState {
    Active,
    Retired,
    PendingVerification,
}

impl RegistrySourceState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retired => "retired",
            Self::PendingVerification => "pending-verification",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryCapture {
    pub version: u64,
    pub capture_id: String,
    pub source_id: String,
    pub relative_path: String,
    pub sha256: String,
    pub observed_at: String,
    pub representation: String,
}

/// The resolved active head used by extraction and deterministic rendering.
#[derive(Clone, Debug)]
pub struct RegistryActiveCapture {
    source: RegistrySource,
    capture: RegistryCapture,
}

impl RegistryActiveCapture {
    pub fn source(&self) -> &RegistrySource {
        &self.source
    }

    pub fn capture(&self) -> &RegistryCapture {
        &self.capture
    }

    pub fn relative_path(&self) -> &str {
        &self.source.relative_path
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RegistryHeader {
    version: u64,
    catalog_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RegistryRun {
    version: u64,
    run_id: String,
    source_id: String,
    capture_id: String,
    stage: String,
    status: RegistryRunStatus,
    processor: String,
    started_at: String,
    finished_at: Option<String>,
    actor: Option<String>,
    agent_run_id: Option<String>,
    model_requested: Option<String>,
    model_reported: Option<String>,
    profile_digest: Option<String>,
    prompt_schema_digest: Option<String>,
    evidence_manifest_digest: Option<String>,
    output_digest: Option<String>,
    provider_request_id: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_microunits: Option<u64>,
    latency_ms: Option<u64>,
    retry_count: Option<u64>,
    error_class: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegistryRunStatus {
    Succeeded,
    Failed,
}

/// The latest safe processing metadata for one capture/stage pair.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RegistryRunSummary {
    pub source_id: String,
    pub capture_id: String,
    pub stage: String,
    pub status: RegistryRunStatus,
    pub processor: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub actor: Option<String>,
    pub agent_run_id: Option<String>,
    pub model_requested: Option<String>,
    pub model_reported: Option<String>,
    pub provider_request_id: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_microunits: Option<u64>,
    pub latency_ms: Option<u64>,
    pub retry_count: Option<u64>,
    pub error_class: Option<String>,
}

impl From<&RegistryRun> for RegistryRunSummary {
    fn from(run: &RegistryRun) -> Self {
        Self {
            source_id: run.source_id.clone(),
            capture_id: run.capture_id.clone(),
            stage: run.stage.clone(),
            status: run.status,
            processor: run.processor.clone(),
            started_at: run.started_at.clone(),
            finished_at: run.finished_at.clone(),
            actor: run.actor.clone(),
            agent_run_id: run.agent_run_id.clone(),
            model_requested: run.model_requested.clone(),
            model_reported: run.model_reported.clone(),
            provider_request_id: run.provider_request_id.clone(),
            input_tokens: run.input_tokens,
            output_tokens: run.output_tokens,
            cost_microunits: run.cost_microunits,
            latency_ms: run.latency_ms,
            retry_count: run.retry_count,
            error_class: run.error_class.clone(),
        }
    }
}

/// Versioned policy for deciding which active captures need model work.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FreshnessPolicy {
    pub version: u64,
    pub model_stage: String,
    pub model_max_age_seconds: u64,
    #[serde(default)]
    pub source_priorities: BTreeMap<String, i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RegistryReview {
    version: u64,
    review_id: String,
    decision: RegistryReviewDecision,
    reviewer: String,
    reviewed_at: String,
    plan_sha256: String,
    capture_set_sha256: String,
    draft_sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegistryReviewDecision {
    Approved,
    Rejected,
}

/// One immutable review decision that can promote a matching article draft.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RegistryReviewSummary {
    pub review_id: String,
    pub decision: RegistryReviewDecision,
    pub reviewer: String,
    pub reviewed_at: String,
    pub plan_sha256: String,
    pub capture_set_sha256: String,
    pub draft_sha256: String,
}

impl From<&RegistryReview> for RegistryReviewSummary {
    fn from(review: &RegistryReview) -> Self {
        Self {
            review_id: review.review_id.clone(),
            decision: review.decision,
            reviewer: review.reviewer.clone(),
            reviewed_at: review.reviewed_at.clone(),
            plan_sha256: review.plan_sha256.clone(),
            capture_set_sha256: review.capture_set_sha256.clone(),
            draft_sha256: review.draft_sha256.clone(),
        }
    }
}

impl RegistrySnapshot {
    /// Load and validate one complete registry tree. No source bytes are read.
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let root = checked_registry_root(root)?;
        let records = collect_records(&root)?;
        ensure!(
            records.len() <= MAX_RECORDS,
            "registry exceeds the {MAX_RECORDS}-record limit"
        );
        let tree_sha256 = registry_tree_sha256(&records);

        let header_value = records
            .get(Path::new(REGISTRY_FILE))
            .context("registry.json is missing")?;
        let header: RegistryHeader = deserialize_record(header_value, REGISTRY_FILE)?;
        ensure!(
            header.version == REGISTRY_VERSION,
            "unsupported registry version"
        );
        validate_id("catalog_id", &header.catalog_id)?;

        let mut origins = BTreeMap::new();
        let mut sources = BTreeMap::new();
        let mut captures = BTreeMap::new();
        let mut runs = Vec::new();
        let mut reviews = Vec::new();
        let mut freshness_policy = None;

        for (path, value) in records {
            if path == Path::new(REGISTRY_FILE) {
                continue;
            }
            match classify_record_path(&path)? {
                RecordPath::Origin { origin_id } => {
                    let origin: RegistryOrigin = deserialize_record(&value, &display_path(&path))?;
                    validate_origin(&origin, &origin_id)?;
                    ensure!(
                        origins.insert(origin.origin_id.clone(), origin).is_none(),
                        "duplicate registry origin_id"
                    );
                }
                RecordPath::Source { shard, source_id } => {
                    let source: RegistrySource = deserialize_record(&value, &display_path(&path))?;
                    validate_source(&source, &source_id, &shard)?;
                    ensure!(
                        sources.insert(source.source_id.clone(), source).is_none(),
                        "duplicate registry source_id"
                    );
                }
                RecordPath::Capture {
                    shard,
                    source_id,
                    capture_id,
                } => {
                    let capture: RegistryCapture =
                        deserialize_record(&value, &display_path(&path))?;
                    validate_capture(&capture, &source_id, &capture_id, &shard)?;
                    ensure!(
                        captures
                            .insert(capture.capture_id.clone(), capture)
                            .is_none(),
                        "duplicate registry capture_id"
                    );
                }
                RecordPath::Run {
                    shard,
                    source_id,
                    capture_id,
                    stage,
                    run_id,
                } => {
                    let run: RegistryRun = deserialize_record(&value, &display_path(&path))?;
                    validate_run(&run, &source_id, &capture_id, &stage, &run_id, &shard)?;
                    runs.push(run);
                }
                RecordPath::Review { review_id } => {
                    let review: RegistryReview = deserialize_record(&value, &display_path(&path))?;
                    validate_review(&review, &review_id)?;
                    reviews.push(review);
                }
                RecordPath::Freshness => {
                    let policy: FreshnessPolicy = deserialize_record(&value, &display_path(&path))?;
                    validate_freshness_policy(&policy)?;
                    ensure!(
                        freshness_policy.replace(policy).is_none(),
                        "duplicate registry freshness policy"
                    );
                }
                RecordPath::Schema => validate_control_value(&value)?,
            }
        }

        validate_closure(&origins, &sources, &captures, &runs, &reviews)?;
        Ok(Self {
            catalog_id: header.catalog_id,
            tree_sha256,
            origins,
            sources,
            captures,
            runs,
            reviews,
            freshness_policy,
        })
    }

    pub fn catalog_id(&self) -> &str {
        &self.catalog_id
    }

    /// Stable digest of every validated path and canonical record in this tree.
    pub fn tree_sha256(&self) -> &str {
        &self.tree_sha256
    }

    pub fn origins(&self) -> &BTreeMap<String, RegistryOrigin> {
        &self.origins
    }

    pub fn sources(&self) -> &BTreeMap<String, RegistrySource> {
        &self.sources
    }

    /// Return the latest immutable run for one active capture and stage.
    pub fn latest_run_for_capture(
        &self,
        capture_id: &str,
        stage: &str,
    ) -> Option<RegistryRunSummary> {
        self.runs
            .iter()
            .filter(|run| run.capture_id == capture_id && run.stage == stage)
            .max_by(|left, right| {
                left.started_at
                    .cmp(&right.started_at)
                    .then_with(|| left.run_id.cmp(&right.run_id))
            })
            .map(RegistryRunSummary::from)
    }

    /// Return the latest immutable run for every stage of one capture.
    pub fn latest_runs_for_capture(&self, capture_id: &str) -> Vec<RegistryRunSummary> {
        let mut latest = BTreeMap::<&str, &RegistryRun>::new();
        for run in self.runs.iter().filter(|run| run.capture_id == capture_id) {
            let replace = latest.get(run.stage.as_str()).is_none_or(|existing| {
                run.started_at > existing.started_at
                    || (run.started_at == existing.started_at && run.run_id > existing.run_id)
            });
            if replace {
                latest.insert(run.stage.as_str(), run);
            }
        }
        latest.into_values().map(RegistryRunSummary::from).collect()
    }

    /// Return immutable review decisions in deterministic review-record order.
    pub fn reviews(&self) -> Vec<RegistryReviewSummary> {
        let mut reviews = self
            .reviews
            .iter()
            .map(RegistryReviewSummary::from)
            .collect::<Vec<_>>();
        reviews.sort_by(|left, right| {
            left.reviewed_at
                .cmp(&right.reviewed_at)
                .then_with(|| left.review_id.cmp(&right.review_id))
        });
        reviews
    }

    pub fn freshness_policy(&self) -> Option<&FreshnessPolicy> {
        self.freshness_policy.as_ref()
    }

    pub fn captures(&self) -> &BTreeMap<String, RegistryCapture> {
        &self.captures
    }

    pub fn active_captures(&self) -> Vec<RegistryActiveCapture> {
        self.sources
            .values()
            .filter(|source| source.state == RegistrySourceState::Active)
            .filter_map(|source| {
                let capture_id = source.active_capture_id.as_ref()?;
                self.captures
                    .get(capture_id)
                    .cloned()
                    .map(|capture| RegistryActiveCapture {
                        source: source.clone(),
                        capture,
                    })
            })
            .collect()
    }
}

/// Initialize an empty Registry v1 directory. The caller owns Git commit and review.
pub fn initialize_tree(root: &Path, catalog_id: &str) -> anyhow::Result<RegistrySnapshot> {
    validate_id("catalog_id", catalog_id)?;
    prepare_empty_registry_root(root)?;
    write_new_record(
        root,
        Path::new(REGISTRY_FILE),
        &RegistryHeader {
            version: REGISTRY_VERSION,
            catalog_id: catalog_id.to_owned(),
        },
    )?;
    RegistrySnapshot::load(root)
}

/// Add an origin containing only a logical name; its real location is local state.
pub fn add_origin(root: &Path, origin: RegistryOrigin) -> anyhow::Result<RegistrySnapshot> {
    validate_origin(&origin, &origin.origin_id)?;
    let path = PathBuf::from("origins").join(format!("{}.json", origin.origin_id));
    write_new_record(root, &path, &origin)?;
    RegistrySnapshot::load(root)
}

/// Start tracking one source without reading or storing its bytes.
///
/// The source remains pending until a verified scan appends a capture. This
/// keeps an interrupted registry change valid and makes source selection a
/// reviewable Git artifact before any local binding is used.
pub fn track_source(
    root: &Path,
    source_id: &str,
    origin_id: &str,
    relative_path: &str,
) -> anyhow::Result<RegistrySnapshot> {
    let snapshot = RegistrySnapshot::load(root)?;
    ensure!(
        snapshot.origins().contains_key(origin_id),
        "registry source references an unknown origin"
    );
    let source = RegistrySource {
        version: REGISTRY_VERSION,
        source_id: source_id.to_owned(),
        origin_id: origin_id.to_owned(),
        relative_path: relative_path.to_owned(),
        state: RegistrySourceState::PendingVerification,
        active_capture_id: None,
    };
    validate_source(&source, source_id, &shard_for_source_id(source_id))?;
    write_new_record(root, &source_record_path(source_id), &source)?;
    RegistrySnapshot::load(root)
}

/// Add an immutable capture and atomically advance its source head at Git-commit granularity.
///
/// For a first capture, the pending source head is written first, preserving a
/// valid registry even if the process stops before the capture is available.
pub fn append_capture_and_activate(
    root: &Path,
    capture: RegistryCapture,
    origin_id: Option<&str>,
) -> anyhow::Result<RegistrySnapshot> {
    let snapshot = RegistrySnapshot::load(root)?;
    validate_capture(
        &capture,
        &capture.source_id,
        &capture.capture_id,
        &shard_for_source_id(&capture.source_id),
    )?;
    let capture_path = capture_record_path(&capture);
    if let Some(existing) = snapshot.sources().get(&capture.source_id) {
        ensure!(
            origin_id.is_none_or(|origin| origin == existing.origin_id),
            "registry source origin cannot change while appending a capture"
        );
    } else {
        let origin_id = origin_id.context("first capture requires origin_id")?;
        ensure!(
            snapshot.origins().contains_key(origin_id),
            "registry source references an unknown origin"
        );
        let pending = RegistrySource {
            version: REGISTRY_VERSION,
            source_id: capture.source_id.clone(),
            origin_id: origin_id.to_owned(),
            relative_path: capture.relative_path.clone(),
            state: RegistrySourceState::PendingVerification,
            active_capture_id: None,
        };
        write_new_record(root, &source_record_path(&pending.source_id), &pending)?;
        RegistrySnapshot::load(root)?;
    }
    write_new_record(root, &capture_path, &capture)?;
    let snapshot = RegistrySnapshot::load(root)?;
    let existing = snapshot
        .sources()
        .get(&capture.source_id)
        .context("capture source disappeared during registry update")?;
    let head = RegistrySource {
        version: REGISTRY_VERSION,
        source_id: existing.source_id.clone(),
        origin_id: existing.origin_id.clone(),
        relative_path: capture.relative_path.clone(),
        state: RegistrySourceState::Active,
        active_capture_id: Some(capture.capture_id.clone()),
    };
    write_replacing_record(root, &source_record_path(&head.source_id), &head)?;
    RegistrySnapshot::load(root)
}

/// Retire a source without deleting capture, graph, or review history.
pub fn retire_source(root: &Path, source_id: &str) -> anyhow::Result<RegistrySnapshot> {
    let snapshot = RegistrySnapshot::load(root)?;
    let source = snapshot
        .sources()
        .get(source_id)
        .context("registry source does not exist")?;
    let retired = RegistrySource {
        version: REGISTRY_VERSION,
        source_id: source.source_id.clone(),
        origin_id: source.origin_id.clone(),
        relative_path: source.relative_path.clone(),
        state: RegistrySourceState::Retired,
        active_capture_id: None,
    };
    write_replacing_record(root, &source_record_path(source_id), &retired)?;
    RegistrySnapshot::load(root)
}

/// Change a source's logical location without claiming its bytes are unchanged.
///
/// The old immutable capture remains in history, while the head becomes
/// pending verification until a locally scanned digest is explicitly published.
pub fn rename_source(
    root: &Path,
    source_id: &str,
    relative_path: &str,
) -> anyhow::Result<RegistrySnapshot> {
    let snapshot = RegistrySnapshot::load(root)?;
    let source = snapshot
        .sources()
        .get(source_id)
        .context("registry source does not exist")?;
    let pending = RegistrySource {
        version: REGISTRY_VERSION,
        source_id: source.source_id.clone(),
        origin_id: source.origin_id.clone(),
        relative_path: relative_path.to_owned(),
        state: RegistrySourceState::PendingVerification,
        active_capture_id: None,
    };
    validate_source(&pending, source_id, &shard_for_source_id(source_id))?;
    write_replacing_record(root, &source_record_path(source_id), &pending)?;
    RegistrySnapshot::load(root)
}

/// Restore or explicitly resolve a source head to an existing capture.
pub fn activate_capture(
    root: &Path,
    source_id: &str,
    capture_id: &str,
) -> anyhow::Result<RegistrySnapshot> {
    let snapshot = RegistrySnapshot::load(root)?;
    let source = snapshot
        .sources()
        .get(source_id)
        .context("registry source does not exist")?;
    let capture = snapshot
        .captures()
        .get(capture_id)
        .context("registry capture does not exist")?;
    ensure!(
        capture.source_id == source.source_id,
        "registry capture belongs to another source"
    );
    let active = RegistrySource {
        version: REGISTRY_VERSION,
        source_id: source.source_id.clone(),
        origin_id: source.origin_id.clone(),
        relative_path: capture.relative_path.clone(),
        state: RegistrySourceState::Active,
        active_capture_id: Some(capture.capture_id.clone()),
    };
    write_replacing_record(root, &source_record_path(source_id), &active)?;
    RegistrySnapshot::load(root)
}

/// Append one immutable, secret-free processing provenance record.
///
/// The JSON schema deliberately has no field for raw prompts, source text, or
/// credentials; strict parsing rejects them before anything is written.
pub fn record_run(root: &Path, bytes: &[u8]) -> anyhow::Result<RegistrySnapshot> {
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "registry run record exceeds the {MAX_RECORD_BYTES}-byte limit"
    );
    let value = parse_strict_json(bytes)?;
    let run: RegistryRun = deserialize_record(&value, "registry run input")?;
    validate_run(
        &run,
        &run.source_id,
        &run.capture_id,
        &run.stage,
        &run.run_id,
        &shard_for_source_id(&run.source_id),
    )?;
    let snapshot = RegistrySnapshot::load(root)?;
    let capture = snapshot
        .captures()
        .get(&run.capture_id)
        .context("registry run references an unknown capture")?;
    ensure!(
        capture.source_id == run.source_id,
        "registry run source/capture closure is invalid"
    );
    let path = PathBuf::from("runs")
        .join(shard_for_source_id(&run.source_id))
        .join(&run.source_id)
        .join(&run.capture_id)
        .join(&run.stage)
        .join(format!("{}.json", run.run_id));
    write_new_record(root, &path, &run)?;
    RegistrySnapshot::load(root)
}

/// Append one immutable, secret-free article review decision.
///
/// The record binds a reviewed article draft to a plan and capture-set digest;
/// the materializer verifies that those digests still match before promotion.
pub fn record_review(root: &Path, bytes: &[u8]) -> anyhow::Result<RegistrySnapshot> {
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "registry review record exceeds the {MAX_RECORD_BYTES}-byte limit"
    );
    let value = parse_strict_json(bytes)?;
    let review: RegistryReview = deserialize_record(&value, "registry review input")?;
    validate_review(&review, &review.review_id)?;
    let path = PathBuf::from("reviews").join(format!("{}.json", review.review_id));
    write_new_record(root, &path, &review)?;
    RegistrySnapshot::load(root)
}

fn source_record_path(source_id: &str) -> PathBuf {
    PathBuf::from("sources")
        .join(shard_for_source_id(source_id))
        .join(format!("{source_id}.json"))
}

fn capture_record_path(capture: &RegistryCapture) -> PathBuf {
    PathBuf::from("captures")
        .join(shard_for_source_id(&capture.source_id))
        .join(&capture.source_id)
        .join(format!("{}.json", capture.capture_id))
}

fn prepare_empty_registry_root(root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root).with_context(|| format!("create registry root {}", root.display()))?;
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("inspect registry root {}", root.display()))?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "registry root must be a non-symlinked directory"
    );
    ensure!(
        fs::read_dir(root)?.next().is_none(),
        "registry root must be empty for initialization"
    );
    Ok(())
}

fn write_new_record<T: Serialize>(root: &Path, relative: &Path, record: &T) -> anyhow::Result<()> {
    let path = checked_record_path(root, relative)?;
    ensure!(
        fs::symlink_metadata(&path).is_err(),
        "registry record already exists"
    );
    write_record(&path, record, true)
}

fn write_replacing_record<T: Serialize>(
    root: &Path,
    relative: &Path,
    record: &T,
) -> anyhow::Result<()> {
    let path = checked_record_path(root, relative)?;
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspect registry record {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "registry source head must be an existing regular file"
    );
    write_record(&path, record, false)
}

fn checked_record_path(root: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
    let root = checked_registry_root(root)?;
    ensure!(
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "registry record path must be relative and normalized"
    );
    let parent = root.join(relative.parent().unwrap_or_else(|| Path::new("")));
    fs::create_dir_all(&parent)?;
    let parent_metadata = fs::symlink_metadata(&parent)?;
    ensure!(
        parent_metadata.file_type().is_dir() && !parent_metadata.file_type().is_symlink(),
        "registry record parent must be a non-symlinked directory"
    );
    let parent = fs::canonicalize(parent)?;
    ensure!(
        parent.starts_with(&root),
        "registry record parent escaped root"
    );
    Ok(parent.join(
        relative
            .file_name()
            .context("registry record has no file name")?,
    ))
}

fn write_record<T: Serialize>(path: &Path, record: &T, create_new: bool) -> anyhow::Result<()> {
    let value = serde_json::to_value(record).context("serialize registry record")?;
    let bytes = format!("{}\n", canonical_json(&value)).into_bytes();
    if create_new {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(path)?;
        use std::io::Write as _;
        file.write_all(&bytes)?;
        file.sync_all()?;
        return Ok(());
    }
    let parent = path.parent().context("registry record has no parent")?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(&temporary)?;
    use std::io::Write as _;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn registry_tree_sha256(records: &BTreeMap<PathBuf, Value>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"graphoxide-registry-tree-v1\0");
    for (path, value) in records {
        digest.update(display_path(path).as_bytes());
        digest.update(b"\0");
        digest.update(canonical_json(value).as_bytes());
        digest.update(b"\0");
    }
    hex::encode(digest.finalize())
}

fn checked_registry_root(root: &Path) -> anyhow::Result<PathBuf> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("inspect registry root {}", root.display()))?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "registry root must be a non-symlinked directory"
    );
    fs::canonicalize(root).with_context(|| format!("resolve registry root {}", root.display()))
}

fn collect_records(root: &Path) -> anyhow::Result<BTreeMap<PathBuf, Value>> {
    let mut paths = Vec::new();
    collect_files(root, root, &mut paths)?;
    let mut records = BTreeMap::new();
    for path in paths {
        let relative = path
            .strip_prefix(root)
            .context("registry record escaped root")?
            .to_path_buf();
        let value = read_canonical_json(&path)?;
        ensure!(
            records.insert(relative, value).is_none(),
            "duplicate registry record path"
        );
    }
    Ok(records)
}

fn collect_files(root: &Path, directory: &Path, paths: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("read registry directory {}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        // A Registry v1 tree is normally the root of a dedicated Git
        // repository. Git's private administration entry is not registry
        // data, including when a linked checkout represents it as a file.
        if directory == root && entry.file_name() == ".git" {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect registry path {}", path.display()))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "registry must not contain symlinks"
        );
        if metadata.file_type().is_dir() {
            collect_files(root, &path, paths)?;
        } else if metadata.file_type().is_file() {
            ensure!(path.starts_with(root), "registry path escaped root");
            paths.push(path);
        } else {
            anyhow::bail!("registry must contain only regular files and directories");
        }
    }
    Ok(())
}

fn read_canonical_json(path: &Path) -> anyhow::Result<Value> {
    let unsafe_input = || anyhow!("registry record changed or is unsafe");
    let mut file = crate::detect::open_control_file_nofollow(path).map_err(|_| unsafe_input())?;
    let opened = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    ensure!(
        opened.length_bytes() <= MAX_RECORD_BYTES,
        "registry record exceeds the {MAX_RECORD_BYTES}-byte limit"
    );
    let mut bytes = Vec::with_capacity(opened.length_bytes() as usize);
    file.by_ref()
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read registry record {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "registry record exceeds the {MAX_RECORD_BYTES}-byte limit"
    );
    let after = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    let current = crate::detect::open_control_file_nofollow(path).map_err(|_| unsafe_input())?;
    let current = graphoxide_index_runtime::validate_opened_regular_single_link(&current)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    ensure!(
        opened == after && opened == current && opened.length_bytes() == bytes.len() as u64,
        "registry record changed or is unsafe"
    );

    let value = parse_strict_json(&bytes).with_context(|| format!("parse {}", path.display()))?;
    let canonical = format!("{}\n", canonical_json(&value));
    ensure!(
        bytes == canonical.as_bytes(),
        "registry record must use canonical JSON with one trailing newline"
    );
    Ok(value)
}

fn parse_strict_json(bytes: &[u8]) -> anyhow::Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictJsonValue::deserialize(&mut deserializer)
        .map_err(|error| anyhow!(error.to_string()))?
        .0;
    deserializer
        .end()
        .map_err(|error| anyhow!(error.to_string()))?;
    Ok(value)
}

struct StrictJsonValue(Value);

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate keys or floating-point numbers")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(StrictJsonValue(Value::Null))
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(StrictJsonValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(StrictJsonValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(StrictJsonValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Err(E::custom(
            "registry JSON must not contain floating-point numbers",
        ))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(StrictJsonValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(StrictJsonValue(Value::String(value)))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictJsonValue>()? {
            values.push(value.0);
        }
        Ok(StrictJsonValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some((key, value)) = map.next_entry::<String, StrictJsonValue>()? {
            if values.insert(key.clone(), value.0).is_some() {
                return Err(A::Error::custom(format!(
                    "duplicate JSON object key {key:?}"
                )));
            }
        }
        Ok(StrictJsonValue(Value::Object(values)))
    }
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).expect("string serialization"),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key serialization"),
                        canonical_json(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

fn deserialize_record<T: for<'de> Deserialize<'de>>(
    value: &Value,
    path: &str,
) -> anyhow::Result<T> {
    serde_json::from_value(value.clone()).with_context(|| format!("decode registry record {path}"))
}

enum RecordPath {
    Origin {
        origin_id: String,
    },
    Source {
        shard: String,
        source_id: String,
    },
    Capture {
        shard: String,
        source_id: String,
        capture_id: String,
    },
    Run {
        shard: String,
        source_id: String,
        capture_id: String,
        stage: String,
        run_id: String,
    },
    Review {
        review_id: String,
    },
    Freshness,
    Schema,
}

fn classify_record_path(path: &Path) -> anyhow::Result<RecordPath> {
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_owned),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .context("registry path must be portable UTF-8")?;
    match components.as_slice() {
        [directory, name] if directory == "origins" => Ok(RecordPath::Origin {
            origin_id: file_id(name)?,
        }),
        [directory, shard, name] if directory == "sources" => Ok(RecordPath::Source {
            shard: shard.clone(),
            source_id: file_id(name)?,
        }),
        [directory, shard, source_id, name] if directory == "captures" => Ok(RecordPath::Capture {
            shard: shard.clone(),
            source_id: source_id.clone(),
            capture_id: file_id(name)?,
        }),
        [directory, shard, source_id, capture_id, stage, name] if directory == "runs" => {
            Ok(RecordPath::Run {
                shard: shard.clone(),
                source_id: source_id.clone(),
                capture_id: capture_id.clone(),
                stage: stage.clone(),
                run_id: file_id(name)?,
            })
        }
        [directory, name] if directory == "reviews" => Ok(RecordPath::Review {
            review_id: file_id(name)?,
        }),
        [directory, name] if directory == "policies" && name == "freshness.json" => {
            Ok(RecordPath::Freshness)
        }
        [directory, rest @ ..] if directory == "schema" && !rest.is_empty() => {
            Ok(RecordPath::Schema)
        }
        _ => anyhow::bail!("registry path is not part of Registry v1 layout"),
    }
}

fn file_id(name: &str) -> anyhow::Result<String> {
    let id = name
        .strip_suffix(".json")
        .context("registry record must end in .json")?;
    ensure!(!id.is_empty(), "registry record identifier is empty");
    Ok(id.to_owned())
}

fn validate_origin(origin: &RegistryOrigin, path_id: &str) -> anyhow::Result<()> {
    ensure!(
        origin.version == REGISTRY_VERSION,
        "unsupported registry origin version"
    );
    ensure!(
        origin.origin_id == path_id,
        "registry origin_id does not match its path"
    );
    validate_id("origin_id", &origin.origin_id)?;
    validate_id("origin kind", &origin.kind)?;
    validate_metadata("origin logical_name", &origin.logical_name)
}

fn validate_source(source: &RegistrySource, path_id: &str, shard: &str) -> anyhow::Result<()> {
    ensure!(
        source.version == REGISTRY_VERSION,
        "unsupported registry source version"
    );
    ensure!(
        source.source_id == path_id,
        "registry source_id does not match its path"
    );
    validate_id("source_id", &source.source_id)?;
    ensure!(
        shard == shard_for_source_id(&source.source_id),
        "registry source shard does not match source_id"
    );
    validate_id("origin_id", &source.origin_id)?;
    validate_relative_path(&source.relative_path)?;
    if let Some(capture_id) = &source.active_capture_id {
        validate_id("active_capture_id", capture_id)?;
    }
    match source.state {
        RegistrySourceState::Active => ensure!(
            source.active_capture_id.is_some(),
            "active registry source requires active_capture_id"
        ),
        RegistrySourceState::Retired | RegistrySourceState::PendingVerification => ensure!(
            source.active_capture_id.is_none(),
            "retired or pending-verification source must not choose an active capture"
        ),
    }
    Ok(())
}

fn validate_capture(
    capture: &RegistryCapture,
    path_source_id: &str,
    path_capture_id: &str,
    shard: &str,
) -> anyhow::Result<()> {
    ensure!(
        capture.version == REGISTRY_VERSION,
        "unsupported registry capture version"
    );
    ensure!(
        capture.source_id == path_source_id,
        "registry capture source_id does not match its path"
    );
    ensure!(
        capture.capture_id == path_capture_id,
        "registry capture_id does not match its path"
    );
    validate_id("source_id", &capture.source_id)?;
    validate_id("capture_id", &capture.capture_id)?;
    ensure!(
        shard == shard_for_source_id(&capture.source_id),
        "registry capture shard does not match source_id"
    );
    validate_relative_path(&capture.relative_path)?;
    validate_sha256("capture sha256", &capture.sha256)?;
    ensure!(
        utc_timestamp_is_valid(&capture.observed_at),
        "capture observed_at must be UTC RFC3339"
    );
    validate_id("capture representation", &capture.representation)
}

fn validate_run(
    run: &RegistryRun,
    path_source_id: &str,
    path_capture_id: &str,
    path_stage: &str,
    path_run_id: &str,
    shard: &str,
) -> anyhow::Result<()> {
    ensure!(
        run.version == REGISTRY_VERSION,
        "unsupported registry run version"
    );
    ensure!(
        run.source_id == path_source_id,
        "registry run source_id does not match its path"
    );
    ensure!(
        run.capture_id == path_capture_id,
        "registry run capture_id does not match its path"
    );
    ensure!(
        run.stage == path_stage,
        "registry run stage does not match its path"
    );
    ensure!(
        run.run_id == path_run_id,
        "registry run_id does not match its path"
    );
    validate_id("run source_id", &run.source_id)?;
    validate_id("run capture_id", &run.capture_id)?;
    validate_id("run stage", &run.stage)?;
    validate_id("run_id", &run.run_id)?;
    ensure!(
        shard == shard_for_source_id(&run.source_id),
        "registry run shard does not match source_id"
    );
    validate_metadata("run processor", &run.processor)?;
    ensure!(
        utc_timestamp_is_valid(&run.started_at),
        "run started_at must be UTC RFC3339"
    );
    if let Some(finished_at) = &run.finished_at {
        ensure!(
            utc_timestamp_is_valid(finished_at),
            "run finished_at must be UTC RFC3339"
        );
    }
    if run.status == RegistryRunStatus::Succeeded {
        ensure!(
            run.finished_at.is_some(),
            "successful registry run requires finished_at"
        );
    }
    for (name, value) in [
        ("run actor", run.actor.as_deref()),
        ("run agent_run_id", run.agent_run_id.as_deref()),
        ("run model_requested", run.model_requested.as_deref()),
        ("run model_reported", run.model_reported.as_deref()),
        (
            "run provider_request_id",
            run.provider_request_id.as_deref(),
        ),
        ("run error_class", run.error_class.as_deref()),
    ] {
        if let Some(value) = value {
            validate_metadata(name, value)?;
        }
    }
    for (name, digest) in [
        ("run profile_digest", run.profile_digest.as_deref()),
        (
            "run prompt_schema_digest",
            run.prompt_schema_digest.as_deref(),
        ),
        (
            "run evidence_manifest_digest",
            run.evidence_manifest_digest.as_deref(),
        ),
        ("run output_digest", run.output_digest.as_deref()),
    ] {
        if let Some(digest) = digest {
            validate_sha256(name, digest)?;
        }
    }
    Ok(())
}

fn validate_freshness_policy(policy: &FreshnessPolicy) -> anyhow::Result<()> {
    ensure!(
        policy.version == REGISTRY_VERSION,
        "unsupported freshness policy version"
    );
    validate_id("freshness model_stage", &policy.model_stage)?;
    ensure!(
        (1..=10 * 366 * 24 * 60 * 60).contains(&policy.model_max_age_seconds),
        "freshness model_max_age_seconds must be between one second and ten years"
    );
    for (source_id, priority) in &policy.source_priorities {
        validate_id("freshness source priority source_id", source_id)?;
        ensure!(
            (0..=1_000_000).contains(priority),
            "freshness source priority must be between zero and one million"
        );
    }
    Ok(())
}

fn validate_review(review: &RegistryReview, path_id: &str) -> anyhow::Result<()> {
    ensure!(
        review.version == REGISTRY_VERSION,
        "unsupported registry review version"
    );
    ensure!(
        review.review_id == path_id,
        "registry review_id does not match its path"
    );
    validate_id("review_id", &review.review_id)?;
    validate_metadata("reviewer", &review.reviewer)?;
    ensure!(
        utc_timestamp_is_valid(&review.reviewed_at),
        "review reviewed_at must be UTC RFC3339"
    );
    for (name, digest) in [
        ("review plan_sha256", &review.plan_sha256),
        ("review capture_set_sha256", &review.capture_set_sha256),
        ("review draft_sha256", &review.draft_sha256),
    ] {
        validate_sha256(name, digest)?;
    }
    Ok(())
}

fn validate_closure(
    origins: &BTreeMap<String, RegistryOrigin>,
    sources: &BTreeMap<String, RegistrySource>,
    captures: &BTreeMap<String, RegistryCapture>,
    runs: &[RegistryRun],
    reviews: &[RegistryReview],
) -> anyhow::Result<()> {
    for source in sources.values() {
        ensure!(
            origins.contains_key(&source.origin_id),
            "registry source references an unknown origin"
        );
        if let Some(capture_id) = &source.active_capture_id {
            let capture = captures
                .get(capture_id)
                .context("registry source references an unknown active capture")?;
            ensure!(
                capture.source_id == source.source_id
                    && capture.relative_path == source.relative_path,
                "registry active capture does not match source head"
            );
        }
    }
    for capture in captures.values() {
        ensure!(
            sources.contains_key(&capture.source_id),
            "registry capture references an unknown source"
        );
    }
    for run in runs {
        let capture = captures
            .get(&run.capture_id)
            .context("registry run references an unknown capture")?;
        ensure!(
            capture.source_id == run.source_id,
            "registry run source/capture closure is invalid"
        );
    }
    for review in reviews {
        let _ = review.decision;
    }
    Ok(())
}

fn validate_control_value(value: &Value) -> anyhow::Result<()> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
        Value::String(value) => validate_metadata("registry control value", value),
        Value::Array(values) => values.iter().try_for_each(validate_control_value),
        Value::Object(values) => values.values().try_for_each(validate_control_value),
    }
}

fn validate_id(name: &str, value: &str) -> anyhow::Result<()> {
    ensure!(
        value.len() <= MAX_ID_BYTES
            && value.as_bytes().split_first().is_some_and(|(first, rest)| {
                first.is_ascii_alphanumeric()
                    && rest.iter().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            }),
        "registry {name} must be a bounded identifier"
    );
    Ok(())
}

fn validate_relative_path(value: &str) -> anyhow::Result<()> {
    ensure!(
        crate::project_path::normalize_project_path(value).as_deref() == Some(value)
            && !value.is_empty(),
        "registry relative_path must be a normalized logical relative path"
    );
    Ok(())
}

fn validate_metadata(name: &str, value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= MAX_METADATA_BYTES
            && !value.chars().any(char::is_control)
            && !crate::structured::structured_string_is_sensitive(value),
        "registry {name} must be bounded, credential-free metadata"
    );
    ensure!(
        !value.starts_with('/')
            && !value.starts_with(r"\\")
            && !value.contains("://")
            && !value.contains('?')
            && !value.contains('#'),
        "registry {name} must not contain an absolute path, host, query, or fragment"
    );
    Ok(())
}

fn validate_sha256(name: &str, value: &str) -> anyhow::Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "registry {name} must be 64 lowercase hexadecimal digits"
    );
    Ok(())
}

/// Convert one validated UTC RFC3339 timestamp into Unix seconds.
pub fn utc_timestamp_to_unix_seconds(value: &str) -> anyhow::Result<i64> {
    ensure!(
        utc_timestamp_is_valid(value),
        "timestamp must be UTC RFC3339"
    );
    let bytes = value.as_bytes();
    let year = i64::from(decimal(&bytes[0..4]).expect("validated timestamp year"));
    let month = decimal(&bytes[5..7]).expect("validated timestamp month");
    let day = decimal(&bytes[8..10]).expect("validated timestamp day");
    let hour = i64::from(decimal(&bytes[11..13]).expect("validated timestamp hour"));
    let minute = i64::from(decimal(&bytes[14..16]).expect("validated timestamp minute"));
    let second = i64::from(
        decimal(&bytes[17..19])
            .expect("validated timestamp second")
            .min(59),
    );
    let days = days_from_civil(year, month, day);
    days.checked_mul(24 * 60 * 60)
        .and_then(|seconds| seconds.checked_add(hour * 60 * 60 + minute * 60 + second))
        .context("timestamp is outside the supported Unix-second range")
}

fn utc_timestamp_is_valid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || !bytes.ends_with(b"Z")
    {
        return false;
    }
    let Some(year) = decimal(&bytes[0..4]) else {
        return false;
    };
    let Some(month) = decimal(&bytes[5..7]) else {
        return false;
    };
    let Some(day) = decimal(&bytes[8..10]) else {
        return false;
    };
    let Some(hour) = decimal(&bytes[11..13]) else {
        return false;
    };
    let Some(minute) = decimal(&bytes[14..16]) else {
        return false;
    };
    let Some(second) = decimal(&bytes[17..19]) else {
        return false;
    };
    if year == 0
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return false;
    }
    match bytes.get(19..bytes.len() - 1) {
        Some([]) => true,
        Some(fraction) if fraction.first() == Some(&b'.') && fraction.len() > 1 => {
            fraction[1..].iter().all(u8::is_ascii_digit)
        }
        _ => false,
    }
}

// Howard Hinnant's civil-date algorithm, shifted so 1970-01-01 is day zero.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - if month <= 2 { 1 } else { 0 };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(byte - b'0'))
    })
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
