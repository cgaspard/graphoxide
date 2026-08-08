//! Append-only, I/O-owner cache artifacts for the isolated indexing runtime.
//!
//! This module deliberately owns paths and filesystem operations. It is not
//! used by parser-facing [`crate::ReadyInput`] values: an I/O worker opens one
//! [`RuntimeCache`] for the duration of its assigned cache partitions, does
//! lookup and persistence there, and passes only validated bytes onward. An
//! OS-level owner lock prevents independent processes from corrupting the
//! append-only catalogs; in-process clients route through one bounded sender
//! instead of sharing filesystem state behind a mutex.
//!
//! Runtime-v1 is additive. Callers can use [`RuntimeCache::get_or_legacy`] to
//! retain the existing JSON cache as a read-only, fail-open fallback while
//! new framed artifacts are populated. The service never removes or mutates a
//! legacy cache entry.

use crate::{
    ByteCreditLease, ByteCreditLedger, CreditReservationError, FileReadRequest,
    RuntimeCancellation, SourceIdentityEvidence,
};
use std::{
    cell::Cell,
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    path::{Component, Path, PathBuf},
    sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError},
    thread::{self, JoinHandle, ThreadId},
    time::Duration,
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
/// Maximum aggregate artifact bytes retained across every shard by default.
pub const DEFAULT_RUNTIME_CACHE_MAX_TOTAL_ARTIFACT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Maximum directory entries inspected in any one cache shard.
pub const DEFAULT_RUNTIME_CACHE_MAX_SHARD_ENTRIES: usize = 4096;
/// Maximum aggregate directory entries inspected across all cache shards.
pub const DEFAULT_RUNTIME_CACHE_MAX_TOTAL_SHARD_ENTRIES: usize = 65_536;

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
/// Hard control-command slot bound for explicit/custom service construction.
pub const MAX_RUNTIME_CACHE_IO_QUEUE_CAPACITY: usize = 1024;
const CACHE_RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const RUNTIME_CACHE_IO_STACK_BYTES: usize = 512 * 1024;
const RUNTIME_CACHE_BASE_RESIDENT_BYTES: usize = 64 * 1024;
/// Conservative retained-memory charge for one decoded catalog row.
///
/// This includes the key/location payload, B-tree links, allocator metadata,
/// and alignment headroom. Disk catalog bytes are translated through this
/// charge when limits are derived from the runtime cache/run partition.
const CATALOG_ENTRY_RESIDENT_BYTES: usize = 256;

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
pub struct RuntimeCacheHit {
    /// Raw payload bytes. Decoding belongs to the CPU-side consumer.
    pub payload: Vec<u8>,
    /// Cache tier that supplied the payload.
    pub source: RuntimeCacheSource,
    // Service reads retain shared byte credit until the consumer finishes
    // decoding and drops the hit. Direct `RuntimeCache` reads use `None`.
    transfer_credit: Option<ByteCreditLease>,
}

impl std::fmt::Debug for RuntimeCacheHit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeCacheHit")
            .field("payload", &self.payload)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl PartialEq for RuntimeCacheHit {
    fn eq(&self, other: &Self) -> bool {
        self.payload == other.payload && self.source == other.source
    }
}

impl Eq for RuntimeCacheHit {}

/// Detailed result of probing one cache key.
///
/// A catalog row whose referenced artifact fails framing, bounds, checksum,
/// digest, or safe-path validation is distinguishable from a key that was
/// never stored. Callers can therefore report truthful cache telemetry while
/// treating both outcomes as fail-open misses.
#[derive(Debug, PartialEq, Eq)]
pub enum RuntimeCacheProbeOutcome {
    /// A validated artifact was returned.
    Hit(RuntimeCacheHit),
    /// No catalog row exists for this key.
    Missing,
    /// A row existed but was corrupt, incomplete, unsafe, or stale.
    RejectedCorruptOrStale,
    /// The source generation or its canonical path/root binding changed while
    /// a metadata-only lookup was validated.
    SourceChanged,
    /// The request/platform cannot produce strong metadata-only evidence.
    MetadataOnlyUnsupported,
}

impl RuntimeCacheProbeOutcome {
    /// Borrow the validated hit, if this outcome contains one.
    #[must_use]
    pub const fn hit(&self) -> Option<&RuntimeCacheHit> {
        match self {
            Self::Hit(hit) => Some(hit),
            Self::Missing
            | Self::RejectedCorruptOrStale
            | Self::SourceChanged
            | Self::MetadataOnlyUnsupported => None,
        }
    }

    /// Consume the outcome and return its validated hit, if any.
    #[must_use]
    pub fn into_hit(self) -> Option<RuntimeCacheHit> {
        match self {
            Self::Hit(hit) => Some(hit),
            Self::Missing
            | Self::RejectedCorruptOrStale
            | Self::SourceChanged
            | Self::MetadataOnlyUnsupported => None,
        }
    }
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
    /// Appending an artifact would exceed the aggregate on-disk cache policy.
    #[error(
        "runtime cache artifacts require {artifact_bytes} bytes, exceeding the aggregate {max_artifact_bytes}-byte limit"
    )]
    AggregateArtifactsTooLarge {
        /// Existing or attempted aggregate artifact bytes.
        artifact_bytes: u64,
        /// Configured aggregate bound.
        max_artifact_bytes: u64,
    },
    /// A shard contains more directory entries than the bounded open policy
    /// will inspect.
    #[error("runtime cache shard {} exceeds its {max_entries}-entry scan limit", path.display())]
    TooManyShardEntries {
        /// Shard whose enumeration was stopped.
        path: PathBuf,
        /// Maximum entries inspected per shard.
        max_entries: usize,
    },
    /// Aggregate shard enumeration exceeded the bounded startup policy.
    #[error("runtime cache exceeds its aggregate {max_entries}-entry shard scan limit")]
    TooManyTotalShardEntries {
        /// Maximum entries inspected across all shards.
        max_entries: usize,
    },
    /// A prior partial or ambiguous append disabled further writes until the
    /// cache is reopened and its framed tail is repaired.
    #[error("runtime cache writes are disabled after an ambiguous append failure")]
    StoreDisabled,
    /// A cache-owned path was a symlink or a non-regular path type.
    #[error("runtime cache refuses unsafe owned path {}", path.display())]
    UnsafePath {
        /// Path that failed containment validation.
        path: PathBuf,
    },
    /// Another process/service already owns this output's append-only cache.
    /// Callers should disable cache use for this run and continue extraction.
    #[error("runtime cache already has an active I/O owner at {}", path.display())]
    OwnerBusy {
        /// Cache-owned coordination file.
        path: PathBuf,
    },
    /// A caller supplied anything outside the exact legacy AST cache grammar.
    #[error("runtime cache legacy path must match cache/ast/v<digits>/<64-lowercase-hex>.json")]
    InvalidLegacyPath,
    /// A strict pre-counted encoder returned a different logical byte count or
    /// grew its allocation beyond the credit acquired before serialization.
    #[error(
        "runtime cache encoder reserved {reserved_bytes} bytes but produced length {actual_bytes} with capacity {actual_capacity_bytes}"
    )]
    EncodedSizeMismatch {
        /// Credit acquired before invoking the encoder.
        reserved_bytes: usize,
        /// Serialized logical length.
        actual_bytes: usize,
        /// Serialized allocation capacity.
        actual_capacity_bytes: usize,
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
    /// The requested queue would retain too much control-plane state.
    #[error("runtime cache I/O queue capacity {requested} exceeds the {maximum}-slot hard limit")]
    QueueCapacityTooLarge {
        /// Requested command slots.
        requested: usize,
        /// Hard service limit.
        maximum: usize,
    },
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
    /// The caller cancelled while a bounded cache command or response was
    /// waiting. The I/O owner may finish an already admitted durable append,
    /// but the cancelled caller never accepts its result.
    #[error("runtime cache operation cancelled")]
    Cancelled,
    /// A requested payload transfer cannot fit the service's shared byte-credit
    /// partition and waiting could therefore never make progress.
    #[error(
        "runtime cache transfer requires {requested_bytes} bytes, exceeding its {capacity_bytes}-byte credit capacity"
    )]
    TransferTooLarge {
        /// Requested reservation.
        requested_bytes: usize,
        /// Total shared transfer capacity.
        capacity_bytes: usize,
    },
}

/// Result of a cache probe performed by the dedicated I/O owner.
#[derive(Debug, PartialEq, Eq)]
pub struct RuntimeCacheIoProbe {
    /// Detailed effective outcome after any requested legacy fallback.
    pub outcome: RuntimeCacheProbeOutcome,
    /// Whether a rejected runtime-v1 row was bypassed before a valid legacy
    /// fallback was returned. This preserves corruption telemetry even when a
    /// fallback lets indexing proceed without parsing.
    pub runtime_rejected_before_legacy: bool,
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
    /// A catalog row existed but its artifact was rejected, so the I/O owner
    /// appended a new validated record and made it authoritative.
    RepairedRejected { io_thread_id: ThreadId },
    /// The caller semantically rejected an otherwise valid framed envelope and
    /// explicitly requested an authoritative replacement record.
    ReplacedExisting { io_thread_id: ThreadId },
}

/// Bounded request for a legacy cache file below the service's output root.
///
/// The path is relative by construction. The cache I/O owner performs the
/// actual contained, no-follow, single-link read; callers and CPU extractors
/// never receive a filesystem capability through this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCacheLegacyFileRequest {
    /// Runtime key checked before the read-only legacy fallback.
    pub key: RuntimeCacheKey,
    relative_path: PathBuf,
    max_bytes: usize,
}

/// Metadata-only lookup request executed wholly by the cache I/O owner.
///
/// The owner opens and holds the source handle, compares the expected bound
/// evidence, probes the cache, and verifies the same handle/path/root binding
/// once more before returning a hit. CPU work never receives the source path.
pub struct RuntimeCacheMetadataProbeRequest {
    /// Content-addressed runtime artifact selected by the manifest.
    pub key: RuntimeCacheKey,
    source: FileReadRequest,
    expected_source_identity: SourceIdentityEvidence,
}

impl RuntimeCacheMetadataProbeRequest {
    /// Construct a manifest-authorized metadata-only cache request.
    #[must_use]
    pub const fn new(
        key: RuntimeCacheKey,
        source: FileReadRequest,
        expected_source_identity: SourceIdentityEvidence,
    ) -> Self {
        Self {
            key,
            source,
            expected_source_identity,
        }
    }
}

impl RuntimeCacheLegacyFileRequest {
    pub fn new(
        key: RuntimeCacheKey,
        relative_path: impl Into<PathBuf>,
        max_bytes: usize,
    ) -> Result<Self, RuntimeCacheError> {
        let relative_path = relative_path.into();
        if max_bytes == 0 || !is_legacy_ast_relative_path(&relative_path) {
            return Err(RuntimeCacheError::InvalidLegacyPath);
        }
        Ok(Self {
            key,
            relative_path,
            max_bytes,
        })
    }

    #[must_use]
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}

