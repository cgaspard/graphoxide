//! Disposable local state for one Git-backed registry.
//!
//! The database stores only source identifiers, local bindings, stat/hash
//! evidence, and scheduler state. It is intentionally rebuildable from a
//! pinned registry tree and a fresh source scan.

use anyhow::{ensure, Context as _};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    io::Read as _,
    path::{Path, PathBuf},
};

const CACHE_FILE: &str = "registry.sqlite3";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Availability {
    Available,
    Missing,
    Inaccessible,
}

impl Availability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Missing => "missing",
            Self::Inaccessible => "inaccessible",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "available" => Ok(Self::Available),
            "missing" => Ok(Self::Missing),
            "inaccessible" => Ok(Self::Inaccessible),
            _ => anyhow::bail!("invalid local registry availability"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceObservation {
    pub availability: Availability,
    pub size_bytes: Option<u64>,
    pub mtime_ns: Option<i64>,
    pub ctime_ns: Option<i64>,
    pub sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanDisposition {
    Unchanged,
    HashRequired,
    Missing,
    Inaccessible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueReason {
    Manual,
    Changed,
    MissingExtraction,
    MissingModel,
    Expired,
    Retryable,
    ReviewInvalidated,
}

impl QueueReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Changed => "changed",
            Self::MissingExtraction => "missing-extraction",
            Self::MissingModel => "missing-model",
            Self::Expired => "expired",
            Self::Retryable => "retryable",
            Self::ReviewInvalidated => "review-invalidated",
        }
    }

    fn rank(self) -> i64 {
        match self {
            Self::Manual => 0,
            Self::Changed => 1,
            Self::MissingExtraction => 2,
            Self::MissingModel => 3,
            Self::Expired => 4,
            Self::Retryable => 5,
            Self::ReviewInvalidated => 6,
        }
    }

    fn from_rank(rank: i64) -> anyhow::Result<Self> {
        match rank {
            0 => Ok(Self::Manual),
            1 => Ok(Self::Changed),
            2 => Ok(Self::MissingExtraction),
            3 => Ok(Self::MissingModel),
            4 => Ok(Self::Expired),
            5 => Ok(Self::Retryable),
            6 => Ok(Self::ReviewInvalidated),
            _ => anyhow::bail!("invalid local registry queue reason"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkItem {
    pub source_id: String,
    pub stage: String,
    pub reason: QueueReason,
    pub tag_priority: i64,
}

/// One local SQLite cache. It is not a shared scheduler or registry authority.
#[derive(Debug)]
pub struct RegistryLocalState {
    path: PathBuf,
    connection: Connection,
}

impl RegistryLocalState {
    /// Open the conventional XDG cache path for one registry revision.
    pub fn open(catalog_id: &str, registry_revision: &str) -> anyhow::Result<Self> {
        let cache_home = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .context("XDG_CACHE_HOME or HOME is required for registry local state")?;
        Self::open_in(&cache_home, catalog_id, registry_revision)
    }

    /// Open a testable cache location beneath an explicit cache home.
    pub fn open_in(
        cache_home: &Path,
        catalog_id: &str,
        registry_revision: &str,
    ) -> anyhow::Result<Self> {
        validate_id("catalog_id", catalog_id)?;
        validate_metadata("registry_revision", registry_revision)?;
        let path = cache_home
            .join("graphoxide")
            .join("catalogs")
            .join(catalog_id)
            .join(CACHE_FILE);
        fs::create_dir_all(path.parent().context("registry cache path lacks parent")?)
            .with_context(|| format!("create registry cache parent {}", path.display()))?;
        let connection = Connection::open(&path)
            .with_context(|| format!("open registry local state {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS registry_meta (
                catalog_id TEXT PRIMARY KEY NOT NULL,
                registry_revision TEXT NOT NULL
            ) STRICT;
            CREATE TABLE IF NOT EXISTS origin_binding (
                origin_id TEXT PRIMARY KEY NOT NULL,
                local_path TEXT NOT NULL
            ) STRICT;
            CREATE TABLE IF NOT EXISTS source_scan (
                source_id TEXT PRIMARY KEY NOT NULL,
                origin_id TEXT,
                relative_path TEXT,
                availability TEXT NOT NULL,
                size_bytes INTEGER,
                mtime_ns INTEGER,
                ctime_ns INTEGER,
                sha256 TEXT
            ) STRICT;
            CREATE TABLE IF NOT EXISTS work_queue (
                source_id TEXT NOT NULL,
                stage TEXT NOT NULL,
                reason_rank INTEGER NOT NULL,
                tag_priority INTEGER NOT NULL,
                enqueued_at INTEGER NOT NULL,
                lease_owner TEXT,
                lease_expires_at INTEGER,
                PRIMARY KEY (source_id, stage)
            ) STRICT;
            CREATE INDEX IF NOT EXISTS work_queue_order
              ON work_queue(reason_rank, tag_priority DESC, enqueued_at, source_id, stage);
            ",
        )?;
        ensure_source_scan_binding_columns(&connection)?;

        let previous = connection
            .query_row(
                "SELECT registry_revision FROM registry_meta WHERE catalog_id = ?1",
                [catalog_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if previous.as_deref() != Some(registry_revision) {
            // Scan fingerprints are tied to the immutable source binding below,
            // rather than an unrelated registry commit. Queue intent is not.
            connection.execute_batch("DELETE FROM work_queue;")?;
            connection.execute(
                "INSERT INTO registry_meta(catalog_id, registry_revision) VALUES(?1, ?2)
                 ON CONFLICT(catalog_id) DO UPDATE SET registry_revision = excluded.registry_revision",
                params![catalog_id, registry_revision],
            )?;
        }
        Ok(Self { path, connection })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bind a logical registry origin to a machine-local source root.
    pub fn bind_origin(&self, origin_id: &str, local_path: &Path) -> anyhow::Result<()> {
        validate_id("origin_id", origin_id)?;
        let local_path = local_path
            .to_str()
            .context("local origin binding must be UTF-8")?;
        ensure!(!local_path.is_empty(), "local origin binding is empty");
        let previous = self.origin_binding(origin_id)?;
        self.connection.execute(
            "INSERT INTO origin_binding(origin_id, local_path) VALUES(?1, ?2)
             ON CONFLICT(origin_id) DO UPDATE SET local_path = excluded.local_path",
            params![origin_id, local_path],
        )?;
        if previous
            .as_deref()
            .is_some_and(|previous| previous != local_path)
        {
            self.connection
                .execute("DELETE FROM source_scan WHERE origin_id = ?1", [origin_id])?;
        }
        Ok(())
    }

    pub fn origin_binding(&self, origin_id: &str) -> anyhow::Result<Option<String>> {
        validate_id("origin_id", origin_id)?;
        self.connection
            .query_row(
                "SELECT local_path FROM origin_binding WHERE origin_id = ?1",
                [origin_id],
                |row| row.get(0),
            )
            .optional()
            .context("read local origin binding")
    }

    /// Read the last completed local observation for fast changed scans.
    pub fn observation(&self, source_id: &str) -> anyhow::Result<Option<SourceObservation>> {
        validate_id("source_id", source_id)?;
        self.connection
            .query_row(
                "SELECT availability, size_bytes, mtime_ns, ctime_ns, sha256
                 FROM source_scan WHERE source_id = ?1",
                [source_id],
                |row| {
                    Ok(SourceObservation {
                        availability: Availability::parse(row.get::<_, String>(0)?.as_str())
                            .map_err(to_sqlite_error)?,
                        size_bytes: row
                            .get::<_, Option<i64>>(1)?
                            .map(to_u64)
                            .transpose()
                            .map_err(to_sqlite_error)?,
                        mtime_ns: row.get(2)?,
                        ctime_ns: row.get(3)?,
                        sha256: row.get(4)?,
                    })
                },
            )
            .optional()
            .context("read local source observation")
    }

    /// Return all scan observations keyed by source ID for local catalog views.
    pub fn observations(&self) -> anyhow::Result<BTreeMap<String, SourceObservation>> {
        let mut statement = self.connection.prepare(
            "SELECT source_id, availability, size_bytes, mtime_ns, ctime_ns, sha256
             FROM source_scan ORDER BY source_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SourceObservation {
                    availability: Availability::parse(row.get::<_, String>(1)?.as_str())
                        .map_err(to_sqlite_error)?,
                    size_bytes: row
                        .get::<_, Option<i64>>(2)?
                        .map(to_u64)
                        .transpose()
                        .map_err(to_sqlite_error)?,
                    mtime_ns: row.get(3)?,
                    ctime_ns: row.get(4)?,
                    sha256: row.get(5)?,
                },
            ))
        })?;
        let mut observations = BTreeMap::new();
        for row in rows {
            let (source_id, observation) = row?;
            observations.insert(source_id, observation);
        }
        Ok(observations)
    }

    /// Decide whether a source needs hashing without reading its content.
    pub fn scan_disposition(
        &self,
        source_id: &str,
        origin_id: &str,
        relative_path: &str,
        observation: &SourceObservation,
    ) -> anyhow::Result<ScanDisposition> {
        validate_id("source_id", source_id)?;
        validate_id("origin_id", origin_id)?;
        validate_relative_path(relative_path)?;
        match observation.availability {
            Availability::Missing => return Ok(ScanDisposition::Missing),
            Availability::Inaccessible => return Ok(ScanDisposition::Inaccessible),
            Availability::Available => {}
        }
        let previous = self
            .connection
            .query_row(
                "SELECT origin_id, relative_path, availability, size_bytes, mtime_ns, ctime_ns, sha256
                 FROM source_scan WHERE source_id = ?1",
                [source_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        SourceObservation {
                            availability: Availability::parse(row.get::<_, String>(2)?.as_str())
                                .map_err(to_sqlite_error)?,
                            size_bytes: row
                                .get::<_, Option<i64>>(3)?
                                .map(to_u64)
                                .transpose()
                                .map_err(to_sqlite_error)?,
                            mtime_ns: row.get(4)?,
                            ctime_ns: row.get(5)?,
                            sha256: row.get(6)?,
                        },
                    ))
                },
            )
            .optional()
            .context("read local source scan binding")?;
        let unchanged = previous.is_some_and(|(previous_origin, previous_path, previous)| {
            previous_origin.as_deref() == Some(origin_id)
                && previous_path.as_deref() == Some(relative_path)
                && previous.availability == Availability::Available
                && previous.size_bytes == observation.size_bytes
                && previous.mtime_ns == observation.mtime_ns
                && previous.ctime_ns == observation.ctime_ns
                && previous.sha256.is_some()
        });
        Ok(if unchanged {
            ScanDisposition::Unchanged
        } else {
            ScanDisposition::HashRequired
        })
    }

    /// Persist a completed local scan observation; source bytes are never retained.
    pub fn record_observation(
        &self,
        source_id: &str,
        origin_id: &str,
        relative_path: &str,
        observation: &SourceObservation,
    ) -> anyhow::Result<()> {
        validate_id("source_id", source_id)?;
        validate_id("origin_id", origin_id)?;
        validate_relative_path(relative_path)?;
        if observation.availability == Availability::Available {
            ensure!(
                observation.size_bytes.is_some()
                    && observation.mtime_ns.is_some()
                    && observation.ctime_ns.is_some()
                    && observation.sha256.as_deref().is_some_and(is_sha256),
                "available source observations require stat evidence and SHA-256"
            );
        }
        self.connection.execute(
            "INSERT INTO source_scan(source_id, origin_id, relative_path, availability, size_bytes, mtime_ns, ctime_ns, sha256)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(source_id) DO UPDATE SET
                 origin_id = excluded.origin_id,
                 relative_path = excluded.relative_path,
                 availability = excluded.availability,
                 size_bytes = excluded.size_bytes,
                 mtime_ns = excluded.mtime_ns,
                 ctime_ns = excluded.ctime_ns,
                 sha256 = excluded.sha256",
            params![
                source_id,
                origin_id,
                relative_path,
                observation.availability.as_str(),
                observation.size_bytes.map(to_i64).transpose()?,
                observation.mtime_ns,
                observation.ctime_ns,
                observation.sha256,
            ],
        )?;
        Ok(())
    }

    /// Queue one stage with the fixed global priority ordering from the policy.
    pub fn enqueue(
        &self,
        source_id: &str,
        stage: &str,
        reason: QueueReason,
        tag_priority: i64,
        enqueued_at: i64,
    ) -> anyhow::Result<()> {
        validate_id("source_id", source_id)?;
        validate_id("stage", stage)?;
        self.connection.execute(
            "INSERT INTO work_queue(source_id, stage, reason_rank, tag_priority, enqueued_at, lease_owner, lease_expires_at)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL)
             ON CONFLICT(source_id, stage) DO UPDATE SET
                 reason_rank = MIN(work_queue.reason_rank, excluded.reason_rank),
                 tag_priority = MAX(work_queue.tag_priority, excluded.tag_priority),
                 enqueued_at = MIN(work_queue.enqueued_at, excluded.enqueued_at),
                 lease_owner = NULL,
                 lease_expires_at = NULL",
            params![source_id, stage, reason.rank(), tag_priority, enqueued_at],
        )?;
        Ok(())
    }

    /// Lease the next deterministic work item. A caller must complete or retry it.
    pub fn claim_next(
        &self,
        lease_owner: &str,
        now: i64,
        lease_expires_at: i64,
    ) -> anyhow::Result<Option<WorkItem>> {
        validate_metadata("lease_owner", lease_owner)?;
        ensure!(
            lease_expires_at > now,
            "work lease must expire after it starts"
        );
        self.connection
            .query_row(
                "WITH candidate AS (
                    SELECT rowid FROM work_queue
                    WHERE lease_expires_at IS NULL OR lease_expires_at <= ?1
                    ORDER BY reason_rank, tag_priority DESC, enqueued_at, source_id, stage
                    LIMIT 1
                )
                UPDATE work_queue
                SET lease_owner = ?2, lease_expires_at = ?3
                WHERE rowid = (SELECT rowid FROM candidate)
                RETURNING source_id, stage, reason_rank, tag_priority",
                params![now, lease_owner, lease_expires_at],
                |row| {
                    Ok(WorkItem {
                        source_id: row.get(0)?,
                        stage: row.get(1)?,
                        reason: QueueReason::from_rank(row.get(2)?).map_err(to_sqlite_error)?,
                        tag_priority: row.get(3)?,
                    })
                },
            )
            .optional()
            .context("claim local registry work")
    }

    /// Return queued local work in the same deterministic order used for leases.
    pub fn queued_work(&self) -> anyhow::Result<Vec<WorkItem>> {
        let mut statement = self.connection.prepare(
            "SELECT source_id, stage, reason_rank, tag_priority
             FROM work_queue
             ORDER BY reason_rank, tag_priority DESC, enqueued_at, source_id, stage",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(WorkItem {
                source_id: row.get(0)?,
                stage: row.get(1)?,
                reason: QueueReason::from_rank(row.get(2)?).map_err(to_sqlite_error)?,
                tag_priority: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("read local registry work queue")
    }

    /// Finish a work lease; retrying is explicit through `enqueue`.
    pub fn complete(&self, item: &WorkItem, lease_owner: &str) -> anyhow::Result<()> {
        let removed = self.connection.execute(
            "DELETE FROM work_queue WHERE source_id = ?1 AND stage = ?2 AND lease_owner = ?3",
            params![item.source_id, item.stage, lease_owner],
        )?;
        ensure!(
            removed == 1,
            "local registry work lease is no longer owned by caller"
        );
        Ok(())
    }
}

fn ensure_source_scan_binding_columns(connection: &Connection) -> anyhow::Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(source_scan)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|column| column == "origin_id") {
        connection.execute_batch("ALTER TABLE source_scan ADD COLUMN origin_id TEXT;")?;
    }
    if !columns.iter().any(|column| column == "relative_path") {
        connection.execute_batch("ALTER TABLE source_scan ADD COLUMN relative_path TEXT;")?;
    }
    Ok(())
}

