//! Append-only, I/O-owner cache artifacts for the isolated indexing runtime.
//!
//! This module deliberately owns paths and filesystem operations. It is not
//! used by parser-facing [`crate::ReadyInput`] values: an I/O worker opens one
//! [`RuntimeCache`] for the duration of its assigned cache partitions, does
//! lookup and persistence there, and passes only ready bytes or decoded work
//! to CPU workers. The service has no global lock and is intentionally
//! `!Sync`, so sharing it between I/O owners requires an explicit routing
//! layer instead of an accidental mutex.
//!
//! Runtime-v1 is additive. Callers can use [`RuntimeCache::get_or_legacy`] to
//! retain the existing JSON cache as a read-only, fail-open fallback while
//! new framed artifacts are populated. The service never removes or mutates a
//! legacy cache entry.

use std::{
    cell::Cell,
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender},
    thread::{self, JoinHandle, ThreadId},
};

/// Number of stable logical cache partitions.
pub const RUNTIME_CACHE_SHARDS: usize = 64;
/// Largest artifact payload accepted by the default runtime cache.
pub const DEFAULT_RUNTIME_CACHE_MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum bytes retained in one append-only artifact segment.
pub const DEFAULT_RUNTIME_CACHE_SEGMENT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum bytes read from one append-only catalog during cache open.
pub const DEFAULT_RUNTIME_CACHE_MAX_CATALOG_BYTES: usize = 64 * 1024 * 1024;
/// Maximum aggregate catalog bytes read across all cache shards.
pub const DEFAULT_RUNTIME_CACHE_MAX_TOTAL_CATALOG_BYTES: usize = 64 * 1024 * 1024;

const RUNTIME_CACHE_DIRECTORY: &str = "cache/runtime-v1";
const CATALOG_FILE: &str = "catalog.gxi";
const ACTIVE_PREFIX: &str = "active-";
const ACTIVE_SUFFIX: &str = ".gxa";
const SEALED_PREFIX: &str = "sealed-";
const FRAME_MAGIC: [u8; 8] = *b"GOXCACHE";
const FRAME_VERSION: u8 = 1;
const FRAME_ALGORITHM_BLAKE3: u8 = 1;
const FRAME_HEADER_LEN: usize = 56;
const CATALOG_RECORD_VERSION: u8 = 1;
const CATALOG_RECORD_LEN: usize = 1 + 32 + 8 + 8 + 8 + 32;
const CATALOG_FRAME_LEN: usize = FRAME_HEADER_LEN + CATALOG_RECORD_LEN;

/// Bounded number of control-plane cache commands retained ahead of the
/// dedicated cache I/O owner.
///
/// The caller of [`RuntimeCacheIoService`] is the control plane, not a CPU
/// extractor. A small bounded queue prevents a large extraction result from
/// retaining an unbounded second copy while cache frames are appended.
pub const DEFAULT_RUNTIME_CACHE_IO_QUEUE_CAPACITY: usize = 8;

/// A deterministic, opaque runtime cache key.
///
/// Keys are content-addressed by callers. [`RuntimeCacheKey::for_bytes`]
/// supplies the standard domain-separated construction for byte extractors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuntimeCacheKey([u8; 32]);

impl RuntimeCacheKey {
    /// Construct a key from a caller-owned digest.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Build a domain-separated key from stable extractor identity, normalized
    /// source path, and source bytes. No filesystem operation is performed.
    #[must_use]
    pub fn for_bytes(namespace: &str, normalized_path: &str, source: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"graphoxide-runtime-cache-v1\0");
        hasher.update(namespace.as_bytes());
        hasher.update(b"\0");
        hasher.update(normalized_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(source);
        Self(*hasher.finalize().as_bytes())
    }

    /// Build a key with an explicit extractor-schema version in addition to
    /// the normal domain separator. Bumping a parser's schema version cannot
    /// replay an older runtime artifact under this construction.
    #[must_use]
    pub fn for_versioned_bytes(
        namespace: &str,
        schema_version: u32,
        normalized_path: &str,
        source: &[u8],
    ) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"graphoxide-runtime-cache-v1\0");
        hasher.update(namespace.as_bytes());
        hasher.update(b"\0");
        hasher.update(&schema_version.to_le_bytes());
        hasher.update(b"\0");
        hasher.update(normalized_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(source);
        Self(*hasher.finalize().as_bytes())
    }

    /// Return the raw digest for diagnostics or stable partition routing.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Where a successful cache payload originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeCacheSource {
    /// A validated framed runtime-v1 artifact.
    RuntimeV1,
    /// Caller-supplied legacy read-only fallback.
    Legacy,
}

/// A validated cache payload and its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCacheHit {
    /// Raw payload bytes. Decoding belongs to the CPU-side consumer.
    pub payload: Vec<u8>,
    /// Cache tier that supplied the payload.
    pub source: RuntimeCacheSource,
}