fn is_legacy_ast_relative_path(path: &Path) -> bool {
    if path.is_absolute() {
        return false;
    }
    let mut components = path.components();
    let (
        Some(Component::Normal(cache)),
        Some(Component::Normal(ast)),
        Some(Component::Normal(version)),
        Some(Component::Normal(file)),
        None,
    ) = (
        components.next(),
        components.next(),
        components.next(),
        components.next(),
        components.next(),
    )
    else {
        return false;
    };
    let (Some(cache), Some(ast), Some(version), Some(file)) = (
        cache.to_str(),
        ast.to_str(),
        version.to_str(),
        file.to_str(),
    ) else {
        return false;
    };
    let Some(version_digits) = version.strip_prefix('v') else {
        return false;
    };
    let Some(digest) = file.strip_suffix(".json") else {
        return false;
    };
    cache == "cache"
        && ast == "ast"
        && !version_digits.is_empty()
        && version_digits.bytes().all(|byte| byte.is_ascii_digit())
        && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Worst-case managed memory retained by one runtime-cache service.
///
/// Disk segment size is deliberately excluded: append-only bytes are streamed
/// by the I/O owner. Catalog entries, one worker framing buffer, the shared
/// transfer-credit partition, and bounded control commands are included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCacheMemoryAccounting {
    /// Dedicated worker stack plus fixed cache/shard/path state.
    pub service_overhead_bytes: usize,
    /// Maximum resident decoded catalog index across every shard.
    pub max_catalog_resident_bytes: usize,
    /// Shared reservation covering every queued, active, or returned payload.
    pub max_in_flight_transfer_bytes: usize,
    /// Maximum temporary framing/read buffer owned by the one I/O worker.
    pub max_worker_scratch_bytes: usize,
    /// Conservative inline command storage in the bounded channel and worker.
    pub max_control_queue_bytes: usize,
    /// Sum a caller must subtract from its cache/run memory partition.
    pub max_resident_bytes: usize,
}

/// Dynamic transfer-credit snapshot for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCacheMemoryUsage {
    /// Fixed catalog, worker-scratch, and control-plane reservation.
    pub fixed_resident_bytes: usize,
    /// Bytes currently held by queued, active, or consumer-owned payloads.
    pub in_flight_transfer_bytes: usize,
    /// Current total committed managed bytes.
    pub committed_bytes: usize,
    /// Hard upper bound reported at service construction.
    pub max_resident_bytes: usize,
}

/// Strict writer backed by byte credit acquired before serialization.
///
/// It deliberately exposes only [`Write`], so an encoder cannot reserve or
/// grow the backing allocation beyond the exact pre-count supplied to the
/// cache client.
pub struct RuntimeCacheEncodeBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl RuntimeCacheEncodeBuffer {
    fn with_exact_capacity(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit),
            limit,
        }
    }
}

impl Write for RuntimeCacheEncodeBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime cache encoder exceeded its exact byte reservation",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Keeping the metadata request inline makes the bounded channel's
// `size_of::<RuntimeCacheIoCommand>()` accounting complete. Boxing the large
// variant would hide a per-command heap allocation from that fixed charge.
#[allow(clippy::large_enum_variant)]
enum RuntimeCacheIoCommand {
    Probe {
        key: RuntimeCacheKey,
        cancellation: RuntimeCancellation,
        credit: ByteCreditLease,
        response: SyncSender<Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError>>,
    },
    ProbeMetadataOnly {
        request: RuntimeCacheMetadataProbeRequest,
        cancellation: RuntimeCancellation,
        credit: ByteCreditLease,
        response: SyncSender<Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError>>,
    },
    ProbeOrLegacyFile {
        request: RuntimeCacheLegacyFileRequest,
        cancellation: RuntimeCancellation,
        credit: ByteCreditLease,
        response: SyncSender<Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError>>,
    },
    PersistIfAbsent {
        key: RuntimeCacheKey,
        payload: Vec<u8>,
        replace_existing: bool,
        cancellation: RuntimeCancellation,
        _credit: ByteCreditLease,
        response: SyncSender<Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError>>,
    },
    Shutdown {
        response: SyncSender<()>,
    },
    #[cfg(test)]
    HoldForTest {
        entered: SyncSender<()>,
        release: mpsc::Receiver<()>,
    },
}

/// Cloneable, filesystem-capability-free client for a cache I/O owner.
///
/// The client contains only a bounded sender, byte-credit ledger, and numeric
/// limits. It exposes neither a source path nor a cache path. All filesystem
/// work remains on the dedicated owner.
#[derive(Clone)]
pub struct RuntimeCacheIoClient {
    sender: SyncSender<RuntimeCacheIoCommand>,
    transfer_credits: ByteCreditLedger,
    max_artifact_bytes: usize,
    accounting: RuntimeCacheMemoryAccounting,
}

impl RuntimeCacheIoClient {
    /// Largest logical cache payload this service can transfer or persist.
    #[must_use]
    pub const fn max_artifact_bytes(&self) -> usize {
        self.max_artifact_bytes
    }

    pub fn probe(
        &self,
        key: RuntimeCacheKey,
    ) -> Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError> {
        self.probe_with_cancellation(key, &RuntimeCancellation::new())
    }

    pub fn probe_with_cancellation(
        &self,
        key: RuntimeCacheKey,
        cancellation: &RuntimeCancellation,
    ) -> Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError> {
        let credit = reserve_cache_credit(
            &self.transfer_credits,
            self.accounting.max_in_flight_transfer_bytes,
            cancellation,
        )?;
        let (response, receiver) = mpsc::sync_channel(0);
        send_cache_command(
            &self.sender,
            RuntimeCacheIoCommand::Probe {
                key,
                cancellation: cancellation.clone(),
                credit,
                response,
            },
            cancellation,
        )?;
        wait_cache_response(receiver, cancellation)
    }

    /// Validate a manifest-authorized source generation around one cache
    /// probe on the dedicated I/O owner.
    pub fn probe_metadata_only(
        &self,
        request: RuntimeCacheMetadataProbeRequest,
    ) -> Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError> {
        self.probe_metadata_only_with_cancellation(request, &RuntimeCancellation::new())
    }

    /// Cancellation-aware metadata-only probe.
    pub fn probe_metadata_only_with_cancellation(
        &self,
        request: RuntimeCacheMetadataProbeRequest,
        cancellation: &RuntimeCancellation,
    ) -> Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError> {
        let credit = reserve_cache_credit(
            &self.transfer_credits,
            self.accounting.max_in_flight_transfer_bytes,
            cancellation,
        )?;
        let (response, receiver) = mpsc::sync_channel(0);
        send_cache_command(
            &self.sender,
            RuntimeCacheIoCommand::ProbeMetadataOnly {
                request,
                cancellation: cancellation.clone(),
                credit,
                response,
            },
            cancellation,
        )?;
        wait_cache_response(receiver, cancellation)
    }

    pub fn probe_or_legacy_file(
        &self,
        request: RuntimeCacheLegacyFileRequest,
    ) -> Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError> {
        self.probe_or_legacy_file_with_cancellation(request, &RuntimeCancellation::new())
    }

    pub fn probe_or_legacy_file_with_cancellation(
        &self,
        request: RuntimeCacheLegacyFileRequest,
        cancellation: &RuntimeCancellation,
    ) -> Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError> {
        if request.max_bytes > self.max_artifact_bytes {
            return Err(RuntimeCacheIoServiceError::TransferTooLarge {
                requested_bytes: request.max_bytes,
                capacity_bytes: self.max_artifact_bytes,
            });
        }
        let credit = reserve_cache_credit(
            &self.transfer_credits,
            self.accounting.max_in_flight_transfer_bytes,
            cancellation,
        )?;
        let (response, receiver) = mpsc::sync_channel(0);
        send_cache_command(
            &self.sender,
            RuntimeCacheIoCommand::ProbeOrLegacyFile {
                request,
                cancellation: cancellation.clone(),
                credit,
                response,
            },
            cancellation,
        )?;
        wait_cache_response(receiver, cancellation)
    }

    pub fn persist_if_absent(
        &self,
        key: RuntimeCacheKey,
        payload: Vec<u8>,
    ) -> Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError> {
        self.persist_if_absent_with_cancellation(key, payload, &RuntimeCancellation::new())
    }

    pub fn persist_if_absent_with_cancellation(
        &self,
        key: RuntimeCacheKey,
        payload: Vec<u8>,
        cancellation: &RuntimeCancellation,
    ) -> Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError> {
        self.persist_owned(key, payload, false, cancellation)
    }

    /// Authoritatively append a replacement for a caller-rejected envelope.
    ///
    /// This is intentionally explicit: low-level framing validation cannot
    /// know whether a syntactically valid payload has the wrong outer schema.
    pub fn persist_replacing(
        &self,
        key: RuntimeCacheKey,
        payload: Vec<u8>,
    ) -> Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError> {
        self.persist_replacing_with_cancellation(key, payload, &RuntimeCancellation::new())
    }

    /// Cancellation-aware authoritative replacement.
    pub fn persist_replacing_with_cancellation(
        &self,
        key: RuntimeCacheKey,
        payload: Vec<u8>,
        cancellation: &RuntimeCancellation,
    ) -> Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError> {
        self.persist_owned(key, payload, true, cancellation)
    }

    /// Acquire exact byte credit before invoking a strict serializer.
    ///
    /// `encode` receives a bounded [`Write`] implementation whose capacity is
    /// exactly the caller's pre-count. Producing a different length is rejected.
    pub fn persist_encoded_with_cancellation<F>(
        &self,
        key: RuntimeCacheKey,
        encoded_bytes: usize,
        replace_existing: bool,
        cancellation: &RuntimeCancellation,
        encode: F,
    ) -> Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError>
    where
        F: FnOnce(&mut RuntimeCacheEncodeBuffer) -> Result<(), RuntimeCacheError>,
    {
        if encoded_bytes > self.max_artifact_bytes {
            return Err(RuntimeCacheIoServiceError::Cache(
                RuntimeCacheError::PayloadTooLarge {
                    payload_bytes: encoded_bytes,
                    max_payload_bytes: self.max_artifact_bytes,
                },
            ));
        }
        let credit = reserve_cache_credit(&self.transfer_credits, encoded_bytes, cancellation)?;
        let mut output = RuntimeCacheEncodeBuffer::with_exact_capacity(encoded_bytes);
        encode(&mut output).map_err(RuntimeCacheIoServiceError::Cache)?;
        if output.bytes.len() != encoded_bytes || output.bytes.capacity() != encoded_bytes {
            return Err(RuntimeCacheIoServiceError::Cache(
                RuntimeCacheError::EncodedSizeMismatch {
                    reserved_bytes: encoded_bytes,
                    actual_bytes: output.bytes.len(),
                    actual_capacity_bytes: output.bytes.capacity(),
                },
            ));
        }
        self.send_persist(key, output.bytes, replace_existing, credit, cancellation)
    }