/// Scan one source below a locally bound origin without retaining its bytes.
/// A changing or unsafe input is an error and cannot be published as a capture.
pub fn scan_bound_file(
    origin_root: &Path,
    relative_path: &str,
) -> anyhow::Result<SourceObservation> {
    let (root, candidate, resolved) = match resolve_bound_source(origin_root, relative_path)? {
        BoundSource::Available {
            root,
            candidate,
            resolved,
        } => (root, candidate, resolved),
        BoundSource::Unavailable(availability) => return Ok(unavailable(availability)),
    };
    let unsafe_input = || anyhow::anyhow!("local registry source changed or is unsafe");
    let mut file =
        crate::detect::open_control_file_nofollow(&candidate).map_err(|_| unsafe_input())?;
    let opened = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    let before = file.metadata().map_err(|_| unsafe_input())?;
    let mut digest = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .context("read local registry source")?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        length = length
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .context("local registry source length overflow")?;
    }
    let after = file.metadata().map_err(|_| unsafe_input())?;
    let after_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    let current =
        crate::detect::open_control_file_nofollow(&candidate).map_err(|_| unsafe_input())?;
    let current_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&current)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    ensure!(
        opened == after_identity
            && opened == current_identity
            && before.len() == length
            && after.len() == length
            && fs::canonicalize(&candidate).ok().as_deref() == Some(resolved.as_path())
            && fs::canonicalize(origin_root).ok().as_deref() == Some(root.as_path()),
        "local registry source changed or is unsafe"
    );
    let (mtime_ns, ctime_ns) = metadata_times(&after);
    Ok(SourceObservation {
        availability: Availability::Available,
        size_bytes: Some(length),
        mtime_ns,
        ctime_ns,
        sha256: Some(hex::encode(digest.finalize())),
    })
}