/// Cache-service failures that are actionable for an I/O owner.
///
/// Corrupt or truncated on-disk records are intentionally *not* returned as
/// errors from [`RuntimeCache::get`]. They are ordinary fail-open cache misses.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeCacheError {
    /// The configured size bounds cannot represent a valid framed artifact.
    #[error("runtime cache limits must be non-zero and segment size must fit a frame")]
    InvalidLimits,
    /// An I/O operation needed to create, append, rotate, or synchronize the
    /// new runtime cache failed.
    #[error("runtime cache I/O failed: {0}")]
    Io(#[from] io::Error),
    /// A caller could not encode a cacheable CPU result.
    #[error("runtime cache payload encoding failed: {0}")]
    Encode(String),
    /// A caller attempted to cache an artifact larger than the configured
    /// bounded payload policy.
    #[error("runtime cache payload is {payload_bytes} bytes, exceeding {max_payload_bytes} bytes")]
    PayloadTooLarge {
        /// Attempted payload bytes.
        payload_bytes: usize,
        /// Configured bound.
        max_payload_bytes: usize,
    },
    /// Appending another framed catalog record would exceed the configured
    /// bounded catalog policy.
    #[error("runtime cache catalog would exceed {max_catalog_bytes} bytes")]
    CatalogTooLarge {
        /// Configured bound.
        max_catalog_bytes: usize,
    },
    /// Existing or newly appended catalogs exceed the aggregate cache budget.
    #[error(
        "runtime cache catalogs require {catalog_bytes} bytes, exceeding the aggregate {max_catalog_bytes}-byte limit"
    )]
    AggregateCatalogTooLarge {
        /// Existing or attempted aggregate catalog bytes.
        catalog_bytes: u64,
        /// Configured aggregate bound.
        max_catalog_bytes: usize,
    },
    /// A cache-owned path was a symlink or a non-regular path type.
    #[error("runtime cache refuses unsafe owned path {}", path.display())]
    UnsafePath {
        /// Path that failed containment validation.
        path: PathBuf,
    },
}

/// Failure while communicating with the dedicated runtime-cache I/O owner.
///
/// A cache worker failure is intentionally distinct from an artifact failure:
/// callers can surface a diagnostic and continue indexing without allowing a
/// cache problem to change graph output.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeCacheIoServiceError {
    /// A bounded control-to-I/O handoff needs at least one command slot.
    #[error("runtime cache I/O queue capacity must be non-zero")]
    InvalidQueueCapacity,
    /// Opening or operating the cache on its I/O owner failed.
    #[error(transparent)]
    Cache(#[from] RuntimeCacheError),
    /// The local dedicated cache I/O thread could not be started.
    #[error("could not start runtime cache I/O owner: {0}")]
    WorkerSpawn(#[source] io::Error),
    /// The dedicated cache I/O owner stopped before replying.
    #[error("runtime cache I/O owner is unavailable")]
    WorkerUnavailable,
    /// The dedicated cache I/O owner panicked while processing a command.
    #[error("runtime cache I/O owner panicked")]
    WorkerPanicked,
}

/// Result of a cache probe performed by the dedicated I/O owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCacheIoProbe {
    /// A validated runtime-v1 artifact, if one exists.
    pub hit: Option<RuntimeCacheHit>,
    /// Thread that performed the filesystem probe. This is intentionally
    /// exposed for ownership-boundary tests and diagnostics only.
    pub io_thread_id: ThreadId,
}

/// Result of an append request performed by the dedicated cache I/O owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeCacheIoPersistOutcome {
    /// The worker appended a new framed cache artifact and catalog record.
    Stored { io_thread_id: ThreadId },
    /// The worker found a validated artifact for this key and deliberately
    /// left the existing deterministic record untouched.
    AlreadyPresent { io_thread_id: ThreadId },
}

enum RuntimeCacheIoCommand {
    Probe {
        key: RuntimeCacheKey,
        response: SyncSender<Result<RuntimeCacheIoProbe, RuntimeCacheError>>,
    },
    PersistIfAbsent {
        key: RuntimeCacheKey,
        payload: Vec<u8>,
        response: SyncSender<Result<RuntimeCacheIoPersistOutcome, RuntimeCacheError>>,
    },
}

/// Control-plane handle for one dedicated runtime-v1 cache I/O owner.
///
/// The worker creates, probes, and persists [`RuntimeCache`] artifacts. The
/// handle is deliberately `!Sync`, so a `read_files_concurrently` CPU closure
/// (which must be `Sync`) cannot accidentally capture the configured cache
/// service. CPU extractors therefore remain byte-only; the control plane may
/// submit completed artifacts after extraction without running filesystem
/// operations itself.
///
/// Commands are bounded and ordered by the single control-plane producer.
/// This is an SPSC handoff: no global cache mutex, worker stealing, or CPU
/// extractor I/O is introduced.
pub struct RuntimeCacheIoService {
    sender: Option<SyncSender<RuntimeCacheIoCommand>>,
    worker: Option<JoinHandle<()>>,
    _control_plane_only: PhantomData<Cell<()>>,
}

impl RuntimeCacheIoService {
    /// Start a dedicated cache I/O owner below `output_dir`.
    ///
    /// Opening the cache happens in the spawned owner rather than on the
    /// caller's thread. A zero queue capacity is rejected because it could
    /// make control-plane batching unable to make progress.
    pub fn start(
        output_dir: PathBuf,
        queue_capacity: usize,
    ) -> Result<Self, RuntimeCacheIoServiceError> {
        Self::start_with_limits(output_dir, queue_capacity, RuntimeCacheLimits::default())
    }