    /// Exact pre-counted encoding without an external cancellation token.
    pub fn persist_encoded<F>(
        &self,
        key: RuntimeCacheKey,
        encoded_bytes: usize,
        replace_existing: bool,
        encode: F,
    ) -> Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError>
    where
        F: FnOnce(&mut RuntimeCacheEncodeBuffer) -> Result<(), RuntimeCacheError>,
    {
        self.persist_encoded_with_cancellation(
            key,
            encoded_bytes,
            replace_existing,
            &RuntimeCancellation::new(),
            encode,
        )
    }

    /// Current fixed and in-flight managed-memory commitment.
    #[must_use]
    pub fn memory_usage(&self) -> RuntimeCacheMemoryUsage {
        let in_flight_transfer_bytes = self
            .transfer_credits
            .capacity()
            .saturating_sub(self.transfer_credits.available());
        let fixed_resident_bytes = self
            .accounting
            .max_resident_bytes
            .saturating_sub(self.accounting.max_in_flight_transfer_bytes);
        RuntimeCacheMemoryUsage {
            fixed_resident_bytes,
            in_flight_transfer_bytes,
            committed_bytes: fixed_resident_bytes.saturating_add(in_flight_transfer_bytes),
            max_resident_bytes: self.accounting.max_resident_bytes,
        }
    }

    fn persist_owned(
        &self,
        key: RuntimeCacheKey,
        payload: Vec<u8>,
        replace_existing: bool,
        cancellation: &RuntimeCancellation,
    ) -> Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError> {
        if payload.len() > self.max_artifact_bytes {
            return Err(RuntimeCacheIoServiceError::Cache(
                RuntimeCacheError::PayloadTooLarge {
                    payload_bytes: payload.len(),
                    max_payload_bytes: self.max_artifact_bytes,
                },
            ));
        }
        let charged_bytes = payload.capacity();
        let credit = reserve_cache_credit(&self.transfer_credits, charged_bytes, cancellation)?;
        self.send_persist(key, payload, replace_existing, credit, cancellation)
    }

    fn send_persist(
        &self,
        key: RuntimeCacheKey,
        payload: Vec<u8>,
        replace_existing: bool,
        credit: ByteCreditLease,
        cancellation: &RuntimeCancellation,
    ) -> Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError> {
        let (response, receiver) = mpsc::sync_channel(0);
        send_cache_command(
            &self.sender,
            RuntimeCacheIoCommand::PersistIfAbsent {
                key,
                payload,
                replace_existing,
                cancellation: cancellation.clone(),
                _credit: credit,
                response,
            },
            cancellation,
        )?;
        wait_cache_response(receiver, cancellation)
    }
}

fn reserve_cache_credit(
    credits: &ByteCreditLedger,
    bytes: usize,
    cancellation: &RuntimeCancellation,
) -> Result<ByteCreditLease, RuntimeCacheIoServiceError> {
    loop {
        if cancellation.is_cancelled() {
            return Err(RuntimeCacheIoServiceError::Cancelled);
        }
        match credits.try_reserve(bytes) {
            Ok(credit) => return Ok(credit),
            Err(CreditReservationError::TooLarge {
                requested,
                capacity,
            }) => {
                return Err(RuntimeCacheIoServiceError::TransferTooLarge {
                    requested_bytes: requested,
                    capacity_bytes: capacity,
                });
            }
            Err(CreditReservationError::Insufficient { .. }) => {
                thread::park_timeout(CACHE_RESPONSE_POLL_INTERVAL);
            }
        }
    }
}

fn send_cache_command(
    sender: &SyncSender<RuntimeCacheIoCommand>,
    mut command: RuntimeCacheIoCommand,
    cancellation: &RuntimeCancellation,
) -> Result<(), RuntimeCacheIoServiceError> {
    loop {
        if cancellation.is_cancelled() {
            return Err(RuntimeCacheIoServiceError::Cancelled);
        }
        match sender.try_send(command) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                command = returned;
                thread::park_timeout(CACHE_RESPONSE_POLL_INTERVAL);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(RuntimeCacheIoServiceError::WorkerUnavailable);
            }
        }
    }
}

fn wait_cache_response<T>(
    receiver: mpsc::Receiver<Result<T, RuntimeCacheIoServiceError>>,
    cancellation: &RuntimeCancellation,
) -> Result<T, RuntimeCacheIoServiceError> {
    loop {
        if cancellation.is_cancelled() {
            return Err(RuntimeCacheIoServiceError::Cancelled);
        }
        match receiver.recv_timeout(CACHE_RESPONSE_POLL_INTERVAL) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(RuntimeCacheIoServiceError::WorkerUnavailable);
            }
        }
    }
}

fn cache_io_probe(
    outcome: RuntimeCacheProbeOutcome,
    runtime_rejected_before_legacy: bool,
    io_thread_id: ThreadId,
    credit: ByteCreditLease,
) -> RuntimeCacheIoProbe {
    let mut probe = RuntimeCacheIoProbe {
        outcome,
        runtime_rejected_before_legacy,
        io_thread_id,
    };
    attach_transfer_credit(&mut probe.outcome, credit);
    probe
}

fn attach_transfer_credit(outcome: &mut RuntimeCacheProbeOutcome, credit: ByteCreditLease) {
    if let RuntimeCacheProbeOutcome::Hit(hit) = outcome {
        hit.transfer_credit = Some(credit);
    }
}

fn probe_metadata_only_on_owner(
    cache: &RuntimeCache,
    request: RuntimeCacheMetadataProbeRequest,
    cancellation: RuntimeCancellation,
    io_thread_id: ThreadId,
    credit: ByteCreditLease,
) -> Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError> {
    if cancellation.is_cancelled() {
        return Err(RuntimeCacheIoServiceError::Cancelled);
    }
    let Some(admitted_evidence) = request.source.source_identity_evidence() else {
        return Ok(cache_io_probe(
            RuntimeCacheProbeOutcome::MetadataOnlyUnsupported,
            false,
            io_thread_id,
            credit,
        ));
    };
    if admitted_evidence != request.expected_source_identity {
        return Ok(cache_io_probe(
            RuntimeCacheProbeOutcome::SourceChanged,
            false,
            io_thread_id,
            credit,
        ));
    }
    let guard = match request.source.begin_metadata_only_validation(&cancellation) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            return Ok(cache_io_probe(
                RuntimeCacheProbeOutcome::SourceChanged,
                false,
                io_thread_id,
                credit,
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            return Err(RuntimeCacheIoServiceError::Cancelled);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(cache_io_probe(
                RuntimeCacheProbeOutcome::SourceChanged,
                false,
                io_thread_id,
                credit,
            ));
        }
        Err(error) => return Err(RuntimeCacheError::Io(error).into()),
    };
    if guard.evidence() != request.expected_source_identity {
        return Ok(cache_io_probe(
            RuntimeCacheProbeOutcome::SourceChanged,
            false,
            io_thread_id,
            credit,
        ));
    }
    let outcome = cache.probe(request.key);
    match guard.finish(&cancellation) {
        Ok(true) => Ok(cache_io_probe(outcome, false, io_thread_id, credit)),
        Ok(false) => Ok(cache_io_probe(
            RuntimeCacheProbeOutcome::SourceChanged,
            false,
            io_thread_id,
            credit,
        )),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            Err(RuntimeCacheIoServiceError::Cancelled)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(cache_io_probe(
            RuntimeCacheProbeOutcome::SourceChanged,
            false,
            io_thread_id,
            credit,
        )),
        Err(error) => Err(RuntimeCacheError::Io(error).into()),
    }
}

/// Owner and lifecycle handle for one dedicated runtime-v1 cache I/O worker.
///
/// Call [`Self::client`] to obtain bounded, clonable request handles. The
/// service retains the only join handle and an explicit shutdown command, so
/// outstanding clients cannot accidentally keep a detached worker alive.
///
pub struct RuntimeCacheIoService {
    client: RuntimeCacheIoClient,
    worker: Option<JoinHandle<()>>,
    accounting: RuntimeCacheMemoryAccounting,
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

