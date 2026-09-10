use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
#[cfg(unix)]
use std::{
    ffi::{CStr, CString, OsString},
    os::unix::{
        ffi::OsStringExt as _,
        fs::{MetadataExt as _, OpenOptionsExt as _},
        io::{AsRawFd as _, FromRawFd as _},
        process::CommandExt as _,
    },
};

const SOURCE_INDEX_SCHEMA: &str = "graphoxide.source-index";
const MAX_SOURCE_INDEX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SOURCE_BATCH_FILES: usize = 4_096;
const MAX_SOURCE_BATCH_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SOURCE_BATCH_ENTRIES: usize = 16_384;
const SOURCE_BINDINGS_SCHEMA: &str = "graphoxide.source-bindings";
const MAX_SOURCE_BINDINGS_BYTES: usize = 64 * 1024;
const HTTPS_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// An explicit caller acknowledgement that adding this source transfers data over HTTPS.
#[derive(Debug, Clone, Copy)]
pub struct HttpsFetchConsent(());

/// Make an HTTPS source transfer explicit at the call site.
pub fn allow_https_fetch() -> HttpsFetchConsent {
    HttpsFetchConsent(())
}

/// One typed pointer request within an atomic, heterogeneous source admission.
///
/// A clean, tracked local Git file resolves to a stable Git pointer; dirty or untracked Git
/// files are rejected. Only non-Git inputs become logical bound-path pointers. Source bytes
/// are read transiently and never retained.
#[derive(Debug, Clone)]
pub enum SourceAdmissionInput {
    LocalPath(PathBuf),
    Directory(PathBuf),
    BoundPath {
        binding: String,
        path: String,
    },
    Https {
        url: String,
        consent: HttpsFetchConsent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceIndex {
    pub schema: String,
    pub sources: Vec<SourceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceEntry {
    pub source_id: String,
    pub location: SourceLocation,
    pub content_sha256: String,
    pub bytes: u64,
    pub status: SourceStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SourceLocation {
    Git {
        remote: String,
        commit: String,
        path: String,
    },
    Https {
        url: String,
    },
    BoundPath {
        binding: String,
        path: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SourceStatus {
    Provisional,
    AiReviewed,
    HumanConfirmed,
    StaleError,
}

/// Opaque test-only snapshot of a canonical source index before a pointer admission begins.
///
/// Production callers must use [`SourceAdmissionTransaction`], which owns the add operation
/// and cannot be given an arbitrary post-add pointer list to seal.
#[cfg(test)]
pub(crate) struct SourceAdmission {
    root: PathBuf,
    pre_sources: BTreeMap<String, SourceEntry>,
}

/// Internal source-owned pointer admission transaction.
///
/// It records only pointers returned from its own add operations. Sealing compares that record
/// with the full source-index delta, so concurrent pointers cannot be claimed by this request.
struct SourceAdmissionTransaction {
    root: PathBuf,
    pre_sources: BTreeMap<String, SourceEntry>,
    added: Vec<SourceEntry>,
}

/// Opaque proof that these exact pointer revisions were absent before admission.
pub struct SourceAdmissionReceipt {
    root: PathBuf,
    index_revision: String,
    sources: Vec<SourceEntry>,
}

impl SourceAdmissionReceipt {
    /// Pointer metadata admitted by this receipt, sorted by stable source identifier.
    pub fn sources(&self) -> &[SourceEntry] {
        &self.sources
    }
}

impl SourceAdmissionTransaction {
    /// Start a source-owned admission transaction at the current canonical source index.
    fn begin(root: &Path) -> Result<Self> {
        let root = canonical_admission_root(root)?;
        let pre_sources = load_source_index_or_empty(&root)?
            .sources
            .into_iter()
            .map(|source| (source.source_id.clone(), source))
            .collect();
        Ok(Self {
            root,
            pre_sources,
            added: Vec::new(),
        })
    }

    /// Add local files or directories and retain only their resulting pointers for sealing.
    fn add_local_batch<I, P>(&mut self, inputs: I) -> Result<Vec<SourceEntry>>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let added = match add_sources_for_admission(&self.root, inputs, &self.pre_sources) {
            Ok(added) => added,
            Err(error) => return self.fail_after_mutation(error),
        };
        self.record_operation(&added)?;
        Ok(added)
    }

    /// Add logical bound paths and retain only their resulting pointers for sealing.
    fn add_bound_batch(&mut self, inputs: &[(String, String)]) -> Result<Vec<SourceEntry>> {
        let added = match add_bound_sources_for_admission(&self.root, inputs, &self.pre_sources) {
            Ok(added) => added,
            Err(error) => return self.fail_after_mutation(error),
        };
        self.record_operation(&added)?;
        Ok(added)
    }

    fn add_https_with_fetcher<F>(
        &mut self,
        url: &str,
        consent: HttpsFetchConsent,
        fetch: F,
    ) -> Result<SourceEntry>
    where
        F: FnOnce(&str, &mut fs::File, usize) -> Result<u64>,
    {
        let added = match add_https_source_with_fetcher_for_admission(
            &self.root,
            url,
            consent,
            fetch,
            &self.pre_sources,
        ) {
            Ok(added) => added,
            Err(error) => return self.fail_after_mutation(error),
        };
        self.record_operation(std::slice::from_ref(&added))?;
        Ok(added)
    }

    /// Add a bounded directory while preserving its deterministic outcomes for the caller.
    fn add_directory<F>(&mut self, input: &Path, mut report: F) -> Result<SourceAddSummary>
    where
        F: FnMut(SourceAddOutcome),
    {
        let mut added = Vec::new();
        let result =
            add_source_directory_for_admission(&self.root, input, &self.pre_sources, |outcome| {
                if let SourceAddOutcome::Added { source } = &outcome {
                    added.push(source.clone());
                }
                report(outcome);
            });
        self.record_operation(&added)?;
        match result {
            Ok(summary) => Ok(summary),
            Err(error) => self.fail_after_mutation(error),
        }
    }

    /// Seal an opaque receipt for exactly the pointers added by this transaction.
    ///
    /// A seal failure makes a best-effort rollback of every exact request-owned pointer before
    /// returning the failure. Pre-existing and concurrently changed pointers are never retired.
    fn seal(self) -> Result<SourceAdmissionReceipt> {
        match seal_source_admission(&self.root, &self.pre_sources, &self.added) {
            Ok(receipt) => Ok(receipt),
            Err(error) => match self.rollback_added() {
                Ok(()) => Err(error),
                Err(rollback) => Err(error.context(format!(
                    "source admission seal rollback was incomplete: {rollback:#}"
                ))),
            },
        }
    }

    fn record_added(&mut self, added: &[SourceEntry]) -> Result<()> {
        for source in added {
            source.validate()?;
            if let Some(existing) = self
                .added
                .iter()
                .find(|existing| existing.source_id == source.source_id)
            {
                anyhow::ensure!(
                    existing == source,
                    "source admission operation returned conflicting pointer revisions"
                );
            } else {
                self.added.push(source.clone());
            }
        }
        Ok(())
    }

    fn record_operation(&mut self, added: &[SourceEntry]) -> Result<()> {
        match self.record_added(added) {
            Ok(()) => Ok(()),
            Err(error) => self.fail_after_mutation(error),
        }
    }

    fn fail_after_mutation<T>(&self, error: anyhow::Error) -> Result<T> {
        match self.rollback_added() {
            Ok(()) => Err(error),
            Err(rollback) => Err(error.context(format!(
                "source admission operation rollback was incomplete: {rollback:#}"
            ))),
        }
    }

    fn rollback_added(&self) -> Result<()> {
        rollback_unsealed_admission(&self.root, &self.pre_sources, &self.added)
    }
}

/// Bounded counters for a bulk directory import.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceAddSummary {
    pub added: u64,
    pub errors: u64,
    pub skipped: u64,
}

/// One safe, locator-only bulk import result delivered immediately after its partition publishes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SourceAddOutcome {
    Added { source: SourceEntry },
    Error { error: SourceAddError },
    Skipped { skip: SourceAddSkip },
}

/// A safe locator-only failure emitted while bulk-adding a directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceAddError {
    pub location: SourceLocation,
    pub kind: SourceAddErrorKind,
}

/// The bounded error classes exposed by a bulk directory add.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SourceAddErrorKind {
    Oversized,
    UnsafeOrUnreadable,
}

/// A locator intentionally omitted during a directory import.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceAddSkip {
    pub location: SourceLocation,
    pub kind: SourceAddSkipKind,
}

/// Exact non-content structures skipped by directory import only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SourceAddSkipKind {
    AdministrativeDirectory,
    StructuralFile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SourceBindings {
    schema: String,
    bindings: Vec<SourceBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SourceBinding {
    alias: String,
    root: PathBuf,
}

#[derive(Debug, Clone, Copy)]
struct SourceBatchLimits {
    files: usize,
    bytes: u64,
    entries: usize,
}

const DEFAULT_SOURCE_BATCH_LIMITS: SourceBatchLimits = SourceBatchLimits {
    files: MAX_SOURCE_BATCH_FILES,
    bytes: MAX_SOURCE_BATCH_BYTES,
    entries: MAX_SOURCE_BATCH_ENTRIES,
};

/// Add a regular local file without copying its body into the knowledgebase.
#[cfg(test)]
fn add_local_source(root: &Path, input: &Path) -> Result<SourceEntry> {
    let mut sources = add_sources(root, [input])?;
    anyhow::ensure!(
        sources.len() == 1,
        "single source add must produce one source"
    );
    Ok(sources.remove(0))
}

/// Add a pointer through an existing ignored local binding without exposing its physical root.
#[cfg(test)]
fn add_bound_source(root: &Path, binding: &str, relative_path: &str) -> Result<SourceEntry> {
    let mut sources = add_bound_sources(root, &[(binding.to_owned(), relative_path.to_owned())])?;
    anyhow::ensure!(
        sources.len() == 1,
        "single logical source add must produce one source"
    );
    Ok(sources.remove(0))
}

/// Atomically add logical pointers through existing ignored local bindings.
#[cfg(test)]
fn add_bound_sources(root: &Path, inputs: &[(String, String)]) -> Result<Vec<SourceEntry>> {
    add_bound_sources_with_admission_pre(root, inputs, None)
}

fn add_bound_sources_for_admission(
    root: &Path,
    inputs: &[(String, String)],
    pre_sources: &BTreeMap<String, SourceEntry>,
) -> Result<Vec<SourceEntry>> {
    add_bound_sources_with_admission_pre(root, inputs, Some(pre_sources))
}

fn add_bound_sources_with_admission_pre(
    root: &Path,
    inputs: &[(String, String)],
    pre_sources: Option<&BTreeMap<String, SourceEntry>>,
) -> Result<Vec<SourceEntry>> {
    anyhow::ensure!(
        !inputs.is_empty(),
        "bound source add requires at least one input"
    );
    let mut locations = BTreeMap::new();
    for (binding, relative_path) in inputs {
        let location = SourceLocation::BoundPath {
            binding: binding.clone(),
            path: relative_path.clone(),
        };
        location.validate()?;
        locations.entry(location.source_id()).or_insert(location);
    }
    let bindings_path = root.join(".graphoxide/source-bindings.yaml");
    ensure_source_bindings_git_preflight(root)
        .map_err(|_| anyhow::anyhow!("bound source configuration is unavailable"))?;
    ensure_source_bindings_git_ignored(root)
        .map_err(|_| anyhow::anyhow!("bound source configuration is unavailable"))?;
    let bindings = load_source_bindings(&bindings_path)
        .map_err(|_| anyhow::anyhow!("bound source configuration is unavailable"))?;
    let mut sources = Vec::with_capacity(locations.len());
    for (source_id, location) in locations {
        let SourceLocation::BoundPath { binding, path } = &location else {
            unreachable!("logical source locations are always bound paths");
        };
        let bound_root = bindings
            .bindings
            .iter()
            .find(|candidate| candidate.alias == binding.as_str())
            .map(|candidate| &candidate.root)
            .context("bound source binding is unavailable")?;
        let body = crate::enrich::safe_read_bounded(
            bound_root,
            &bound_root.join(path),
            MAX_SOURCE_INDEX_BYTES as usize,
        )
        .map_err(|_| anyhow::anyhow!("bound source is unavailable or unsafe"))?;
        sources.push(SourceEntry {
            source_id,
            location,
            content_sha256: hex::encode(Sha256::digest(&body)),
            bytes: body.len() as u64,
            status: SourceStatus::Provisional,
        });
    }
    let mut index = match load_source_index(root) {
        Ok(index) => index,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            SourceIndex {
                schema: SOURCE_INDEX_SCHEMA.into(),
                sources: Vec::new(),
            }
        }
        Err(error) => return Err(error),
    };
    ensure_admission_sources_are_new(pre_sources, &index, &sources)?;
    for source in &mut sources {
        if let Some(existing) = index
            .sources
            .iter()
            .find(|existing| existing.source_id == source.source_id)
            && existing.content_sha256 == source.content_sha256
            && matches!(
                existing.status,
                SourceStatus::AiReviewed | SourceStatus::HumanConfirmed
            )
        {
            source.status = existing.status.clone();
        }
    }
    let source_ids = sources
        .iter()
        .map(|source| source.source_id.as_str())
        .collect::<HashSet<_>>();
    index
        .sources
        .retain(|existing| !source_ids.contains(existing.source_id.as_str()));
    index.sources.extend(sources.iter().cloned());
    write_source_index(root, &index)?;
    Ok(sources)
}

/// Read the persisted source pointers without opening bindings or contacting a source.
pub fn source_status(root: &Path) -> Result<Vec<SourceEntry>> {
    Ok(load_source_index(root)?.sources)
}

/// Test-only generic snapshot helper. Production callers use `SourceAdmissionTransaction`.
#[cfg(test)]
pub(crate) fn begin_source_admission(root: &Path) -> Result<SourceAdmission> {
    let root = canonical_admission_root(root)?;
    let pre_sources = load_source_index_or_empty(&root)?
        .sources
        .into_iter()
        .map(|source| (source.source_id.clone(), source))
        .collect();
    Ok(SourceAdmission { root, pre_sources })
}

/// Test-only generic sealing helper. Production callers cannot supply pointer lists to seal.
#[cfg(test)]
pub(crate) fn finish_source_admission(
    root: &Path,
    admission: SourceAdmission,
    added: &[SourceEntry],
) -> Result<SourceAdmissionReceipt> {
    let root = canonical_admission_root(root)?;
    anyhow::ensure!(
        root == admission.root,
        "source admission root does not match its snapshot"
    );
    seal_source_admission(&root, &admission.pre_sources, added)
}

/// Add local files or directories in one source-owned transaction and seal their receipt.
pub fn admit_sources<I, P>(root: &Path, inputs: I) -> Result<SourceAdmissionReceipt>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut admission = SourceAdmissionTransaction::begin(root)?;
    admission.add_local_batch(inputs)?;
    admission.seal()
}

/// Add logical bound paths in one source-owned transaction and seal their receipt.
pub fn admit_bound_sources(
    root: &Path,
    inputs: &[(String, String)],
) -> Result<SourceAdmissionReceipt> {
    let mut admission = SourceAdmissionTransaction::begin(root)?;
    admission.add_bound_batch(inputs)?;
    admission.seal()
}

/// Fetch one explicitly consented HTTPS source in a source-owned transaction and seal its receipt.
pub fn admit_https_source(
    root: &Path,
    url: &str,
    consent: HttpsFetchConsent,
) -> Result<SourceAdmissionReceipt> {
    admit_https_source_with_fetcher(root, url, consent, |url, writer, max_bytes| {
        graphoxide_core::safe_fetch_https_to_writer_no_redirect(
            url,
            writer,
            max_bytes,
            HTTPS_FETCH_TIMEOUT,
        )
    })
}

fn admit_https_source_with_fetcher<F>(
    root: &Path,
    url: &str,
    consent: HttpsFetchConsent,
    fetch: F,
) -> Result<SourceAdmissionReceipt>
where
    F: FnOnce(&str, &mut fs::File, usize) -> Result<u64>,
{
    let mut admission = SourceAdmissionTransaction::begin(root)?;
    admission.add_https_with_fetcher(url, consent, fetch)?;
    admission.seal()
}

/// Add a bounded directory in a source-owned transaction and return its deterministic outcomes.
pub fn admit_source_directory<F>(
    root: &Path,
    input: &Path,
    report: F,
) -> Result<(SourceAdmissionReceipt, SourceAddSummary)>
where
    F: FnMut(SourceAddOutcome),
{
    let mut admission = SourceAdmissionTransaction::begin(root)?;
    let summary = admission.add_directory(input, report)?;
    let receipt = admission.seal()?;
    Ok((receipt, summary))
}

/// Admit a heterogeneous source request as one all-or-nothing pointer transaction.
///
/// Directory items retain their documented deterministic soft outcomes through `report`.
/// Any request-level failure rolls back every exact pointer created by this request before the
/// error is returned. The resulting receipt contains pointer metadata only.
pub fn admit_source_inputs<F>(
    root: &Path,
    inputs: &[SourceAdmissionInput],
    report: F,
) -> Result<SourceAdmissionReceipt>
where
    F: FnMut(SourceAddOutcome),
{
    admit_source_inputs_with_https_fetcher(root, inputs, report, |url, writer, max_bytes| {
        graphoxide_core::safe_fetch_https_to_writer_no_redirect(
            url,
            writer,
            max_bytes,
            HTTPS_FETCH_TIMEOUT,
        )
    })
}

fn admit_source_inputs_with_https_fetcher<F, H>(
    root: &Path,
    inputs: &[SourceAdmissionInput],
    mut report: F,
    mut fetch_https: H,
) -> Result<SourceAdmissionReceipt>
where
    F: FnMut(SourceAddOutcome),
    H: FnMut(&str, &mut fs::File, usize) -> Result<u64>,
{
    anyhow::ensure!(
        !inputs.is_empty(),
        "source admission requires at least one input"
    );
    let mut admission = SourceAdmissionTransaction::begin(root)?;
    for input in inputs {
        match input {
            SourceAdmissionInput::LocalPath(path) => {
                admission.add_local_batch([path])?;
            }
            SourceAdmissionInput::Directory(path) => {
                admission.add_directory(path, &mut report)?;
            }
            SourceAdmissionInput::BoundPath { binding, path } => {
                admission.add_bound_batch(&[(binding.clone(), path.clone())])?;
            }
            SourceAdmissionInput::Https { url, consent } => {
                admission.add_https_with_fetcher(url, *consent, |url, writer, max_bytes| {
                    fetch_https(url, writer, max_bytes)
                })?;
            }
        }
    }
    admission.seal()
}