    /// Start a dedicated cache owner with bounds derived by the control plane.
    pub fn start_with_limits(
        output_dir: PathBuf,
        queue_capacity: usize,
        limits: RuntimeCacheLimits,
    ) -> Result<Self, RuntimeCacheIoServiceError> {
        if queue_capacity == 0 {
            return Err(RuntimeCacheIoServiceError::InvalidQueueCapacity);
        }
        limits.validate()?;
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
        let worker = thread::Builder::new()
            .name("graphoxide-runtime-cache-io".into())
            .spawn(move || {
                let mut cache = match RuntimeCache::open_with_limits(&output_dir, limits) {
                    Ok(cache) => {
                        // If the control plane went away while the worker was
                        // starting, there is no work to own.
                        if ready_sender.send(Ok(())).is_err() {
                            return;
                        }
                        cache
                    }
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                while let Ok(command) = receiver.recv() {
                    let io_thread_id = thread::current().id();
                    match command {
                        RuntimeCacheIoCommand::Probe { key, response } => {
                            let _ = response.send(Ok(RuntimeCacheIoProbe {
                                hit: cache.get(key),
                                io_thread_id,
                            }));
                        }
                        RuntimeCacheIoCommand::PersistIfAbsent {
                            key,
                            payload,
                            response,
                        } => {
                            let outcome = if cache.get(key).is_some() {
                                Ok(RuntimeCacheIoPersistOutcome::AlreadyPresent { io_thread_id })
                            } else {
                                cache
                                    .put(key, &payload)
                                    .map(|()| RuntimeCacheIoPersistOutcome::Stored { io_thread_id })
                            };
                            let _ = response.send(outcome);
                        }
                    }
                }
            })
            .map_err(RuntimeCacheIoServiceError::WorkerSpawn)?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                sender: Some(sender),
                worker: Some(worker),
                _control_plane_only: PhantomData,
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(RuntimeCacheIoServiceError::Cache(error))
            }
            Err(_) => {
                let _ = worker.join();
                Err(RuntimeCacheIoServiceError::WorkerUnavailable)
            }
        }
    }

    /// Probe a framed runtime-v1 artifact on the dedicated I/O owner.
    ///
    /// This method may wait only on the calling control plane. It must not be
    /// called from a CPU extractor closure; the handle's `!Sync` contract
    /// prevents that accidental capture.
    pub fn probe(
        &self,
        key: RuntimeCacheKey,
    ) -> Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(0);
        self.send(RuntimeCacheIoCommand::Probe {
            key,
            response: response_sender,
        })?;
        response_receiver
            .recv()
            .map_err(|_| RuntimeCacheIoServiceError::WorkerUnavailable)?
            .map_err(RuntimeCacheIoServiceError::Cache)
    }

    /// Probe and append a cache artifact on the dedicated I/O owner.
    ///
    /// If a valid artifact already exists, the worker leaves it untouched.
    /// That combines the cache probe and persistence in one owner-serialized
    /// command and avoids redundant segment growth during forced scans.
    pub fn persist_if_absent(
        &self,
        key: RuntimeCacheKey,
        payload: Vec<u8>,
    ) -> Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(0);
        self.send(RuntimeCacheIoCommand::PersistIfAbsent {
            key,
            payload,
            response: response_sender,
        })?;
        response_receiver
            .recv()
            .map_err(|_| RuntimeCacheIoServiceError::WorkerUnavailable)?
            .map_err(RuntimeCacheIoServiceError::Cache)
    }

    fn send(&self, command: RuntimeCacheIoCommand) -> Result<(), RuntimeCacheIoServiceError> {
        self.sender
            .as_ref()
            .ok_or(RuntimeCacheIoServiceError::WorkerUnavailable)?
            .send(command)
            .map_err(|_| RuntimeCacheIoServiceError::WorkerUnavailable)
    }

    /// Shut down the dedicated cache owner after all submitted commands have
    /// completed. Dropping the sole sender drains ordered work and lets the
    /// worker exit without a separate blocking shutdown command.
    pub fn shutdown(mut self) -> Result<(), RuntimeCacheIoServiceError> {
        drop(self.sender.take());
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| RuntimeCacheIoServiceError::WorkerPanicked)
    }
}

/// Settings used to bound one I/O-owned runtime cache service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCacheLimits {
    /// Maximum decoded artifact payload size.
    pub max_artifact_bytes: usize,
    /// Maximum framed bytes stored in one active artifact segment.
    pub max_segment_bytes: usize,
    /// Maximum catalog bytes read or appended per shard.
    pub max_catalog_bytes: usize,
    /// Maximum catalog bytes read or appended across all 64 shards.
    pub max_total_catalog_bytes: usize,
}

impl Default for RuntimeCacheLimits {
    fn default() -> Self {
        Self {
            max_artifact_bytes: DEFAULT_RUNTIME_CACHE_MAX_ARTIFACT_BYTES,
            max_segment_bytes: DEFAULT_RUNTIME_CACHE_SEGMENT_BYTES,
            max_catalog_bytes: DEFAULT_RUNTIME_CACHE_MAX_CATALOG_BYTES,
            max_total_catalog_bytes: DEFAULT_RUNTIME_CACHE_MAX_TOTAL_CATALOG_BYTES,
        }
    }
}

impl RuntimeCacheLimits {
    /// Divide a control-plane cache/run memory partition between one decoded
    /// artifact and aggregate catalog indexes.
    ///
    /// Runtime-v1 is not active in the production scan until read-through is
    /// implemented, but any caller that enables it must derive limits from the
    /// same explicit partition rather than accepting fixed per-shard maxima.
    #[must_use]
    pub fn for_memory_budget(memory_budget_bytes: usize) -> Self {
        let catalog_budget = memory_budget_bytes / 2;
        let artifact_budget = memory_budget_bytes.saturating_sub(catalog_budget);
        Self {
            max_artifact_bytes: artifact_budget
                .saturating_sub(FRAME_HEADER_LEN)
                .min(DEFAULT_RUNTIME_CACHE_MAX_ARTIFACT_BYTES),
            max_segment_bytes: artifact_budget.min(DEFAULT_RUNTIME_CACHE_SEGMENT_BYTES),
            max_catalog_bytes: catalog_budget.min(DEFAULT_RUNTIME_CACHE_MAX_CATALOG_BYTES),
            max_total_catalog_bytes: catalog_budget
                .min(DEFAULT_RUNTIME_CACHE_MAX_TOTAL_CATALOG_BYTES),
        }
    }