    /// Start a production service whose catalog and in-flight command bounds
    /// are derived from one explicit cache/run memory allowance.
    pub fn start_for_memory_budget(
        output_dir: PathBuf,
        memory_budget_bytes: usize,
    ) -> Result<Self, RuntimeCacheIoServiceError> {
        let queue_capacity = 1;
        Self::start_with_limits(
            output_dir,
            queue_capacity,
            RuntimeCacheLimits::for_memory_budget(memory_budget_bytes),
        )
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
        if queue_capacity > MAX_RUNTIME_CACHE_IO_QUEUE_CAPACITY {
            return Err(RuntimeCacheIoServiceError::QueueCapacityTooLarge {
                requested: queue_capacity,
                maximum: MAX_RUNTIME_CACHE_IO_QUEUE_CAPACITY,
            });
        }
        limits.validate()?;
        let accounting = limits.memory_accounting(queue_capacity);
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
        let worker = thread::Builder::new()
            .name("graphoxide-runtime-cache-io".into())
            .stack_size(RUNTIME_CACHE_IO_STACK_BYTES)
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
                        RuntimeCacheIoCommand::Probe {
                            key,
                            cancellation,
                            credit,
                            response,
                        } => {
                            let outcome = if cancellation.is_cancelled() {
                                Err(RuntimeCacheIoServiceError::Cancelled)
                            } else {
                                Ok(cache_io_probe(
                                    cache.probe(key),
                                    false,
                                    io_thread_id,
                                    credit,
                                ))
                            };
                            let _ = response.send(outcome);
                        }
                        RuntimeCacheIoCommand::ProbeMetadataOnly {
                            request,
                            cancellation,
                            credit,
                            response,
                        } => {
                            let outcome = probe_metadata_only_on_owner(
                                &cache,
                                request,
                                cancellation,
                                io_thread_id,
                                credit,
                            );
                            let _ = response.send(outcome);
                        }
                        RuntimeCacheIoCommand::ProbeOrLegacyFile {
                            request,
                            cancellation,
                            credit,
                            response,
                        } => {
                            let outcome = if cancellation.is_cancelled() {
                                Err(RuntimeCacheIoServiceError::Cancelled)
                            } else {
                                cache
                                    .probe_or_legacy_file(&request)
                                    .map(|(outcome, runtime_rejected_before_legacy)| {
                                        RuntimeCacheIoProbe {
                                            outcome,
                                            runtime_rejected_before_legacy,
                                            io_thread_id,
                                        }
                                    })
                                    .map(|mut probe| {
                                        attach_transfer_credit(&mut probe.outcome, credit);
                                        probe
                                    })
                                    .map_err(RuntimeCacheIoServiceError::Cache)
                            };
                            let _ = response.send(outcome);
                        }
                        RuntimeCacheIoCommand::PersistIfAbsent {
                            key,
                            payload,
                            replace_existing,
                            cancellation,
                            _credit,
                            response,
                        } => {
                            let outcome = if cancellation.is_cancelled() {
                                Err(RuntimeCacheIoServiceError::Cancelled)
                            } else {
                                let cache_outcome = if replace_existing {
                                    cache.put(key, &payload).map(|()| {
                                        RuntimeCacheIoPersistOutcome::ReplacedExisting {
                                            io_thread_id,
                                        }
                                    })
                                } else {
                                    match cache.probe(key) {
                                        RuntimeCacheProbeOutcome::Hit(_) => {
                                            Ok(RuntimeCacheIoPersistOutcome::AlreadyPresent {
                                                io_thread_id,
                                            })
                                        }
                                        RuntimeCacheProbeOutcome::Missing => cache
                                            .put(key, &payload)
                                            .map(|()| RuntimeCacheIoPersistOutcome::Stored {
                                                io_thread_id,
                                            }),
                                        RuntimeCacheProbeOutcome::RejectedCorruptOrStale => {
                                            cache.put(key, &payload).map(|()| {
                                                RuntimeCacheIoPersistOutcome::RepairedRejected {
                                                    io_thread_id,
                                                }
                                            })
                                        }
                                        RuntimeCacheProbeOutcome::SourceChanged
                                        | RuntimeCacheProbeOutcome::MetadataOnlyUnsupported => {
                                            unreachable!(
                                                "plain cache probes do not inspect sources"
                                            )
                                        }
                                    }
                                };
                                cache_outcome.map_err(RuntimeCacheIoServiceError::Cache)
                            };
                            let _ = response.send(outcome);
                        }
                        RuntimeCacheIoCommand::Shutdown { response } => {
                            let _ = response.send(());
                            break;
                        }
                        #[cfg(test)]
                        RuntimeCacheIoCommand::HoldForTest { entered, release } => {
                            let _ = entered.send(());
                            let _ = release.recv();
                        }
                    }
                }
            })
            .map_err(RuntimeCacheIoServiceError::WorkerSpawn)?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                client: RuntimeCacheIoClient {
                    sender,
                    transfer_credits: ByteCreditLedger::new(
                        accounting.max_in_flight_transfer_bytes,
                    ),
                    max_artifact_bytes: limits.max_artifact_bytes,
                    accounting,
                },
                worker: Some(worker),
                accounting,
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

    #[must_use]
    pub fn client(&self) -> RuntimeCacheIoClient {
        self.client.clone()
    }

    #[must_use]
    pub const fn memory_accounting(&self) -> RuntimeCacheMemoryAccounting {
        self.accounting
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
        self.client.probe(key)
    }

    pub fn probe_or_legacy_file(
        &self,
        request: RuntimeCacheLegacyFileRequest,
    ) -> Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError> {
        self.client.probe_or_legacy_file(request)
    }

    /// Validate a manifest-authorized source and probe without reading source
    /// payload bytes, entirely on the dedicated cache I/O owner.
    pub fn probe_metadata_only(
        &self,
        request: RuntimeCacheMetadataProbeRequest,
    ) -> Result<RuntimeCacheIoProbe, RuntimeCacheIoServiceError> {
        self.client.probe_metadata_only(request)
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
        self.client.persist_if_absent(key, payload)
    }

    /// Replace a caller-rejected but correctly framed semantic envelope.
    pub fn persist_replacing(
        &self,
        key: RuntimeCacheKey,
        payload: Vec<u8>,
    ) -> Result<RuntimeCacheIoPersistOutcome, RuntimeCacheIoServiceError> {
        self.client.persist_replacing(key, payload)
    }

    /// Current cache memory commitment, including consumer-held hit bytes.
    #[must_use]
    pub fn memory_usage(&self) -> RuntimeCacheMemoryUsage {
        self.client.memory_usage()
    }

    /// Shut down and join the dedicated cache owner after earlier admitted
    /// commands. Outstanding cloned clients observe an unavailable worker.
    pub fn shutdown(mut self) -> Result<(), RuntimeCacheIoServiceError> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<(), RuntimeCacheIoServiceError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let cancellation = RuntimeCancellation::new();
        let (response, receiver) = mpsc::sync_channel(0);
        let send_result = send_cache_command(
            &self.client.sender,
            RuntimeCacheIoCommand::Shutdown { response },
            &cancellation,
        );
        if send_result.is_ok() {
            let _ = receiver.recv();
        }
        let join_result = worker
            .join()
            .map_err(|_| RuntimeCacheIoServiceError::WorkerPanicked);
        send_result.and(join_result)
    }
}

impl Drop for RuntimeCacheIoService {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
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
    /// Maximum aggregate framed artifact bytes across every shard/generation.
    pub max_total_artifact_bytes: u64,
    /// Maximum directory entries inspected while opening one shard.
    pub max_shard_entries: usize,
    /// Maximum aggregate directory entries inspected across every shard.
    pub max_total_shard_entries: usize,
}

impl Default for RuntimeCacheLimits {
    fn default() -> Self {
        Self {
            max_artifact_bytes: DEFAULT_RUNTIME_CACHE_MAX_ARTIFACT_BYTES,
            max_segment_bytes: DEFAULT_RUNTIME_CACHE_SEGMENT_BYTES,
            max_catalog_bytes: DEFAULT_RUNTIME_CACHE_MAX_CATALOG_BYTES,
            max_total_catalog_bytes: DEFAULT_RUNTIME_CACHE_MAX_TOTAL_CATALOG_BYTES,
            max_total_artifact_bytes: DEFAULT_RUNTIME_CACHE_MAX_TOTAL_ARTIFACT_BYTES,
            max_shard_entries: DEFAULT_RUNTIME_CACHE_MAX_SHARD_ENTRIES,
            max_total_shard_entries: DEFAULT_RUNTIME_CACHE_MAX_TOTAL_SHARD_ENTRIES,
        }
    }
}

impl RuntimeCacheLimits {
    /// Divide a control-plane cache/run memory partition between one decoded
    /// artifact and aggregate catalog indexes.
    ///
    /// Production callers derive these limits from the same explicit
    /// cache/run partition rather than accepting fixed per-shard maxima.
    #[must_use]
    pub fn for_memory_budget(memory_budget_bytes: usize) -> Self {
        // One quarter backs all payloads via shared byte credit and one
        // quarter backs the owner's framing scratch. The remaining half pays
        // both decoded catalog rows and the transient framed catalog read.
        let control_budget = 2usize.saturating_mul(std::mem::size_of::<RuntimeCacheIoCommand>());
        let service_overhead_bytes =
            RUNTIME_CACHE_IO_STACK_BYTES.saturating_add(RUNTIME_CACHE_BASE_RESIDENT_BYTES);
        let managed_budget = memory_budget_bytes
            .saturating_sub(control_budget)
            .saturating_sub(service_overhead_bytes);
        let transfer_budget = managed_budget / 4;
        let catalog_budget = managed_budget.saturating_sub(transfer_budget * 2);
        let catalog_entries = catalog_budget
            .checked_div(CATALOG_ENTRY_RESIDENT_BYTES + CATALOG_FRAME_LEN)
            .unwrap_or_default();
        let catalog_disk_bytes = catalog_entries.saturating_mul(CATALOG_FRAME_LEN);
        Self {
            max_artifact_bytes: transfer_budget
                .saturating_sub(FRAME_HEADER_LEN)
                .min(DEFAULT_RUNTIME_CACHE_MAX_ARTIFACT_BYTES),
            max_segment_bytes: DEFAULT_RUNTIME_CACHE_SEGMENT_BYTES,
            max_catalog_bytes: catalog_disk_bytes.min(DEFAULT_RUNTIME_CACHE_MAX_CATALOG_BYTES),
            max_total_catalog_bytes: catalog_disk_bytes
                .min(DEFAULT_RUNTIME_CACHE_MAX_TOTAL_CATALOG_BYTES),
            max_total_artifact_bytes: (memory_budget_bytes as u64)
                .saturating_mul(8)
                .max(DEFAULT_RUNTIME_CACHE_SEGMENT_BYTES as u64)
                .min(DEFAULT_RUNTIME_CACHE_MAX_TOTAL_ARTIFACT_BYTES),
            max_shard_entries: DEFAULT_RUNTIME_CACHE_MAX_SHARD_ENTRIES,
            max_total_shard_entries: DEFAULT_RUNTIME_CACHE_MAX_TOTAL_SHARD_ENTRIES,
        }
    }

    /// Translate on-disk limits to their conservative managed-memory charge.
    #[must_use]
    pub fn memory_accounting(self, queue_capacity: usize) -> RuntimeCacheMemoryAccounting {
        let catalog_entries = self.max_total_catalog_bytes / CATALOG_FRAME_LEN;
        let max_catalog_resident_bytes =
            catalog_entries.saturating_mul(CATALOG_ENTRY_RESIDENT_BYTES);
        let max_in_flight_transfer_bytes = self.max_artifact_bytes.saturating_add(FRAME_HEADER_LEN);
        let max_worker_scratch_bytes = self
            .max_artifact_bytes
            .saturating_add(FRAME_HEADER_LEN)
            .max(self.max_catalog_bytes);
        let max_control_queue_bytes = queue_capacity
            .saturating_add(1)
            .saturating_mul(std::mem::size_of::<RuntimeCacheIoCommand>());
        let service_overhead_bytes =
            RUNTIME_CACHE_IO_STACK_BYTES.saturating_add(RUNTIME_CACHE_BASE_RESIDENT_BYTES);
        let max_resident_bytes = service_overhead_bytes
            .saturating_add(max_catalog_resident_bytes)
            .saturating_add(max_in_flight_transfer_bytes)
            .saturating_add(max_worker_scratch_bytes)
            .saturating_add(max_control_queue_bytes);
        RuntimeCacheMemoryAccounting {
            service_overhead_bytes,
            max_catalog_resident_bytes,
            max_in_flight_transfer_bytes,
            max_worker_scratch_bytes,
            max_control_queue_bytes,
            max_resident_bytes,
        }
    }

    fn validate(self) -> Result<(), RuntimeCacheError> {
        if self.max_artifact_bytes == 0
            || self.max_segment_bytes < FRAME_HEADER_LEN
            || self.max_catalog_bytes < CATALOG_FRAME_LEN
            || self.max_total_catalog_bytes < CATALOG_FRAME_LEN
            || self.max_catalog_bytes > self.max_total_catalog_bytes
            || self.max_total_artifact_bytes < self.max_segment_bytes as u64
            || self.max_shard_entries < 2
            || self.max_total_shard_entries < self.max_shard_entries
        {
            return Err(RuntimeCacheError::InvalidLimits);
        }
        Ok(())
    }
}

/// I/O-owned cache service backed by 64 append-only catalog/artifact shards.
///
/// `RuntimeCache` contains `Cell` marker state to make it `!Sync` and holds an
/// OS-level exclusive owner lock. It is movable before work begins; concurrent
/// in-process callers use [`RuntimeCacheIoClient`] and a second process fails
/// open with [`RuntimeCacheError::OwnerBusy`].
pub struct RuntimeCache {
    output_root: PathBuf,
    root: PathBuf,
    limits: RuntimeCacheLimits,
    shards: Vec<ShardState>,
    total_catalog_len: u64,
    total_artifact_len: u64,
    artifact_store_available: bool,
    store_disabled: bool,
    _owner_lock: File,
    _io_owner_only: PhantomData<Cell<()>>,
}