fn seal_source_admission(
    root: &Path,
    pre_sources: &BTreeMap<String, SourceEntry>,
    added: &[SourceEntry],
) -> Result<SourceAdmissionReceipt> {
    anyhow::ensure!(
        !added.is_empty(),
        "source admission receipt requires at least one request-owned pointer"
    );
    let mut sources = added.to_vec();
    sources.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    anyhow::ensure!(
        sources
            .windows(2)
            .all(|pair| pair[0].source_id != pair[1].source_id),
        "source admission receipt contains duplicate pointers"
    );
    for source in &sources {
        source.validate()?;
        anyhow::ensure!(
            !pre_sources.contains_key(&source.source_id),
            "source admission pointer already existed before admission"
        );
    }

    let current = load_source_index_or_empty(root)?;
    let current_sources = current
        .sources
        .iter()
        .map(|source| (source.source_id.clone(), source))
        .collect::<BTreeMap<_, _>>();
    anyhow::ensure!(
        pre_sources.iter().all(|(source_id, before)| {
            current_sources
                .get(source_id)
                .is_some_and(|current| *current == before)
        }),
        "source admission pre-existing pointer changed during the request"
    );
    let current_new = current_sources
        .into_iter()
        .filter_map(|(source_id, source)| {
            (!pre_sources.contains_key(&source_id)).then_some(source.clone())
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        current_new == sources,
        "source admission current index contains pointers not owned by this request"
    );
    Ok(SourceAdmissionReceipt {
        root: root.to_path_buf(),
        index_revision: source_index_revision(&current)?,
        sources,
    })
}

fn rollback_unsealed_admission(
    root: &Path,
    pre_sources: &BTreeMap<String, SourceEntry>,
    added: &[SourceEntry],
) -> Result<()> {
    let mut failures = Vec::new();
    for source in added {
        if !pre_sources.contains_key(&source.source_id)
            && retire_source_if_current(root, source).is_err()
        {
            failures.push(source.source_id.as_str());
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "source admission rollback could not remove {} source(s): {}",
        failures.len(),
        failures.join(",")
    );
    Ok(())
}

fn ensure_admission_sources_are_new(
    pre_sources: Option<&BTreeMap<String, SourceEntry>>,
    current: &SourceIndex,
    candidates: &[SourceEntry],
) -> Result<()> {
    let Some(pre_sources) = pre_sources else {
        return Ok(());
    };
    for candidate in candidates {
        anyhow::ensure!(
            !pre_sources.contains_key(&candidate.source_id),
            "source admission cannot re-admit an existing pointer"
        );
        anyhow::ensure!(
            !current
                .sources
                .iter()
                .any(|current| current.source_id == candidate.source_id),
            "source admission pointer was independently added during the request"
        );
    }
    Ok(())
}

/// Validate that a receipt still names the complete current source-index revision.
pub fn validate_source_admission(
    root: &Path,
    receipt: &SourceAdmissionReceipt,
) -> Result<Vec<SourceEntry>> {
    let root = canonical_admission_root(root)?;
    anyhow::ensure!(
        root == receipt.root,
        "source admission root does not match its receipt"
    );
    let current = load_source_index_or_empty(&root)?;
    anyhow::ensure!(
        source_index_revision(&current)? == receipt.index_revision,
        "source admission receipt no longer matches the current source index"
    );
    for source in &receipt.sources {
        anyhow::ensure!(
            current
                .sources
                .iter()
                .find(|current| current.source_id == source.source_id)
                .is_some_and(|current| current == source),
            "source admission receipt no longer matches a current pointer"
        );
    }
    Ok(receipt.sources.clone())
}

/// Best-effort rollback of only exact pointer revisions admitted by an opaque receipt.
pub fn rollback_source_admission(root: &Path, receipt: &SourceAdmissionReceipt) -> Result<()> {
    let root = canonical_admission_root(root)?;
    anyhow::ensure!(
        root == receipt.root,
        "source admission root does not match its receipt"
    );
    let mut failures = Vec::new();
    for source in &receipt.sources {
        if retire_source_if_current(&root, source).is_err() {
            failures.push(source.source_id.as_str());
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "source admission rollback could not remove {} source(s): {}",
        failures.len(),
        failures.join(",")
    );
    Ok(())
}

/// Read one current source into memory without changing its pointer or lifecycle state.
pub fn read_source_transient(
    root: &Path,
    source: &SourceEntry,
    remote_consent: Option<HttpsFetchConsent>,
) -> Result<Vec<u8>> {
    read_source_transient_with_https_fetcher(
        root,
        source,
        remote_consent,
        |url, writer, max_bytes| {
            graphoxide_core::safe_fetch_https_to_writer_no_redirect(
                url,
                writer,
                max_bytes,
                HTTPS_FETCH_TIMEOUT,
            )
        },
    )
}

fn read_source_transient_with_https_fetcher<F>(
    root: &Path,
    source: &SourceEntry,
    remote_consent: Option<HttpsFetchConsent>,
    fetch_https: F,
) -> Result<Vec<u8>>
where
    F: FnOnce(&str, &mut fs::File, usize) -> Result<u64>,
{
    source.validate()?;
    let indexed = load_source_index(root)?;
    let current = indexed
        .sources
        .iter()
        .find(|current| current.source_id == source.source_id)
        .context("source pointer is unavailable")?;
    anyhow::ensure!(current == source, "source pointer is no longer current");
    let body = match &source.location {
        SourceLocation::BoundPath { binding, path } => refresh_bound_source(root, binding, path)
            .map_err(|_| anyhow::anyhow!("bound source is unavailable or unsafe"))?,
        SourceLocation::Https { url } => match remote_consent {
            Some(_) => fetch_https_to_bytes(root, url, fetch_https)
                .map_err(|_| anyhow::anyhow!("remote source is unavailable or unsafe"))?,
            None => anyhow::bail!("remote source read requires explicit consent"),
        },
        SourceLocation::Git {
            remote,
            commit,
            path,
        } => match read_bound_git_source(root, remote, commit, path)? {
            Some(body) => body,
            None => return Err(remote_git_fetch_disabled()),
        },
    };
    anyhow::ensure!(
        hex::encode(Sha256::digest(&body)) == source.content_sha256,
        "source content no longer matches its indexed digest"
    );
    anyhow::ensure!(
        body.len() as u64 == source.bytes,
        "source content no longer matches its indexed byte count"
    );
    Ok(body)
}

/// Refresh one source transiently. Remote locations require an explicit consent token.
pub fn refresh_source(
    root: &Path,
    source_id: &str,
    remote_consent: Option<HttpsFetchConsent>,
) -> Result<SourceEntry> {
    refresh_source_with_https_fetcher(root, source_id, remote_consent, |url, writer, max_bytes| {
        graphoxide_core::safe_fetch_https_to_writer_no_redirect(
            url,
            writer,
            max_bytes,
            HTTPS_FETCH_TIMEOUT,
        )
    })
}

/// A transient refresh candidate; the pointer remains unchanged until its
/// derived artifacts can be published with the same revision.
pub(crate) struct SourceRefresh {
    pub previous: SourceEntry,
    pub refreshed: SourceEntry,
    pub body: Option<Vec<u8>>,
}

pub(crate) fn prepare_source_refresh(
    root: &Path,
    source_id: &str,
    remote_consent: Option<HttpsFetchConsent>,
) -> Result<SourceRefresh> {
    prepare_source_refresh_with_https_fetcher(
        root,
        source_id,
        remote_consent,
        |url, writer, max_bytes| {
            graphoxide_core::safe_fetch_https_to_writer_no_redirect(
                url,
                writer,
                max_bytes,
                HTTPS_FETCH_TIMEOUT,
            )
        },
    )
}

fn refresh_source_with_https_fetcher<F>(
    root: &Path,
    source_id: &str,
    remote_consent: Option<HttpsFetchConsent>,
    fetch_https: F,
) -> Result<SourceEntry>
where
    F: FnOnce(&str, &mut fs::File, usize) -> Result<u64>,
{
    let prepared =
        prepare_source_refresh_with_https_fetcher(root, source_id, remote_consent, fetch_https)?;
    let mut index = load_source_index(root)?;
    let current = index
        .sources
        .iter_mut()
        .find(|source| source.source_id == source_id)
        .context("source pointer is unavailable")?;
    anyhow::ensure!(
        *current == prepared.previous,
        "source pointer is no longer current"
    );
    *current = prepared.refreshed.clone();
    write_source_index(root, &index)?;
    Ok(prepared.refreshed)
}

fn prepare_source_refresh_with_https_fetcher<F>(
    root: &Path,
    source_id: &str,
    remote_consent: Option<HttpsFetchConsent>,
    fetch_https: F,
) -> Result<SourceRefresh>
where
    F: FnOnce(&str, &mut fs::File, usize) -> Result<u64>,
{
    let index = load_source_index(root)?;
    let source = index
        .sources
        .iter()
        .find(|source| source.source_id == source_id)
        .cloned()
        .with_context(|| format!("source {source_id:?} is not indexed"))?;
    let result = match &source.location {
        SourceLocation::BoundPath { binding, path } => refresh_bound_source(root, binding, path),
        SourceLocation::Https { url } => match remote_consent {
            Some(_) => fetch_https_to_bytes(root, url, fetch_https),
            None => Err(anyhow::anyhow!(
                "remote source refresh requires explicit consent"
            )),
        },
        SourceLocation::Git {
            remote,
            commit,
            path,
        } => Ok(read_bound_git_source(root, remote, commit, path)
            .map_err(|_| anyhow::anyhow!("local Git source binding is unavailable or unsafe"))?
            .ok_or_else(remote_git_fetch_disabled)?),
    };
    let mut refreshed = source.clone();
    let body = match result {
        Ok(bytes) => {
            refreshed.content_sha256 = hex::encode(Sha256::digest(&bytes));
            refreshed.bytes = bytes.len() as u64;
            if refreshed.content_sha256 != source.content_sha256
                || !matches!(
                    source.status,
                    SourceStatus::AiReviewed | SourceStatus::HumanConfirmed
                )
            {
                refreshed.status = SourceStatus::Provisional;
            }
            Some(bytes)
        }
        Err(_) => {
            refreshed.status = SourceStatus::StaleError;
            None
        }
    };
    Ok(SourceRefresh {
        previous: source,
        refreshed,
        body,
    })
}

/// Remove one pointer and only its now-unreferenced ignored local binding alias.
pub fn retire_source(root: &Path, source_id: &str) -> Result<()> {
    retire_source_with_binding_writer(root, source_id, write_source_bindings)
}

fn retire_source_if_current(root: &Path, expected: &SourceEntry) -> Result<()> {
    retire_source_with_expected_binding_writer(
        root,
        &expected.source_id,
        Some(expected),
        write_source_bindings,
    )
}

fn retire_source_with_binding_writer<F>(
    root: &Path,
    source_id: &str,
    write_bindings: F,
) -> Result<()>
where
    F: FnOnce(&Path, &SourceBindings) -> Result<()>,
{
    retire_source_with_expected_binding_writer(root, source_id, None, write_bindings)
}

fn retire_source_with_expected_binding_writer<F>(
    root: &Path,
    source_id: &str,
    expected: Option<&SourceEntry>,
    write_bindings: F,
) -> Result<()>
where
    F: FnOnce(&Path, &SourceBindings) -> Result<()>,
{
    let mut index = load_source_index(root)?;
    let previous_index = index.clone();
    let source = index
        .sources
        .iter()
        .find(|source| source.source_id == source_id)
        .cloned()
        .with_context(|| format!("source {source_id:?} is not indexed"))?;
    anyhow::ensure!(
        expected.is_none_or(|expected| source == *expected),
        "source pointer no longer matches its expected revision"
    );
    index
        .sources
        .retain(|existing| existing.source_id != source.source_id);
    let referenced_bindings = index
        .sources
        .iter()
        .filter_map(|source| match &source.location {
            SourceLocation::BoundPath { binding, .. } => Some(binding.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let bindings_path = root.join(".graphoxide/source-bindings.yaml");
    let bindings = if bindings_path.exists() {
        let previous_text = read_source_bindings_text(root)?;
        let existing = load_source_bindings(&bindings_path)?;
        let mut retained = existing.clone();
        retained.bindings.retain(|binding| {
            referenced_bindings.contains(binding.alias.as_str())
                || index.sources.iter().any(|source| {
                    matches!(
                        &source.location,
                        SourceLocation::Git {
                            remote,
                            commit,
                            path,
                        } if binding_supplies_git_source(binding, remote, commit, path)
                    )
                })
        });
        (retained != existing).then_some((retained, previous_text))
    } else {
        None
    };
    write_source_index(root, &index)?;
    if let Some((bindings, previous_text)) = bindings
        && let Err(cleanup) = write_bindings(root, &bindings)
    {
        return match restore_source_bindings_text(root, &previous_text) {
            Ok(()) => match write_source_index(root, &previous_index) {
                Ok(()) => {
                    Err(cleanup).context("retirement rolled back after binding cleanup failure")
                }
                Err(rollback) => Err(cleanup).context(format!(
                    "binding cleanup failed and bindings were restored, but pointer restore failed: {rollback:#}"
                )),
            },
            Err(binding_rollback) => Err(cleanup).context(format!(
                "binding cleanup failed and binding restore failed; source remains retired: {binding_rollback:#}"
            )),
        };
    }
    Ok(())
}

fn read_source_bindings_text(root: &Path) -> Result<String> {
    let path = root.join(".graphoxide/source-bindings.yaml");
    let bytes = crate::enrich::safe_read_bounded(root, &path, MAX_SOURCE_BINDINGS_BYTES)
        .context("read source binding artifact before retirement")?;
    String::from_utf8(bytes).context("source binding artifact must be UTF-8")
}

fn restore_source_bindings_text(root: &Path, text: &str) -> Result<()> {
    graphoxide_core::write_text_atomic_strict(root.join(".graphoxide/source-bindings.yaml"), text)
        .context("restore source binding artifact atomically")
}

fn refresh_bound_source(root: &Path, binding: &str, path: &str) -> Result<Vec<u8>> {
    let bindings = load_source_bindings(&root.join(".graphoxide/source-bindings.yaml"))?;
    let binding = bindings
        .bindings
        .iter()
        .find(|candidate| candidate.alias == binding)
        .context("source binding is unavailable")?;
    crate::enrich::safe_read_bounded(
        &binding.root,
        &binding.root.join(path),
        MAX_SOURCE_INDEX_BYTES as usize,
    )
    .context("read bounded local source refresh")
}

fn read_bound_git_source(
    root: &Path,
    remote: &str,
    commit: &str,
    path: &str,
) -> Result<Option<Vec<u8>>> {
    let bindings = load_source_bindings(&root.join(".graphoxide/source-bindings.yaml"))
        .map_err(|_| anyhow::anyhow!("local Git source binding is unavailable or unsafe"))?;
    for binding in &bindings.bindings {
        if !binding_matches_git_remote(binding, remote) {
            continue;
        }
        let object = format!("{commit}:{path}");
        let command = || {
            let mut command = git_source_command();
            command.arg("-C").arg(&binding.root);
            command
        };
        let Some(kind) = read_git_output_bounded(command().args(["cat-file", "-t", &object]), 32)?
        else {
            continue;
        };
        anyhow::ensure!(kind == b"blob\n", "pinned Git source must be a blob");
        let Some(size) = read_git_output_bounded(command().args(["cat-file", "-s", &object]), 32)?
        else {
            continue;
        };
        let size = std::str::from_utf8(&size)?.trim().parse::<u64>()?;
        anyhow::ensure!(
            size <= MAX_SOURCE_INDEX_BYTES,
            "pinned Git source exceeds the size limit"
        );
        let Some(body) = read_git_output_bounded(
            command().args(["cat-file", "blob", &object]),
            MAX_SOURCE_INDEX_BYTES as usize,
        )?
        else {
            continue;
        };
        anyhow::ensure!(
            body.len() as u64 == size,
            "pinned Git source size changed while reading"
        );
        return Ok(Some(body));
    }
    Ok(None)
}

fn binding_supplies_git_source(
    binding: &SourceBinding,
    remote: &str,
    commit: &str,
    path: &str,
) -> bool {
    if !binding_matches_git_remote(binding, remote) {
        return false;
    }
    let object = format!("{commit}:{path}");
    git_source_command()
        .args(["-C"])
        .arg(&binding.root)
        .args(["cat-file", "-e", &object])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn binding_matches_git_remote(binding: &SourceBinding, remote: &str) -> bool {
    let output = match git_source_command()
        .args(["-C"])
        .arg(&binding.root)
        .args(["remote", "get-url", "origin"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return false,
    };
    std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|value| normalize_git_remote(value.trim()).ok())
        .as_deref()
        == Some(remote)
}

fn fetch_https_to_bytes<F>(root: &Path, url: &str, fetch: F) -> Result<Vec<u8>>
where
    F: FnOnce(&str, &mut fs::File, usize) -> Result<u64>,
{
    validate_https_url(url, "HTTPS source")?;
    let staging = root
        .canonicalize()?
        .parent()
        .context("knowledgebase root must have an external staging parent")?
        .to_owned();
    let mut staged = tempfile::NamedTempFile::new_in(staging)
        .context("stage HTTPS refresh outside the knowledgebase")?;
    let reported = fetch(url, staged.as_file_mut(), MAX_SOURCE_INDEX_BYTES as usize)?;
    staged.flush()?;
    let bytes = read_staged_source(&staged)?;
    anyhow::ensure!(
        reported == bytes.len() as u64,
        "HTTPS refresh byte count does not match response"
    );
    Ok(bytes)
}

pub(crate) fn remote_git_fetch_disabled() -> anyhow::Error {
    anyhow::anyhow!(
        "remote Git fetching is disabled until transfer, storage, output, and process limits are enforced; restore a local binding containing the pinned revision"
    )
}

#[cfg(test)]
fn add_https_source_with_fetcher<F>(
    root: &Path,
    url: &str,
    consent: HttpsFetchConsent,
    fetch: F,
) -> Result<SourceEntry>
where
    F: FnOnce(&str, &mut fs::File, usize) -> Result<u64>,
{
    add_https_source_with_fetcher_with_admission_pre(root, url, consent, fetch, None)
}

fn add_https_source_with_fetcher_for_admission<F>(
    root: &Path,
    url: &str,
    consent: HttpsFetchConsent,
    fetch: F,
    pre_sources: &BTreeMap<String, SourceEntry>,
) -> Result<SourceEntry>
where
    F: FnOnce(&str, &mut fs::File, usize) -> Result<u64>,
{
    add_https_source_with_fetcher_with_admission_pre(root, url, consent, fetch, Some(pre_sources))
}

fn add_https_source_with_fetcher_with_admission_pre<F>(
    root: &Path,
    url: &str,
    _consent: HttpsFetchConsent,
    fetch: F,
    pre_sources: Option<&BTreeMap<String, SourceEntry>>,
) -> Result<SourceEntry>
where
    F: FnOnce(&str, &mut fs::File, usize) -> Result<u64>,
{
    // Validate the persisted locator before the injected transport can run.
    validate_https_url(url, "HTTPS source")?;
    let root_metadata = fs::symlink_metadata(root)
        .with_context(|| format!("read knowledgebase root at {}", root.display()))?;
    anyhow::ensure!(
        root_metadata.file_type().is_dir() && !root_metadata.file_type().is_symlink(),
        "knowledgebase root must be a real directory"
    );
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize knowledgebase root at {}", root.display()))?;
    let staging_directory = root
        .parent()
        .context("knowledgebase root must have an external parent for HTTPS staging")?;
    let mut staged = tempfile::NamedTempFile::new_in(staging_directory)
        .context("stage HTTPS source outside the knowledgebase")?;
    anyhow::ensure!(
        !staged.path().starts_with(&root),
        "HTTPS source staging must remain outside the knowledgebase"
    );
    let reported_bytes = fetch(url, staged.as_file_mut(), MAX_SOURCE_INDEX_BYTES as usize)
        .context("fetch bounded HTTPS source without redirects")?;
    staged.flush().context("flush staged HTTPS source")?;
    let body = read_staged_source(&staged)?;
    anyhow::ensure!(
        reported_bytes == body.len() as u64,
        "HTTPS source fetch byte count does not match the staged response"
    );

    let location = SourceLocation::Https { url: url.into() };
    let mut source = SourceEntry {
        source_id: location.source_id(),
        location,
        content_sha256: hex::encode(Sha256::digest(&body)),
        bytes: reported_bytes,
        status: SourceStatus::Provisional,
    };
    let mut index = match load_source_index(&root) {
        Ok(index) => index,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            SourceIndex {
                schema: SOURCE_INDEX_SCHEMA.into(),
                sources: Vec::new(),
            }
        }
        Err(error) => return Err(error),
    };
    ensure_admission_sources_are_new(pre_sources, &index, std::slice::from_ref(&source))?;
    if let Some(existing) = index
        .sources
        .iter()
        .find(|existing| existing.source_id == source.source_id)
        && existing.content_sha256 == source.content_sha256
        && matches!(
            existing.status,
            SourceStatus::AiReviewed | SourceStatus::HumanConfirmed
        )
    {
        source.status = existing.status.clone();
    }
    index
        .sources
        .retain(|existing| existing.source_id != source.source_id);
    index.sources.push(source.clone());
    write_source_index(&root, &index)?;
    Ok(source)
}

/// Add local files or directories in deterministic order without retaining bodies.
#[cfg(test)]
fn add_sources<I, P>(root: &Path, inputs: I) -> Result<Vec<SourceEntry>>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    add_sources_with_index_writer(
        root,
        &inputs
            .into_iter()
            .map(|input| input.as_ref().to_path_buf())
            .collect::<Vec<_>>(),
        write_source_index,
    )
}

fn add_sources_for_admission<I, P>(
    root: &Path,
    inputs: I,
    pre_sources: &BTreeMap<String, SourceEntry>,
) -> Result<Vec<SourceEntry>>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    add_sources_with_index_writer_and_limits_with_admission_pre(
        root,
        &inputs
            .into_iter()
            .map(|input| input.as_ref().to_path_buf())
            .collect::<Vec<_>>(),
        write_source_index,
        DEFAULT_SOURCE_BATCH_LIMITS,
        Some(pre_sources),
    )
}

/// Add a directory through bounded descriptor-held partitions. Each outcome is
/// delivered after the containing partition has either safely published or been rejected.
#[cfg(test)]
fn add_source_directory<F>(root: &Path, input: &Path, mut report: F) -> Result<SourceAddSummary>
where
    F: FnMut(SourceAddOutcome),
{
    add_source_directory_with_admission_pre(root, input, None, &mut report)
}

fn add_source_directory_for_admission<F>(
    root: &Path,
    input: &Path,
    pre_sources: &BTreeMap<String, SourceEntry>,
    mut report: F,
) -> Result<SourceAddSummary>
where
    F: FnMut(SourceAddOutcome),
{
    add_source_directory_with_admission_pre(root, input, Some(pre_sources), &mut report)
}

fn add_source_directory_with_admission_pre<F>(
    root: &Path,
    input: &Path,
    pre_sources: Option<&BTreeMap<String, SourceEntry>>,
    mut report: F,
) -> Result<SourceAddSummary>
where
    F: FnMut(SourceAddOutcome),
{
    let root_metadata = fs::symlink_metadata(root)
        .with_context(|| format!("read knowledgebase root at {}", root.display()))?;
    anyhow::ensure!(
        root_metadata.file_type().is_dir() && !root_metadata.file_type().is_symlink(),
        "knowledgebase root must be a real directory"
    );
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize knowledgebase root at {}", root.display()))?;
    ensure_source_bindings_git_preflight(&root)?;
    let metadata = fs::symlink_metadata(input)
        .with_context(|| format!("read local source directory at {}", input.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "bulk source input must be a real directory"
    );
    let source_root = input
        .canonicalize()
        .with_context(|| format!("canonicalize local source directory at {}", input.display()))?;
    ensure_utf8_path(&source_root, "bulk source directory")?;
    anyhow::ensure!(
        !source_root.starts_with(&root),
        "local source must be outside the knowledgebase root"
    );

    #[cfg(unix)]
    {
        let alias = local_binding_alias(&source_root);
        let source_directory = open_directory_nofollow(&source_root)?;
        let mut walker = BulkDirectoryWalker::new(source_directory)?;
        let mut summary = SourceAddSummary {
            added: 0,
            errors: 0,
            skipped: 0,
        };
        let mut batch = BulkBatch::default();
        while let Some(item) = walker.next()? {
            if batch.entries == MAX_SOURCE_BATCH_ENTRIES {
                publish_bulk_partition(
                    &root,
                    &source_root,
                    &alias,
                    pre_sources,
                    &mut batch,
                    &mut report,
                    &mut summary,
                )?;
            }
            batch.entries += 1;
            let location = SourceLocation::BoundPath {
                binding: alias.clone(),
                path: item.relative().to_owned(),
            };
            match item {
                BulkWalkItem::EnteredDirectory => {}
                BulkWalkItem::Skipped { kind, .. } => {
                    batch
                        .outcomes
                        .push(BulkPendingOutcome::Skipped(SourceAddSkip {
                            location,
                            kind,
                        }))
                }
                BulkWalkItem::Error { .. } => {
                    batch
                        .outcomes
                        .push(BulkPendingOutcome::Error(SourceAddError {
                            location,
                            kind: SourceAddErrorKind::UnsafeOrUnreadable,
                        }))
                }
                BulkWalkItem::File {
                    relative: _,
                    directory,
                    name,
                } => {
                    let staged = stage_opened_source(&directory, Path::new(&name));
                    let (staged, content_sha256, bytes) = match staged {
                        Ok(staged) => staged,
                        Err(error) => {
                            batch
                                .outcomes
                                .push(BulkPendingOutcome::Error(SourceAddError {
                                    location,
                                    kind: classify_bulk_error(&error),
                                }));
                            continue;
                        }
                    };
                    let body = match read_staged_source(&staged) {
                        Ok(body) => body,
                        Err(error) => {
                            batch
                                .outcomes
                                .push(BulkPendingOutcome::Error(SourceAddError {
                                    location,
                                    kind: classify_bulk_error(&error),
                                }));
                            continue;
                        }
                    };
                    drop(staged);
                    if bytes > MAX_SOURCE_BATCH_BYTES {
                        batch
                            .outcomes
                            .push(BulkPendingOutcome::Error(SourceAddError {
                                location,
                                kind: SourceAddErrorKind::Oversized,
                            }));
                        continue;
                    }
                    if batch.sources.len() == MAX_SOURCE_BATCH_FILES
                        || batch.bytes.saturating_add(bytes) > MAX_SOURCE_BATCH_BYTES
                    {
                        publish_bulk_partition(
                            &root,
                            &source_root,
                            &alias,
                            pre_sources,
                            &mut batch,
                            &mut report,
                            &mut summary,
                        )?;
                        batch.entries = 1;
                    }
                    let location = match git_location_from_descriptor(&directory, &name, &body) {
                        Ok(Some(location)) => location,
                        Ok(None) => location,
                        Err(error) => {
                            batch
                                .outcomes
                                .push(BulkPendingOutcome::Error(SourceAddError {
                                    location,
                                    kind: classify_bulk_error(&error),
                                }));
                            continue;
                        }
                    };
                    let source = SourceEntry {
                        source_id: location.source_id(),
                        location,
                        content_sha256,
                        bytes,
                        status: SourceStatus::Provisional,
                    };
                    let index = batch.sources.len();
                    batch.bytes += bytes;
                    batch.sources.push(source);
                    batch.outcomes.push(BulkPendingOutcome::Added(index));
                }
            }
        }
        publish_bulk_partition(
            &root,
            &source_root,
            &alias,
            pre_sources,
            &mut batch,
            &mut report,
            &mut summary,
        )?;
        Ok(summary)
    }
    #[cfg(not(unix))]
    {
        let _ = source_root;
        let _ = report;
        anyhow::bail!(
            "bulk directory source add is unavailable until no-follow traversal is implemented"
        )
    }
}

#[cfg(unix)]
#[derive(Default)]
struct BulkBatch {
    bytes: u64,
    entries: usize,
    sources: Vec<SourceEntry>,
    outcomes: Vec<BulkPendingOutcome>,
}

#[cfg(unix)]
enum BulkPendingOutcome {
    Added(usize),
    Error(SourceAddError),
    Skipped(SourceAddSkip),
}

#[cfg(unix)]
struct BulkDirectoryCursor {
    directory: fs::File,
    relative: PathBuf,
    after: Option<OsString>,
}

#[cfg(unix)]
struct BulkDirectoryWalker {
    cursors: Vec<BulkDirectoryCursor>,
}

#[cfg(unix)]
enum BulkWalkItem {
    EnteredDirectory,
    File {
        relative: String,
        directory: fs::File,
        name: OsString,
    },
    Error {
        relative: String,
    },
    Skipped {
        relative: String,
        kind: SourceAddSkipKind,
    },
}

#[cfg(unix)]
impl BulkWalkItem {
    fn relative(&self) -> &str {
        match self {
            Self::EnteredDirectory => "",
            Self::File { relative, .. }
            | Self::Error { relative }
            | Self::Skipped { relative, .. } => relative,
        }
    }
}

#[cfg(unix)]
impl BulkDirectoryWalker {
    fn new(root: fs::File) -> Result<Self> {
        Ok(Self {
            cursors: vec![BulkDirectoryCursor {
                directory: root,
                relative: PathBuf::new(),
                after: None,
            }],
        })
    }

    fn next(&mut self) -> Result<Option<BulkWalkItem>> {
        loop {
            let Some(cursor) = self.cursors.last_mut() else {
                return Ok(None);
            };
            let Some(name) =
                next_sorted_directory_entry_name(&cursor.directory, cursor.after.as_ref())?
            else {
                self.cursors.pop();
                continue;
            };
            cursor.after = Some(name.clone());
            if name == ".git" {
                continue;
            }
            let relative = cursor.relative.join(&name);
            let relative = relative
                .to_str()
                .context("local source directory contains a non-UTF-8 path component")?
                .to_owned();
            match directory_entry_kind(&cursor.directory, &name) {
                Ok(DirectoryEntryKind::Directory)
                    if matches!(
                        name.to_str(),
                        Some("_raw" | "_eval" | "_schema" | "_sources")
                    ) =>
                {
                    return Ok(Some(BulkWalkItem::Skipped {
                        relative,
                        kind: SourceAddSkipKind::AdministrativeDirectory,
                    }));
                }
                Ok(DirectoryEntryKind::Directory) => {
                    match open_child_directory(&cursor.directory, &name) {
                        Ok(child) => {
                            self.cursors.push(BulkDirectoryCursor {
                                directory: child,
                                relative: PathBuf::from(&relative),
                                after: None,
                            });
                            return Ok(Some(BulkWalkItem::EnteredDirectory));
                        }
                        Err(_) => return Ok(Some(BulkWalkItem::Error { relative })),
                    }
                }
                Ok(DirectoryEntryKind::RegularFile)
                    if name
                        .to_str()
                        .and_then(|name| name.split('.').next())
                        .is_some_and(|stem| matches!(stem, "README" | "index")) =>
                {
                    return Ok(Some(BulkWalkItem::Skipped {
                        relative,
                        kind: SourceAddSkipKind::StructuralFile,
                    }));
                }
                Ok(DirectoryEntryKind::RegularFile) => {
                    return Ok(Some(BulkWalkItem::File {
                        relative,
                        directory: cursor
                            .directory
                            .try_clone()
                            .context("duplicate stable bulk source directory descriptor")?,
                        name,
                    }))
                }
                Err(_) => return Ok(Some(BulkWalkItem::Error { relative })),
            }
        }
    }
}

#[cfg(unix)]
fn next_sorted_directory_entry_name(
    directory: &fs::File,
    after: Option<&OsString>,
) -> Result<Option<OsString>> {
    // ponytail: repeated descriptor scans keep traversal memory bounded; use an external bounded
    // sorter if a very large flat directory makes this O(n²) selection measurable.
    let dot = CString::new(".").expect("literal path has no NUL");
    let duplicate = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error())
            .context("reopen stable bulk source directory descriptor");
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(std::io::Error::last_os_error()).context("open bulk source directory stream");
    }
    let stream = DirectoryStream(stream);
    let mut next = None;
    loop {
        clear_errno()?;
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error().is_some_and(|code| code != 0) {
                return Err(error).context("read bulk source directory entry");
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = OsString::from_vec(name.to_vec());
        anyhow::ensure!(
            name.to_str().is_some(),
            "local source directory contains a non-UTF-8 path component"
        );
        if after.is_some_and(|after| name.as_encoded_bytes() <= after.as_encoded_bytes()) {
            continue;
        }
        if next
            .as_ref()
            .is_none_or(|next: &OsString| name.as_encoded_bytes() < next.as_encoded_bytes())
        {
            next = Some(name);
        }
    }
    Ok(next)
}

fn classify_bulk_error(error: &anyhow::Error) -> SourceAddErrorKind {
    if format!("{error:#}").contains("size limit") {
        SourceAddErrorKind::Oversized
    } else {
        SourceAddErrorKind::UnsafeOrUnreadable
    }
}

#[cfg(unix)]
fn publish_bulk_partition<F>(
    root: &Path,
    source_root: &Path,
    alias: &str,
    pre_sources: Option<&BTreeMap<String, SourceEntry>>,
    batch: &mut BulkBatch,
    report: &mut F,
    summary: &mut SourceAddSummary,
) -> Result<()>
where
    F: FnMut(SourceAddOutcome),
{
    if !batch.sources.is_empty() {
        let mut index = match load_source_index(root) {
            Ok(index) => index,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                SourceIndex {
                    schema: SOURCE_INDEX_SCHEMA.into(),
                    sources: Vec::new(),
                }
            }
            Err(error) => return Err(error),
        };
        let ids = batch
            .sources
            .iter()
            .map(|source| source.source_id.as_str())
            .collect::<HashSet<_>>();
        ensure_admission_sources_are_new(pre_sources, &index, &batch.sources)?;
        index
            .sources
            .retain(|source| !ids.contains(source.source_id.as_str()));
        index.sources.extend(batch.sources.iter().cloned());
        let bindings_path = root.join(".graphoxide/source-bindings.yaml");
        let mut bindings = load_source_bindings(&bindings_path)?;
        add_binding(&mut bindings, alias, source_root)?;
        bindings
            .bindings
            .sort_by(|left, right| left.alias.cmp(&right.alias));
        write_source_bindings(root, &bindings)?;
        write_source_index(root, &index)?;
    }
    for outcome in std::mem::take(&mut batch.outcomes) {
        match outcome {
            BulkPendingOutcome::Added(index) => {
                summary.added += 1;
                report(SourceAddOutcome::Added {
                    source: batch.sources[index].clone(),
                });
            }
            BulkPendingOutcome::Error(error) => {
                summary.errors += 1;
                report(SourceAddOutcome::Error { error });
            }
            BulkPendingOutcome::Skipped(skip) => {
                summary.skipped += 1;
                report(SourceAddOutcome::Skipped { skip });
            }
        }
    }
    *batch = BulkBatch::default();
    Ok(())
}

#[cfg(test)]
fn add_local_source_with_index_writer<F>(
    root: &Path,
    input: &Path,
    write_index: F,
) -> Result<SourceEntry>
where
    F: FnOnce(&Path, &SourceIndex) -> Result<()>,
{
    let mut sources = add_sources_with_index_writer(root, &[input.to_path_buf()], write_index)?;
    anyhow::ensure!(
        sources.len() == 1,
        "single source add must produce one source"
    );
    Ok(sources.remove(0))
}

#[cfg(test)]
fn add_sources_with_index_writer<F>(
    root: &Path,
    inputs: &[PathBuf],
    write_index: F,
) -> Result<Vec<SourceEntry>>
where
    F: FnOnce(&Path, &SourceIndex) -> Result<()>,
{
    add_sources_with_index_writer_and_limits(root, inputs, write_index, DEFAULT_SOURCE_BATCH_LIMITS)
}

#[cfg(test)]
fn add_sources_with_index_writer_and_limits<F>(
    root: &Path,
    inputs: &[PathBuf],
    write_index: F,
    limits: SourceBatchLimits,
) -> Result<Vec<SourceEntry>>
where
    F: FnOnce(&Path, &SourceIndex) -> Result<()>,
{
    add_sources_with_index_writer_and_limits_with_admission_pre(
        root,
        inputs,
        write_index,
        limits,
        None,
    )
}

fn add_sources_with_index_writer_and_limits_with_admission_pre<F>(
    root: &Path,
    inputs: &[PathBuf],
    write_index: F,
    limits: SourceBatchLimits,
    pre_sources: Option<&BTreeMap<String, SourceEntry>>,
) -> Result<Vec<SourceEntry>>
where
    F: FnOnce(&Path, &SourceIndex) -> Result<()>,
{
    let root_metadata = fs::symlink_metadata(root)
        .with_context(|| format!("read knowledgebase root at {}", root.display()))?;
    anyhow::ensure!(
        root_metadata.file_type().is_dir() && !root_metadata.file_type().is_symlink(),
        "knowledgebase root must be a real directory"
    );
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize knowledgebase root at {}", root.display()))?;
    anyhow::ensure!(!inputs.is_empty(), "source add requires at least one input");
    let bindings_path = root.join(".graphoxide/source-bindings.yaml");
    ensure_source_bindings_git_preflight(&root)?;
    let mut bindings = load_source_bindings(&bindings_path)?;
    let candidates = expand_source_inputs(&root, inputs, limits)?;
    let mut sources = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let body = read_staged_source(&candidate.staged)?;
        let location =
            git_location(&candidate.path, &body)?.unwrap_or_else(|| SourceLocation::BoundPath {
                binding: local_binding_alias(&candidate.source_root),
                path: candidate.relative,
            });
        add_binding(
            &mut bindings,
            &local_binding_alias(&candidate.source_root),
            &candidate.source_root,
        )?;
        sources.push(SourceEntry {
            source_id: location.source_id(),
            location,
            content_sha256: candidate.content_sha256,
            bytes: candidate.bytes,
            status: SourceStatus::Provisional,
        });
    }
    bindings
        .bindings
        .sort_by(|left, right| left.alias.cmp(&right.alias));
    let mut index = match load_source_index(&root) {
        Ok(index) => index,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            SourceIndex {
                schema: SOURCE_INDEX_SCHEMA.into(),
                sources: Vec::new(),
            }
        }
        Err(error) => return Err(error),
    };
    ensure_admission_sources_are_new(pre_sources, &index, &sources)?;
    let ids = sources
        .iter()
        .map(|source| source.source_id.as_str())
        .collect::<HashSet<_>>();
    index
        .sources
        .retain(|source| !ids.contains(source.source_id.as_str()));
    index.sources.extend(sources.iter().cloned());

    write_source_bindings(&root, &bindings)?;
    write_index(&root, &index)?;
    Ok(sources)
}

#[derive(Debug)]
struct SourceCandidate {
    path: PathBuf,
    source_root: PathBuf,
    relative: String,
    staged: tempfile::NamedTempFile,
    content_sha256: String,
    bytes: u64,
}

fn expand_source_inputs(
    root: &Path,
    inputs: &[PathBuf],
    limits: SourceBatchLimits,
) -> Result<Vec<SourceCandidate>> {
    let mut inputs = inputs.to_vec();
    inputs.sort();
    let mut candidates = BTreeMap::new();
    let mut entries = 0;
    for input in inputs {
        expand_source_input(root, &input, &mut candidates, &mut entries, limits)?;
    }
    anyhow::ensure!(
        !candidates.is_empty(),
        "source add inputs contain no regular files"
    );
    Ok(candidates.into_values().collect())
}

fn expand_source_input(
    root: &Path,
    input: &Path,
    candidates: &mut BTreeMap<PathBuf, SourceCandidate>,
    entries: &mut usize,
    limits: SourceBatchLimits,
) -> Result<()> {
    let metadata = fs::symlink_metadata(input)
        .with_context(|| format!("read local source input at {}", input.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "local source input must not be a symlink"
    );
    let canonical = input
        .canonicalize()
        .with_context(|| format!("canonicalize local source input at {}", input.display()))?;
    ensure_utf8_path(&canonical, "local source input")?;
    anyhow::ensure!(
        !canonical.starts_with(root),
        "local source must be outside the knowledgebase root"
    );
    if metadata.file_type().is_dir() {
        #[cfg(unix)]
        {
            let directory = open_directory_nofollow(&canonical)?;
            let mut expansion = DirectoryExpansion {
                root,
                source_root: &canonical,
                source_root_directory: &directory,
                candidates,
                entries,
                limits,
            };
            expand_directory(&mut expansion, &directory, Path::new(""))?;
        }
        #[cfg(not(unix))]
        anyhow::bail!("directory source add is unavailable on this platform until no-follow traversal is implemented");
    } else {
        let parent = canonical
            .parent()
            .context("local source must have a parent directory")?;
        #[cfg(unix)]
        {
            let directory = open_directory_nofollow(parent)?;
            add_candidate(
                root,
                parent,
                &directory,
                Path::new(
                    canonical
                        .file_name()
                        .context("local source must have a file name")?,
                ),
                candidates,
                limits,
            )?;
        }
        #[cfg(not(unix))]
        add_candidate_nonunix(root, parent, &canonical, candidates, limits)?;
    }
    Ok(())
}

#[cfg(unix)]
struct DirectoryExpansion<'a> {
    root: &'a Path,
    source_root: &'a Path,
    source_root_directory: &'a fs::File,
    candidates: &'a mut BTreeMap<PathBuf, SourceCandidate>,
    entries: &'a mut usize,
    limits: SourceBatchLimits,
}

#[cfg(unix)]
fn expand_directory(
    expansion: &mut DirectoryExpansion<'_>,
    directory: &fs::File,
    relative_directory: &Path,
) -> Result<()> {
    for name in
        sorted_directory_entry_names(directory, expansion.entries, expansion.limits.entries)?
    {
        if name == ".git" {
            continue;
        }
        let relative = relative_directory.join(&name);
        match directory_entry_kind(directory, &name)? {
            DirectoryEntryKind::Directory => {
                let child = open_child_directory(directory, &name)?;
                expand_directory(expansion, &child, &relative)?;
            }
            DirectoryEntryKind::RegularFile => {
                add_candidate(
                    expansion.root,
                    expansion.source_root,
                    expansion.source_root_directory,
                    &relative,
                    expansion.candidates,
                    expansion.limits,
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn add_candidate(
    root: &Path,
    source_root: &Path,
    directory: &fs::File,
    relative: &Path,
    candidates: &mut BTreeMap<PathBuf, SourceCandidate>,
    limits: SourceBatchLimits,
) -> Result<()> {
    let relative = relative
        .to_str()
        .context("local source path must be valid UTF-8")?
        .to_owned();
    validate_relative_path(&relative)?;
    let path = source_root.join(&relative);
    ensure_utf8_path(&path, "local source path")?;
    anyhow::ensure!(
        !path.starts_with(root),
        "local source must be outside the knowledgebase root"
    );
    anyhow::ensure!(
        candidates.len() < limits.files,
        "source add exceeds the {}-file batch limit",
        limits.files
    );
    let (staged, content_sha256, bytes) = stage_opened_source(directory, Path::new(&relative))?;
    let staged_bytes = candidates
        .values()
        .map(|candidate| candidate.bytes)
        .sum::<u64>();
    anyhow::ensure!(
        staged_bytes.saturating_add(bytes) <= limits.bytes,
        "source add exceeds the {}-byte batch limit",
        limits.bytes
    );
    candidates.entry(path.clone()).or_insert(SourceCandidate {
        path,
        source_root: source_root.to_owned(),
        relative,
        staged,
        content_sha256,
        bytes,
    });
    Ok(())
}

#[cfg(unix)]
fn open_directory_nofollow(path: &Path) -> Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| {
            format!(
                "open local source directory without following links at {}",
                path.display()
            )
        })
}

#[cfg(unix)]
fn sorted_directory_entry_names(
    directory: &fs::File,
    entries: &mut usize,
    max_entries: usize,
) -> Result<Vec<OsString>> {
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error())
            .context("duplicate local source directory descriptor");
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(std::io::Error::last_os_error()).context("open local source directory stream");
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        clear_errno()?;
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error().is_some_and(|code| code != 0) {
                return Err(error).context("read local source directory entry");
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            *entries = entries
                .checked_add(1)
                .context("source add directory entry count overflow")?;
            anyhow::ensure!(
                *entries <= max_entries,
                "source add exceeds the {max_entries}-entry traversal limit"
            );
            let name = OsString::from_vec(name.to_vec());
            anyhow::ensure!(
                name.to_str().is_some(),
                "local source directory contains a non-UTF-8 path component"
            );
            names.push(name);
        }
    }
    names.sort_by(|left, right| left.to_str().cmp(&right.to_str()));
    Ok(names)
}

#[cfg(not(unix))]
fn add_candidate_nonunix(
    root: &Path,
    source_root: &Path,
    path: &Path,
    candidates: &mut BTreeMap<PathBuf, SourceCandidate>,
    limits: SourceBatchLimits,
) -> Result<()> {
    let body = crate::enrich::safe_read_bounded(source_root, path, MAX_SOURCE_INDEX_BYTES as usize)
        .context("read bounded no-symlink local source")?;
    let relative = path
        .strip_prefix(source_root)
        .context("derive local source path relative to binding")?
        .to_str()
        .context("local source path must be valid UTF-8")?
        .to_owned();
    validate_relative_path(&relative)?;
    anyhow::ensure!(
        !path.starts_with(root),
        "local source must be outside the knowledgebase root"
    );
    anyhow::ensure!(
        candidates.len() < limits.files,
        "source add exceeds the {}-file batch limit",
        limits.files
    );
    let staged_bytes = candidates
        .values()
        .map(|candidate| candidate.bytes)
        .sum::<u64>();
    anyhow::ensure!(
        staged_bytes.saturating_add(body.len() as u64) <= limits.bytes,
        "source add exceeds the {}-byte batch limit",
        limits.bytes
    );
    candidates
        .entry(path.to_owned())
        .or_insert(SourceCandidate {
            path: path.to_owned(),
            source_root: source_root.to_owned(),
            relative,
            staged: stage_source(&body)?,
            content_sha256: hex::encode(Sha256::digest(&body)),
            bytes: body.len() as u64,
        });
    Ok(())
}

fn ensure_utf8_path(path: &Path, label: &str) -> Result<()> {
    anyhow::ensure!(
        path.to_str().is_some(),
        "{label} must use UTF-8 path components"
    );
    Ok(())
}

#[cfg(unix)]
struct DirectoryStream(*mut libc::DIR);

#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

#[cfg(unix)]
enum DirectoryEntryKind {
    Directory,
    RegularFile,
}

#[cfg(unix)]
fn directory_entry_kind(directory: &fs::File, name: &OsString) -> Result<DirectoryEntryKind> {
    let name = CString::new(name.as_encoded_bytes())
        .context("local source directory contains a NUL path component")?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .context("inspect local source directory entry");
    }
    match unsafe { stat.assume_init().st_mode } & libc::S_IFMT {
        libc::S_IFDIR => Ok(DirectoryEntryKind::Directory),
        libc::S_IFREG => Ok(DirectoryEntryKind::RegularFile),
        _ => {
            anyhow::bail!("local source directory must contain only regular files and directories")
        }
    }
}

#[cfg(unix)]
fn open_child_directory(directory: &fs::File, name: &OsString) -> Result<fs::File> {
    let name = CString::new(name.as_encoded_bytes())
        .context("local source directory contains a NUL path component")?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .context("open local source child directory without following links");
    }
    Ok(unsafe { fs::File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn stage_opened_source(
    directory: &fs::File,
    relative: &Path,
) -> Result<(tempfile::NamedTempFile, String, u64)> {
    let relative = relative
        .to_str()
        .context("local source path must be valid UTF-8")?;
    validate_relative_path(relative)?;
    let mut current = directory
        .try_clone()
        .context("duplicate local source root descriptor")?;
    let mut components = Path::new(relative).components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            anyhow::bail!("local source path must be relative and normalized");
        };
        let name = CString::new(name.as_encoded_bytes())
            .context("local source path contains a NUL component")?;
        let flags = if components.peek().is_some() {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC
        };
        let descriptor = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error())
                .context("open local source without following links");
        }
        current = unsafe { fs::File::from_raw_fd(descriptor) };
    }
    let before = current.metadata().context("inspect opened local source")?;
    anyhow::ensure!(
        before.file_type().is_file() && before.nlink() == 1,
        "local source must be an unlinked regular file"
    );
    anyhow::ensure!(
        before.len() <= MAX_SOURCE_INDEX_BYTES,
        "local source exceeds the {MAX_SOURCE_INDEX_BYTES}-byte size limit"
    );
    let mut bytes = Vec::with_capacity(before.len() as usize);
    std::io::Read::by_ref(&mut current)
        .take(MAX_SOURCE_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read bounded local source")?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_SOURCE_INDEX_BYTES,
        "local source exceeds the {MAX_SOURCE_INDEX_BYTES}-byte size limit"
    );
    let after = current
        .metadata()
        .context("reinspect opened local source")?;
    anyhow::ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && after.file_type().is_file()
            && after.nlink() == 1
            && after.len() <= MAX_SOURCE_INDEX_BYTES,
        "local source changed while being read"
    );
    let content_sha256 = hex::encode(Sha256::digest(&bytes));
    let bytes_len = bytes.len() as u64;
    let staged = stage_source(&bytes)?;
    Ok((staged, content_sha256, bytes_len))
}