/// Read stat evidence for a bound source without reading its content.
pub fn stat_bound_file(
    origin_root: &Path,
    relative_path: &str,
) -> anyhow::Result<SourceObservation> {
    let (root, candidate, resolved) = match resolve_bound_source(origin_root, relative_path)? {
        BoundSource::Available {
            root,
            candidate,
            resolved,
        } => (root, candidate, resolved),
        BoundSource::Unavailable(availability) => return Ok(unavailable(availability)),
    };
    let unsafe_input = || anyhow::anyhow!("local registry source changed or is unsafe");
    let file = crate::detect::open_control_file_nofollow(&candidate).map_err(|error| {
        if matches!(error.kind(), std::io::ErrorKind::NotFound) {
            anyhow::anyhow!("local registry source changed or is unsafe")
        } else {
            unsafe_input()
        }
    })?;
    let opened = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    let metadata = file.metadata().map_err(|_| unsafe_input())?;
    let current =
        crate::detect::open_control_file_nofollow(&candidate).map_err(|_| unsafe_input())?;
    let current = graphoxide_index_runtime::validate_opened_regular_single_link(&current)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    ensure!(
        opened == current
            && opened.length_bytes() == metadata.len()
            && fs::canonicalize(&candidate).ok().as_deref() == Some(resolved.as_path())
            && fs::canonicalize(origin_root).ok().as_deref() == Some(root.as_path()),
        "local registry source changed or is unsafe"
    );
    let (mtime_ns, ctime_ns) = metadata_times(&metadata);
    Ok(SourceObservation {
        availability: Availability::Available,
        size_bytes: Some(metadata.len()),
        mtime_ns,
        ctime_ns,
        sha256: None,
    })
}