    fn validate(self) -> Result<(), RuntimeCacheError> {
        if self.max_artifact_bytes == 0
            || self.max_segment_bytes < FRAME_HEADER_LEN
            || self.max_catalog_bytes < CATALOG_FRAME_LEN
            || self.max_total_catalog_bytes < CATALOG_FRAME_LEN
            || self.max_catalog_bytes > self.max_total_catalog_bytes
        {
            return Err(RuntimeCacheError::InvalidLimits);
        }
        Ok(())
    }
}

/// I/O-owned cache service backed by 64 append-only catalog/artifact shards.
///
/// `RuntimeCache` contains `Cell` marker state to make it `!Sync`: a cache
/// partition has exactly one writer. It is still movable between I/O workers
/// before work begins, but concurrent callers must route each key to its
/// owner rather than placing this service behind a shared lock.
pub struct RuntimeCache {
    root: PathBuf,
    limits: RuntimeCacheLimits,
    shards: Vec<ShardState>,
    total_catalog_len: u64,
    _io_owner_only: PhantomData<Cell<()>>,
}

#[derive(Debug)]
struct ShardState {
    entries: BTreeMap<RuntimeCacheKey, ArtifactLocation>,
    active_generation: u64,
    active_len: u64,
    catalog_len: u64,
}

#[derive(Debug, Clone, Copy)]
struct ArtifactLocation {
    generation: u64,
    offset: u64,
    frame_len: u64,
    payload_digest: [u8; 32],
}

impl RuntimeCache {
    /// Open (and, if necessary, create) runtime-v1 below an output directory.
    ///
    /// This is an I/O-plane operation. It creates all 64 logical shard
    /// directories eagerly so layout and partition projection are stable even
    /// for an empty project.
    pub fn open(output_dir: &Path) -> Result<Self, RuntimeCacheError> {
        Self::open_with_limits(output_dir, RuntimeCacheLimits::default())
    }

    /// Open runtime-v1 with explicit test or deployment bounds.
    pub fn open_with_limits(
        output_dir: &Path,
        limits: RuntimeCacheLimits,
    ) -> Result<Self, RuntimeCacheError> {
        limits.validate()?;
        fs::create_dir_all(output_dir)?;
        let output_dir = fs::canonicalize(output_dir)?;
        let root = ensure_owned_directory_path(&output_dir, Path::new(RUNTIME_CACHE_DIRECTORY))?;
        let mut shards = Vec::with_capacity(RUNTIME_CACHE_SHARDS);
        let mut total_catalog_len = 0u64;
        for index in 0..RUNTIME_CACHE_SHARDS {
            let shard_dir = ensure_owned_directory_path(
                &root,
                &Path::new("shards").join(format!("{index:02x}")),
            )?;
            let existing_catalog_len =
                safe_regular_file_len(&shard_dir.join(CATALOG_FILE))?.unwrap_or_default();
            let prospective_catalog_len = total_catalog_len
                .checked_add(existing_catalog_len)
                .ok_or(RuntimeCacheError::AggregateCatalogTooLarge {
                    catalog_bytes: u64::MAX,
                    max_catalog_bytes: limits.max_total_catalog_bytes,
                })?;
            if prospective_catalog_len > limits.max_total_catalog_bytes as u64 {
                return Err(RuntimeCacheError::AggregateCatalogTooLarge {
                    catalog_bytes: prospective_catalog_len,
                    max_catalog_bytes: limits.max_total_catalog_bytes,
                });
            }
            let catalog = read_catalog(&shard_dir, limits)?;
            if catalog.repair_tail {
                truncate_catalog_tail(&shard_dir.join(CATALOG_FILE), catalog.len)?;
            }
            total_catalog_len = total_catalog_len.saturating_add(if catalog.repair_tail {
                catalog.len
            } else {
                existing_catalog_len
            });
            let (active_generation, active_len) = discover_active_segment(&shard_dir)?;
            shards.push(ShardState {
                entries: catalog.entries,
                active_generation,
                active_len,
                catalog_len: catalog.len,
            });
        }
        Ok(Self {
            root,
            limits,
            shards,
            total_catalog_len,
            _io_owner_only: PhantomData,
        })
    }

    /// Return the stable cache shard that owns `key`.
    #[must_use]
    pub fn shard_for_key(key: RuntimeCacheKey) -> usize {
        usize::from(key.0[0] & 63)
    }

    /// Return the on-disk runtime-v1 root for diagnostics and tests.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read a validated framed runtime artifact. Corrupt, missing, truncated,
    /// oversize, or checksum-mismatched records return `None` as a cache miss.
    #[must_use]
    pub fn get(&self, key: RuntimeCacheKey) -> Option<RuntimeCacheHit> {
        let shard = Self::shard_for_key(key);
        let location = *self.shards.get(shard)?.entries.get(&key)?;
        let path = artifact_path(
            &self.root,
            shard,
            location.generation,
            self.shards[shard].active_generation,
        );
        let frame = read_artifact_frame(&path, location, self.limits.max_artifact_bytes)?;
        Some(RuntimeCacheHit {
            payload: frame,
            source: RuntimeCacheSource::RuntimeV1,
        })
    }

    /// Read runtime-v1 first, then invoke an explicitly supplied legacy
    /// reader on a miss. The closure is the only legacy integration point and
    /// this method never mutates the legacy cache.
    #[must_use]
    pub fn get_or_legacy<F>(&self, key: RuntimeCacheKey, legacy: F) -> Option<RuntimeCacheHit>
    where
        F: FnOnce() -> Option<Vec<u8>>,
    {
        self.get(key).or_else(|| {
            legacy().map(|payload| RuntimeCacheHit {
                payload,
                source: RuntimeCacheSource::Legacy,
            })
        })
    }