#[derive(Debug)]
struct ShardState {
    entries: BTreeMap<RuntimeCacheKey, ArtifactLocation>,
    active_generation: u64,
    active_len: u64,
    active_exists: bool,
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
        match fs::symlink_metadata(output_dir) {
            Ok(metadata) if metadata_is_reparse(&metadata) || !metadata.is_dir() => {
                return Err(RuntimeCacheError::UnsafePath {
                    path: output_dir.to_path_buf(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(output_dir)?;
                let metadata = fs::symlink_metadata(output_dir)?;
                if metadata_is_reparse(&metadata) || !metadata.is_dir() {
                    return Err(RuntimeCacheError::UnsafePath {
                        path: output_dir.to_path_buf(),
                    });
                }
            }
            Err(error) => return Err(error.into()),
        }
        // `output_dir` is a caller-selected capability. This library rejects a
        // reparse final component and hardens every cache-owned descendant;
        // callers that accept untrusted output ancestors must validate those
        // ancestors against their own sandbox/root policy before calling.
        let output_dir = fs::canonicalize(output_dir)?;
        let root = ensure_owned_directory_path(&output_dir, Path::new(RUNTIME_CACHE_DIRECTORY))?;
        let owner_lock = acquire_owner_lock(&root)?;
        let mut shards = Vec::with_capacity(RUNTIME_CACHE_SHARDS);
        let mut total_catalog_len = 0u64;
        let mut total_artifact_len = 0u64;
        let mut total_shard_entries = 0usize;
        let mut generation_exhausted = false;
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
            let discovery = discover_shard_artifacts(
                &shard_dir,
                limits.max_shard_entries,
                limits.max_total_artifact_bytes,
            )?;
            total_shard_entries = total_shard_entries
                .checked_add(discovery.entry_count)
                .ok_or(RuntimeCacheError::TooManyTotalShardEntries {
                    max_entries: limits.max_total_shard_entries,
                })?;
            if total_shard_entries > limits.max_total_shard_entries {
                return Err(RuntimeCacheError::TooManyTotalShardEntries {
                    max_entries: limits.max_total_shard_entries,
                });
            }
            generation_exhausted |= discovery.generation_exhausted;
            total_artifact_len = total_artifact_len.checked_add(discovery.total_len).ok_or(
                RuntimeCacheError::AggregateArtifactsTooLarge {
                    artifact_bytes: u64::MAX,
                    max_artifact_bytes: limits.max_total_artifact_bytes,
                },
            )?;
            shards.push(ShardState {
                entries: catalog.entries,
                active_generation: discovery.active_generation,
                active_len: discovery.active_len,
                active_exists: discovery.active_exists,
                catalog_len: catalog.len,
            });
        }
        Ok(Self {
            output_root: output_dir,
            root,
            limits,
            shards,
            total_catalog_len,
            total_artifact_len,
            artifact_store_available: total_artifact_len <= limits.max_total_artifact_bytes,
            store_disabled: generation_exhausted,
            _owner_lock: owner_lock,
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
        self.probe(key).into_hit()
    }

    /// Probe with explicit missing-versus-rejected classification.
    #[must_use]
    pub fn probe(&self, key: RuntimeCacheKey) -> RuntimeCacheProbeOutcome {
        let shard = Self::shard_for_key(key);
        let Some(location) = self
            .shards
            .get(shard)
            .and_then(|state| state.entries.get(&key))
            .copied()
        else {
            return RuntimeCacheProbeOutcome::Missing;
        };
        let path = artifact_path(
            &self.root,
            shard,
            location.generation,
            self.shards[shard].active_generation,
            self.shards[shard].active_exists,
        );
        let Some(frame) = read_artifact_frame(&path, location, self.limits.max_artifact_bytes)
        else {
            return RuntimeCacheProbeOutcome::RejectedCorruptOrStale;
        };
        RuntimeCacheProbeOutcome::Hit(RuntimeCacheHit {
            payload: frame,
            source: RuntimeCacheSource::RuntimeV1,
            transfer_credit: None,
        })
    }

    fn probe_or_legacy_file(
        &self,
        request: &RuntimeCacheLegacyFileRequest,
    ) -> Result<(RuntimeCacheProbeOutcome, bool), RuntimeCacheError> {
        let runtime_outcome = self.probe(request.key);
        if matches!(runtime_outcome, RuntimeCacheProbeOutcome::Hit(_)) {
            return Ok((runtime_outcome, false));
        }
        let runtime_rejected = matches!(
            runtime_outcome,
            RuntimeCacheProbeOutcome::RejectedCorruptOrStale
        );
        let legacy_path = self.output_root.join(&request.relative_path);
        validate_contained_legacy_path(&self.output_root, &legacy_path)?;
        let Some(mut file) = open_cache_file_read(&legacy_path)? else {
            return Ok((runtime_outcome, false));
        };
        let length = file.metadata()?.len();
        if length > request.max_bytes as u64 {
            return Ok((RuntimeCacheProbeOutcome::RejectedCorruptOrStale, false));
        }
        let length = usize::try_from(length).map_err(|_| RuntimeCacheError::PayloadTooLarge {
            payload_bytes: usize::MAX,
            max_payload_bytes: request.max_bytes,
        })?;
        let mut bytes = vec![0; length];
        if file.read_exact(&mut bytes).is_err()
            || safe_opened_file_len(&file, &legacy_path)? != length as u64
        {
            return Ok((RuntimeCacheProbeOutcome::RejectedCorruptOrStale, false));
        }
        if bytes.starts_with(&FRAME_MAGIC) {
            let Some((payload, consumed)) = unframe_at(&bytes, request.max_bytes) else {
                return Ok((RuntimeCacheProbeOutcome::RejectedCorruptOrStale, false));
            };
            if consumed != bytes.len() {
                return Ok((RuntimeCacheProbeOutcome::RejectedCorruptOrStale, false));
            }
            let payload_len = payload.len();
            bytes.copy_within(FRAME_HEADER_LEN..consumed, 0);
            bytes.truncate(payload_len);
        }
        Ok((
            RuntimeCacheProbeOutcome::Hit(RuntimeCacheHit {
                payload: bytes,
                source: RuntimeCacheSource::Legacy,
                transfer_credit: None,
            }),
            runtime_rejected,
        ))
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
                transfer_credit: None,
            })
        })
    }

    /// Append a bounded framed payload and a separately framed catalog record.
    ///
    /// The artifact file is synchronized before its catalog record is made
    /// durable. A crash can therefore leave an unreferenced artifact, but not
    /// a trusted catalog reference to an artifact that was not flushed first.
    pub fn put(&mut self, key: RuntimeCacheKey, payload: &[u8]) -> Result<(), RuntimeCacheError> {
        if self.store_disabled {
            return Err(RuntimeCacheError::StoreDisabled);
        }
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
        if !self.artifact_store_available {
            return Err(RuntimeCacheError::AggregateArtifactsTooLarge {
                artifact_bytes: self.total_artifact_len,
                max_artifact_bytes: self.limits.max_total_artifact_bytes,
            });
        }
        let next_total_artifact_len = self
            .total_artifact_len
            .checked_add(artifact_frame_len)
            .ok_or(RuntimeCacheError::AggregateArtifactsTooLarge {
                artifact_bytes: u64::MAX,
                max_artifact_bytes: self.limits.max_total_artifact_bytes,
            })?;
        if next_total_artifact_len > self.limits.max_total_artifact_bytes {
            return Err(RuntimeCacheError::AggregateArtifactsTooLarge {
                artifact_bytes: next_total_artifact_len,
                max_artifact_bytes: self.limits.max_total_artifact_bytes,
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
            let Some(next_generation) = state.active_generation.checked_add(1) else {
                self.store_disabled = true;
                return Err(RuntimeCacheError::StoreDisabled);
            };
            seal_active_segment(&shard_dir, state.active_generation)?;
            state.active_generation = next_generation;
            state.active_len = 0;
            state.active_exists = false;
        }

        let artifact_path = active_artifact_path(&shard_dir, state.active_generation);
        let offset = state.active_len;
        if let Err(error) = append_durable(&artifact_path, &artifact_frame) {
            if let Ok(Some(observed_len)) = safe_regular_file_len(&artifact_path) {
                let growth = observed_len.saturating_sub(offset);
                state.active_len = observed_len;
                self.total_artifact_len = self.total_artifact_len.saturating_add(growth);
                if self.total_artifact_len > self.limits.max_total_artifact_bytes {
                    self.artifact_store_available = false;
                }
            }
            self.store_disabled = true;
            return Err(error);
        }
        state.active_len = state.active_len.saturating_add(artifact_frame_len);
        state.active_exists = true;
        self.total_artifact_len = next_total_artifact_len;
        let location = ArtifactLocation {
            generation: state.active_generation,
            offset,
            frame_len: artifact_frame_len,
            payload_digest: *blake3::hash(payload).as_bytes(),
        };
        let catalog_frame = frame(&encode_catalog_record(key, location));
        debug_assert_eq!(catalog_frame.len() as u64, catalog_frame_len);
        if let Err(error) = append_durable(&shard_dir.join(CATALOG_FILE), &catalog_frame) {
            // A partial catalog tail is repaired on reopen. Stop appending in
            // this owner so later records cannot become hidden behind it.
            self.store_disabled = true;
            return Err(error);
        }
        state.catalog_len = state.catalog_len.saturating_add(catalog_frame_len);
        self.total_catalog_len = next_total_catalog_len;
        state.entries.insert(key, location);
        Ok(())
    }
}

fn ensure_owned_directory_path(base: &Path, relative: &Path) -> Result<PathBuf, RuntimeCacheError> {
    let canonical_base = fs::canonicalize(base)?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(RuntimeCacheError::UnsafePath {
                path: base.join(relative),
            });
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata_is_reparse(&metadata) || !metadata.is_dir() => {
                return Err(RuntimeCacheError::UnsafePath {
                    path: current.clone(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                let metadata = fs::symlink_metadata(&current)?;
                if metadata_is_reparse(&metadata) || !metadata.is_dir() {
                    return Err(RuntimeCacheError::UnsafePath {
                        path: current.clone(),
                    });
                }
            }
            Err(error) => return Err(error.into()),
        }
        let canonical_current = fs::canonicalize(&current)?;
        if !canonical_current.starts_with(&canonical_base) || canonical_current != current {
            return Err(RuntimeCacheError::UnsafePath {
                path: current.clone(),
            });
        }
    }
    Ok(current)
}

fn safe_regular_file_len(path: &Path) -> Result<Option<u64>, RuntimeCacheError> {
    let Some(file) = open_cache_file_read(path)? else {
        return Ok(None);
    };
    safe_opened_file_len(&file, path).map(Some)
}

fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn open_cache_file_read(path: &Path) -> Result<Option<File>, RuntimeCacheError> {
    let mut options = OpenOptions::new();
    options.read(true);
    apply_no_follow(&mut options);
    match options.open(path) {
        Ok(file) => {
            safe_opened_file_len(&file, path)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn open_cache_file_append(path: &Path) -> Result<File, RuntimeCacheError> {
    let mut options = OpenOptions::new();
    options.read(true).append(true).create(true);
    apply_no_follow(&mut options);
    let file = options.open(path)?;
    safe_opened_file_len(&file, path)?;
    Ok(file)
}

fn open_cache_file_write(path: &Path) -> Result<File, RuntimeCacheError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    apply_no_follow(&mut options);
    let file = options.open(path)?;
    safe_opened_file_len(&file, path)?;
    Ok(file)
}

fn open_cache_lock_file(path: &Path) -> Result<File, RuntimeCacheError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    apply_no_follow(&mut options);
    let file = options.open(path)?;
    safe_opened_file_len(&file, path)?;
    Ok(file)
}

fn apply_no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // A no-follow open can still block forever if an attacker substitutes
        // a FIFO before the held-handle regular-file check. Nonblocking mode
        // is inert for regular files and lets us reject special files after
        // open without trusting the path observation that preceded it.
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

fn safe_opened_file_len(file: &File, path: &Path) -> Result<u64, RuntimeCacheError> {
    let path_metadata = fs::symlink_metadata(path).map_err(RuntimeCacheError::Io)?;
    let metadata = file.metadata().map_err(RuntimeCacheError::Io)?;
    if metadata_is_reparse(&path_metadata)
        || metadata_is_reparse(&metadata)
        || !metadata.is_file()
        || opened_file_link_count(file, &metadata)? != 1
    {
        return Err(RuntimeCacheError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    Ok(metadata.len())
}

#[cfg(unix)]
fn opened_file_link_count(_file: &File, metadata: &fs::Metadata) -> Result<u64, RuntimeCacheError> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(metadata.nlink())
}

#[cfg(windows)]
fn opened_file_link_count(file: &File, _metadata: &fs::Metadata) -> Result<u64, RuntimeCacheError> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns a live handle and `information` is exact writable
    // output storage for the duration of the call.
    let succeeded = unsafe {
        GetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            std::ptr::addr_of_mut!(information),
        )
    };
    if succeeded == 0 {
        return Err(RuntimeCacheError::Io(io::Error::last_os_error()));
    }
    Ok(u64::from(information.nNumberOfLinks))
}

#[cfg(not(any(unix, windows)))]
fn opened_file_link_count(
    _file: &File,
    _metadata: &fs::Metadata,
) -> Result<u64, RuntimeCacheError> {
    Ok(1)
}

fn acquire_owner_lock(root: &Path) -> Result<File, RuntimeCacheError> {
    let path = root.join("owner.lock");
    let file = open_cache_lock_file(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd as _;
        // SAFETY: this calls `flock` on the live descriptor owned by `file`.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(RuntimeCacheError::OwnerBusy { path });
            }
            return Err(error.into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::{
            Foundation::{ERROR_LOCK_VIOLATION, HANDLE},
            Storage::FileSystem::LockFile,
        };
        // SAFETY: `file` owns a live handle. `LockFile` synchronously acquires
        // this cache-owned byte range and fails immediately on contention.
        let succeeded =
            unsafe { LockFile(file.as_raw_handle() as HANDLE, 0, 0, u32::MAX, u32::MAX) };
        if succeeded == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
                return Err(RuntimeCacheError::OwnerBusy { path });
            }
            return Err(error.into());
        }
    }
    safe_opened_file_len(&file, &path)?;
    Ok(file)
}