enum BoundSource {
    Available {
        root: PathBuf,
        candidate: PathBuf,
        resolved: PathBuf,
    },
    Unavailable(Availability),
}

fn resolve_bound_source(origin_root: &Path, relative_path: &str) -> anyhow::Result<BoundSource> {
    ensure!(
        crate::project_path::normalize_project_path(relative_path).as_deref()
            == Some(relative_path)
            && !relative_path.is_empty(),
        "local registry source path must be normalized and relative"
    );
    let root_metadata = fs::symlink_metadata(origin_root)
        .with_context(|| format!("inspect local origin root {}", origin_root.display()))?;
    ensure!(
        root_metadata.file_type().is_dir() && !root_metadata.file_type().is_symlink(),
        "local registry origin root must be a non-symlinked directory"
    );
    let root = fs::canonicalize(origin_root)
        .with_context(|| format!("resolve local origin root {}", origin_root.display()))?;
    let candidate = root.join(relative_path);
    let resolved = match fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BoundSource::Unavailable(Availability::Missing));
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(BoundSource::Unavailable(Availability::Inaccessible));
        }
        Err(error) => return Err(error).context("resolve local registry source"),
    };
    ensure!(
        resolved.starts_with(&root) && resolved.is_file(),
        "local registry source escaped its bound origin or is not a file"
    );
    Ok(BoundSource::Available {
        root,
        candidate,
        resolved,
    })
}