    /// Append a bounded framed payload and a separately framed catalog record.
    ///
    /// The artifact file is synchronized before its catalog record is made
    /// durable. A crash can therefore leave an unreferenced artifact, but not
    /// a trusted catalog reference to an artifact that was not flushed first.
    pub fn put(&mut self, key: RuntimeCacheKey, payload: &[u8]) -> Result<(), RuntimeCacheError> {
        if payload.len() > self.limits.max_artifact_bytes {
            return Err(RuntimeCacheError::PayloadTooLarge {
                payload_bytes: payload.len(),
                max_payload_bytes: self.limits.max_artifact_bytes,
            });
        }
        let artifact_frame = frame(payload);
        let artifact_frame_len = u64::try_from(artifact_frame.len()).map_err(|_| {
            RuntimeCacheError::PayloadTooLarge {
                payload_bytes: payload.len(),
                max_payload_bytes: self.limits.max_artifact_bytes,
            }
        })?;
        if artifact_frame_len > self.limits.max_segment_bytes as u64 {
            return Err(RuntimeCacheError::PayloadTooLarge {
                payload_bytes: payload.len(),
                max_payload_bytes: self
                    .limits
                    .max_segment_bytes
                    .saturating_sub(FRAME_HEADER_LEN),
            });
        }

        let shard = Self::shard_for_key(key);
        let shard_dir = shard_directory(&self.root, shard);
        let state = &mut self.shards[shard];
        let catalog_frame_len =
            u64::try_from(CATALOG_FRAME_LEN).expect("catalog frame length fits u64");
        if state.catalog_len.saturating_add(catalog_frame_len)
            > self.limits.max_catalog_bytes as u64
        {
            return Err(RuntimeCacheError::CatalogTooLarge {
                max_catalog_bytes: self.limits.max_catalog_bytes,
            });
        }
        let next_total_catalog_len = self
            .total_catalog_len
            .checked_add(catalog_frame_len)
            .ok_or(RuntimeCacheError::AggregateCatalogTooLarge {
                catalog_bytes: u64::MAX,
                max_catalog_bytes: self.limits.max_total_catalog_bytes,
            })?;
        if next_total_catalog_len > self.limits.max_total_catalog_bytes as u64 {
            return Err(RuntimeCacheError::AggregateCatalogTooLarge {
                catalog_bytes: next_total_catalog_len,
                max_catalog_bytes: self.limits.max_total_catalog_bytes,
            });
        }
        if state.active_len > 0
            && state.active_len.saturating_add(artifact_frame_len)
                > self.limits.max_segment_bytes as u64
        {
            seal_active_segment(&shard_dir, state.active_generation)?;
            state.active_generation = state.active_generation.saturating_add(1);
            state.active_len = 0;
        }

        let artifact_path = active_artifact_path(&shard_dir, state.active_generation);
        let offset = state.active_len;
        append_durable(&artifact_path, &artifact_frame)?;
        let location = ArtifactLocation {
            generation: state.active_generation,
            offset,
            frame_len: artifact_frame_len,
            payload_digest: *blake3::hash(payload).as_bytes(),
        };
        let catalog_frame = frame(&encode_catalog_record(key, location));
        debug_assert_eq!(catalog_frame.len() as u64, catalog_frame_len);
        append_durable(&shard_dir.join(CATALOG_FILE), &catalog_frame)?;
        state.catalog_len = state.catalog_len.saturating_add(catalog_frame_len);
        self.total_catalog_len = next_total_catalog_len;
        state.active_len = state.active_len.saturating_add(artifact_frame_len);
        state.entries.insert(key, location);
        Ok(())
    }
}