/// Validate every existing ancestor. The final no-follow open and link-count
/// check close final-component substitution. A same-user actor that can rename
/// already-open cache directories remains outside this library's trust model;
/// production output permissions and the exclusive owner lock are required.
fn validate_contained_legacy_path(root: &Path, path: &Path) -> Result<(), RuntimeCacheError> {
    if !path.starts_with(root) {
        return Err(RuntimeCacheError::InvalidLegacyPath);
    }
    let Some(parent) = path.parent() else {
        return Err(RuntimeCacheError::InvalidLegacyPath);
    };
    let mut current = root.to_path_buf();
    let relative_parent = parent
        .strip_prefix(root)
        .map_err(|_| RuntimeCacheError::InvalidLegacyPath)?;
    for component in relative_parent.components() {
        let Component::Normal(component) = component else {
            return Err(RuntimeCacheError::InvalidLegacyPath);
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current)?;
        if metadata_is_reparse(&metadata) || !metadata.is_dir() {
            return Err(RuntimeCacheError::UnsafePath {
                path: current.clone(),
            });
        }
        let canonical = fs::canonicalize(&current)?;
        if canonical != current || !canonical.starts_with(root) {
            return Err(RuntimeCacheError::UnsafePath {
                path: current.clone(),
            });
        }
    }
    Ok(())
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

fn artifact_path(
    root: &Path,
    shard: usize,
    generation: u64,
    active_generation: u64,
    active_exists: bool,
) -> PathBuf {
    let shard_dir = shard_directory(root, shard);
    if active_exists && generation == active_generation {
        active_artifact_path(&shard_dir, generation)
    } else {
        sealed_artifact_path(&shard_dir, generation)
    }
}

struct ShardArtifactDiscovery {
    active_generation: u64,
    active_len: u64,
    active_exists: bool,
    total_len: u64,
    entry_count: usize,
    generation_exhausted: bool,
}

fn discover_shard_artifacts(
    shard_dir: &Path,
    max_entries: usize,
    max_total_artifact_bytes: u64,
) -> Result<ShardArtifactDiscovery, RuntimeCacheError> {
    let mut active = None;
    let mut max_sealed_generation = None;
    let mut total_len = 0u64;
    let mut entry_count = 0usize;
    for entry in fs::read_dir(shard_dir)? {
        let entry = entry?;
        entry_count = entry_count.saturating_add(1);
        if entry_count > max_entries {
            return Err(RuntimeCacheError::TooManyShardEntries {
                path: shard_dir.to_path_buf(),
                max_entries,
            });
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == CATALOG_FILE {
            safe_regular_file_len(&entry.path())?
                .ok_or_else(|| RuntimeCacheError::UnsafePath { path: entry.path() })?;
            continue;
        }
        let active_generation = name
            .strip_prefix(ACTIVE_PREFIX)
            .and_then(|value| value.strip_suffix(ACTIVE_SUFFIX))
            .and_then(|value| value.parse::<u64>().ok());
        let sealed_generation = name
            .strip_prefix(SEALED_PREFIX)
            .and_then(|value| value.strip_suffix(ACTIVE_SUFFIX))
            .and_then(|value| value.parse::<u64>().ok());
        let Some(generation) = active_generation.or(sealed_generation) else {
            return Err(RuntimeCacheError::UnsafePath { path: entry.path() });
        };
        let length = safe_regular_file_len(&entry.path())?
            .ok_or_else(|| RuntimeCacheError::UnsafePath { path: entry.path() })?;
        total_len =
            total_len
                .checked_add(length)
                .ok_or(RuntimeCacheError::AggregateArtifactsTooLarge {
                    artifact_bytes: u64::MAX,
                    max_artifact_bytes: max_total_artifact_bytes,
                })?;
        if active_generation.is_some() {
            if active.replace((generation, length)).is_some() {
                return Err(RuntimeCacheError::UnsafePath {
                    path: shard_dir.to_path_buf(),
                });
            }
        } else if max_sealed_generation.is_none_or(|current| generation > current) {
            max_sealed_generation = Some(generation);
        }
    }
    let (active_generation, active_len, active_exists, generation_exhausted) = match active {
        Some((generation, length)) => {
            if max_sealed_generation.is_some_and(|sealed| sealed >= generation) {
                return Err(RuntimeCacheError::UnsafePath {
                    path: shard_dir.to_path_buf(),
                });
            }
            (generation, length, true, generation == u64::MAX)
        }
        None => match max_sealed_generation {
            Some(generation) => match generation.checked_add(1) {
                Some(next) => (next, 0, false, false),
                None => (generation, 0, false, true),
            },
            None => (0, 0, false, false),
        },
    };
    Ok(ShardArtifactDiscovery {
        active_generation,
        active_len,
        active_exists,
        total_len,
        entry_count,
        generation_exhausted,
    })
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
    fs::rename(active, &sealed)?;
    safe_regular_file_len(&sealed)?.ok_or_else(|| RuntimeCacheError::UnsafePath {
        path: sealed.clone(),
    })?;
    Ok(())
}

fn append_durable(path: &Path, bytes: &[u8]) -> Result<(), RuntimeCacheError> {
    let mut file = open_cache_file_append(path)?;
    file.write_all(bytes)?;
    file.sync_data()?;
    safe_opened_file_len(&file, path)?;
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
    let bytes = match open_cache_file_read(&path) {
        Ok(Some(mut file)) => {
            let mut bytes = vec![0; metadata_len as usize];
            if file.read_exact(&mut bytes).is_err()
                || safe_opened_file_len(&file, &path).ok() != Some(metadata_len)
            {
                return Ok(CatalogLoad {
                    entries: BTreeMap::new(),
                    len: limits.max_catalog_bytes as u64 + 1,
                    repair_tail: false,
                });
            }
            bytes
        }
        Ok(None) | Err(_) => {
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
    let file = open_cache_file_write(path)?;
    file.set_len(len)?;
    file.sync_data()?;
    if safe_opened_file_len(&file, path)? != len {
        return Err(RuntimeCacheError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
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
    let mut file = open_cache_file_read(path).ok().flatten()?;
    let initial_len = safe_opened_file_len(&file, path).ok()?;
    if location.offset.checked_add(location.frame_len)? > initial_len {
        return None;
    }
    file.seek(SeekFrom::Start(location.offset)).ok()?;
    let mut bytes = vec![0; frame_len];
    file.read_exact(&mut bytes).ok()?;
    let (payload, consumed) = unframe_at(&bytes, max_payload_bytes)?;
    if consumed != bytes.len() || *blake3::hash(payload).as_bytes() != location.payload_digest {
        return None;
    }
    if safe_opened_file_len(&file, path).ok()? != initial_len {
        return None;
    }
    let payload_len = payload.len();
    bytes.copy_within(FRAME_HEADER_LEN..consumed, 0);
    bytes.truncate(payload_len);
    Some(bytes)
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

    #[cfg(unix)]
    fn make_fifo(path: &Path) {
        use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

        let path = CString::new(path.as_os_str().as_bytes()).expect("FIFO path has no NUL");
        // SAFETY: `path` is a live NUL-terminated pathname and mkfifo does not
        // retain the pointer after returning.
        let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(result, 0, "mkfifo failed: {}", io::Error::last_os_error());
    }

    fn legacy_path() -> PathBuf {
        PathBuf::from(format!("cache/ast/v26/{}.json", "a".repeat(64)))
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
            ..RuntimeCacheLimits::default()
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
    fn sealed_only_crash_window_reopens_at_the_next_generation_without_aliasing_reads() {
        let temp = tempfile::tempdir().expect("temporary output");
        let first = key("sealed-only-first");
        let second = (0..1024)
            .map(|index| key(&format!("sealed-only-second-{index}")))
            .find(|candidate| {
                RuntimeCache::shard_for_key(*candidate) == RuntimeCache::shard_for_key(first)
            })
            .expect("same shard");
        let shard = RuntimeCache::shard_for_key(first);
        {
            let mut cache = RuntimeCache::open(temp.path()).expect("cache");
            cache.put(first, b"first").expect("first");
            seal_active_segment(&shard_directory(cache.root(), shard), 0)
                .expect("simulate crash after sealing and before next append");
        }
        let mut reopened = RuntimeCache::open(temp.path()).expect("reopen sealed-only shard");
        assert_eq!(
            reopened
                .get(first)
                .expect("sealed record remains routed")
                .payload,
            b"first"
        );
        assert_eq!(reopened.shards[shard].active_generation, 1);
        assert!(!reopened.shards[shard].active_exists);
        reopened
            .put(second, b"second")
            .expect("append generation one");
        assert!(active_artifact_path(&shard_directory(reopened.root(), shard), 1).is_file());
        assert_eq!(reopened.get(first).expect("old hit").payload, b"first");
        assert_eq!(reopened.get(second).expect("new hit").payload, b"second");
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
            ..RuntimeCacheLimits::default()
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
        let accounting = derived.memory_accounting(1);
        assert!(accounting.max_resident_bytes <= memory_budget);
        assert!(derived.max_artifact_bytes + FRAME_HEADER_LEN <= derived.max_segment_bytes);

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
            ..RuntimeCacheLimits::default()
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
        assert_eq!(
            probe.outcome.into_hit().expect("cache hit").payload,
            b"payload"
        );
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
    fn detailed_probe_and_explicit_replacement_repair_valid_wrong_envelopes() {
        let temp = tempfile::tempdir().expect("temporary output");
        let service = RuntimeCacheIoService::start(temp.path().to_path_buf(), 1).expect("service");
        let cache_key = key("semantic-repair");
        assert!(matches!(
            service.probe(cache_key).expect("missing probe").outcome,
            RuntimeCacheProbeOutcome::Missing
        ));
        service
            .persist_if_absent(cache_key, b"valid frame, wrong envelope".to_vec())
            .expect("initial envelope");
        assert!(matches!(
            service
                .persist_replacing(cache_key, b"correct envelope".to_vec())
                .expect("replace envelope"),
            RuntimeCacheIoPersistOutcome::ReplacedExisting { .. }
        ));
        let hit = service
            .probe(cache_key)
            .expect("repaired probe")
            .outcome
            .into_hit()
            .expect("hit");
        assert_eq!(hit.payload, b"correct envelope");
    }

    #[test]
    fn strict_encoding_and_returned_hits_share_one_transfer_credit_pool() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RuntimeCacheIoClient>();

        let temp = tempfile::tempdir().expect("temporary output");
        let service = RuntimeCacheIoService::start(temp.path().to_path_buf(), 8).expect("service");
        let client = service.client();
        let cache_key = key("credit");
        client
            .persist_encoded(cache_key, 7, false, |buffer| {
                buffer.write_all(b"payload")?;
                Ok(())
            })
            .expect("strict persistence");
        assert_eq!(client.memory_usage().in_flight_transfer_bytes, 0);

        let probe = client.probe(cache_key).expect("probe");
        assert_eq!(
            client.memory_usage().in_flight_transfer_bytes,
            service.memory_accounting().max_in_flight_transfer_bytes,
            "returned payload owns credit until decode/drop"
        );
        let hit = probe.outcome.into_hit().expect("hit");
        assert_eq!(hit.payload, b"payload");
        drop(hit);
        assert_eq!(client.memory_usage().in_flight_transfer_bytes, 0);

        assert!(matches!(
            client.persist_encoded(key("wrong-count"), 2, false, |buffer| {
                buffer.write_all(&[1])?;
                Ok(())
            }),
            Err(RuntimeCacheIoServiceError::Cache(
                RuntimeCacheError::EncodedSizeMismatch { .. }
            ))
        ));
        assert!(matches!(
            client.persist_encoded(key("overrun"), 1, false, |buffer| {
                buffer.write_all(&[1, 2])?;
                Ok(())
            }),
            Err(RuntimeCacheIoServiceError::Cache(RuntimeCacheError::Io(error)))
                if error.kind() == io::ErrorKind::InvalidData
        ));
        let cancelled = RuntimeCancellation::new();
        cancelled.cancel();
        let mut encoded = false;
        assert!(matches!(
            client.persist_encoded_with_cancellation(
                key("cancel-before-encode"),
                1,
                false,
                &cancelled,
                |buffer| {
                    encoded = true;
                    buffer.write_all(&[1])?;
                    Ok(())
                },
            ),
            Err(RuntimeCacheIoServiceError::Cancelled)
        ));
        assert!(!encoded, "cancellation is observed before serialization");
    }

    #[test]
    fn metadata_only_probe_holds_and_rechecks_the_verified_source_generation() {
        let temp = tempfile::tempdir().expect("temporary output");
        let source_root = tempfile::tempdir().expect("source root");
        let source = source_root.path().join("source.rs");
        fs::write(&source, b"old bytes").expect("source");
        let request = FileReadRequest::new_verified_under(
            crate::InputIdentity::new("source.rs", 0),
            source.clone(),
            source_root.path(),
        )
        .expect("verified source");
        let evidence = request
            .source_identity_evidence()
            .expect("strong source evidence");
        let cache_key = key("metadata-only");
        let service = RuntimeCacheIoService::start(temp.path().to_path_buf(), 1).expect("service");
        service
            .persist_if_absent(cache_key, b"cached".to_vec())
            .expect("populate");

        let hit = service
            .probe_metadata_only(RuntimeCacheMetadataProbeRequest::new(
                cache_key,
                request.clone(),
                evidence,
            ))
            .expect("metadata probe")
            .outcome
            .into_hit()
            .expect("metadata hit");
        assert_eq!(hit.payload, b"cached");
        drop(hit);

        fs::write(&source, b"new bytes").expect("same-size source mutation");
        assert!(matches!(
            service
                .probe_metadata_only(RuntimeCacheMetadataProbeRequest::new(
                    cache_key, request, evidence,
                ))
                .expect("changed source classification")
                .outcome,
            RuntimeCacheProbeOutcome::SourceChanged
        ));
        let unverified = FileReadRequest::new(crate::InputIdentity::new("source.rs", 0), source);
        assert!(matches!(
            service
                .probe_metadata_only(RuntimeCacheMetadataProbeRequest::new(
                    cache_key, unverified, evidence,
                ))
                .expect("unsupported classification")
                .outcome,
            RuntimeCacheProbeOutcome::MetadataOnlyUnsupported
        ));
    }

    #[test]
    fn legacy_file_fallback_is_bounded_contained_and_owned_by_the_io_worker() {
        let temp = tempfile::tempdir().expect("temporary output");
        let relative_legacy = legacy_path();
        let legacy_dir = temp
            .path()
            .join(relative_legacy.parent().expect("legacy parent"));
        fs::create_dir_all(&legacy_dir).expect("legacy directory");
        let legacy_file = temp.path().join(&relative_legacy);
        fs::write(&legacy_file, b"{\"legacy\":true}").expect("legacy payload");
        let service = RuntimeCacheIoService::start(temp.path().to_path_buf(), 1).expect("service");
        let request = RuntimeCacheLegacyFileRequest::new(key("legacy"), relative_legacy, 32)
            .expect("bounded request");
        let hit = service
            .probe_or_legacy_file(request.clone())
            .expect("legacy probe")
            .outcome
            .into_hit()
            .expect("legacy hit");
        assert_eq!(hit.source, RuntimeCacheSource::Legacy);
        drop(hit);

        fs::write(&legacy_file, vec![b'x'; 33]).expect("oversize legacy");
        assert!(matches!(
            service
                .probe_or_legacy_file(request.clone())
                .expect("oversize classification")
                .outcome,
            RuntimeCacheProbeOutcome::RejectedCorruptOrStale
        ));
        fs::write(&legacy_file, FRAME_MAGIC).expect("truncated framed legacy");
        assert!(matches!(
            service
                .probe_or_legacy_file(request)
                .expect("corrupt classification")
                .outcome,
            RuntimeCacheProbeOutcome::RejectedCorruptOrStale
        ));
        assert!(matches!(
            RuntimeCacheLegacyFileRequest::new(key("bad"), "../outside", 10),
            Err(RuntimeCacheError::InvalidLegacyPath)
        ));
        for invalid in [
            format!("cache/ast/v1/{}.json:stream", "a".repeat(64)),
            format!("cache/ast/v1/{}.json", "A".repeat(64)),
            format!("cache/ast/v1/{}/extra.json", "a".repeat(64)),
            format!("cache/ast/vx/{}.json", "a".repeat(64)),
        ] {
            assert!(matches!(
                RuntimeCacheLegacyFileRequest::new(key("bad-alias"), invalid, 10),
                Err(RuntimeCacheError::InvalidLegacyPath)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_cache_and_legacy_files_are_refused() {
        let temp = tempfile::tempdir().expect("temporary output");
        let cache_key = key("hardlink-artifact");
        {
            let mut cache = RuntimeCache::open(temp.path()).expect("cache");
            cache.put(cache_key, b"payload").expect("populate");
        }
        let shard = RuntimeCache::shard_for_key(cache_key);
        let artifact = active_artifact_path(
            &shard_directory(&temp.path().join(RUNTIME_CACHE_DIRECTORY), shard),
            0,
        );
        let outside = temp.path().join("artifact-alias");
        fs::hard_link(&artifact, &outside).expect("artifact hardlink");
        assert!(matches!(
            RuntimeCache::open(temp.path()),
            Err(RuntimeCacheError::UnsafePath { .. })
        ));

        fs::remove_file(outside).expect("remove artifact alias");
        let relative_legacy = legacy_path();
        let legacy_dir = temp
            .path()
            .join(relative_legacy.parent().expect("legacy parent"));
        fs::create_dir_all(&legacy_dir).expect("legacy directory");
        let legacy = temp.path().join(&relative_legacy);
        fs::write(&legacy, b"{}").expect("legacy");
        fs::hard_link(&legacy, temp.path().join("legacy-alias")).expect("legacy hardlink");
        let service = RuntimeCacheIoService::start(temp.path().to_path_buf(), 1).expect("service");
        assert!(matches!(
            service.probe_or_legacy_file(
                RuntimeCacheLegacyFileRequest::new(key("hardlink-legacy"), relative_legacy, 10,)
                    .expect("request")
            ),
            Err(RuntimeCacheIoServiceError::Cache(
                RuntimeCacheError::UnsafePath { .. }
            ))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_catalog_is_rejected_without_blocking_cache_open() {
        let temp = tempfile::tempdir().expect("temporary output");
        let cache = RuntimeCache::open(temp.path()).expect("initialize cache layout");
        let catalog = shard_directory(cache.root(), 0).join(CATALOG_FILE);
        drop(cache);
        make_fifo(&catalog);

        assert!(matches!(
            RuntimeCache::open(temp.path()),
            Err(RuntimeCacheError::UnsafePath { path }) if path == catalog
        ));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_artifact_is_rejected_without_blocking_probe() {
        let temp = tempfile::tempdir().expect("temporary output");
        let cache_key = key("fifo-artifact");
        let mut cache = RuntimeCache::open(temp.path()).expect("open cache");
        cache.put(cache_key, b"payload").expect("populate artifact");

        let shard = RuntimeCache::shard_for_key(cache_key);
        let state = &cache.shards[shard];
        let location = state.entries[&cache_key];
        let artifact = artifact_path(
            cache.root(),
            shard,
            location.generation,
            state.active_generation,
            state.active_exists,
        );
        fs::remove_file(&artifact).expect("replace artifact");
        make_fifo(&artifact);

        assert!(matches!(
            cache.probe(cache_key),
            RuntimeCacheProbeOutcome::RejectedCorruptOrStale
        ));
    }

    #[cfg(unix)]
    #[test]
    fn output_directory_symlink_is_refused_before_cache_creation() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("fixture");
        let real = fixture.path().join("real");
        let alias = fixture.path().join("alias");
        fs::create_dir(&real).expect("real output");
        symlink(&real, &alias).expect("output symlink");
        assert!(matches!(
            RuntimeCache::open(&alias),
            Err(RuntimeCacheError::UnsafePath { .. })
        ));
        assert!(!real.join("cache").exists());
    }

    #[test]
    fn exclusive_owner_and_raii_drop_prevent_concurrent_catalog_writers() {
        let temp = tempfile::tempdir().expect("temporary output");
        let service = RuntimeCacheIoService::start(temp.path().to_path_buf(), 1).expect("owner");
        assert!(matches!(
            RuntimeCacheIoService::start(temp.path().to_path_buf(), 1),
            Err(RuntimeCacheIoServiceError::Cache(
                RuntimeCacheError::OwnerBusy { .. }
            ))
        ));
        drop(service);
        RuntimeCacheIoService::start(temp.path().to_path_buf(), 1)
            .expect("RAII joined and released owner")
            .shutdown()
            .expect("shutdown replacement owner");
    }

    #[test]
    fn aggregate_artifact_quota_counts_rotations_shards_and_orphans_on_reopen() {
        let temp = tempfile::tempdir().expect("temporary output");
        let limits = RuntimeCacheLimits {
            max_artifact_bytes: 4,
            max_segment_bytes: FRAME_HEADER_LEN + 4,
            max_catalog_bytes: 4096,
            max_total_catalog_bytes: 4096,
            max_total_artifact_bytes: ((FRAME_HEADER_LEN + 1) * 2) as u64,
            max_shard_entries: 16,
            max_total_shard_entries: 128,
        };
        let first = key("quota-first");
        let second = (0..1024)
            .map(|index| key(&format!("quota-second-{index}")))
            .find(|candidate| {
                RuntimeCache::shard_for_key(*candidate) != RuntimeCache::shard_for_key(first)
            })
            .expect("different shard");
        let third = key("quota-third");
        {
            let mut cache = RuntimeCache::open_with_limits(temp.path(), limits).expect("cache");
            cache.put(first, b"x").expect("first boundary frame");
            cache.put(second, b"y").expect("second boundary frame");
            assert!(matches!(
                cache.put(third, b"z"),
                Err(RuntimeCacheError::AggregateArtifactsTooLarge { .. })
            ));
        }
        {
            let cache = RuntimeCache::open_with_limits(temp.path(), limits).expect("reopen");
            assert_eq!(
                cache.get(first).expect("first remains readable").payload,
                b"x"
            );
            assert_eq!(
                cache.get(second).expect("second remains readable").payload,
                b"y"
            );
        }

        let shard_dir = shard_directory(
            &temp.path().join(RUNTIME_CACHE_DIRECTORY),
            RuntimeCache::shard_for_key(third),
        );
        fs::write(sealed_artifact_path(&shard_dir, 999), b"orphan").expect("orphan artifact");
        let mut reopened = RuntimeCache::open_with_limits(temp.path(), limits)
            .expect("oversize existing cache opens read-only");
        assert_eq!(
            reopened
                .get(first)
                .expect("validated read survives")
                .payload,
            b"x"
        );
        assert!(matches!(
            reopened.put(third, b"z"),
            Err(RuntimeCacheError::AggregateArtifactsTooLarge { .. })
        ));
    }

    #[test]
    fn shard_enumeration_stops_at_the_explicit_entry_bound() {
        let temp = tempfile::tempdir().expect("temporary output");
        RuntimeCache::open(temp.path()).expect("layout");
        let shard = shard_directory(&temp.path().join(RUNTIME_CACHE_DIRECTORY), 0);
        for generation in 0..3 {
            fs::write(sealed_artifact_path(&shard, generation), b"x").expect("orphan segment");
        }
        let limits = RuntimeCacheLimits {
            max_shard_entries: 2,
            ..RuntimeCacheLimits::default()
        };
        assert!(matches!(
            RuntimeCache::open_with_limits(temp.path(), limits),
            Err(RuntimeCacheError::TooManyShardEntries { .. })
        ));

        let aggregate = tempfile::tempdir().expect("aggregate fixture");
        RuntimeCache::open(aggregate.path()).expect("aggregate layout");
        for shard in 0..5 {
            fs::write(
                sealed_artifact_path(
                    &shard_directory(&aggregate.path().join(RUNTIME_CACHE_DIRECTORY), shard),
                    0,
                ),
                b"x",
            )
            .expect("cross-shard orphan");
        }
        let aggregate_limits = RuntimeCacheLimits {
            max_shard_entries: 4,
            max_total_shard_entries: 4,
            ..RuntimeCacheLimits::default()
        };
        assert!(matches!(
            RuntimeCache::open_with_limits(aggregate.path(), aggregate_limits),
            Err(RuntimeCacheError::TooManyTotalShardEntries { .. })
        ));
    }

    #[test]
    fn artifact_orphan_after_catalog_failure_is_accounted_and_disables_current_owner() {
        let temp = tempfile::tempdir().expect("temporary output");
        let cache_key = key("catalog-failure");
        let mut cache = RuntimeCache::open(temp.path()).expect("cache");
        let shard = RuntimeCache::shard_for_key(cache_key);
        let catalog = shard_directory(cache.root(), shard).join(CATALOG_FILE);
        fs::create_dir(&catalog).expect("block catalog file creation");
        assert!(cache.put(cache_key, b"payload").is_err());
        assert!(matches!(
            cache.put(key("after-catalog-failure"), b"payload"),
            Err(RuntimeCacheError::StoreDisabled)
        ));
        let orphan_bytes = cache.total_artifact_len;
        assert!(orphan_bytes >= (FRAME_HEADER_LEN + b"payload".len()) as u64);
        drop(cache);

        fs::remove_dir(&catalog).expect("remove injected catalog directory");
        let reopened = RuntimeCache::open(temp.path()).expect("reopen counts orphan");
        assert_eq!(reopened.total_artifact_len, orphan_bytes);
        assert!(
            reopened.get(cache_key).is_none(),
            "orphan has no catalog row"
        );
    }

    #[test]
    fn exhausted_active_generation_keeps_reads_but_disables_future_stores() {
        let temp = tempfile::tempdir().expect("temporary output");
        RuntimeCache::open(temp.path()).expect("layout");
        let shard = shard_directory(&temp.path().join(RUNTIME_CACHE_DIRECTORY), 0);
        fs::write(active_artifact_path(&shard, u64::MAX), b"orphan").expect("max generation");
        let mut reopened = RuntimeCache::open(temp.path()).expect("read-only reopen");
        assert!(matches!(
            reopened.put(key("generation-exhausted"), b"x"),
            Err(RuntimeCacheError::StoreDisabled)
        ));
    }

    #[test]
    fn saturated_queue_and_shared_credits_cancel_all_waiters_without_deadlock() {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().expect("temporary output");
        let service = RuntimeCacheIoService::start(temp.path().to_path_buf(), 1).expect("service");
        let client = service.client();

        let (entered_one_tx, entered_one_rx) = mpsc::sync_channel(0);
        let (release_one_tx, release_one_rx) = mpsc::sync_channel(0);
        client
            .sender
            .send(RuntimeCacheIoCommand::HoldForTest {
                entered: entered_one_tx,
                release: release_one_rx,
            })
            .expect("block owner");
        entered_one_rx.recv().expect("owner entered hold");
        let (entered_two_tx, entered_two_rx) = mpsc::sync_channel(0);
        let (release_two_tx, release_two_rx) = mpsc::sync_channel(0);
        client
            .sender
            .try_send(RuntimeCacheIoCommand::HoldForTest {
                entered: entered_two_tx,
                release: release_two_rx,
            })
            .expect("saturate queue");

        const WAITERS: usize = 16;
        let cancellation = RuntimeCancellation::new();
        let barrier = Arc::new(Barrier::new(WAITERS + 1));
        let (done_tx, done_rx) = mpsc::channel();
        let mut workers = Vec::new();
        for index in 0..WAITERS {
            let client = client.clone();
            let cancellation = cancellation.clone();
            let barrier = Arc::clone(&barrier);
            let done_tx = done_tx.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                let result =
                    client.probe_with_cancellation(key(&format!("waiter-{index}")), &cancellation);
                done_tx
                    .send(matches!(result, Err(RuntimeCacheIoServiceError::Cancelled)))
                    .expect("report cancellation");
            }));
        }
        barrier.wait();
        thread::sleep(Duration::from_millis(40));
        cancellation.cancel();
        for _ in 0..WAITERS {
            assert!(done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("cancelled waiter returned"));
        }
        for worker in workers {
            worker.join().expect("waiter joined");
        }
        release_one_tx.send(()).expect("release owner");
        entered_two_rx.recv().expect("queued hold entered");
        release_two_tx.send(()).expect("release queued hold");
        service.shutdown().expect("shutdown");
        let cache = RuntimeCache::open(temp.path()).expect("reopen after cancellation");
        assert_eq!(
            cache.total_artifact_len, 0,
            "cancelled probes consume no quota"
        );
    }

    #[test]
    fn dedicated_cache_io_service_rejects_an_unbounded_zero_slot_handoff() {
        let temp = tempfile::tempdir().expect("temporary output");
        assert!(matches!(
            RuntimeCacheIoService::start(temp.path().to_path_buf(), 0),
            Err(RuntimeCacheIoServiceError::InvalidQueueCapacity)
        ));
        assert!(matches!(
            RuntimeCacheIoService::start(
                temp.path().to_path_buf(),
                MAX_RUNTIME_CACHE_IO_QUEUE_CAPACITY + 1,
            ),
            Err(RuntimeCacheIoServiceError::QueueCapacityTooLarge { .. })
        ));
    }
}