fn unavailable(availability: Availability) -> SourceObservation {
    SourceObservation {
        availability,
        size_bytes: None,
        mtime_ns: None,
        ctime_ns: None,
        sha256: None,
    }
}

#[cfg(unix)]
fn metadata_times(metadata: &fs::Metadata) -> (Option<i64>, Option<i64>) {
    use std::os::unix::fs::MetadataExt as _;

    (
        seconds_and_nanos(metadata.mtime(), metadata.mtime_nsec()),
        seconds_and_nanos(metadata.ctime(), metadata.ctime_nsec()),
    )
}

#[cfg(not(unix))]
fn metadata_times(metadata: &fs::Metadata) -> (Option<i64>, Option<i64>) {
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok());
    (mtime, None)
}

fn seconds_and_nanos(seconds: i64, nanos: i64) -> Option<i64> {
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(nanos))
}

fn validate_id(name: &str, value: &str) -> anyhow::Result<()> {
    ensure!(
        value.len() <= 256
            && value.as_bytes().split_first().is_some_and(|(first, rest)| {
                first.is_ascii_alphanumeric()
                    && rest.iter().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            }),
        "local registry {name} must be a bounded identifier"
    );
    Ok(())
}

fn validate_relative_path(value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && crate::project_path::normalize_project_path(value).as_deref() == Some(value),
        "local registry source path must be normalized and relative"
    );
    Ok(())
}

fn validate_metadata(name: &str, value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 4_096 && !value.chars().any(char::is_control),
        "local registry {name} must be bounded metadata"
    );
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn to_i64(value: u64) -> anyhow::Result<i64> {
    i64::try_from(value).context("local source size exceeds SQLite integer range")
}

fn to_u64(value: i64) -> anyhow::Result<u64> {
    u64::try_from(value).context("local source size is negative")
}

fn to_sqlite_error(error: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(error.to_string())),
    )
}