fn ensure_owned_directory_path(base: &Path, relative: &Path) -> Result<PathBuf, RuntimeCacheError> {
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(RuntimeCacheError::UnsafePath {
                path: base.join(relative),
            });
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(RuntimeCacheError::UnsafePath {
                    path: current.clone(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                let metadata = fs::symlink_metadata(&current)?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(RuntimeCacheError::UnsafePath {
                        path: current.clone(),
                    });
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

fn safe_regular_file_len(path: &Path) -> Result<Option<u64>, RuntimeCacheError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(RuntimeCacheError::UnsafePath {
                path: path.to_path_buf(),
            })
        }
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn shard_directory(root: &Path, shard: usize) -> PathBuf {
    root.join("shards").join(format!("{shard:02x}"))
}

fn active_artifact_path(shard_dir: &Path, generation: u64) -> PathBuf {
    shard_dir.join(format!("{ACTIVE_PREFIX}{generation}{ACTIVE_SUFFIX}"))
}

fn sealed_artifact_path(shard_dir: &Path, generation: u64) -> PathBuf {
    shard_dir.join(format!("{SEALED_PREFIX}{generation}{ACTIVE_SUFFIX}"))
}

fn artifact_path(root: &Path, shard: usize, generation: u64, active_generation: u64) -> PathBuf {
    let shard_dir = shard_directory(root, shard);
    if generation == active_generation {
        active_artifact_path(&shard_dir, generation)
    } else {
        sealed_artifact_path(&shard_dir, generation)
    }
}

fn discover_active_segment(shard_dir: &Path) -> Result<(u64, u64), RuntimeCacheError> {
    let mut selected = None;
    for entry in fs::read_dir(shard_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(generation) = name
            .strip_prefix(ACTIVE_PREFIX)
            .and_then(|value| value.strip_suffix(ACTIVE_SUFFIX))
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_file() {
            return Err(RuntimeCacheError::UnsafePath { path: entry.path() });
        }
        let length = entry.metadata()?.len();
        if selected.is_none_or(|(current, _)| generation > current) {
            selected = Some((generation, length));
        }
    }
    Ok(selected.unwrap_or((0, 0)))
}

fn seal_active_segment(shard_dir: &Path, generation: u64) -> Result<(), RuntimeCacheError> {
    let active = active_artifact_path(shard_dir, generation);
    if safe_regular_file_len(&active)?.is_none() {
        return Ok(());
    }
    let sealed = sealed_artifact_path(shard_dir, generation);
    if fs::symlink_metadata(&sealed).is_ok() {
        return Err(RuntimeCacheError::UnsafePath { path: sealed });
    }
    fs::rename(active, sealed)?;
    Ok(())
}

fn append_durable(path: &Path, bytes: &[u8]) -> Result<(), RuntimeCacheError> {
    let _ = safe_regular_file_len(path)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_data()?;
    Ok(())
}

struct CatalogLoad {
    entries: BTreeMap<RuntimeCacheKey, ArtifactLocation>,
    len: u64,
    repair_tail: bool,
}

fn read_catalog(
    shard_dir: &Path,
    limits: RuntimeCacheLimits,
) -> Result<CatalogLoad, RuntimeCacheError> {
    let path = shard_dir.join(CATALOG_FILE);
    let Some(metadata_len) = safe_regular_file_len(&path)? else {
        return Ok(CatalogLoad {
            entries: BTreeMap::new(),
            len: 0,
            repair_tail: false,
        });
    };
    if metadata_len > limits.max_catalog_bytes as u64 {
        return Ok(CatalogLoad {
            entries: BTreeMap::new(),
            // Do not truncate a valid-but-over-policy catalog. Its contents
            // are ignored as a bounded fail-open miss and new records are
            // refused until an explicit cache lifecycle operation replaces it.
            len: limits.max_catalog_bytes as u64 + 1,
            repair_tail: false,
        });
    }
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Ok(CatalogLoad {
                entries: BTreeMap::new(),
                len: limits.max_catalog_bytes as u64 + 1,
                repair_tail: false,
            });
        }
    };
    let mut entries = BTreeMap::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some((payload, consumed)) = unframe_at(&bytes[cursor..], CATALOG_RECORD_LEN) else {
            break;
        };
        let Some((key, location)) = decode_catalog_record(payload) else {
            break;
        };
        entries.insert(key, location);
        cursor = cursor.saturating_add(consumed);
    }
    Ok(CatalogLoad {
        entries,
        len: cursor as u64,
        repair_tail: cursor != bytes.len(),
    })
}

fn truncate_catalog_tail(path: &Path, len: u64) -> Result<(), RuntimeCacheError> {
    safe_regular_file_len(path)?.ok_or_else(|| RuntimeCacheError::UnsafePath {
        path: path.to_path_buf(),
    })?;
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    file.sync_data()?;
    Ok(())
}