fn stage_source(bytes: &[u8]) -> Result<tempfile::NamedTempFile> {
    let mut staged = tempfile::NamedTempFile::new()
        .context("stage local source in the OS temporary directory")?;
    staged
        .write_all(bytes)
        .context("stage bounded local source")?;
    staged.flush().context("flush staged local source")?;
    crate::enrich::safe_read_bounded(
        staged
            .path()
            .parent()
            .context("staged source must have a parent directory")?,
        staged.path(),
        MAX_SOURCE_INDEX_BYTES as usize,
    )
    .context("verify staged local source")?;
    Ok(staged)
}

fn read_staged_source(staged: &tempfile::NamedTempFile) -> Result<Vec<u8>> {
    crate::enrich::safe_read_bounded(
        staged
            .path()
            .parent()
            .context("staged source must have a parent directory")?,
        staged.path(),
        MAX_SOURCE_INDEX_BYTES as usize,
    )
    .context("read staged local source")
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[cfg(unix)]
fn clear_errno() -> Result<()> {
    unsafe { *libc::__errno_location() = 0 };
    Ok(())
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly"
))]
#[cfg(unix)]
fn clear_errno() -> Result<()> {
    unsafe { *libc::__error() = 0 };
    Ok(())
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))
))]
fn clear_errno() -> Result<()> {
    anyhow::bail!(
        "directory source add is unavailable on this platform until errno handling is implemented"
    )
}