fn read_artifact_frame(
    path: &Path,
    location: ArtifactLocation,
    max_payload_bytes: usize,
) -> Option<Vec<u8>> {
    let max_frame_len = FRAME_HEADER_LEN.checked_add(max_payload_bytes)?;
    let frame_len = usize::try_from(location.frame_len).ok()?;
    if frame_len < FRAME_HEADER_LEN || frame_len > max_frame_len {
        return None;
    }
    safe_regular_file_len(path).ok().flatten()?;
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(location.offset)).ok()?;
    let mut bytes = vec![0; frame_len];
    file.read_exact(&mut bytes).ok()?;
    let (payload, consumed) = unframe_at(&bytes, max_payload_bytes)?;
    if consumed != bytes.len() || *blake3::hash(payload).as_bytes() != location.payload_digest {
        return None;
    }
    Some(payload.to_vec())
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    frame.extend_from_slice(&FRAME_MAGIC);
    frame.push(FRAME_VERSION);
    frame.push(FRAME_ALGORITHM_BLAKE3);
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    frame.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    frame.extend_from_slice(blake3::hash(payload).as_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Return a validated frame payload and its byte length. Any malformed or
/// incomplete record is intentionally indistinguishable from a cache miss.
fn unframe_at(bytes: &[u8], max_payload_bytes: usize) -> Option<(&[u8], usize)> {
    if bytes.len() < FRAME_HEADER_LEN || bytes[..8] != FRAME_MAGIC {
        return None;
    }
    if bytes[8] != FRAME_VERSION || bytes[9] != FRAME_ALGORITHM_BLAKE3 || bytes[10..12] != [0, 0] {
        return None;
    }
    let payload_len = usize::try_from(u64::from_le_bytes(bytes[12..20].try_into().ok()?)).ok()?;
    if payload_len > max_payload_bytes {
        return None;
    }
    let frame_len = FRAME_HEADER_LEN.checked_add(payload_len)?;
    if bytes.len() < frame_len {
        return None;
    }
    let expected_crc = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
    let expected_digest = &bytes[24..56];
    let payload = &bytes[FRAME_HEADER_LEN..frame_len];
    if crc32fast::hash(payload) != expected_crc
        || blake3::hash(payload).as_bytes() != expected_digest
    {
        return None;
    }
    Some((payload, frame_len))
}

fn encode_catalog_record(key: RuntimeCacheKey, location: ArtifactLocation) -> Vec<u8> {
    let mut record = Vec::with_capacity(CATALOG_RECORD_LEN);
    record.push(CATALOG_RECORD_VERSION);
    record.extend_from_slice(&key.0);
    record.extend_from_slice(&location.generation.to_le_bytes());
    record.extend_from_slice(&location.offset.to_le_bytes());
    record.extend_from_slice(&location.frame_len.to_le_bytes());
    record.extend_from_slice(&location.payload_digest);
    record
}

fn decode_catalog_record(payload: &[u8]) -> Option<(RuntimeCacheKey, ArtifactLocation)> {
    if payload.len() != CATALOG_RECORD_LEN || payload[0] != CATALOG_RECORD_VERSION {
        return None;
    }
    let key = RuntimeCacheKey(payload[1..33].try_into().ok()?);
    let generation = u64::from_le_bytes(payload[33..41].try_into().ok()?);
    let offset = u64::from_le_bytes(payload[41..49].try_into().ok()?);
    let frame_len = u64::from_le_bytes(payload[49..57].try_into().ok()?);
    let payload_digest = payload[57..89].try_into().ok()?;
    Some((
        key,
        ArtifactLocation {
            generation,
            offset,
            frame_len,
            payload_digest,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(label: &str) -> RuntimeCacheKey {
        RuntimeCacheKey::for_bytes("test", label, label.as_bytes())
    }

    #[test]
    fn opens_all_64_stable_shards_and_round_trips_after_reopen() {
        let temp = tempfile::tempdir().expect("temporary output");
        let mut cache = RuntimeCache::open(temp.path()).expect("open cache");
        for shard in 0..RUNTIME_CACHE_SHARDS {
            assert!(
                shard_directory(cache.root(), shard).is_dir(),
                "shard {shard}"
            );
        }
        let cache_key = key("one");
        cache.put(cache_key, b"one payload").expect("store");
        assert_eq!(cache.get(cache_key).expect("hit").payload, b"one payload");
        drop(cache);

        let reopened = RuntimeCache::open(temp.path()).expect("reopen cache");
        assert_eq!(
            reopened.get(cache_key).expect("persisted hit").payload,
            b"one payload"
        );
        assert_eq!(
            RuntimeCache::shard_for_key(cache_key),
            usize::from(cache_key.as_bytes()[0] & 63)
        );
        assert_ne!(
            RuntimeCacheKey::for_versioned_bytes("ast", 1, "src/lib.rs", b"source"),
            RuntimeCacheKey::for_versioned_bytes("ast", 2, "src/lib.rs", b"source"),
            "schema version participates in runtime cache eligibility"
        );
    }

    #[test]
    fn catalogs_and_artifacts_are_framed_and_segment_rotation_preserves_hits() {
        let temp = tempfile::tempdir().expect("temporary output");
        let limits = RuntimeCacheLimits {
            max_artifact_bytes: 128,
            max_segment_bytes: 70,
            max_catalog_bytes: 4096,
            max_total_catalog_bytes: 4096,
        };
        let mut cache = RuntimeCache::open_with_limits(temp.path(), limits).expect("open");
        let first = key("first");
        let second = (0..1024)
            .map(|index| key(&format!("second-{index}")))
            .find(|candidate| {
                RuntimeCache::shard_for_key(*candidate) == RuntimeCache::shard_for_key(first)
            })
            .expect("same-shard second key");
        cache.put(first, b"one").expect("first store");
        cache.put(second, b"two").expect("second store");
        let first_shard = RuntimeCache::shard_for_key(first);
        let first_catalog = fs::read(shard_directory(cache.root(), first_shard).join(CATALOG_FILE))
            .expect("catalog bytes");
        assert!(first_catalog.starts_with(&FRAME_MAGIC));
        assert!(sealed_artifact_path(&shard_directory(cache.root(), first_shard), 0).is_file());
        assert_eq!(cache.get(first).expect("sealed hit").payload, b"one");
        assert_eq!(cache.get(second).expect("active hit").payload, b"two");
    }

    #[test]
    fn corrupt_and_truncated_artifacts_fail_open_without_legacy_mutation() {
        let temp = tempfile::tempdir().expect("temporary output");
        let mut cache = RuntimeCache::open(temp.path()).expect("open");
        let cache_key = key("broken");
        cache.put(cache_key, b"payload").expect("store");
        let shard = RuntimeCache::shard_for_key(cache_key);
        let location = cache.shards[shard].entries[&cache_key];
        let artifact =
            active_artifact_path(&shard_directory(cache.root(), shard), location.generation);
        let file = OpenOptions::new()
            .write(true)
            .open(&artifact)
            .expect("artifact");
        file.set_len(location.offset + location.frame_len - 1)
            .expect("truncate artifact");
        assert!(
            cache.get(cache_key).is_none(),
            "truncated artifact is a miss"
        );

        let mut calls = 0;
        let hit = cache
            .get_or_legacy(cache_key, || {
                calls += 1;
                Some(b"legacy".to_vec())
            })
            .expect("legacy fallback");
        assert_eq!(calls, 1);
        assert_eq!(hit.source, RuntimeCacheSource::Legacy);
        assert_eq!(hit.payload, b"legacy");
    }

    #[test]
    fn incomplete_catalog_tail_recovers_prior_entries_and_accepts_new_records() {
        let temp = tempfile::tempdir().expect("temporary output");
        let mut cache = RuntimeCache::open(temp.path()).expect("open");
        let cache_key = key("stable");
        cache.put(cache_key, b"payload").expect("store");
        let shard = RuntimeCache::shard_for_key(cache_key);
        let catalog = shard_directory(cache.root(), shard).join(CATALOG_FILE);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&catalog)
            .expect("catalog");
        file.write_all(&FRAME_MAGIC[..4]).expect("partial tail");
        file.sync_data().expect("sync tail");
        drop(cache);

        let mut reopened = RuntimeCache::open(temp.path()).expect("reopen");
        assert_eq!(
            reopened.get(cache_key).expect("prior hit remains").payload,
            b"payload"
        );
        let next = key("new-after-tail");
        reopened
            .put(next, b"new payload")
            .expect("recovered append");
        drop(reopened);
        let recovered = RuntimeCache::open(temp.path()).expect("reopen after append");
        assert_eq!(
            recovered.get(next).expect("new hit").payload,
            b"new payload"
        );
    }

    #[test]
    fn bounds_reject_without_creating_an_artifact_record() {
        let temp = tempfile::tempdir().expect("temporary output");
        let limits = RuntimeCacheLimits {
            max_artifact_bytes: 3,
            max_segment_bytes: 128,
            max_catalog_bytes: 4096,
            max_total_catalog_bytes: 4096,
        };
        let mut cache = RuntimeCache::open_with_limits(temp.path(), limits).expect("open");
        let cache_key = key("oversize");
        assert!(matches!(
            cache.put(cache_key, b"four"),
            Err(RuntimeCacheError::PayloadTooLarge { .. })
        ));
        assert!(cache.get(cache_key).is_none());
        let shard = RuntimeCache::shard_for_key(cache_key);
        assert!(!shard_directory(cache.root(), shard)
            .join(CATALOG_FILE)
            .exists());
    }

    #[test]
    fn catalog_budget_is_aggregate_across_shards_and_derived_from_runtime_memory() {
        let memory_budget = 4 * 1024 * 1024;
        let derived = RuntimeCacheLimits::for_memory_budget(memory_budget);
        assert_eq!(
            derived.max_total_catalog_bytes + derived.max_segment_bytes,
            memory_budget
        );
        assert!(derived.max_artifact_bytes < derived.max_segment_bytes);

        let temp = tempfile::tempdir().expect("temporary output");
        let mut cache = RuntimeCache::open(temp.path()).expect("open default cache");
        let mut keys = Vec::new();
        for index in 0..10_000 {
            let candidate = key(&format!("aggregate-{index}"));
            if keys.iter().all(|existing| {
                RuntimeCache::shard_for_key(*existing) != RuntimeCache::shard_for_key(candidate)
            }) {
                keys.push(candidate);
            }
            if keys.len() == 3 {
                break;
            }
        }
        assert_eq!(keys.len(), 3);
        for cache_key in &keys {
            cache.put(*cache_key, b"x").expect("populate catalog");
        }
        drop(cache);

        let constrained = RuntimeCacheLimits {
            max_artifact_bytes: 16,
            max_segment_bytes: 128,
            max_catalog_bytes: CATALOG_FRAME_LEN * 2,
            max_total_catalog_bytes: CATALOG_FRAME_LEN * 2,
        };
        assert!(matches!(
            RuntimeCache::open_with_limits(temp.path(), constrained),
            Err(RuntimeCacheError::AggregateCatalogTooLarge { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_cache_owned_directory_is_refused() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary output");
        let outside = tempfile::tempdir().expect("outside directory");
        fs::create_dir(temp.path().join("cache")).expect("cache parent");
        symlink(outside.path(), temp.path().join(RUNTIME_CACHE_DIRECTORY))
            .expect("malicious runtime-v1 symlink");

        assert!(matches!(
            RuntimeCache::open(temp.path()),
            Err(RuntimeCacheError::UnsafePath { .. })
        ));
        assert!(
            fs::read_dir(outside.path())
                .expect("outside remains readable")
                .next()
                .is_none(),
            "cache open must not create shards through the symlink"
        );
    }

    #[test]
    fn dedicated_cache_io_service_owns_probe_and_persistence() {
        let temp = tempfile::tempdir().expect("temporary output");
        let control_thread = thread::current().id();
        let service = RuntimeCacheIoService::start(
            temp.path().to_path_buf(),
            DEFAULT_RUNTIME_CACHE_IO_QUEUE_CAPACITY,
        )
        .expect("start dedicated cache I/O owner");
        let cache_key = key("dedicated-owner");

        let stored = service
            .persist_if_absent(cache_key, b"payload".to_vec())
            .expect("persist from I/O owner");
        let RuntimeCacheIoPersistOutcome::Stored { io_thread_id } = stored else {
            panic!("first persistence must store a new artifact");
        };
        assert_ne!(
            io_thread_id, control_thread,
            "the control plane must not perform the cache write"
        );

        let probe = service.probe(cache_key).expect("probe from I/O owner");
        assert_eq!(probe.io_thread_id, io_thread_id);
        assert_eq!(probe.hit.expect("cache hit").payload, b"payload");
        let repeated = service
            .persist_if_absent(cache_key, b"different payload".to_vec())
            .expect("probe and preserve existing record");
        assert_eq!(
            repeated,
            RuntimeCacheIoPersistOutcome::AlreadyPresent { io_thread_id },
            "the worker combines the probe and append decision without a caller-side cache race"
        );
        service.shutdown().expect("join dedicated I/O owner");

        let reopened = RuntimeCache::open(temp.path()).expect("reopen persisted cache");
        assert_eq!(
            reopened.get(cache_key).expect("persisted hit").payload,
            b"payload",
            "the first artifact remains authoritative after an I/O-owner probe"
        );
    }

    #[test]
    fn dedicated_cache_io_service_rejects_an_unbounded_zero_slot_handoff() {
        let temp = tempfile::tempdir().expect("temporary output");
        assert!(matches!(
            RuntimeCacheIoService::start(temp.path().to_path_buf(), 0),
            Err(RuntimeCacheIoServiceError::InvalidQueueCapacity)
        ));
    }
}