fn add_binding(bindings: &mut SourceBindings, alias: &str, root: &Path) -> Result<()> {
    if let Some(existing) = bindings
        .bindings
        .iter()
        .find(|existing| existing.alias == alias)
    {
        anyhow::ensure!(
            existing.root == root,
            "local source binding alias collision maps to a different root"
        );
    } else {
        bindings.bindings.push(SourceBinding {
            alias: alias.to_owned(),
            root: root.to_owned(),
        });
    }
    Ok(())
}

fn git_location(path: &Path, bytes: &[u8]) -> Result<Option<SourceLocation>> {
    let parent = path
        .parent()
        .context("Git source must have a parent directory")?;
    let root = match git_discovery(
        GitWorkingDirectory::Path(parent).output(&["rev-parse", "--show-toplevel"])?,
    )? {
        Some(output) => PathBuf::from(std::str::from_utf8(&output)?.trim()).canonicalize()?,
        None => return Ok(None),
    };
    let relative = git_relative_path(
        path.strip_prefix(&root)
            .context("Git source must be below repository root")?,
    )?;
    git_location_in_worktree(GitWorkingDirectory::Path(&root), &relative, bytes).map(Some)
}

fn git_relative_path(path: &Path) -> Result<String> {
    // Git object paths and pathspecs use slashes on every host platform.
    let components = path
        .components()
        .map(|component| {
            let Component::Normal(name) = component else {
                anyhow::bail!("Git source path must be relative and normalized");
            };
            name.to_str().context("Git source path must be UTF-8")
        })
        .collect::<Result<Vec<_>>>()?;
    let relative = components.join("/");
    validate_relative_path(&relative)?;
    Ok(relative)
}

enum GitWorkingDirectory<'a> {
    Path(&'a Path),
    #[cfg(unix)]
    Descriptor(&'a fs::File),
}

fn git_source_command() -> Command {
    let mut command = Command::new("git");
    command
        .arg("--no-replace-objects")
        .env("LC_ALL", "C")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn read_git_output_bounded(command: &mut Command, max_bytes: usize) -> Result<Option<Vec<u8>>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start pinned Git source read")?;
    let mut body = Vec::new();
    let read = child
        .stdout
        .take()
        .expect("pinned Git source stdout is piped")
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut body);
    if read.is_err() || body.len() > max_bytes {
        let _ = child.kill();
        let _ = child.wait();
        read.context("read pinned Git source output")?;
        anyhow::bail!("pinned Git source output exceeds the size limit");
    }
    let status = child.wait().context("finish pinned Git source read")?;
    Ok(status.success().then_some(body))
}

impl GitWorkingDirectory<'_> {
    fn output(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut command = git_source_command();
        match self {
            Self::Path(path) => {
                command.arg("-C").arg(path);
            }
            #[cfg(unix)]
            Self::Descriptor(directory) => {
                let descriptor = directory.as_raw_fd();
                // Enter the held directory in the child before exec closes CLOEXEC descriptors.
                // fchdir is async-signal-safe and avoids reopening a path that may be replaced.
                unsafe {
                    command.pre_exec(move || {
                        if libc::fchdir(descriptor) == 0 {
                            Ok(())
                        } else {
                            Err(std::io::Error::last_os_error())
                        }
                    });
                }
            }
        }
        command
            .args(args)
            .output()
            .context("run Git source command")
    }
}

fn git_discovery(output: std::process::Output) -> Result<Option<Vec<u8>>> {
    if output.status.success() {
        return Ok(Some(output.stdout));
    }
    let error = std::str::from_utf8(&output.stderr).unwrap_or_default();
    if output.status.code() == Some(128)
        && (error
            .starts_with("fatal: not a git repository (or any of the parent directories): .git\n")
            || error.starts_with("fatal: not a git repository (or any parent up to mount point "))
    {
        return Ok(None);
    }
    anyhow::bail!("Git source discovery failed; verify repository access, ownership, and metadata")
}

fn git_location_in_worktree(
    worktree: GitWorkingDirectory<'_>,
    relative: &str,
    bytes: &[u8],
) -> Result<SourceLocation> {
    let commit = worktree.output(&["rev-parse", "HEAD"])?;
    anyhow::ensure!(commit.status.success(), "Git source requires a HEAD commit");
    let commit = std::str::from_utf8(&commit.stdout)?.trim().to_owned();
    let object = format!("{commit}:{relative}");
    let tracked = worktree.output(&[
        "ls-files",
        "--error-unmatch",
        "--",
        &format!(":(top){relative}"),
    ])?;
    anyhow::ensure!(
        tracked.status.success(),
        "Git source must be tracked at HEAD"
    );
    let size = worktree.output(&["cat-file", "-s", &object])?;
    anyhow::ensure!(
        size.status.success(),
        "Git source must resolve to a HEAD blob"
    );
    anyhow::ensure!(
        std::str::from_utf8(&size.stdout)?.trim().parse::<u64>()? <= MAX_SOURCE_INDEX_BYTES,
        "Git source exceeds the {MAX_SOURCE_INDEX_BYTES}-byte size limit"
    );
    let blob = worktree.output(&["show", &object])?;
    anyhow::ensure!(
        blob.status.success() && blob.stdout == bytes,
        "Git source must exactly match its HEAD blob"
    );
    let remote = worktree.output(&["remote", "get-url", "origin"])?;
    anyhow::ensure!(
        remote.status.success(),
        "Git source requires an origin remote"
    );
    let remote = normalize_git_remote(std::str::from_utf8(&remote.stdout)?.trim())?;
    Ok(SourceLocation::Git {
        remote,
        commit,
        path: relative.to_owned(),
    })
}

#[cfg(unix)]
fn git_location_from_descriptor(
    directory: &fs::File,
    name: &OsString,
    bytes: &[u8],
) -> Result<Option<SourceLocation>> {
    // Keep the directory away from standard streams that Command::output redirects in the child.
    let descriptor = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .context("duplicate stable Git source directory descriptor");
    }
    let directory = unsafe { fs::File::from_raw_fd(descriptor) };
    let worktree = GitWorkingDirectory::Descriptor(&directory);
    let prefix = match git_discovery(worktree.output(&["rev-parse", "--show-prefix"])?)? {
        Some(output) => std::str::from_utf8(&output)?.trim().to_owned(),
        None => return Ok(None),
    };
    let name = name.to_str().context("Git source path must be UTF-8")?;
    validate_relative_path(name)?;
    let relative = format!("{prefix}{name}");
    validate_relative_path(&relative)?;
    git_location_in_worktree(worktree, &relative, bytes).map(Some)
}

fn normalize_git_remote(remote: &str) -> Result<String> {
    let normalized = if remote.starts_with("https://") {
        remote.to_owned()
    } else if remote.starts_with("ssh://") {
        let parsed = reqwest::Url::parse(remote).context("Git SSH remote must be a valid URL")?;
        anyhow::ensure!(
            parsed.scheme() == "ssh"
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "Git SSH remote must not contain a password, query, or fragment"
        );
        let host = parsed
            .host_str()
            .context("Git SSH remote must include a host")?;
        let port = parsed
            .port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        format!("ssh://{host}{port}{}", parsed.path())
    } else if let Some((_, host_path)) = remote.split_once('@') {
        let (host, path) = host_path
            .split_once(':')
            .context("Git scp-style remote requires one path separator")?;
        anyhow::ensure!(
            !host.is_empty() && !path.is_empty() && !host.contains(':') && !path.contains(':'),
            "Git scp-style remote is malformed"
        );
        format!("ssh://{host}/{path}")
    } else {
        anyhow::bail!("Git remote must be HTTPS or a standard SSH Git address");
    };
    validate_git_remote(&normalized, "Git remote")?;
    Ok(normalized)
}

fn local_binding_alias(root: &Path) -> String {
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    format!("local-{}", hex::encode(digest))
}

fn load_source_bindings(path: &Path) -> Result<SourceBindings> {
    let root = path
        .parent()
        .and_then(Path::parent)
        .context("source binding path must be below the knowledgebase root")?;
    match crate::enrich::safe_read_bounded(root, path, MAX_SOURCE_BINDINGS_BYTES) {
        Ok(bytes) => {
            let bindings: SourceBindings = serde_norway::from_slice(&bytes)
                .context("parse source add binding configuration YAML")?;
            validate_source_bindings(&bindings)?;
            Ok(bindings)
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(SourceBindings {
                schema: SOURCE_BINDINGS_SCHEMA.into(),
                bindings: Vec::new(),
            })
        }
        Err(error) => Err(error).context("read source add binding configuration"),
    }
}

fn validate_source_bindings(bindings: &SourceBindings) -> Result<()> {
    anyhow::ensure!(
        bindings.schema == SOURCE_BINDINGS_SCHEMA,
        "source add binding configuration has an unsupported schema"
    );
    let mut aliases = HashSet::new();
    let mut roots = HashSet::new();
    let mut previous_alias = None;
    for binding in &bindings.bindings {
        anyhow::ensure!(
            binding.alias.starts_with("local-") && is_lower_hex(&binding.alias[6..], 64),
            "source binding alias must be a stable local digest"
        );
        anyhow::ensure!(
            aliases.insert(&binding.alias),
            "source binding aliases must be unique"
        );
        anyhow::ensure!(
            previous_alias.is_none_or(|previous| previous < binding.alias.as_str()),
            "source bindings must be sorted by alias"
        );
        previous_alias = Some(binding.alias.as_str());
        anyhow::ensure!(
            binding.root.is_absolute(),
            "source binding root must be absolute"
        );
        let metadata = fs::symlink_metadata(&binding.root)
            .with_context(|| format!("read source binding root at {}", binding.root.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "source binding root must be a real directory"
        );
        let canonical = binding
            .root
            .canonicalize()
            .context("canonicalize source binding root")?;
        anyhow::ensure!(
            canonical == binding.root,
            "source binding root must be canonical"
        );
        anyhow::ensure!(
            roots.insert(&binding.root),
            "source binding roots must be unique"
        );
    }
    Ok(())
}

fn write_source_bindings(root: &Path, bindings: &SourceBindings) -> Result<()> {
    validate_source_bindings(bindings)?;
    ensure_source_bindings_git_preflight(root)?;
    let directory = root.join(".graphoxide");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) => anyhow::ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "source binding directory must be a real directory"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&directory)
            .with_context(|| {
                format!("create source binding directory at {}", directory.display())
            })?,
        Err(error) => return Err(error).context("read source binding directory"),
    }
    let yaml =
        serde_norway::to_string(bindings).context("serialize source add binding configuration")?;
    ensure_source_bindings_ignored(root)?;
    ensure_source_bindings_git_ignored(root)?;
    graphoxide_core::write_text_atomic_strict(directory.join("source-bindings.yaml"), &yaml)
        .context("write source add bindings atomically")
}

fn ensure_source_bindings_ignored(root: &Path) -> Result<()> {
    let ignore = root.join(".gitignore");
    let mut text = match crate::enrich::safe_read_bounded(root, &ignore, MAX_SOURCE_BINDINGS_BYTES)
    {
        Ok(bytes) => String::from_utf8(bytes).context("source ignore file must be UTF-8")?,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            String::new()
        }
        Err(error) => return Err(error).context("read source ignore file"),
    };
    if !text
        .lines()
        .any(|line| line == ".graphoxide/source-bindings.yaml")
    {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(".graphoxide/source-bindings.yaml\n");
        graphoxide_core::write_text_atomic_strict(&ignore, &text)
            .context("write source binding ignore rule")?;
    }
    Ok(())
}

fn source_bindings_git(root: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("run git while validating source binding privacy")
}

fn ensure_source_bindings_git_preflight(root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .context("canonicalize knowledgebase root for source binding privacy")?;
    let root = root.as_path();
    let worktree = source_bindings_git(root, &["rev-parse", "--is-inside-work-tree"])?;
    anyhow::ensure!(
        worktree.status.success() && worktree.stdout == b"true\n",
        "source add requires a Git worktree"
    );
    let git_root = source_bindings_git(root, &["rev-parse", "--show-toplevel"])?;
    anyhow::ensure!(
        git_root.status.success(),
        "source add requires a Git worktree root"
    );
    let git_root = PathBuf::from(
        std::str::from_utf8(&git_root.stdout)
            .context("Git worktree root must be UTF-8")?
            .trim(),
    )
    .canonicalize()
    .context("canonicalize Git worktree root")?;
    anyhow::ensure!(
        git_root == root,
        "source add requires the knowledgebase root to be the Git worktree root"
    );
    let tracked = source_bindings_git(
        root,
        &[
            "ls-files",
            "--error-unmatch",
            "--",
            ".graphoxide/source-bindings.yaml",
        ],
    )?;
    anyhow::ensure!(
        !tracked.status.success(),
        "source bindings must not be Git-tracked"
    );
    Ok(())
}

fn ensure_source_bindings_git_ignored(root: &Path) -> Result<()> {
    let ignored = source_bindings_git(
        root,
        &[
            "check-ignore",
            "--quiet",
            ".graphoxide/source-bindings.yaml",
        ],
    )?;
    anyhow::ensure!(
        ignored.status.success(),
        "source bindings must be Git-ignored"
    );
    Ok(())
}

impl SourceIndex {
    pub fn from_json(json: impl AsRef<[u8]>) -> Result<Self> {
        let json = json.as_ref();
        anyhow::ensure!(
            json.len() <= MAX_SOURCE_INDEX_BYTES as usize,
            "source index exceeds the {MAX_SOURCE_INDEX_BYTES}-byte size limit"
        );
        let mut index: Self = serde_json::from_slice(json).context("parse source index JSON")?;
        index.normalize_and_validate()?;
        Ok(index)
    }

    fn normalize_and_validate(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.schema == SOURCE_INDEX_SCHEMA,
            "source index has an unsupported schema"
        );
        for source in &mut self.sources {
            source.validate()?;
        }
        self.sources
            .sort_by(|left, right| left.source_id.cmp(&right.source_id));
        anyhow::ensure!(
            self.sources
                .windows(2)
                .all(|pair| pair[0].source_id != pair[1].source_id),
            "source index contains a duplicate source id"
        );
        Ok(())
    }
}

impl SourceEntry {
    fn validate(&self) -> Result<()> {
        self.location.validate()?;
        anyhow::ensure!(
            self.source_id == self.location.source_id(),
            "source index source id does not match its stable location"
        );
        anyhow::ensure!(
            is_lower_hex(&self.content_sha256, 64),
            "source index content digest must be 64 lowercase hexadecimal characters"
        );
        Ok(())
    }
}

impl SourceLocation {
    pub fn source_id(&self) -> String {
        let mut digest = Sha256::new();
        match self {
            Self::Git {
                remote,
                commit: _,
                path,
            } => {
                digest.update(b"git\0");
                digest.update(remote.as_bytes());
                digest.update(b"\0");
                digest.update(path.as_bytes());
            }
            Self::Https { url } => {
                digest.update(b"https\0");
                digest.update(url.as_bytes());
            }
            Self::BoundPath { binding, path } => {
                digest.update(b"bound-path\0");
                digest.update(binding.as_bytes());
                digest.update(b"\0");
                digest.update(path.as_bytes());
            }
        }
        format!("src:{}", hex::encode(digest.finalize()))
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Git {
                remote,
                commit,
                path,
            } => {
                validate_git_remote(remote, "Git remote")?;
                anyhow::ensure!(
                    is_lower_hex(commit, 40) || is_lower_hex(commit, 64),
                    "source index Git commit must be 40 or 64 lowercase hexadecimal characters"
                );
                validate_relative_path(path)?;
            }
            Self::Https { url } => validate_https_url(url, "HTTPS location")?,
            Self::BoundPath { binding, path } => {
                anyhow::ensure!(
                    !binding.is_empty()
                        && binding != "."
                        && binding != ".."
                        && binding
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
                    "source index path binding must be a local alias, not a physical path"
                );
                validate_relative_path(path)?;
            }
        }
        Ok(())
    }
}

fn validate_https_url(value: &str, label: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(value)
        .with_context(|| format!("source index {label} must be a valid HTTPS URL"))?;
    anyhow::ensure!(
        parsed.scheme() == "https" && parsed.host_str().is_some(),
        "source index {label} must be a valid HTTPS URL"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "source index {label} must not contain credentials"
    );
    anyhow::ensure!(
        parsed.query().is_none(),
        "source index {label} must not contain a query string"
    );
    anyhow::ensure!(
        parsed.fragment().is_none(),
        "source index {label} must not contain a fragment"
    );
    anyhow::ensure!(
        parsed.as_str() == value,
        "source index {label} must use its normalized HTTPS form"
    );
    Ok(())
}

fn validate_git_remote(value: &str, label: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(value)
        .with_context(|| format!("source index {label} must be a valid Git remote URL"))?;
    if parsed.scheme() == "https" {
        return validate_https_url(value, label);
    }
    anyhow::ensure!(
        parsed.scheme() == "ssh" && parsed.host_str().is_some(),
        "source index {label} must be a valid HTTPS or SSH Git remote URL"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "source index {label} must not contain credentials"
    );
    anyhow::ensure!(
        parsed.query().is_none(),
        "source index {label} must not contain a query string"
    );
    anyhow::ensure!(
        parsed.fragment().is_none(),
        "source index {label} must not contain a fragment"
    );
    anyhow::ensure!(
        parsed.path().starts_with('/')
            && parsed.path() != "/"
            && parsed
                .path()
                .split('/')
                .skip(1)
                .all(|component| !component.is_empty() && component != "." && component != ".."),
        "source index {label} must contain a normalized repository path"
    );
    anyhow::ensure!(
        parsed.as_str() == value,
        "source index {label} must use its normalized SSH form"
    );
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<()> {
    let windows_drive = path.as_bytes().get(1) == Some(&b':')
        && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    anyhow::ensure!(
        !path.is_empty() && !path.starts_with(['/', '\\']) && !windows_drive,
        "source index path must be relative"
    );
    anyhow::ensure!(
        !path.contains('\\')
            && path
                .split('/')
                .all(|component| !component.is_empty() && component != "." && component != ".."),
        "source index path must be normalized and may not escape its root"
    );
    Ok(())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn load_source_index(root: &Path) -> Result<SourceIndex> {
    let path = root.join("sources/index.json");
    let bytes = crate::enrich::safe_read_bounded(root, &path, MAX_SOURCE_INDEX_BYTES as usize)
        .context("read bounded no-symlink source index")?;
    SourceIndex::from_json(bytes)
}

fn load_source_index_or_empty(root: &Path) -> Result<SourceIndex> {
    match load_source_index(root) {
        Ok(index) => Ok(index),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(SourceIndex {
                schema: SOURCE_INDEX_SCHEMA.into(),
                sources: Vec::new(),
            })
        }
        Err(error) => Err(error),
    }
}

fn canonical_admission_root(root: &Path) -> Result<PathBuf> {
    let metadata =
        fs::symlink_metadata(root).context("read knowledgebase root for source admission")?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "knowledgebase root must be a real directory"
    );
    root.canonicalize()
        .context("canonicalize knowledgebase root for source admission")
}

fn source_index_revision(index: &SourceIndex) -> Result<String> {
    let mut canonical = index.clone();
    canonical.normalize_and_validate()?;
    let bytes = serde_json::to_vec(&canonical).context("serialize canonical source index")?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub fn write_source_index(root: &Path, index: &SourceIndex) -> Result<()> {
    let mut normalized = index.clone();
    normalized.normalize_and_validate()?;

    let root_metadata = fs::symlink_metadata(root)
        .with_context(|| format!("read knowledgebase root at {}", root.display()))?;
    anyhow::ensure!(
        root_metadata.file_type().is_dir() && !root_metadata.file_type().is_symlink(),
        "knowledgebase root must be a real directory"
    );
    let sources = root.join("sources");
    match fs::symlink_metadata(&sources) {
        Ok(metadata) => anyhow::ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "source index directory must be a real directory"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&sources).with_context(|| {
                format!("create source index directory at {}", sources.display())
            })?;
        }
        Err(error) => return Err(error).context("read source index directory"),
    }
    graphoxide_core::write_json_atomic_strict(sources.join("index.json"), &normalized, true)
        .context("write source index atomically")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::fs;

    const BOUND_SOURCE_ID: &str =
        "src:679fcf54f8248af6dbf8eb5600f0ee77cb322699d12b84524d51208601c38b4c";

    fn valid_bound_index() -> Value {
        json!({
            "schema": "graphoxide.source-index",
            "sources": [{
                "source_id": BOUND_SOURCE_ID,
                "location": {"kind": "bound-path", "binding": "private", "path": "docs/a.md"},
                "content_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "bytes": 6,
                "status": "provisional"
            }]
        })
    }

    fn parse_value(value: &Value) -> Result<SourceIndex> {
        SourceIndex::from_json(serde_json::to_vec(value).expect("serialize test index"))
    }

    fn walk_files(root: &Path) -> Vec<String> {
        fn visit(root: &Path, directory: &Path, files: &mut Vec<String>) {
            for entry in fs::read_dir(directory).expect("read test directory") {
                let entry = entry.expect("read test entry");
                let path = entry.path();
                if path.file_name().is_some_and(|name| name == ".git") {
                    continue;
                }
                if entry.file_type().expect("read test entry type").is_dir() {
                    visit(root, &path, files);
                } else {
                    files.push(
                        path.strip_prefix(root)
                            .expect("test file under root")
                            .iter()
                            .map(|component| component.to_str().expect("UTF-8 test path"))
                            .collect::<Vec<_>>()
                            .join("/"),
                    );
                }
            }
        }

        let mut files = Vec::new();
        visit(root, root, &mut files);
        files.sort();
        files
    }

    fn init_git(root: &Path) {
        let initialized = std::process::Command::new("git")
            .args(["init", "--quiet", root.to_str().expect("UTF-8 root")])
            .status()
            .expect("initialize test repository");
        assert!(initialized.success());
    }

    fn seal_error(admission: SourceAdmissionTransaction) -> anyhow::Error {
        match admission.seal() {
            Ok(_) => panic!("source admission seal must fail"),
            Err(error) => error,
        }
    }

    #[cfg(unix)]
    fn collect_bulk_outcomes(
        root: &Path,
        input: &Path,
    ) -> Result<(SourceAddSummary, Vec<SourceAddOutcome>)> {
        let mut outcomes = Vec::new();
        let summary = add_source_directory(root, input, |outcome| outcomes.push(outcome))?;
        Ok((summary, outcomes))
    }

    #[test]
    fn source_index_rejects_a_local_absolute_path_and_raw_body_field() {
        let error = SourceIndex::from_json(
            br#"{
                "schema":"graphoxide.source-index",
                "sources":[{
                    "source_id":"src:x",
                    "location":{"kind":"bound-path","binding":"private","path":"/private/example.md"},
                    "content_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "bytes":6,
                    "status":"provisional",
                    "raw":"secret"
                }]
            }"#,
        )
        .expect_err("unsafe index must fail");

        assert!(error.to_string().contains("source index"));
    }

    #[test]
    fn source_index_from_json_rejects_oversized_input_before_parsing() {
        let error = SourceIndex::from_json(vec![b' '; MAX_SOURCE_INDEX_BYTES as usize + 1])
            .expect_err("oversized in-memory index must fail");
        assert!(error.to_string().contains("size limit"), "{error:#}");
    }

    #[test]
    fn source_index_rejects_derived_page_ids() {
        let mut index = valid_bound_index();
        index["sources"][0]["page_ids"] = json!(["page:derived"]);

        let error =
            parse_value(&index).expect_err("derived page ids must not enter the locator index");
        assert!(
            format!("{error:#}").contains("unknown field `page_ids`"),
            "{error:#}"
        );
    }

    #[test]
    fn source_index_rejects_absolute_and_escaping_bound_paths() {
        for (binding, path) in [
            ("private", "/private/example.md"),
            ("private", "docs/../private.md"),
            ("private", r"C:\Users\user\private.md"),
            ("../private", "docs/a.md"),
        ] {
            let mut value = valid_bound_index();
            value["sources"][0]["location"]["binding"] = binding.into();
            value["sources"][0]["location"]["path"] = path.into();

            let error = parse_value(&value).expect_err("physical or escaping path must fail");
            assert!(error.to_string().contains("source index path"));
        }
    }

    #[test]
    fn source_index_rejects_credential_bearing_urls() {
        for (location, expected) in [
            (
                json!({"kind":"https", "url":"https://docs.example.com/a?X-Amz-Signature=secret"}),
                "query",
            ),
            (
                json!({"kind":"https", "url":"https://user:secret@docs.example.com/a"}),
                "credentials",
            ),
            (
                json!({"kind":"https", "url":"https://docs.example.com/a#private"}),
                "fragment",
            ),
            (
                json!({"kind":"git", "remote":"https://git.example.com/group/repo.git?token=secret", "commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "path":"docs/a.md"}),
                "query",
            ),
            (
                json!({"kind":"git", "remote":"https://user:secret@git.example.com/group/repo.git", "commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "path":"docs/a.md"}),
                "credentials",
            ),
            (
                json!({"kind":"git", "remote":"https://git.example.com/group/repo.git#private", "commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "path":"docs/a.md"}),
                "fragment",
            ),
        ] {
            let mut value = valid_bound_index();
            value["sources"][0]["location"] = location;

            let error = parse_value(&value).expect_err("credential-bearing URL must fail");
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn git_remote_normalization_preserves_credential_free_ssh_hosts_ports_and_paths() {
        assert_eq!(
            normalize_git_remote("git@git.example.com:group/repo.git")
                .expect("normalize scp remote"),
            "ssh://git.example.com/group/repo.git"
        );
        assert_eq!(
            normalize_git_remote("ssh://git@git.example.com:2222/group/project.git")
                .expect("normalize ssh remote"),
            "ssh://git.example.com:2222/group/project.git"
        );
        assert!(normalize_git_remote("git@git.example.com:group:repo.git").is_err());
        for unsafe_remote in [
            "ssh://git@git.example.com:12051/group/repo.git?token=secret",
            "ssh://git@git.example.com:12051/group/repo.git#private",
            "ssh://git:secret@git.example.com:12051/group/repo.git",
            "file:///private/repo.git",
        ] {
            assert!(
                normalize_git_remote(unsafe_remote).is_err(),
                "{unsafe_remote}"
            );
        }
    }

    #[test]
    fn source_index_rejects_invalid_entry_structure() {
        for (field, value, expected) in [
            ("source_id", json!("source:not-derived"), "source id"),
            ("content_sha256", json!("ABC123"), "content digest"),
        ] {
            let mut index = valid_bound_index();
            index["sources"][0][field] = value;

            let error = parse_value(&index).expect_err("invalid source entry must fail");
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn source_id_is_derived_only_from_the_stable_location() {
        let location = SourceLocation::BoundPath {
            binding: "private".into(),
            path: "docs/a.md".into(),
        };

        assert_eq!(location.source_id(), BOUND_SOURCE_ID);
    }

    #[test]
    fn git_source_id_is_stable_across_commit_advance() {
        let at_first_commit = SourceLocation::Git {
            remote: "https://git.example.com/group/repo.git".into(),
            commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            path: "docs/a.md".into(),
        };
        let at_second_commit = SourceLocation::Git {
            remote: "https://git.example.com/group/repo.git".into(),
            commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            path: "docs/a.md".into(),
        };

        assert_eq!(at_first_commit.source_id(), at_second_commit.source_id());
    }

    #[test]
    fn ssh_git_location_preserves_port_without_a_username_and_has_a_stable_source_id() {
        let first = SourceLocation::Git {
            remote: "ssh://git.example.com:2222/group/project.git".into(),
            commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            path: "docs/protocols/README.md".into(),
        };
        let second = SourceLocation::Git {
            remote: "ssh://git.example.com:2222/group/project.git".into(),
            commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            path: "docs/protocols/README.md".into(),
        };

        first.validate().expect("canonical SSH locator is valid");
        assert_eq!(first.source_id(), second.source_id());
        assert!(!matches!(&first, SourceLocation::Git { remote, .. } if remote.contains('@')));
    }

    #[test]
    fn source_add_local_file_records_only_a_bound_pointer() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        fs::write(root.path().join(".gitignore"), "# retain this rule\n")
            .expect("write existing ignore rule");
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("private.md");
        fs::write(&input, "private source body").expect("write external source");

        add_local_source(root.path(), &input).expect("add local source");

        let bindings = root.path().join(".graphoxide/source-bindings.yaml");
        let binding_text = fs::read_to_string(&bindings).expect("read source binding");
        assert!(binding_text.contains(
            &external
                .path()
                .canonicalize()
                .expect("canonical root")
                .display()
                .to_string()
        ));

        let index = load_source_index(root.path()).expect("load source index");
        assert_eq!(index.sources.len(), 1);
        assert!(matches!(
            &index.sources[0].location,
            SourceLocation::BoundPath { path, .. } if path == "private.md"
        ));
        assert_eq!(index.sources[0].status, SourceStatus::Provisional);
        assert_eq!(index.sources[0].bytes, "private source body".len() as u64);
        assert_eq!(
            index.sources[0].content_sha256,
            hex::encode(Sha256::digest(b"private source body"))
        );

        let files = walk_files(root.path());
        assert_eq!(
            files,
            [
                ".gitignore",
                ".graphoxide/source-bindings.yaml",
                "sources/index.json",
            ]
        );
        assert_eq!(
            fs::read_to_string(root.path().join(".gitignore")).expect("read ignore file"),
            "# retain this rule\n.graphoxide/source-bindings.yaml\n"
        );
        assert!(std::process::Command::new("git")
            .args([
                "-C",
                root.path().to_str().expect("UTF-8 root"),
                "check-ignore",
                ".graphoxide/source-bindings.yaml",
            ])
            .status()
            .expect("check ignored binding")
            .success());
        for file in &files {
            assert!(
                !fs::read(root.path().join(file))
                    .expect("read knowledgebase artifact")
                    .windows(b"private source body".len())
                    .any(|part| part == b"private source body"),
                "raw source leaked into {file}"
            );
        }
        assert!(!root.path().join("private.md").exists());
    }

    #[test]
    fn legacy_binding_yaml_round_trips_quoted_unicode_roots_deterministically() {
        let root = tempfile::tempdir().expect("knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external parent");
        let directory = external.path().join("références #1 [yes]");
        fs::create_dir(&directory).expect("source directory with YAML punctuation");
        let directory = directory.canonicalize().expect("canonical source root");
        let expected = SourceBindings {
            schema: SOURCE_BINDINGS_SCHEMA.into(),
            bindings: vec![SourceBinding {
                alias: local_binding_alias(&directory),
                root: directory.clone(),
            }],
        };
        fs::create_dir(root.path().join(".graphoxide")).expect("runtime directory");
        let path = root.path().join(".graphoxide/source-bindings.yaml");
        // Existing bindings use ordinary YAML mappings; a JSON-quoted scalar is
        // also valid YAML and preserves Windows backslashes and comment markers.
        let legacy = format!(
            "---\nschema: {SOURCE_BINDINGS_SCHEMA}\nbindings:\n  - alias: {}\n    root: {}\n",
            expected.bindings[0].alias,
            serde_json::to_string(&directory).expect("quoted source root")
        );
        fs::write(&path, legacy).expect("legacy binding YAML");

        assert_eq!(
            load_source_bindings(&path).expect("read legacy YAML"),
            expected
        );
        write_source_bindings(root.path(), &expected).expect("write current YAML");
        let first = fs::read(&path).expect("first serialization");
        assert_eq!(
            load_source_bindings(&path).expect("read current YAML"),
            expected
        );
        write_source_bindings(root.path(), &expected).expect("repeat current serialization");
        assert_eq!(fs::read(path).expect("second serialization"), first);
    }

    #[test]
    fn bound_source_add_resolves_an_ignored_alias_without_persisting_its_body() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("private.md");
        let body = "private bound source body";
        fs::write(&input, body).expect("write external source");
        let initial = add_local_source(root.path(), &input).expect("create ignored binding");
        let (binding, relative_path) = match &initial.location {
            SourceLocation::BoundPath { binding, path } => (binding.clone(), path.clone()),
            location => panic!("expected bound pointer, got {location:?}"),
        };
        let mut index = load_source_index(root.path()).expect("load source index");
        index.sources[0].status = SourceStatus::HumanConfirmed;
        write_source_index(root.path(), &index).expect("record review");

        let source = add_bound_source(root.path(), &binding, &relative_path)
            .expect("resolve the existing logical pointer");

        assert_eq!(source.source_id, initial.source_id);
        assert_eq!(source.status, SourceStatus::HumanConfirmed);
        assert!(matches!(
            &source.location,
            SourceLocation::BoundPath { binding: actual, path }
                if actual == &binding && path == &relative_path
        ));
        assert_eq!(
            source_status(root.path()).expect("read source status"),
            vec![source]
        );
        let index_text =
            fs::read_to_string(root.path().join("sources/index.json")).expect("read pointer index");
        assert!(!index_text.contains(body));
        assert!(!index_text.contains(&external.path().display().to_string()));
    }

    #[test]
    fn bound_source_add_rejects_unsafe_or_unknown_locations_without_root_disclosure() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("private.md");
        fs::write(&input, "private source body").expect("write external source");
        let source = add_local_source(root.path(), &input).expect("create ignored binding");
        let binding = match &source.location {
            SourceLocation::BoundPath { binding, .. } => binding,
            location => panic!("expected bound pointer, got {location:?}"),
        };

        let escape = add_bound_source(root.path(), binding, "../outside.md")
            .expect_err("escaping relative source must be rejected before binding access");
        assert!(format!("{escape:#}").contains("escape"), "{escape:#}");

        let unknown = add_bound_source(
            root.path(),
            "local-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "private.md",
        )
        .expect_err("unknown logical binding must be rejected");
        let detail = format!("{unknown:#}");
        assert!(detail.contains("binding is unavailable"), "{detail}");
        assert!(
            !detail.contains(&external.path().display().to_string()),
            "{detail}"
        );

        let missing = add_bound_source(root.path(), binding, "missing.md")
            .expect_err("missing source must not disclose the physical root");
        let detail = format!("{missing:#}");
        assert!(detail.contains("unavailable or unsafe"), "{detail}");
        assert!(
            !detail.contains(&external.path().display().to_string()),
            "{detail}"
        );
    }

    #[test]
    fn bound_source_batch_validates_every_input_before_publishing_any_pointer() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let first_path = external.path().join("first.md");
        let second_path = external.path().join("second.md");
        fs::write(&first_path, "first source body").expect("write first source");
        fs::write(&second_path, "second source body").expect("write second source");
        let initial = add_local_source(root.path(), &first_path).expect("create ignored binding");
        let binding = match &initial.location {
            SourceLocation::BoundPath { binding, .. } => binding.clone(),
            location => panic!("expected bound pointer, got {location:?}"),
        };
        let index_before = fs::read(root.path().join("sources/index.json")).expect("read index");
        let bindings_before = fs::read(root.path().join(".graphoxide/source-bindings.yaml"))
            .expect("read ignored bindings");

        let error = add_bound_sources(
            root.path(),
            &[
                (binding.clone(), "second.md".into()),
                (binding, "missing.md".into()),
            ],
        )
        .expect_err("a missing requested source must reject the complete batch");

        let detail = format!("{error:#}");
        assert!(detail.contains("unavailable or unsafe"), "{detail}");
        assert!(
            !detail.contains(&external.path().display().to_string()),
            "{detail}"
        );
        assert_eq!(
            fs::read(root.path().join("sources/index.json")).expect("read unchanged index"),
            index_before
        );
        assert_eq!(
            fs::read(root.path().join(".graphoxide/source-bindings.yaml"))
                .expect("read unchanged bindings"),
            bindings_before
        );
    }

    #[test]
    fn bound_source_batch_deduplicates_and_sorts_without_losing_identical_review_state() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let first_path = external.path().join("first.md");
        let second_path = external.path().join("second.md");
        fs::write(&first_path, "first source body").expect("write first source");
        fs::write(&second_path, "second source body").expect("write second source");
        let first = add_local_source(root.path(), &first_path).expect("create ignored binding");
        let binding = match &first.location {
            SourceLocation::BoundPath { binding, .. } => binding.clone(),
            location => panic!("expected bound pointer, got {location:?}"),
        };
        let mut existing = load_source_index(root.path()).expect("load pointer index");
        existing.sources[0].status = SourceStatus::HumanConfirmed;
        write_source_index(root.path(), &existing).expect("record review");

        let sources = add_bound_sources(
            root.path(),
            &[
                (binding.clone(), "second.md".into()),
                (binding.clone(), "first.md".into()),
                (binding, "second.md".into()),
            ],
        )
        .expect("add deterministic logical source batch");

        assert_eq!(sources.len(), 2);
        assert!(sources
            .windows(2)
            .all(|pair| pair[0].source_id < pair[1].source_id));
        assert!(sources.iter().any(|source| {
            source.source_id == first.source_id && source.status == SourceStatus::HumanConfirmed
        }));
        let index = source_status(root.path()).expect("read deduplicated source index");
        assert_eq!(index.len(), 2);
        assert_eq!(
            index
                .iter()
                .map(|source| source.source_id.as_str())
                .collect::<Vec<_>>(),
            sources
                .iter()
                .map(|source| source.source_id.as_str())
                .collect::<Vec<_>>()
        );
        let index_text =
            fs::read_to_string(root.path().join("sources/index.json")).expect("read pointer index");
        assert!(!index_text.contains("first source body"));
        assert!(!index_text.contains("second source body"));
        assert!(!index_text.contains(&external.path().display().to_string()));
    }

    #[test]
    fn source_add_refuses_a_tracked_binding_without_overwriting_it() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        fs::create_dir(root.path().join(".graphoxide")).expect("create graphoxide directory");
        let binding = root.path().join(".graphoxide/source-bindings.yaml");
        let tracked_contents = "schema: graphoxide.source-bindings\nbindings: []\n";
        fs::write(&binding, tracked_contents).expect("write tracked binding");
        std::process::Command::new("git")
            .args([
                "-C",
                root.path().to_str().expect("UTF-8 root"),
                "add",
                ".graphoxide/source-bindings.yaml",
            ])
            .status()
            .expect("stage tracked binding");
        let external = tempfile::NamedTempFile::new().expect("external source");

        let error =
            add_local_source(root.path(), external.path()).expect_err("tracked binding must fail");

        assert!(
            error.to_string().contains("must not be Git-tracked"),
            "{error:#}"
        );
        assert_eq!(
            fs::read_to_string(&binding).expect("read tracked binding"),
            tracked_contents
        );
        assert!(!root.path().join("sources/index.json").exists());
    }

    #[test]
    fn source_add_rejects_linked_inputs() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let root = tempfile::tempdir().expect("temporary knowledgebase");
            let external = tempfile::tempdir().expect("external source root");
            let source = external.path().join("source.md");
            fs::write(&source, "private").expect("write source");
            let link = external.path().join("link.md");
            symlink(&source, &link).expect("create symlink");
            assert!(add_local_source(root.path(), &link).is_err());

            let hard_link = external.path().join("hard-link.md");
            fs::hard_link(&source, &hard_link).expect("create hard link");
            assert!(add_local_source(root.path(), &hard_link).is_err());
        }
    }

    #[test]
    fn source_add_keeps_an_ignored_binding_inert_when_index_publication_fails() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let first_root = tempfile::tempdir().expect("first external root");
        let first = first_root.path().join("first.md");
        fs::write(&first, "first raw body").expect("write first source");
        add_local_source(root.path(), &first).expect("add first source");
        let before_index =
            fs::read(root.path().join("sources/index.json")).expect("read initial index");

        let second_root = tempfile::tempdir().expect("second external root");
        let second = second_root.path().join("second.md");
        fs::write(&second, "second raw body").expect("write second source");
        let error = add_local_source_with_index_writer(root.path(), &second, |_, _| {
            Err(anyhow::anyhow!("forced index publication failure"))
        })
        .expect_err("forced index publication must fail");
        assert!(error.to_string().contains("forced index"));
        assert_eq!(
            fs::read(root.path().join("sources/index.json")).expect("read preserved index"),
            before_index
        );
        let inert_bindings = fs::read(root.path().join(".graphoxide/source-bindings.yaml"))
            .expect("read inert binding");
        let index = load_source_index(root.path()).expect("load preserved index");
        assert!(index.sources.iter().all(|source| {
            !matches!(&source.location, SourceLocation::BoundPath { path, .. } if path == "second.md")
        }));
        for file in walk_files(root.path()) {
            let artifact = fs::read(root.path().join(&file)).expect("read knowledgebase artifact");
            assert!(!artifact
                .windows(b"second raw body".len())
                .any(|part| part == b"second raw body"));
        }

        add_local_source(root.path(), &second).expect("retry local source add");
        assert_eq!(
            fs::read(root.path().join(".graphoxide/source-bindings.yaml"))
                .expect("read retry binding"),
            inert_bindings
        );
        let retry_index =
            fs::read(root.path().join("sources/index.json")).expect("read retry index");
        add_local_source(root.path(), &second).expect("repeat local source add");
        assert_eq!(
            fs::read(root.path().join("sources/index.json")).expect("read repeated index"),
            retry_index
        );
    }

    #[test]
    fn source_add_batch_rejects_knowledgebase_descendants_before_creating_artifacts() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let nested = root.path().join("docs/nested.md");
        fs::create_dir_all(nested.parent().expect("nested parent")).expect("create nested parent");
        fs::write(&nested, "knowledgebase content").expect("write nested source");
        let external = tempfile::NamedTempFile::new().expect("external valid source");
        let before = walk_files(root.path());

        let error = add_sources(root.path(), [external.path(), &nested])
            .expect_err("knowledgebase input must fail");

        assert!(
            format!("{error:#}").contains("outside the knowledgebase root"),
            "{error:#}"
        );
        assert_eq!(walk_files(root.path()), before);
        for forbidden in [
            ".graphoxide/source-bindings.yaml",
            ".graphoxide/source-store",
            ".graphoxide/cache",
            "sources/index.json",
            "raw",
            "captures",
        ] {
            assert!(
                !root.path().join(forbidden).exists(),
                "unexpected {forbidden}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn source_add_batch_rejects_a_symlink_alias_of_the_knowledgebase_before_creating_artifacts() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let parent = tempfile::tempdir().expect("alias parent");
        let alias = parent.path().join("knowledgebase-alias");
        symlink(root.path(), &alias).expect("make knowledgebase alias");
        fs::write(root.path().join("input.md"), "knowledgebase input")
            .expect("write knowledgebase input");
        let before = walk_files(root.path());

        let error = add_sources(root.path(), [alias.join("input.md")])
            .expect_err("knowledgebase alias must fail");
        assert!(
            format!("{error:#}").contains("outside the knowledgebase root"),
            "{error:#}"
        );
        assert_eq!(walk_files(root.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn source_add_batch_rejects_unsafe_directory_descendants_without_mutation() {
        use std::ffi::CString;
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let safe = external.path().join("safe.md");
        fs::write(&safe, "safe source").expect("write safe source");
        let before = walk_files(root.path());

        let linked = external.path().join("linked.md");
        symlink(&safe, &linked).expect("create descendant symlink");
        assert!(add_sources(root.path(), [external.path()]).is_err());
        assert_eq!(walk_files(root.path()), before);
        fs::remove_file(&linked).expect("remove descendant symlink");

        let hardlinked = external.path().join("hardlinked.md");
        fs::hard_link(&safe, &hardlinked).expect("create descendant hard link");
        assert!(add_sources(root.path(), [external.path()]).is_err());
        assert_eq!(walk_files(root.path()), before);
        fs::remove_file(&hardlinked).expect("remove descendant hard link");

        let fifo = external.path().join("unsafe.fifo");
        let fifo = CString::new(fifo.as_os_str().as_encoded_bytes()).expect("FIFO path has no NUL");
        assert_eq!(
            unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) },
            0,
            "create FIFO"
        );
        assert!(add_sources(root.path(), [external.path()]).is_err());
        assert_eq!(walk_files(root.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn source_add_batch_limit_rejects_before_knowledgebase_mutation() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        fs::write(external.path().join("a.md"), "123456").expect("write first source");
        fs::write(external.path().join("b.md"), "abcdef").expect("write second source");
        let before = walk_files(root.path());

        let error = add_sources_with_index_writer_and_limits(
            root.path(),
            &[external.path().to_owned()],
            write_source_index,
            SourceBatchLimits {
                files: 2,
                bytes: 8,
                entries: 8,
            },
        )
        .expect_err("batch byte cap must reject source directory");

        assert!(
            format!("{error:#}").contains("8-byte batch limit"),
            "{error:#}"
        );
        assert_eq!(walk_files(root.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn source_add_traversal_limit_rejects_before_knowledgebase_mutation() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        fs::write(external.path().join("a.md"), "one").expect("write first source");
        fs::write(external.path().join("b.md"), "two").expect("write second source");
        let before = walk_files(root.path());

        let error = add_sources_with_index_writer_and_limits(
            root.path(),
            &[external.path().to_owned()],
            write_source_index,
            SourceBatchLimits {
                files: 8,
                bytes: 1024,
                entries: 1,
            },
        )
        .expect_err("traversal entry cap must reject source directory");

        assert!(
            format!("{error:#}").contains("1-entry traversal limit"),
            "{error:#}"
        );
        assert_eq!(walk_files(root.path()), before);
    }

    #[test]
    fn source_add_explicit_files_respect_batch_byte_limit_without_mutation() {
        let root = tempfile::tempdir().expect("knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external sources");
        let first = external.path().join("first.md");
        let second = external.path().join("second.md");
        fs::write(&first, b"123456").expect("first source");
        fs::write(&second, b"abcdef").expect("second source");
        let before = walk_files(root.path());

        let error = add_sources_with_index_writer_and_limits(
            root.path(),
            &[first, second],
            write_source_index,
            SourceBatchLimits {
                files: 2,
                bytes: 8,
                entries: 8,
            },
        )
        .expect_err("explicit files must enforce the same aggregate byte cap");

        assert!(
            format!("{error:#}").contains("8-byte batch limit"),
            "{error:#}"
        );
        assert_eq!(walk_files(root.path()), before);
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_admission_fails_closed_and_preserves_existing_state() {
        let root = tempfile::tempdir().expect("knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external sources");
        let existing_path = external.path().join("existing.md");
        let new_path = external.path().join("new.md");
        fs::write(&existing_path, b"existing external source").expect("existing source");
        fs::write(&new_path, b"new external source").expect("new source");
        let mut existing =
            add_local_source(root.path(), &existing_path).expect("individual files work");
        existing.status = SourceStatus::HumanConfirmed;
        write_source_index(
            root.path(),
            &SourceIndex {
                schema: SOURCE_INDEX_SCHEMA.into(),
                sources: vec![existing.clone()],
            },
        )
        .expect("preserve confirmed pointer");
        fs::create_dir_all(root.path().join("content/provisional"))
            .expect("existing content directory");
        fs::write(
            root.path().join("content/provisional/existing.md"),
            b"existing derived content",
        )
        .expect("existing derived page");
        fs::create_dir_all(root.path().join("taxonomy/reviews"))
            .expect("existing review directory");
        fs::write(
            root.path().join("taxonomy/reviews/existing.json"),
            b"existing review receipt",
        )
        .expect("existing receipt");
        let snapshot = || {
            walk_files(root.path())
                .into_iter()
                .map(|relative| {
                    let bytes =
                        fs::read(root.path().join(&relative)).expect("knowledgebase artifact");
                    (relative, bytes)
                })
                .collect::<BTreeMap<_, _>>()
        };
        let before = snapshot();
        let plain = external.path().join("plain-directory");
        let repository = external.path().join("git-directory");
        for directory in [&plain, &repository] {
            fs::create_dir(directory).expect("external directory");
            fs::write(directory.join("reference.md"), b"directory source body")
                .expect("directory document");
        }
        init_git(&repository);
        for args in [
            vec!["add", "--all"],
            vec![
                "-c",
                "user.name=Graphoxide Test",
                "-c",
                "user.email=graphoxide@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "directory fixture",
            ],
            vec![
                "remote",
                "add",
                "origin",
                "https://git.example.test/source.git",
            ],
        ] {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(args)
                .status()
                .expect("prepare tracked Git fixture")
                .success());
        }
        for directory in [&plain, &repository] {
            let mut outcomes = Vec::new();
            let error =
                admit_source_directory(root.path(), directory, |outcome| outcomes.push(outcome))
                    .map(|_| ())
                    .expect_err("bulk directory traversal is unavailable on Windows");
            assert!(
                format!("{error:#}").contains("no-follow traversal is implemented"),
                "{error:#}"
            );
            assert!(outcomes.is_empty());
            assert_eq!(snapshot(), before);

            let error = add_source_directory(root.path(), directory, |_| {
                panic!("must not emit directory entries")
            })
            .expect_err("low-level bulk traversal also fails closed");
            assert!(
                format!("{error:#}").contains("no-follow traversal is implemented"),
                "{error:#}"
            );
            assert_eq!(snapshot(), before);

            let error = admit_sources(root.path(), [&new_path, directory])
                .map(|_| ())
                .expect_err("legacy mixed file/directory expansion also rejects directories");
            assert!(
                format!("{error:#}").contains("no-follow traversal is implemented"),
                "{error:#}"
            );
            assert_eq!(snapshot(), before);

            for inputs in [
                vec![
                    SourceAdmissionInput::LocalPath(new_path.clone()),
                    SourceAdmissionInput::Directory(directory.clone()),
                ],
                vec![
                    SourceAdmissionInput::Directory(directory.clone()),
                    SourceAdmissionInput::LocalPath(new_path.clone()),
                ],
            ] {
                let error = admit_source_inputs(root.path(), &inputs, |_| {
                    panic!("must not emit directory entries")
                })
                .map(|_| ())
                .expect_err("mixed request cannot partially admit before unsupported traversal");
                assert!(
                    format!("{error:#}").contains("no-follow traversal is implemented"),
                    "{error:#}"
                );
                assert_eq!(
                    snapshot(),
                    before,
                    "failed mixed request preserves every existing artifact"
                );
                assert_eq!(
                    source_status(root.path()).expect("unchanged pointer"),
                    vec![existing.clone()]
                );
            }
            assert_eq!(
                fs::read(directory.join("reference.md")).expect("external document"),
                b"directory source body"
            );
        }
        assert_eq!(
            fs::read(existing_path).expect("external existing source"),
            b"existing external source"
        );
        assert_eq!(
            fs::read(new_path).expect("external new source"),
            b"new external source"
        );
    }

    #[test]
    fn source_add_batch_keeps_only_an_inert_ignored_binding_when_index_write_fails() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("one.md");
        fs::write(&input, "unpublished source body").expect("write source");

        let error = add_sources_with_index_writer(root.path(), &[input], |_, _| {
            Err(anyhow::anyhow!("forced index publication failure"))
        })
        .expect_err("forced index publication must fail");

        assert!(format!("{error:#}").contains("forced index publication failure"));
        assert_eq!(
            walk_files(root.path()),
            [".gitignore", ".graphoxide/source-bindings.yaml"]
        );
        for forbidden in ["source-store", "cache", "raw", "captures", "one.md"] {
            assert!(
                !root.path().join(".graphoxide").join(forbidden).exists()
                    && !root.path().join(forbidden).exists(),
                "unexpected raw/cache artifact {forbidden}"
            );
        }
        assert!(std::process::Command::new("git")
            .args([
                "-C",
                root.path().to_str().expect("UTF-8 root"),
                "check-ignore",
                ".graphoxide/source-bindings.yaml",
            ])
            .status()
            .expect("check ignored binding")
            .success());
        for artifact in walk_files(root.path()) {
            assert!(
                !fs::read(root.path().join(artifact))
                    .expect("read retained artifact")
                    .windows(b"unpublished source body".len())
                    .any(|part| part == b"unpublished source body"),
                "raw source body leaked after failed publication"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn source_add_batch_expands_overlapping_directory_and_file_once_in_lexical_order() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let directory = external.path().join("docs");
        let first = directory.join("a.md");
        let second = directory.join("nested/b.md");
        fs::create_dir_all(second.parent().expect("nested parent"))
            .expect("create nested directory");
        fs::write(&first, "a body").expect("write first source");
        fs::write(&second, "b body").expect("write second source");

        let added = add_sources(root.path(), [&directory, &first]).expect("add overlapping inputs");
        assert_eq!(added.len(), 2);
        assert_eq!(
            added
                .iter()
                .map(|entry| match &entry.location {
                    SourceLocation::BoundPath { binding, path } => (binding.clone(), path.clone()),
                    other => panic!("expected bound path, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            vec![
                (
                    local_binding_alias(&directory.canonicalize().expect("canonical directory")),
                    "a.md".into()
                ),
                (
                    local_binding_alias(&directory.canonicalize().expect("canonical directory")),
                    "nested/b.md".into()
                ),
            ]
        );
        let index = load_source_index(root.path()).expect("load deduplicated index");
        assert_eq!(index.sources.len(), 2);
        assert_eq!(
            index
                .sources
                .iter()
                .map(|entry| entry.source_id.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            2
        );
    }

    #[test]
    fn git_relative_path_uses_slashes_for_nested_native_components() {
        let native = Path::new("docs")
            .join("nested résumé")
            .join("reference #1.md");
        assert_eq!(
            git_relative_path(&native).expect("normalize native Git path"),
            "docs/nested résumé/reference #1.md"
        );
        for invalid in ["", "../outside.md", "/absolute.md"] {
            assert!(
                git_relative_path(Path::new(invalid)).is_err(),
                "{invalid:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn git_relative_path_rejects_a_literal_backslash_in_a_unix_filename() {
        assert!(git_relative_path(Path::new(r"docs/reference\name.md")).is_err());
    }

    #[test]
    fn source_add_records_clean_git_files_and_rejects_modified_or_untracked_files() {
        fn commit_all(repo: &Path) {
            let status = std::process::Command::new("git")
                .args(["-C", repo.to_str().expect("UTF-8 repo"), "add", "--all"])
                .status()
                .expect("stage Git fixture");
            assert!(status.success());
            let status = std::process::Command::new("git")
                .args([
                    "-C",
                    repo.to_str().expect("UTF-8 repo"),
                    "-c",
                    "user.name=Graphoxide Test",
                    "-c",
                    "user.email=graphoxide@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "fixture",
                ])
                .status()
                .expect("commit Git fixture");
            assert!(status.success());
        }

        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let repo = tempfile::tempdir().expect("external Git repository");
        init_git(repo.path());
        let tracked = repo.path().join("docs/clean.md");
        fs::create_dir_all(tracked.parent().expect("Git file parent"))
            .expect("create Git directory");
        fs::write(&tracked, "clean Git source").expect("write Git source");
        commit_all(repo.path());
        let status = std::process::Command::new("git")
            .args([
                "-C",
                repo.path().to_str().expect("UTF-8 repo"),
                "remote",
                "add",
                "origin",
                "git@git.example.com:group/repo.git",
            ])
            .status()
            .expect("add Git origin");
        assert!(status.success());

        let added = add_sources(root.path(), [&tracked]).expect("add clean Git file");
        assert_eq!(added.len(), 1);
        let head = String::from_utf8(
            std::process::Command::new("git")
                .args([
                    "-C",
                    repo.path().to_str().expect("UTF-8 repo"),
                    "rev-parse",
                    "HEAD",
                ])
                .output()
                .expect("read Git HEAD")
                .stdout,
        )
        .expect("UTF-8 head")
        .trim()
        .to_owned();
        assert!(matches!(
            &added[0].location,
            SourceLocation::Git { remote, commit, path }
                if remote == "ssh://git.example.com/group/repo.git"
                    && commit == &head
                    && path == "docs/clean.md"
        ));
        assert_eq!(
            added[0].content_sha256,
            hex::encode(Sha256::digest(b"clean Git source"))
        );
        assert_eq!(added[0].bytes, b"clean Git source".len() as u64);

        fs::write(&tracked, "modified Git source").expect("modify Git source");
        assert!(
            add_sources(root.path(), [&tracked]).is_err(),
            "modified Git file must fail"
        );
        let untracked = repo.path().join("docs/untracked.md");
        fs::write(&untracked, "untracked Git source").expect("write untracked source");
        assert!(
            add_sources(root.path(), [&untracked]).is_err(),
            "untracked Git file must fail"
        );
    }

    #[test]
    fn git_source_add_keeps_tracked_path_chatter_out_of_stdout() {
        let current_test = "wiki_source::tests::source_add_records_clean_git_files_and_rejects_modified_or_untracked_files";
        let output = Command::new(std::env::current_exe().expect("current test binary"))
            .args(["--exact", current_test, "--nocapture"])
            .output()
            .expect("run Git source-add regression in a child test process");

        assert!(
            output.status.success(),
            "child regression failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("docs/clean.md"),
            "Git locator must capture ls-files output rather than leaking it to stdout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_add_directory_skips_git_metadata_and_adds_only_clean_tracked_files() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let repo = tempfile::tempdir().expect("external Git repository");
        init_git(repo.path());
        let tracked = repo.path().join("docs/source.md");
        fs::create_dir_all(tracked.parent().expect("source parent")).expect("create source parent");
        fs::write(&tracked, "tracked source").expect("write source");
        let status = std::process::Command::new("git")
            .args([
                "-C",
                repo.path().to_str().expect("UTF-8 repo"),
                "add",
                "--all",
            ])
            .status()
            .expect("stage source");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args([
                "-C",
                repo.path().to_str().expect("UTF-8 repo"),
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "source",
            ])
            .status()
            .expect("commit source");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args([
                "-C",
                repo.path().to_str().expect("UTF-8 repo"),
                "remote",
                "add",
                "origin",
                "https://git.example.com/group/repo.git",
            ])
            .status()
            .expect("add origin");
        assert!(status.success());

        let added = add_sources(root.path(), [repo.path()]).expect("add Git directory");
        assert_eq!(added.len(), 1);
        assert!(
            matches!(&added[0].location, SourceLocation::Git { path, .. } if path == "docs/source.md")
        );
        let files = walk_files(root.path());
        assert!(files.iter().all(|path| !path.contains("source-store")));
    }

    #[test]
    fn https_source_add_rejects_unsanitized_url_before_fetching_or_writing() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let mut fetched = false;

        let error = add_https_source_with_fetcher(
            root.path(),
            "https://docs.example.com/reference?token=secret",
            allow_https_fetch(),
            |_, _, _| {
                fetched = true;
                Ok(0)
            },
        )
        .expect_err("credential-bearing URL must be rejected before transport");

        assert!(format!("{error:#}").contains("query string"), "{error:#}");
        assert!(!fetched, "transport must not run for an invalid locator");
        assert!(walk_files(root.path()).is_empty());
    }

    #[test]
    fn https_source_add_preserves_reviews_for_identical_bytes_and_resets_changed_bytes() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let url = "https://docs.example.com/reference.md";
        let first_body = b"first transient HTTPS source";

        let first = add_https_source_with_fetcher(
            root.path(),
            url,
            allow_https_fetch(),
            |_, writer, max_bytes| {
                assert!(first_body.len() <= max_bytes);
                writer.write_all(first_body)?;
                Ok(first_body.len() as u64)
            },
        )
        .expect("add HTTPS pointer");

        assert!(
            matches!(&first.location, SourceLocation::Https { url: recorded } if recorded == url)
        );
        assert_eq!(first.status, SourceStatus::Provisional);
        assert_eq!(first.bytes, first_body.len() as u64);
        assert_eq!(
            first.content_sha256,
            hex::encode(Sha256::digest(first_body))
        );
        assert_eq!(walk_files(root.path()), ["sources/index.json"]);
        assert!(!fs::read_to_string(root.path().join("sources/index.json"))
            .expect("read pointer index")
            .contains("first transient HTTPS source"));

        let mut reviewed = load_source_index(root.path()).expect("load pointer index");
        reviewed.sources[0].status = SourceStatus::HumanConfirmed;
        write_source_index(root.path(), &reviewed).expect("record human review");
        let unchanged =
            add_https_source_with_fetcher(root.path(), url, allow_https_fetch(), |_, writer, _| {
                writer.write_all(first_body)?;
                Ok(first_body.len() as u64)
            })
            .expect("re-add unchanged HTTPS pointer");
        assert_eq!(unchanged.status, SourceStatus::HumanConfirmed);

        let mut ai_reviewed = load_source_index(root.path()).expect("load reviewed pointer index");
        ai_reviewed.sources[0].status = SourceStatus::AiReviewed;
        write_source_index(root.path(), &ai_reviewed).expect("record AI review");
        let unchanged =
            add_https_source_with_fetcher(root.path(), url, allow_https_fetch(), |_, writer, _| {
                writer.write_all(first_body)?;
                Ok(first_body.len() as u64)
            })
            .expect("re-add AI-reviewed HTTPS pointer");
        assert_eq!(unchanged.status, SourceStatus::AiReviewed);

        let second_body = b"second transient HTTPS source";
        add_https_source_with_fetcher(root.path(), url, allow_https_fetch(), |_, writer, _| {
            writer.write_all(second_body)?;
            Ok(second_body.len() as u64)
        })
        .expect("replace HTTPS pointer");

        let index = load_source_index(root.path()).expect("load source index");
        assert_eq!(
            index.sources,
            vec![SourceEntry {
                source_id: first.source_id,
                location: SourceLocation::Https { url: url.into() },
                content_sha256: hex::encode(Sha256::digest(second_body)),
                bytes: second_body.len() as u64,
                status: SourceStatus::Provisional,
            }]
        );
    }

    #[test]
    fn https_source_add_drops_failed_fetches_and_byte_mismatches() {
        for failure in ["network-error", "redirect", "over-cap"] {
            let root = tempfile::tempdir().expect("temporary knowledgebase");
            let error = add_https_source_with_fetcher(
                root.path(),
                "https://docs.example.com/reference.md",
                allow_https_fetch(),
                |_, _, max_bytes| {
                    assert_eq!(max_bytes, MAX_SOURCE_INDEX_BYTES as usize);
                    Err(anyhow::anyhow!("simulated {failure} failure"))
                },
            )
            .expect_err("failed transport must not publish a source");
            assert!(format!("{error:#}").contains(failure), "{error:#}");
            assert!(walk_files(root.path()).is_empty());
        }

        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let error = add_https_source_with_fetcher(
            root.path(),
            "https://docs.example.com/reference.md",
            allow_https_fetch(),
            |_, writer, _| {
                writer.write_all(b"actual bytes")?;
                Ok(1)
            },
        )
        .expect_err("reported byte count must match the staged response");
        assert!(format!("{error:#}").contains("byte count"), "{error:#}");
        assert!(walk_files(root.path()).is_empty());
    }

    #[test]
    fn source_status_reads_only_the_persisted_pointer_index() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let source = SourceEntry {
            source_id: SourceLocation::Https {
                url: "https://docs.example.com/reference.md".into(),
            }
            .source_id(),
            location: SourceLocation::Https {
                url: "https://docs.example.com/reference.md".into(),
            },
            content_sha256: hex::encode(Sha256::digest(b"digest-only")),
            bytes: 11,
            status: SourceStatus::HumanConfirmed,
        };
        write_source_index(
            root.path(),
            &SourceIndex {
                schema: SOURCE_INDEX_SCHEMA.into(),
                sources: vec![source.clone()],
            },
        )
        .expect("write pointer index");

        assert_eq!(
            source_status(root.path()).expect("read pointer status"),
            vec![source]
        );
        assert_eq!(walk_files(root.path()), ["sources/index.json"]);
    }

    #[test]
    fn transient_reader_returns_verified_local_bytes_without_mutating_the_pointer_index() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("source.md");
        let body = b"transient local source body";
        fs::write(&input, body).expect("write external source");
        let source = add_local_source(root.path(), &input).expect("add local pointer");
        let before = fs::read(root.path().join("sources/index.json")).expect("read pointer index");

        let read = read_source_transient(root.path(), &source, None)
            .expect("read verified local source only in memory");

        assert_eq!(read, body);
        assert_eq!(
            fs::read(root.path().join("sources/index.json")).expect("read unchanged pointer index"),
            before
        );
        assert!(!before.windows(body.len()).any(|window| window == body));
    }

    #[test]
    fn transient_reader_rejects_changed_local_bytes_without_mutating_the_pointer_index() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("source.md");
        fs::write(&input, "indexed source body").expect("write external source");
        let source = add_local_source(root.path(), &input).expect("add local pointer");
        let before = fs::read(root.path().join("sources/index.json")).expect("read pointer index");
        let changed = "changed source body must not persist";
        fs::write(&input, changed).expect("change external source");

        let error = read_source_transient(root.path(), &source, None)
            .expect_err("changed source must not be exposed as the indexed revision");

        assert!(format!("{error:#}").contains("indexed digest"), "{error:#}");
        assert!(!format!("{error:#}").contains(changed), "{error:#}");
        assert_eq!(
            fs::read(root.path().join("sources/index.json")).expect("read unchanged pointer index"),
            before
        );
    }

    #[test]
    fn transient_reader_verifies_the_indexed_byte_count_before_returning_bytes() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("source.md");
        fs::write(&input, "source with a forged byte count").expect("write external source");
        let mut source = add_local_source(root.path(), &input).expect("add local pointer");
        source.bytes += 1;
        write_source_index(
            root.path(),
            &SourceIndex {
                schema: SOURCE_INDEX_SCHEMA.into(),
                sources: vec![source.clone()],
            },
        )
        .expect("write forged pointer metadata");
        let before = fs::read(root.path().join("sources/index.json")).expect("read pointer index");

        let error = read_source_transient(root.path(), &source, None)
            .expect_err("reader must reject a mismatched indexed byte count");

        assert!(format!("{error:#}").contains("byte count"), "{error:#}");
        assert_eq!(
            fs::read(root.path().join("sources/index.json")).expect("read unchanged pointer index"),
            before
        );
    }

    #[test]
    fn transient_reader_requires_consent_before_calling_remote_transport() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let body = b"indexed HTTPS source body";
        let source = add_https_source_with_fetcher(
            root.path(),
            "https://docs.example.com/reference.md",
            allow_https_fetch(),
            |_, writer, _| {
                writer.write_all(body)?;
                Ok(body.len() as u64)
            },
        )
        .expect("add HTTPS pointer");
        let before = fs::read(root.path().join("sources/index.json")).expect("read pointer index");
        let mut called = false;

        let error =
            read_source_transient_with_https_fetcher(root.path(), &source, None, |_, _, _| {
                called = true;
                Ok(0)
            })
            .expect_err("remote source read requires consent");

        assert!(
            format!("{error:#}").contains("explicit consent"),
            "{error:#}"
        );
        assert!(!called, "remote transport must not run without consent");
        assert_eq!(
            fs::read(root.path().join("sources/index.json")).expect("read unchanged pointer index"),
            before
        );
    }

    #[test]
    fn transient_git_reader_hides_a_stale_private_binding_root() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let source = SourceEntry {
            source_id: SourceLocation::Git {
                remote: "https://git.example.com/group/repository.git".into(),
                commit: "a".repeat(40),
                path: "docs/reference.md".into(),
            }
            .source_id(),
            location: SourceLocation::Git {
                remote: "https://git.example.com/group/repository.git".into(),
                commit: "a".repeat(40),
                path: "docs/reference.md".into(),
            },
            content_sha256: hex::encode(Sha256::digest(b"body")),
            bytes: 4,
            status: SourceStatus::Provisional,
        };
        write_source_index(
            root.path(),
            &SourceIndex {
                schema: SOURCE_INDEX_SCHEMA.into(),
                sources: vec![source.clone()],
            },
        )
        .expect("write source index");
        let stale_root = root.path().join("private-checkout-that-must-not-leak");
        fs::create_dir(root.path().join(".graphoxide")).expect("create private binding directory");
        fs::write(
            root.path().join(".graphoxide/source-bindings.yaml"),
            format!(
                "schema: {SOURCE_BINDINGS_SCHEMA}\nbindings:\n  - alias: local-{}\n    root: {}\n",
                "b".repeat(64),
                stale_root.display()
            ),
        )
        .expect("write stale private binding");

        let error = read_source_transient(root.path(), &source, None)
            .expect_err("stale private binding must not surface its root");
        let text = format!("{error:#}");
        assert!(
            text.contains("local Git source binding is unavailable or unsafe"),
            "{text}"
        );
        assert!(!text.contains(&stale_root.display().to_string()), "{text}");
    }

    #[test]
    fn https_refresh_requires_explicit_consent_and_retains_the_last_pointer() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let body = b"last successfully processed body";
        let source = add_https_source_with_fetcher(
            root.path(),
            "https://docs.example.com/reference.md",
            allow_https_fetch(),
            |_, writer, _| {
                writer.write_all(body)?;
                Ok(body.len() as u64)
            },
        )
        .expect("add HTTPS pointer");
        let mut called = false;

        let stale =
            refresh_source_with_https_fetcher(root.path(), &source.source_id, None, |_, _, _| {
                called = true;
                Ok(0)
            })
            .expect("record denied remote refresh as stale");

        assert!(!called, "the transport must not run without consent");
        assert_eq!(stale.location, source.location);
        assert_eq!(stale.content_sha256, source.content_sha256);
        assert_eq!(stale.bytes, source.bytes);
        assert_eq!(stale.status, SourceStatus::StaleError);
        assert_eq!(
            source_status(root.path()).expect("read status"),
            vec![stale]
        );
        assert!(!fs::read_to_string(root.path().join("sources/index.json"))
            .expect("read pointer index")
            .contains("last successfully processed body"));
    }

    #[test]
    fn git_refresh_rejects_remote_fetch_with_or_without_consent() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let location = SourceLocation::Git {
            remote: "https://git.example.com/group/repository.git".into(),
            commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            path: "docs/reference.md".into(),
        };
        let source = SourceEntry {
            source_id: location.source_id(),
            location,
            content_sha256: hex::encode(Sha256::digest(b"last Git digest")),
            bytes: 15,
            status: SourceStatus::AiReviewed,
        };
        write_source_index(
            root.path(),
            &SourceIndex {
                schema: SOURCE_INDEX_SCHEMA.into(),
                sources: vec![source.clone()],
            },
        )
        .expect("write Git pointer");

        let index_before = fs::read(root.path().join("sources/index.json")).expect("read index");
        let files_before = walk_files(root.path());
        for consent in [None, Some(allow_https_fetch())] {
            let error = refresh_source(root.path(), &source.source_id, consent)
                .expect_err("unbounded remote Git refresh must remain unavailable");
            assert!(
                error
                    .to_string()
                    .contains("remote Git fetching is disabled"),
                "{error:#}"
            );
            let error = read_source_transient(root.path(), &source, consent)
                .expect_err("unbounded remote Git read must remain unavailable");
            assert!(
                error
                    .to_string()
                    .contains("remote Git fetching is disabled"),
                "{error:#}"
            );
            assert_eq!(
                fs::read(root.path().join("sources/index.json")).expect("read index"),
                index_before
            );
            assert_eq!(walk_files(root.path()), files_before);
        }
    }

    #[test]
    fn https_refresh_preserves_review_for_same_digest_and_resets_changed_digest() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let first_body = b"first transient refresh body";
        let source = add_https_source_with_fetcher(
            root.path(),
            "https://docs.example.com/reference.md",
            allow_https_fetch(),
            |_, writer, _| {
                writer.write_all(first_body)?;
                Ok(first_body.len() as u64)
            },
        )
        .expect("add HTTPS pointer");
        let mut index = load_source_index(root.path()).expect("load source index");
        index.sources[0].status = SourceStatus::HumanConfirmed;
        write_source_index(root.path(), &index).expect("record review");

        let unchanged = refresh_source_with_https_fetcher(
            root.path(),
            &source.source_id,
            Some(allow_https_fetch()),
            |_, writer, _| {
                writer.write_all(first_body)?;
                Ok(first_body.len() as u64)
            },
        )
        .expect("refresh unchanged source");
        assert_eq!(unchanged.status, SourceStatus::HumanConfirmed);

        let changed_body = b"changed transient refresh body";
        let changed = refresh_source_with_https_fetcher(
            root.path(),
            &source.source_id,
            Some(allow_https_fetch()),
            |_, writer, _| {
                writer.write_all(changed_body)?;
                Ok(changed_body.len() as u64)
            },
        )
        .expect("refresh changed source");
        assert_eq!(changed.status, SourceStatus::Provisional);
        assert_eq!(changed.bytes, changed_body.len() as u64);
        assert_eq!(
            changed.content_sha256,
            hex::encode(Sha256::digest(changed_body))
        );
    }

    #[test]
    fn local_refresh_failure_marks_stale_without_losing_the_pointer() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("source.md");
        fs::write(&input, "last local source body").expect("write source");
        let source = add_local_source(root.path(), &input).expect("add local pointer");
        fs::remove_file(&input).expect("remove external source");

        let stale = refresh_source(root.path(), &source.source_id, None)
            .expect("record unavailable local source as stale");

        assert_eq!(stale.location, source.location);
        assert_eq!(stale.content_sha256, source.content_sha256);
        assert_eq!(stale.bytes, source.bytes);
        assert_eq!(stale.status, SourceStatus::StaleError);
    }

    #[test]
    fn retire_prunes_only_an_unreferenced_local_binding_alias() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let first_path = external.path().join("first.md");
        let second_path = external.path().join("second.md");
        fs::write(&first_path, "first source body").expect("write first source");
        fs::write(&second_path, "second source body").expect("write second source");
        let sources = add_sources(root.path(), [&first_path, &second_path]).expect("add sources");
        assert_eq!(sources.len(), 2);
        let binding = match &sources[0].location {
            SourceLocation::BoundPath { binding, .. } => binding.clone(),
            location => panic!("expected local binding, got {location:?}"),
        };

        retire_source(root.path(), &sources[0].source_id).expect("retire first pointer");
        let bindings_path = root.path().join(".graphoxide/source-bindings.yaml");
        assert!(fs::read_to_string(&bindings_path)
            .expect("read retained binding")
            .contains(&binding));
        assert_eq!(
            source_status(root.path())
                .expect("read surviving source")
                .len(),
            1
        );

        retire_source(root.path(), &sources[1].source_id).expect("retire second pointer");
        assert!(source_status(root.path())
            .expect("read empty source index")
            .is_empty());
        assert!(!fs::read_to_string(&bindings_path)
            .expect("read pruned bindings")
            .contains(&binding));
        assert!(walk_files(root.path())
            .iter()
            .all(|path| !path.contains("source-store")));
    }

    #[test]
    fn retire_restores_index_and_binding_when_cleanup_fails_after_writing() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("source.md");
        fs::write(&input, "source body").expect("write source");
        let source = add_local_source(root.path(), &input).expect("add local pointer");
        let bindings_path = root.path().join(".graphoxide/source-bindings.yaml");
        let previous_binding = fs::read_to_string(&bindings_path).expect("read original binding");

        let error =
            retire_source_with_binding_writer(root.path(), &source.source_id, |root, bindings| {
                write_source_bindings(root, bindings)?;
                anyhow::bail!("simulated post-write binding validation failure")
            })
            .expect_err("the failed cleanup must roll retirement back");

        assert!(format!("{error:#}").contains("rolled back"), "{error:#}");
        assert_eq!(
            source_status(root.path()).expect("restore source index"),
            vec![source]
        );
        assert_eq!(
            fs::read_to_string(&bindings_path).expect("restore original binding"),
            previous_binding
        );
    }

    #[test]
    fn admission_receipt_rejects_an_existing_pointer_claimed_as_new() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("existing.md");
        fs::write(&input, "existing source body").expect("write source");
        let existing = add_local_source(root.path(), &input).expect("add existing pointer");
        let admission = begin_source_admission(root.path()).expect("snapshot existing index");

        let error = match finish_source_admission(
            root.path(),
            admission,
            std::slice::from_ref(&existing),
        ) {
            Ok(_) => panic!("an existing pointer cannot be claimed as a new admission"),
            Err(error) => error,
        };

        assert!(
            format!("{error:#}").contains("already existed"),
            "{error:#}"
        );
        assert_eq!(
            source_status(root.path()).expect("retain existing pointer"),
            vec![existing]
        );
    }

    #[test]
    fn admission_receipt_validation_rejects_a_changed_current_revision() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("source.md");
        fs::write(&input, "new source body").expect("write source");
        let admission = begin_source_admission(root.path()).expect("snapshot empty index");
        let source = add_local_source(root.path(), &input).expect("add pointer");
        let receipt =
            finish_source_admission(root.path(), admission, std::slice::from_ref(&source))
                .expect("seal newly admitted pointer");
        let mut index = load_source_index(root.path()).expect("load current pointer");
        index.sources[0].status = SourceStatus::AiReviewed;
        write_source_index(root.path(), &index).expect("change current pointer revision");

        let error = validate_source_admission(root.path(), &receipt)
            .expect_err("a receipt must reject a changed pointer revision");

        assert!(
            format!("{error:#}").contains("no longer matches"),
            "{error:#}"
        );
        assert_eq!(receipt.sources(), &[source]);
    }

    #[test]
    fn admission_rollback_attempts_every_new_pointer_and_keeps_changed_entries() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let first_path = external.path().join("first.md");
        let second_path = external.path().join("second.md");
        fs::write(&first_path, "first admitted source").expect("write first source");
        fs::write(&second_path, "second admitted source").expect("write second source");
        let admission = begin_source_admission(root.path()).expect("snapshot empty index");
        let sources = add_sources(root.path(), [&first_path, &second_path]).expect("add sources");
        let receipt = finish_source_admission(root.path(), admission, &sources)
            .expect("seal source batch receipt");
        let changed = sources[0].clone();
        let mut index = load_source_index(root.path()).expect("load source index");
        index
            .sources
            .iter_mut()
            .find(|source| source.source_id == changed.source_id)
            .expect("changed source")
            .status = SourceStatus::HumanConfirmed;
        write_source_index(root.path(), &index).expect("change first pointer revision");

        let error = rollback_source_admission(root.path(), &receipt)
            .expect_err("rollback reports the changed entry after trying every receipt entry");

        assert!(format!("{error:#}").contains("1 source"), "{error:#}");
        let remaining = source_status(root.path()).expect("read post-rollback index");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].source_id, changed.source_id);
        assert_eq!(remaining[0].status, SourceStatus::HumanConfirmed);
    }

    #[test]
    fn source_owned_local_admission_cannot_claim_or_retire_a_foreign_pointer() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let foreign_path = external.path().join("foreign.md");
        let owned_path = external.path().join("owned.md");
        fs::write(&foreign_path, "foreign source body").expect("write foreign source");
        fs::write(&owned_path, "owned source body").expect("write owned source");

        let mut admission = SourceAdmissionTransaction::begin(root.path()).expect("begin request");
        let foreign = add_local_source(root.path(), &foreign_path).expect("independent add");
        admission
            .add_local_batch([&owned_path])
            .expect("request-owned add");
        let error = seal_error(admission);

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert_eq!(
            source_status(root.path()).expect("foreign pointer remains"),
            vec![foreign]
        );
        for artifact in walk_files(root.path()) {
            let bytes = fs::read(root.path().join(artifact)).expect("read pointer artifact");
            assert!(
                !bytes
                    .windows(b"owned source body".len())
                    .any(|part| part == b"owned source body")
                    && !bytes
                        .windows(b"foreign source body".len())
                        .any(|part| part == b"foreign source body"),
                "raw source body leaked into a knowledgebase artifact"
            );
        }
    }

    #[test]
    fn source_owned_bound_admission_cannot_claim_or_retire_a_foreign_pointer() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let setup_path = external.path().join("setup.md");
        let foreign_path = external.path().join("foreign.md");
        let owned_path = external.path().join("owned.md");
        fs::write(&setup_path, "setup source body").expect("write setup source");
        fs::write(&foreign_path, "foreign source body").expect("write foreign source");
        fs::write(&owned_path, "owned source body").expect("write owned source");
        let setup = add_local_source(root.path(), &setup_path).expect("create binding");
        let binding = match &setup.location {
            SourceLocation::BoundPath { binding, .. } => binding.clone(),
            location => panic!("expected bound setup pointer, got {location:?}"),
        };

        let mut admission = SourceAdmissionTransaction::begin(root.path()).expect("begin request");
        let foreign = add_local_source(root.path(), &foreign_path).expect("independent add");
        admission
            .add_bound_batch(&[(binding, "owned.md".into())])
            .expect("request-owned bound add");
        let error = seal_error(admission);

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        let remaining = source_status(root.path()).expect("read surviving pointers");
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().any(|source| source == &setup));
        assert!(remaining.iter().any(|source| source == &foreign));
    }

    #[test]
    fn source_owned_https_admission_cannot_claim_or_retire_a_foreign_pointer() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let foreign_path = external.path().join("foreign.md");
        fs::write(&foreign_path, "foreign source body").expect("write foreign source");

        let mut admission = SourceAdmissionTransaction::begin(root.path()).expect("begin request");
        let foreign = add_local_source(root.path(), &foreign_path).expect("independent add");
        admission
            .add_https_with_fetcher(
                "https://docs.example.test/owned.md",
                allow_https_fetch(),
                |_, writer, _| {
                    writer.write_all(b"owned HTTPS source body")?;
                    Ok(b"owned HTTPS source body".len() as u64)
                },
            )
            .expect("request-owned HTTPS add");
        let error = seal_error(admission);

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert_eq!(
            source_status(root.path()).expect("foreign pointer remains"),
            vec![foreign]
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_owned_directory_admission_cannot_claim_or_retire_a_foreign_pointer() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let foreign_path = external.path().join("foreign.md");
        let directory = external.path().join("owned-directory");
        fs::create_dir_all(&directory).expect("create owned directory");
        fs::write(&foreign_path, "foreign source body").expect("write foreign source");
        fs::write(directory.join("a.md"), "owned directory source body")
            .expect("write owned source");

        let mut admission = SourceAdmissionTransaction::begin(root.path()).expect("begin request");
        let foreign = add_local_source(root.path(), &foreign_path).expect("independent add");
        let mut outcomes = Vec::new();
        admission
            .add_directory(&directory, |outcome| outcomes.push(outcome))
            .expect("request-owned directory add");
        let error = seal_error(admission);

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            source_status(root.path()).expect("foreign pointer remains"),
            vec![foreign]
        );
    }

    #[test]
    fn source_owned_admission_rolls_back_earlier_pointers_when_a_later_https_add_fails() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let local = external.path().join("local.md");
        fs::write(&local, "local source body").expect("write local source");

        let mut admission = SourceAdmissionTransaction::begin(root.path()).expect("begin request");
        admission
            .add_local_batch([&local])
            .expect("add first request pointer");
        let error = admission
            .add_https_with_fetcher(
                "https://docs.example.test/fails.md",
                allow_https_fetch(),
                |_, _, _| anyhow::bail!("injected HTTPS failure"),
            )
            .expect_err("later failed operation must abort its transaction");

        assert!(
            format!("{error:#}").contains("injected HTTPS failure"),
            "{error:#}"
        );
        assert!(
            source_status(root.path())
                .expect("read cleaned source index")
                .is_empty(),
            "earlier request-owned pointer must be retired"
        );
    }

    #[test]
    fn source_owned_re_admission_rejects_before_publication_and_keeps_the_index_byte_identical() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let input = external.path().join("source.md");
        fs::write(&input, "first source revision").expect("write first revision");

        let first = admit_sources(root.path(), [&input]).expect("admit first pointer");
        let before = fs::read(root.path().join("sources/index.json")).expect("read first index");
        fs::write(&input, "changed source revision").expect("write changed revision");

        let error = match admit_sources(root.path(), [&input]) {
            Ok(_) => panic!("re-admission of an existing source must fail before publication"),
            Err(error) => error,
        };

        assert!(
            format!("{error:#}").contains("cannot re-admit"),
            "{error:#}"
        );
        assert_eq!(
            fs::read(root.path().join("sources/index.json")).expect("read unchanged index"),
            before,
            "failed re-admission must not alter pointer metadata"
        );
        assert_eq!(
            source_status(root.path()).expect("read unchanged pointer"),
            first.sources()
        );
    }

    #[test]
    fn heterogeneous_admission_rolls_back_a_local_pointer_when_later_https_fails() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let local = external.path().join("local.md");
        fs::write(&local, "heterogeneous local source body").expect("write local source");
        let inputs = vec![
            SourceAdmissionInput::LocalPath(local),
            SourceAdmissionInput::Https {
                url: "https://docs.example.test/fails.md".into(),
                consent: allow_https_fetch(),
            },
        ];

        let error = match admit_source_inputs_with_https_fetcher(
            root.path(),
            &inputs,
            |_| {},
            |_, _, _| anyhow::bail!("injected heterogeneous HTTPS failure"),
        ) {
            Ok(_) => panic!("a hard request failure must reject the whole admission"),
            Err(error) => error,
        };

        assert!(
            format!("{error:#}").contains("injected heterogeneous HTTPS failure"),
            "{error:#}"
        );
        assert!(
            source_status(root.path())
                .expect("read rolled-back source index")
                .is_empty(),
            "the earlier local pointer must be retired"
        );
        for artifact in walk_files(root.path()) {
            assert!(
                !fs::read(root.path().join(artifact))
                    .expect("read retained artifact")
                    .windows(b"heterogeneous local source body".len())
                    .any(|part| part == b"heterogeneous local source body"),
                "raw local body leaked into a knowledgebase artifact"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn heterogeneous_admission_preserves_directory_outcomes_and_bound_pointers() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let setup = external.path().join("setup.md");
        let bound = external.path().join("bound.md");
        let directory = external.path().join("directory");
        fs::create_dir_all(&directory).expect("create source directory");
        fs::write(&setup, "setup source body").expect("write setup source");
        fs::write(&bound, "bound source body").expect("write bound source");
        fs::write(directory.join("a.md"), "directory source body").expect("write directory source");
        let setup = add_local_source(root.path(), &setup).expect("create binding");
        let binding = match &setup.location {
            SourceLocation::BoundPath { binding, .. } => binding.clone(),
            location => panic!("expected bound setup pointer, got {location:?}"),
        };
        let inputs = vec![
            SourceAdmissionInput::Directory(directory),
            SourceAdmissionInput::BoundPath {
                binding,
                path: "bound.md".into(),
            },
        ];
        let mut outcomes = Vec::new();

        let receipt = admit_source_inputs(root.path(), &inputs, |outcome| outcomes.push(outcome))
            .expect("admit mixed directory and bound sources");

        assert_eq!(receipt.sources().len(), 2);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], SourceAddOutcome::Added { .. }));
        let index = load_source_index(root.path()).expect("read pointer index");
        assert_eq!(index.sources.len(), 3);
        let index_text = fs::read_to_string(root.path().join("sources/index.json"))
            .expect("read pointer index text");
        for raw_body in [
            "setup source body",
            "bound source body",
            "directory source body",
        ] {
            assert!(
                !index_text.contains(raw_body),
                "raw source body leaked into the pointer index"
            );
        }
    }

    #[test]
    fn local_git_reader_rejects_oversized_blobs_and_tree_objects() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let repository = tempfile::tempdir().expect("external Git repository");
        init_git(repository.path());
        fs::create_dir(repository.path().join("docs")).expect("create documents directory");
        fs::write(repository.path().join("docs/source.md"), "source").expect("write source");
        fs::write(
            repository.path().join("oversized.bin"),
            vec![b'x'; MAX_SOURCE_INDEX_BYTES as usize + 1],
        )
        .expect("write oversized blob");
        for args in [
            vec!["add", "--all"],
            vec![
                "-c",
                "user.name=Graphoxide Test",
                "-c",
                "user.email=graphoxide@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
            vec![
                "remote",
                "add",
                "origin",
                "https://git.example.com/source.git",
            ],
        ] {
            assert!(Command::new("git")
                .arg("-C")
                .arg(repository.path())
                .args(args)
                .status()
                .expect("prepare Git fixture")
                .success());
        }
        let repository_root = repository
            .path()
            .canonicalize()
            .expect("canonical repository");
        write_source_bindings(
            root.path(),
            &SourceBindings {
                schema: SOURCE_BINDINGS_SCHEMA.into(),
                bindings: vec![SourceBinding {
                    alias: local_binding_alias(&repository_root),
                    root: repository_root.clone(),
                }],
            },
        )
        .expect("write private binding");
        let head = GitWorkingDirectory::Path(&repository_root)
            .output(&["rev-parse", "HEAD"])
            .expect("read revision");
        let head = std::str::from_utf8(&head.stdout)
            .expect("UTF-8 revision")
            .trim();

        let oversized = read_bound_git_source(
            root.path(),
            "https://git.example.com/source.git",
            head,
            "oversized.bin",
        )
        .expect_err("oversized local blob must be rejected before body read");
        assert!(
            oversized.to_string().contains("size limit"),
            "{oversized:#}"
        );
        let tree = read_bound_git_source(
            root.path(),
            "https://git.example.com/source.git",
            head,
            "docs",
        )
        .expect_err("a tree object is not a source body");
        assert!(tree.to_string().contains("must be a blob"), "{tree:#}");
    }

    #[test]
    fn local_git_reader_caps_subprocess_output() {
        let error = read_git_output_bounded(
            git_source_command().args([
                "-c",
                "graphoxide.probe=long-output",
                "config",
                "--get",
                "graphoxide.probe",
            ]),
            4,
        )
        .expect_err("subprocess output over the cap must be rejected");
        assert!(
            error.to_string().contains("output exceeds the size limit"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_directory_import_reads_the_pinned_local_revision_without_index_path_leakage() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let repository = tempfile::tempdir().expect("external Git repository");
        init_git(repository.path());
        let tracked = repository.path().join("docs/reference.md");
        let sibling = repository.path().join("docs/retained.md");
        fs::create_dir_all(tracked.parent().expect("source parent")).expect("create source parent");
        fs::write(&tracked, "Git-only source body").expect("write source");
        fs::write(&sibling, "Sibling Git-only source body").expect("write sibling source");
        let status = Command::new("git")
            .args([
                "-C",
                repository.path().to_str().expect("UTF-8 repo"),
                "add",
                "--all",
            ])
            .status()
            .expect("stage source");
        assert!(status.success());
        let status = Command::new("git")
            .args([
                "-C",
                repository.path().to_str().expect("UTF-8 repo"),
                "-c",
                "user.name=Graphoxide Test",
                "-c",
                "user.email=graphoxide@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "source",
            ])
            .status()
            .expect("commit source");
        assert!(status.success());
        let status = Command::new("git")
            .args([
                "-C",
                repository.path().to_str().expect("UTF-8 repo"),
                "remote",
                "add",
                "origin",
                "https://git.example.com/group/repository.git",
            ])
            .status()
            .expect("add remote");
        assert!(status.success());

        let (summary, outcomes) = collect_bulk_outcomes(root.path(), repository.path())
            .expect("bulk import clean Git directory");
        assert_eq!(summary.added, 2);
        let sources = outcomes
            .into_iter()
            .filter_map(|outcome| match outcome {
                SourceAddOutcome::Added { source } => Some(source),
                _ => None,
            })
            .collect::<Vec<_>>();
        let source = sources
            .iter()
            .find(|source| matches!(&source.location, SourceLocation::Git { path, .. } if path == "docs/reference.md"))
            .cloned()
            .expect("primary Git source outcome");
        let sibling = sources
            .iter()
            .find(|source| matches!(&source.location, SourceLocation::Git { path, .. } if path == "docs/retained.md"))
            .cloned()
            .expect("sibling Git source outcome");
        assert!(matches!(source.location, SourceLocation::Git { .. }));
        let bindings_path = root.path().join(".graphoxide/source-bindings.yaml");
        assert_eq!(
            read_source_transient(root.path(), &source, None)
                .expect("read the locally bound Git revision without network consent"),
            b"Git-only source body"
        );
        fs::write(&tracked, "uncommitted source edit").expect("change the local worktree");
        assert_eq!(
            refresh_source(root.path(), &source.source_id, None)
                .expect("refresh the pinned local Git revision without network consent"),
            source,
        );
        assert!(
            bindings_path.exists(),
            "Git sources need an ignored local binding for immediate authoring"
        );
        let index = fs::read_to_string(root.path().join("sources/index.json"))
            .expect("read public pointer index");
        assert!(
            !index.contains(repository.path().to_str().expect("UTF-8 repository root")),
            "public pointer index must not retain the local checkout root"
        );

        retire_source(root.path(), &source.source_id).expect("retire Git source");

        assert_eq!(
            read_source_transient(root.path(), &sibling, None)
                .expect("read remaining locally bound Git revision"),
            b"Sibling Git-only source body"
        );
        retire_source(root.path(), &sibling.source_id).expect("retire final Git source");

        assert!(source_status(root.path())
            .expect("read empty source index")
            .is_empty());
        assert!(
            load_source_bindings(&bindings_path)
                .expect("read pruned source bindings")
                .bindings
                .is_empty(),
            "retirement must remove the private checkout root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bulk_directory_add_partitions_lexically_without_retaining_source_bodies() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        for index in 0..=MAX_SOURCE_BATCH_FILES {
            fs::write(
                external.path().join(format!("source-{index:04}.md")),
                format!("body-{index}"),
            )
            .expect("write source");
        }

        let (summary, outcomes) = collect_bulk_outcomes(root.path(), external.path())
            .expect("partition and add all directory sources");
        let added = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                SourceAddOutcome::Added { source } => Some(source),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(summary.errors, 0);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.added, (MAX_SOURCE_BATCH_FILES + 1) as u64);
        assert_eq!(
            added
                .iter()
                .map(|source| match &source.location {
                    SourceLocation::BoundPath { path, .. } => path.as_str(),
                    other => panic!("expected bound locator, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            (0..=MAX_SOURCE_BATCH_FILES)
                .map(|index| format!("source-{index:04}.md"))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            load_source_index(root.path())
                .expect("load published partitions")
                .sources
                .len(),
            MAX_SOURCE_BATCH_FILES + 1
        );
        assert!(walk_files(root.path())
            .iter()
            .all(|path| !path.contains("source-store") && !path.contains("raw")));
    }

    #[cfg(unix)]
    #[test]
    fn bulk_directory_add_reports_unsafe_and_oversized_locators_while_adding_valid_siblings() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        fs::write(external.path().join("a-good.md"), "safe first body")
            .expect("write first source");
        symlink(
            external.path().join("a-good.md"),
            external.path().join("m-unsafe.md"),
        )
        .expect("make unsafe source");
        let oversized = external.path().join("z-oversized.md");
        fs::File::create(&oversized)
            .expect("create oversized source")
            .set_len(MAX_SOURCE_INDEX_BYTES + 1)
            .expect("size oversized source");
        fs::write(external.path().join("zz-good.md"), "safe final body")
            .expect("write final source");

        let (summary, outcomes) = collect_bulk_outcomes(root.path(), external.path())
            .expect("unsafe and oversized children should not abort valid siblings");
        let added = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                SourceAddOutcome::Added { source } => Some(source),
                _ => None,
            })
            .collect::<Vec<_>>();
        let errors = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                SourceAddOutcome::Error { error } => Some(error),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            added
                .iter()
                .map(|source| match &source.location {
                    SourceLocation::BoundPath { path, .. } => path.as_str(),
                    other => panic!("expected bound locator, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            ["a-good.md", "zz-good.md"]
        );
        assert_eq!(
            errors
                .iter()
                .map(|error| (&error.location, &error.kind))
                .collect::<Vec<_>>(),
            vec![
                (
                    &SourceLocation::BoundPath {
                        binding: local_binding_alias(
                            &external
                                .path()
                                .canonicalize()
                                .expect("canonical external root")
                        ),
                        path: "m-unsafe.md".into(),
                    },
                    &SourceAddErrorKind::UnsafeOrUnreadable,
                ),
                (
                    &SourceLocation::BoundPath {
                        binding: local_binding_alias(
                            &external
                                .path()
                                .canonicalize()
                                .expect("canonical external root")
                        ),
                        path: "z-oversized.md".into(),
                    },
                    &SourceAddErrorKind::Oversized,
                ),
            ]
        );
        assert_eq!(summary.added, 2);
        assert_eq!(summary.errors, 2);
        let files = walk_files(root.path());
        assert!(files.iter().all(|path| {
            !path.contains("source-store") && !path.contains("captures") && !path.contains("raw")
        }));
        for artifact in files {
            let bytes = fs::read(root.path().join(artifact)).expect("read knowledgebase artifact");
            assert!(!bytes
                .windows(b"safe first body".len())
                .any(|part| part == b"safe first body"));
            assert!(!bytes
                .windows(b"safe final body".len())
                .any(|part| part == b"safe final body"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn bulk_directory_add_skips_only_recognized_administrative_and_structural_entries() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        fs::write(external.path().join("content.md"), "content body").expect("write content");
        fs::write(external.path().join("README.md"), "structural body").expect("write README");
        fs::create_dir_all(external.path().join("_sources")).expect("create admin directory");
        fs::write(external.path().join("_sources/source.yaml"), "admin body")
            .expect("write admin data");

        let (summary, outcomes) = collect_bulk_outcomes(root.path(), external.path())
            .expect("directory import succeeds with skips");
        let added = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                SourceAddOutcome::Added { source } => Some(source),
                _ => None,
            })
            .collect::<Vec<_>>();
        let skipped = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                SourceAddOutcome::Skipped { skip } => Some(skip),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(summary.added, 1);
        assert!(matches!(
            &added[0].location,
            SourceLocation::BoundPath { path, .. } if path == "content.md"
        ));
        assert_eq!(
            skipped
                .iter()
                .map(|skip| match (&skip.location, &skip.kind) {
                    (SourceLocation::BoundPath { path, .. }, kind) => (path.as_str(), kind),
                    other => panic!("expected bound skip, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            vec![
                ("README.md", &SourceAddSkipKind::StructuralFile),
                ("_sources", &SourceAddSkipKind::AdministrativeDirectory),
            ]
        );
        assert_eq!(summary.errors, 0);
        assert_eq!(summary.skipped, 2);
    }

    #[cfg(unix)]
    #[test]
    fn git_directory_discovery_keeps_the_opened_directory_after_path_replacement() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("temporary source fixture");
        let original = fixture.path().join("original");
        let moved = fixture.path().join("moved");
        let replacement = fixture.path().join("replacement");
        fs::create_dir_all(original.join("docs")).expect("create source directory");
        fs::create_dir_all(replacement.join("docs")).expect("create replacement directory");
        let body = b"pinned Git source";
        fs::write(original.join("docs/source.md"), body).expect("write source");
        fs::write(replacement.join("docs/source.md"), "replacement source")
            .expect("write replacement source");
        init_git(&original);
        for args in [
            vec!["add", "--all"],
            vec![
                "-c",
                "user.name=Graphoxide Test",
                "-c",
                "user.email=graphoxide@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
            vec![
                "remote",
                "add",
                "origin",
                "https://git.example.com/original.git",
            ],
        ] {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&original)
                .args(args)
                .status()
                .expect("prepare Git fixture")
                .success());
        }
        let directory =
            open_directory_nofollow(&original.join("docs")).expect("hold original directory");
        fs::rename(&original, &moved).expect("move opened repository");
        symlink(&replacement, &original).expect("replace original path with a link");

        let location = git_location_from_descriptor(&directory, &OsString::from("source.md"), body)
            .expect("discover held Git directory")
            .expect("held directory remains a Git source");

        assert!(matches!(location, SourceLocation::Git { remote, path, .. }
            if remote == "https://git.example.com/original.git" && path == "docs/source.md"));
    }

    #[cfg(unix)]
    #[test]
    fn directory_import_rejects_broken_git_metadata_instead_of_a_bound_path() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let repository = tempfile::tempdir().expect("broken Git source");
        fs::write(
            repository.path().join(".git"),
            "gitdir: missing-repository\n",
        )
        .expect("write broken Git metadata");
        fs::write(repository.path().join("source.md"), "source body").expect("write source");

        let (summary, outcomes) = collect_bulk_outcomes(root.path(), repository.path())
            .expect("report individual source failure");

        assert_eq!(summary.added, 0);
        assert_eq!(summary.errors, 1);
        assert!(outcomes.iter().any(|outcome| matches!(outcome,
            SourceAddOutcome::Error { error } if matches!(&error.location,
                SourceLocation::BoundPath { path, .. } if path == "source.md"))));
        assert!(load_source_index_or_empty(root.path())
            .expect("read unchanged index")
            .sources
            .is_empty());
        let source = repository
            .path()
            .join("source.md")
            .canonicalize()
            .expect("canonical source");
        let error = git_location(&source, b"source body")
            .expect_err("explicit file also rejects broken Git metadata");
        assert!(
            error.to_string().contains("Git source discovery failed"),
            "{error:#}"
        );
    }

    #[test]
    fn binding_preflight_compares_canonical_roots_but_rejects_subdirectories() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        ensure_source_bindings_git_preflight(&root.path().join("."))
            .expect("equivalent root spelling is accepted");
        let nested = root.path().join("nested");
        fs::create_dir(&nested).expect("create nested directory");
        let error = ensure_source_bindings_git_preflight(&nested)
            .expect_err("a real subdirectory is not a knowledgebase root");
        assert!(error.to_string().contains("Git worktree root"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn public_directory_admission_preserves_git_identity_and_reports_dirty_siblings() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let repository = tempfile::tempdir().expect("external Git repository");
        init_git(repository.path());
        let source = repository.path().join("docs/clean.md");
        let dirty = repository.path().join("docs/dirty.md");
        let untracked = repository.path().join("docs/untracked.md");
        fs::create_dir_all(source.parent().expect("source parent")).expect("create source parent");
        fs::write(&source, "immutable Git source").expect("write source");
        fs::write(&dirty, "clean before mutation").expect("write tracked sibling");
        for arguments in [
            vec!["add", "--all"],
            vec![
                "-c",
                "user.name=Graphoxide Test",
                "-c",
                "user.email=graphoxide@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
            vec![
                "remote",
                "add",
                "origin",
                "ssh://git@git.example.com:2222/group/project.git",
            ],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(repository.path())
                .args(arguments)
                .status()
                .expect("run Git fixture command");
            assert!(status.success());
        }
        let head = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(repository.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("read fixture HEAD")
                .stdout,
        )
        .expect("UTF-8 fixture HEAD")
        .trim()
        .to_owned();
        let status = Command::new("git")
            .arg("-C")
            .arg(repository.path())
            .args(["checkout", "--quiet", "--detach"])
            .status()
            .expect("detach Git fixture HEAD");
        assert!(status.success());
        fs::write(&dirty, "dirty sibling").expect("mutate tracked sibling");
        fs::write(&untracked, "untracked sibling").expect("write untracked sibling");

        let mut outcomes = Vec::new();
        let receipt = admit_source_inputs(
            root.path(),
            &[SourceAdmissionInput::Directory(
                repository.path().to_path_buf(),
            )],
            |outcome| outcomes.push(outcome),
        )
        .expect("admit clean Git directory with dirty siblings");
        let added = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                SourceAddOutcome::Added { source } => Some(source),
                _ => None,
            })
            .collect::<Vec<_>>();
        let errors = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                SourceAddOutcome::Error { error } => Some(error),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(receipt.sources().len(), 1);
        assert_eq!(added.len(), 1);
        assert!(
            matches!(
                &added[0].location,
                SourceLocation::Git { remote, commit, path }
                    if remote == "ssh://git.example.com:2222/group/project.git"
                        && commit == &head
                        && path == "docs/clean.md"
            ),
            "{:?}",
            added[0].location
        );
        assert_eq!(
            errors
                .iter()
                .map(|error| match (&error.location, &error.kind) {
                    (
                        SourceLocation::BoundPath { path, .. },
                        SourceAddErrorKind::UnsafeOrUnreadable,
                    ) => path.as_str(),
                    other => panic!("expected unsafe bound-path error, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            vec!["docs/dirty.md", "docs/untracked.md"]
        );
        let index_text =
            fs::read_to_string(root.path().join("sources/index.json")).expect("read pointer index");
        assert!(
            !index_text.contains(&repository.path().display().to_string()),
            "pointer index must not persist the external checkout root"
        );
    }

    #[test]
    fn explicit_file_add_does_not_apply_directory_structural_skips() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        init_git(root.path());
        let external = tempfile::tempdir().expect("external source root");
        let readme = external.path().join("README.md");
        fs::write(&readme, "explicit source body").expect("write README source");

        let source = add_local_source(root.path(), &readme)
            .expect("an explicitly selected README remains a source");

        assert!(matches!(
            source.location,
            SourceLocation::BoundPath { path, .. } if path == "README.md"
        ));
    }

    #[test]
    fn source_index_rejects_duplicate_sources_and_forbidden_fields() {
        let mut duplicate = valid_bound_index();
        let entry = duplicate["sources"][0].clone();
        duplicate["sources"]
            .as_array_mut()
            .expect("sources array")
            .push(entry);
        let error = parse_value(&duplicate).expect_err("duplicate source must fail");
        assert!(error.to_string().contains("duplicate source id"));

        for field in [
            "raw_body",
            "capture_id",
            "capture_mapping",
            "body_base64",
            "excerpt",
        ] {
            let mut value = valid_bound_index();
            value["sources"][0][field] = "secret".into();
            let error = parse_value(&value).expect_err("forbidden field must fail");
            assert!(error.to_string().contains("source index"));
        }
    }

    #[test]
    fn source_index_writer_round_trips_in_deterministic_order_without_raw_artifacts() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let index = SourceIndex {
            schema: SOURCE_INDEX_SCHEMA.into(),
            sources: vec![
                SourceEntry {
                    source_id:
                        "src:fd21de4e4221b3da536dcb45574345aff45ddd6aeee37b717fd81e367a20596c"
                            .into(),
                    location: SourceLocation::Git {
                        remote: "https://git.example.com/group/repo.git".into(),
                        commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                        path: "docs/a.md".into(),
                    },
                    content_sha256:
                        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
                    bytes: 3,
                    status: SourceStatus::AiReviewed,
                },
                SourceEntry {
                    source_id: BOUND_SOURCE_ID.into(),
                    location: SourceLocation::BoundPath {
                        binding: "private".into(),
                        path: "docs/a.md".into(),
                    },
                    content_sha256:
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                    bytes: 2,
                    status: SourceStatus::HumanConfirmed,
                },
                SourceEntry {
                    source_id:
                        "src:23fa19dcb375811270960f64016f07e6d44bde750a9a9f0d2f3da6a66464dcc9"
                            .into(),
                    location: SourceLocation::Https {
                        url: "https://docs.example.com/a.md".into(),
                    },
                    content_sha256:
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    bytes: 1,
                    status: SourceStatus::Provisional,
                },
            ],
        };

        write_source_index(root.path(), &index).expect("write source index");
        let loaded = load_source_index(root.path()).expect("load source index");

        assert_eq!(
            loaded
                .sources
                .iter()
                .map(|source| source.source_id.as_str())
                .collect::<Vec<_>>(),
            [
                "src:23fa19dcb375811270960f64016f07e6d44bde750a9a9f0d2f3da6a66464dcc9",
                BOUND_SOURCE_ID,
                "src:fd21de4e4221b3da536dcb45574345aff45ddd6aeee37b717fd81e367a20596c",
            ]
        );

        let other_root = tempfile::tempdir().expect("second temporary knowledgebase");
        let mut reversed = index.clone();
        reversed.sources.reverse();
        write_source_index(other_root.path(), &reversed).expect("write reversed source index");
        assert_eq!(
            fs::read(root.path().join("sources/index.json")).expect("read first index"),
            fs::read(other_root.path().join("sources/index.json")).expect("read second index")
        );

        let root_entries = fs::read_dir(root.path())
            .expect("read knowledgebase root")
            .map(|entry| entry.expect("root entry").file_name())
            .collect::<Vec<_>>();
        let source_entries = fs::read_dir(root.path().join("sources"))
            .expect("read sources directory")
            .map(|entry| entry.expect("source entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(root_entries, ["sources"]);
        assert_eq!(source_entries, ["index.json"]);
        let bytes = fs::read(root.path().join("sources/index.json")).expect("read source index");
        assert!(!bytes
            .windows(b"secret raw body".len())
            .any(|part| part == b"secret raw body"));
    }

    #[test]
    fn invalid_source_index_does_not_replace_the_last_valid_index() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let initial = SourceIndex::from_json(
            serde_json::to_vec(&valid_bound_index()).expect("serialize valid index"),
        )
        .expect("valid index");
        write_source_index(root.path(), &initial).expect("write initial index");
        let before = fs::read(root.path().join("sources/index.json")).expect("read initial index");

        let mut invalid = initial;
        invalid.sources[0].content_sha256 = "not-a-digest".into();
        write_source_index(root.path(), &invalid).expect_err("invalid replacement must fail");

        assert_eq!(
            fs::read(root.path().join("sources/index.json")).expect("read preserved index"),
            before
        );
    }

    #[test]
    fn source_index_loader_rejects_oversized_input() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        fs::create_dir(root.path().join("sources")).expect("create sources directory");
        let file = fs::File::create(root.path().join("sources/index.json")).expect("create index");
        file.set_len(MAX_SOURCE_INDEX_BYTES + 1)
            .expect("make oversized index");

        let error = load_source_index(root.path()).expect_err("oversized index must fail");
        assert!(format!("{error:#}").contains("byte cap"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn source_index_io_rejects_symlinked_index() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let outside = tempfile::NamedTempFile::new().expect("outside file");
        fs::write(
            outside.path(),
            serde_json::to_vec(&valid_bound_index()).expect("serialize valid index"),
        )
        .expect("write outside index");
        fs::create_dir(root.path().join("sources")).expect("create sources directory");
        symlink(outside.path(), root.path().join("sources/index.json"))
            .expect("create index symlink");

        let error = load_source_index(root.path()).expect_err("symlinked load must fail");
        assert!(
            format!("{error:#}").contains("unsafe non-regular"),
            "{error:#}"
        );

        let index = SourceIndex::from_json(
            serde_json::to_vec(&valid_bound_index()).expect("serialize valid index"),
        )
        .expect("valid index");
        let error = write_source_index(root.path(), &index).expect_err("symlinked write must fail");
        assert!(
            format!("{error:#}").contains("symlinked publication destination"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_index_loader_rejects_multiply_linked_index() {
        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let outside = tempfile::NamedTempFile::new().expect("outside file");
        fs::write(
            outside.path(),
            serde_json::to_vec(&valid_bound_index()).expect("serialize valid index"),
        )
        .expect("write outside index");
        fs::create_dir(root.path().join("sources")).expect("create sources directory");
        fs::hard_link(outside.path(), root.path().join("sources/index.json"))
            .expect("create hard-linked index");

        let error = load_source_index(root.path()).expect_err("multiply linked load must fail");
        assert!(
            format!("{error:#}").contains("multiply linked"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_index_io_rejects_symlinked_sources_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary knowledgebase");
        let outside = tempfile::tempdir().expect("outside directory");
        fs::write(
            outside.path().join("index.json"),
            serde_json::to_vec(&valid_bound_index()).expect("serialize index"),
        )
        .expect("write outside index");
        symlink(outside.path(), root.path().join("sources")).expect("link sources directory");

        let error = load_source_index(root.path()).expect_err("linked parent load must fail");
        assert!(
            format!("{error:#}").contains("unsafe parent directory"),
            "{error:#}"
        );

        let index = SourceIndex::from_json(
            serde_json::to_vec(&valid_bound_index()).expect("serialize valid index"),
        )
        .expect("valid index");
        let error =
            write_source_index(root.path(), &index).expect_err("linked parent write must fail");
        assert!(format!("{error:#}").contains("real directory"), "{error:#}");
    }
}
