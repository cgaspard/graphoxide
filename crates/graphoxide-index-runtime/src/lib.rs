//! Runtime primitives for the high-throughput indexing pipeline.
//!
//! The runtime primitives do not grant filesystem access to parser-facing
//! types. [`read_files_concurrently`] is the one deliberately narrow I/O
//! service: dedicated I/O workers own all reads and CPU workers receive only
//! [`ReadyInput`] values that have already been materialized and verified.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod cache;

use std::{
    cell::Cell,
    cmp,
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    mem::MaybeUninit,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU8, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::SystemTime,
};

/// Automatic indexing budget used only when no host/container limit is known
/// (512 MiB). The historical `MIN_` name is retained as public API; discovered
/// hard limits may intentionally produce a smaller budget.
pub const MIN_MEMORY_BUDGET_BYTES: usize = 512 * 1024 * 1024;
/// The maximum automatic indexing memory budget (8 GiB).
pub const MAX_MEMORY_BUDGET_BYTES: usize = 8 * 1024 * 1024 * 1024;
/// Default byte count used by future I/O implementations for each read batch.
pub const DEFAULT_READ_BATCH_BYTES: usize = 256 * 1024;
/// Default maximum accepted source size (64 MiB).
pub const DEFAULT_MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const FILE_READ_ATTEMPTS: usize = 3;
const READY_QUEUE_CAPACITY: usize = 2;
const MIN_POOL_BUFFER_BYTES: usize = BufferClass::FourKiB.capacity();
const MIN_CPU_ARENA_BYTES_PER_WORKER: usize = 64 * 1024;
const MAX_RUNTIME_WORKERS: usize = 256;
const MIN_READY_QUEUE_EDGE_BYTES: usize = BufferClass::FourKiB.capacity();

/// Backend requested by the caller for I/O work.
///
/// `IoUring` is intentionally only a request at this stage. The runtime has no
/// `io_uring` dependency, so it resolves to the portable threaded backend until
/// a platform-specific I/O implementation is linked by a future integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoBackendSelection {
    /// Select the best linked backend. Phase 1 resolves this to `Threaded`.
    Auto,
    /// Use the portable, dedicated threaded I/O implementation.
    Threaded,
    /// Request io_uring; falls back to `Threaded` in this foundation crate.
    IoUring,
}

/// The implementation actually selected for a runtime run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveIoBackend {
    /// A dedicated portable I/O worker implementation.
    Threaded,
}

/// Records a requested backend and any deliberate fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoBackendResolution {
    /// Backend requested by configuration.
    pub requested: IoBackendSelection,
    /// Backend the current build can execute.
    pub effective: EffectiveIoBackend,
    /// Stable explanation for an explicit request that cannot be fulfilled.
    pub fallback_reason: Option<&'static str>,
}

impl IoBackendSelection {
    /// Resolve this request without probing the filesystem or kernel.
    #[must_use]
    pub const fn resolve(self) -> IoBackendResolution {
        match self {
            Self::Auto | Self::Threaded => IoBackendResolution {
                requested: self,
                effective: EffectiveIoBackend::Threaded,
                fallback_reason: None,
            },
            Self::IoUring => IoBackendResolution {
                requested: self,
                effective: EffectiveIoBackend::Threaded,
                fallback_reason: Some(
                    "io_uring support is not linked into graphoxide-index-runtime",
                ),
            },
        }
    }
}

/// Memory limits discovered by the control plane.
///
/// Discovery is performed while the control plane constructs an
/// [`IndexRuntimeConfig`], before I/O or CPU workers exist. The parser-facing
/// [`ReadyInput`] API remains filesystem-free. If both values are present, the
/// cgroup limit wins when it is lower.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeMemoryLimits {
    /// Physical-memory limit observed by the host integration, if known.
    pub host_memory_bytes: Option<usize>,
    /// Cgroup or job-object limit observed by the host integration, if known.
    pub cgroup_memory_bytes: Option<usize>,
}

impl RuntimeMemoryLimits {
    /// Discover host and cgroup memory ceilings from the local control-plane
    /// environment.
    ///
    /// Failures are deliberately treated as unknown limits. This keeps the
    /// runtime portable and conservative: [`automatic_budget_bytes`](Self::automatic_budget_bytes)
    /// then selects its documented 512 MiB unknown-limit default rather than failing an
    /// extraction solely because a host does not expose Linux procfs/cgroups.
    #[must_use]
    pub fn discover() -> Self {
        Self {
            host_memory_bytes: std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|contents| parse_meminfo_total_bytes(&contents)),
            cgroup_memory_bytes: discover_cgroup_memory_limit(),
        }
    }

    /// Return the tightest non-zero applicable memory limit.
    #[must_use]
    pub fn effective_memory_bytes(self) -> Option<usize> {
        [self.host_memory_bytes, self.cgroup_memory_bytes]
            .into_iter()
            .flatten()
            .filter(|limit| *limit > 0)
            .min()
    }

    /// Compute the automatic memory budget from the effective limit.
    ///
    /// Unknown limits choose the documented conservative default. Known limits
    /// use one eighth of the limit, bounded above by 8 GiB. The 512 MiB default
    /// is not a floor for a discovered limit: a constrained container must
    /// never be assigned a runtime budget larger than its hard ceiling.
    #[must_use]
    pub fn automatic_budget_bytes(self) -> usize {
        self.effective_memory_bytes()
            .map_or(MIN_MEMORY_BUDGET_BYTES, |limit| {
                (limit / 8).min(MAX_MEMORY_BUDGET_BYTES).min(limit)
            })
    }
}

fn parse_meminfo_total_bytes(contents: &str) -> Option<usize> {
    contents.lines().find_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        (fields.next()? == "MemTotal:").then_some(())?;
        let kibibytes = fields.next()?.parse::<usize>().ok()?;
        kibibytes.checked_mul(1024)
    })
}

fn parse_cgroup_memory_limit(contents: &str) -> Option<usize> {
    let value = contents.trim();
    if value.is_empty() || value == "max" {
        return None;
    }
    let limit = value.parse::<u128>().ok()?;
    // cgroup v1 reports an effectively unlimited controller as a value close
    // to signed 64-bit `MAX`, rather than the v2 `max` sentinel. Treat that
    // representation as unknown so it cannot inflate a constrained runtime
    // when host-memory discovery is unavailable.
    if limit >= (1_u128 << 60) {
        return None;
    }
    usize::try_from(limit).ok().filter(|limit| *limit > 0)
}

fn discover_cgroup_memory_limit() -> Option<usize> {
    let mut paths = vec![
        // cgroup v2 unified hierarchy.
        PathBuf::from("/sys/fs/cgroup/memory.max"),
        // Common cgroup v1 memory controller mount.
        PathBuf::from("/sys/fs/cgroup/memory/memory.limit_in_bytes"),
    ];
    if let Ok(proc_self_cgroup) = std::fs::read_to_string("/proc/self/cgroup") {
        paths.extend(cgroup_memory_limit_paths(&proc_self_cgroup));
    }
    paths.sort();
    paths.dedup();
    paths
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|contents| parse_cgroup_memory_limit(&contents))
        .min()
}

fn cgroup_memory_limit_paths(proc_self_cgroup: &str) -> Vec<PathBuf> {
    proc_self_cgroup
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ':');
            let _hierarchy = fields.next()?;
            let controllers = fields.next()?;
            let relative = fields.next()?.trim_start_matches('/');
            let base = PathBuf::from("/sys/fs/cgroup").join(relative);
            if controllers.is_empty() {
                Some(base.join("memory.max"))
            } else if controllers
                .split(',')
                .any(|controller| controller == "memory")
            {
                Some(
                    PathBuf::from("/sys/fs/cgroup/memory")
                        .join(relative)
                        .join("memory.limit_in_bytes"),
                )
            } else {
                None
            }
        })
        .collect()
}

/// Fixed subdivisions of an indexing managed-memory budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudget {
    /// Total managed-memory budget for this runtime.
    pub total_bytes: usize,
    /// I/O fill buffers (20%).
    pub io_buffers_bytes: usize,
    /// Ready source buffers handed to CPU work (20%).
    pub ready_inputs_bytes: usize,
    /// Registered format-parser allowances and resolver snapshots (20%).
    pub cpu_arenas_bytes: usize,
    /// Cache pages and graph run pages (25%).
    pub cache_and_runs_bytes: usize,
    /// Query service reserve (5%).
    pub query_reserve_bytes: usize,
    /// Emergency/cancellation reserve (10%, including integer remainder).
    pub emergency_reserve_bytes: usize,
}

impl MemoryBudget {
    /// Split a non-zero memory budget according to the phase-one contract.
    #[must_use]
    pub fn from_total(total_bytes: usize) -> Self {
        let io_buffers_bytes = total_bytes / 5;
        let ready_inputs_bytes = total_bytes / 5;
        let cpu_arenas_bytes = total_bytes / 5;
        let cache_and_runs_bytes = total_bytes / 4;
        let query_reserve_bytes = total_bytes / 20;
        let allocated = io_buffers_bytes
            .saturating_add(ready_inputs_bytes)
            .saturating_add(cpu_arenas_bytes)
            .saturating_add(cache_and_runs_bytes)
            .saturating_add(query_reserve_bytes);
        Self {
            total_bytes,
            io_buffers_bytes,
            ready_inputs_bytes,
            cpu_arenas_bytes,
            cache_and_runs_bytes,
            query_reserve_bytes,
            emergency_reserve_bytes: total_bytes.saturating_sub(allocated),
        }
    }
}

/// Configuration shared by the control, I/O, and compute planes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexRuntimeConfig {
    /// Total budget used by [`MemoryBudget`].
    pub memory_budget_bytes: usize,
    /// Number of dedicated I/O workers.
    pub io_workers: usize,
    /// Number of fixed-owner CPU extraction workers.
    pub compute_workers: usize,
    /// I/O backend request.
    pub io_backend: IoBackendSelection,
    /// Requested single-read target size.
    pub read_batch_bytes: usize,
}

impl IndexRuntimeConfig {
    /// Build defaults from a discovered CPU count and externally supplied
    /// host/cgroup limits. A CPU count of zero is normalized to one.
    #[must_use]
    pub fn from_limits(available_cpus: usize, limits: RuntimeMemoryLimits) -> Self {
        let available_cpus = available_cpus.max(1);
        let io_workers = if available_cpus <= 4 {
            1
        } else {
            cmp::min(8, available_cpus.div_ceil(4))
        };
        let compute_workers = available_cpus.saturating_sub(1 + io_workers).max(1);
        Self {
            memory_budget_bytes: limits.automatic_budget_bytes(),
            io_workers,
            compute_workers,
            io_backend: IoBackendSelection::Auto,
            read_batch_bytes: DEFAULT_READ_BATCH_BYTES,
        }
    }

    /// Return the deterministic partitioning for this configuration.
    #[must_use]
    pub fn memory_budget(self) -> MemoryBudget {
        MemoryBudget::from_total(self.memory_budget_bytes)
    }

    /// Validate explicit caller overrides before any work is admitted.
    pub fn validate(self) -> Result<(), RuntimeConfigError> {
        if self.memory_budget_bytes == 0 {
            return Err(RuntimeConfigError::ZeroMemoryBudget);
        }
        if self.io_workers == 0 {
            return Err(RuntimeConfigError::ZeroIoWorkers);
        }
        if self.compute_workers == 0 {
            return Err(RuntimeConfigError::ZeroComputeWorkers);
        }
        if self.read_batch_bytes == 0 {
            return Err(RuntimeConfigError::ZeroReadBatch);
        }
        if self.memory_budget().ready_inputs_bytes < MIN_POOL_BUFFER_BYTES {
            return Err(RuntimeConfigError::ReadyInputBudgetTooSmall);
        }
        Ok(())
    }

    /// Derive the bounded worker and per-owner pool layout for an admitted
    /// request set.
    ///
    /// Explicit worker counts are upper bounds, never allocation commands:
    /// this prevents a malformed CLI configuration from allocating an
    /// unbounded I/O-by-CPU queue matrix or more worker-local state than the
    /// configured managed-memory partitions can support.
    fn bounded_layout(self, request_count: usize) -> RuntimeWorkerLayout {
        let budget = self.memory_budget();
        let request_count = request_count.max(1);
        let max_io_by_pool = (budget.io_buffers_bytes / MIN_POOL_BUFFER_BYTES).max(1);
        let max_compute_by_arena =
            (budget.cpu_arenas_bytes / MIN_CPU_ARENA_BYTES_PER_WORKER).max(1);
        let max_ready_edges = (budget.ready_inputs_bytes / MIN_READY_QUEUE_EDGE_BYTES).max(1);

        let io_workers = self
            .io_workers
            .min(request_count)
            .min(max_io_by_pool)
            .min(max_ready_edges)
            .clamp(1, MAX_RUNTIME_WORKERS);
        let compute_workers = self
            .compute_workers
            .min(request_count)
            .min(max_compute_by_arena)
            .min((max_ready_edges / io_workers).max(1))
            .clamp(1, MAX_RUNTIME_WORKERS);
        let io_pool_bytes = (budget.io_buffers_bytes / io_workers).max(MIN_POOL_BUFFER_BYTES);
        let read_batch_bytes = self.read_batch_bytes.min(io_pool_bytes).max(1);

        RuntimeWorkerLayout {
            io_workers,
            compute_workers,
            io_pool_bytes,
            read_batch_bytes,
        }
    }

    /// Describe the bounded layout selected for a finite set of input
    /// requests.  This is deliberately data-only: callers can expose what was
    /// actually admitted without granting parser-facing code filesystem or
    /// scheduler capabilities.
    ///
    /// The requested worker counts in [`IndexRuntimeConfig`] are upper bounds.
    /// This evidence records the effective values after the memory and request
    /// bounds have been applied, which is the only honest value for runtime
    /// telemetry and benchmark reports.
    #[must_use]
    pub fn execution_evidence(self, request_count: usize) -> RuntimeExecutionEvidence {
        let budget = self.memory_budget();
        let layout = self.bounded_layout(request_count);
        RuntimeExecutionEvidence {
            admitted_requests: request_count,
            effective_io_workers: layout.io_workers,
            effective_compute_workers: layout.compute_workers,
            effective_read_batch_bytes: layout.read_batch_bytes,
            io_pool_bytes_per_worker: layout.io_pool_bytes,
            io_buffers_bytes: budget.io_buffers_bytes,
            ready_inputs_bytes: budget.ready_inputs_bytes,
            cpu_arenas_bytes: budget.cpu_arenas_bytes,
            cache_and_runs_bytes: budget.cache_and_runs_bytes,
            query_reserve_bytes: budget.query_reserve_bytes,
            emergency_reserve_bytes: budget.emergency_reserve_bytes,
        }
    }
}

impl Default for IndexRuntimeConfig {
    fn default() -> Self {
        let available_cpus =
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        Self::from_limits(available_cpus, RuntimeMemoryLimits::discover())
    }
}

/// Invalid explicit runtime configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeConfigError {
    /// A credit and pool partition cannot be created with no memory.
    ZeroMemoryBudget,
    /// At least one I/O worker is required.
    ZeroIoWorkers,
    /// At least one compute worker is required.
    ZeroComputeWorkers,
    /// A zero-byte read target cannot make progress.
    ZeroReadBatch,
    /// The ready-input partition cannot admit even the smallest pooled source.
    ReadyInputBudgetTooSmall,
}

/// Effective bounded worker layout derived from an admitted request set.
///
/// This is intentionally internal to the runtime until runtime telemetry has
/// a stable public schema. Tests retain it as the contract that prevents
/// caller-provided worker counts from exceeding memory-backed limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeWorkerLayout {
    io_workers: usize,
    compute_workers: usize,
    io_pool_bytes: usize,
    read_batch_bytes: usize,
}

/// Immutable evidence of the layout admitted for one finite batch.
///
/// This is intentionally not a live counter set.  It reports the runtime
/// controls and memory partitions selected before work begins, so producing a
/// telemetry sidecar cannot itself introduce synchronization or I/O into the
/// extractor pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeExecutionEvidence {
    /// Number of input requests that constrained the layout.
    pub admitted_requests: usize,
    /// I/O owners actually needed after bounded layout selection.
    pub effective_io_workers: usize,
    /// CPU owners actually needed after bounded layout selection.
    pub effective_compute_workers: usize,
    /// Read target after per-owner I/O-pool clamping.
    pub effective_read_batch_bytes: usize,
    /// I/O buffer capacity owned by each effective I/O worker.
    pub io_pool_bytes_per_worker: usize,
    /// Total I/O-buffer partition.
    pub io_buffers_bytes: usize,
    /// Ready-source partition.
    pub ready_inputs_bytes: usize,
    /// CPU scratch-arena partition.
    pub cpu_arenas_bytes: usize,
    /// Cache and graph-run partition.
    pub cache_and_runs_bytes: usize,
    /// Query-service reserve.
    pub query_reserve_bytes: usize,
    /// Emergency and cancellation reserve.
    pub emergency_reserve_bytes: usize,
}

/// Allocation class used by [`BufferLease`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferClass {
    /// 4 KiB page.
    FourKiB,
    /// 16 KiB page.
    SixteenKiB,
    /// 64 KiB page.
    SixtyFourKiB,
    /// 256 KiB page.
    TwoFiftySixKiB,
    /// 1 MiB page.
    OneMiB,
    /// An explicit bounded size outside the standard classes.
    Exact(usize),
}

impl BufferClass {
    /// Select the smallest standard class that can hold `capacity`.
    #[must_use]
    pub const fn for_capacity(capacity: usize) -> Self {
        match capacity {
            0..=4_096 => Self::FourKiB,
            4_097..=16_384 => Self::SixteenKiB,
            16_385..=65_536 => Self::SixtyFourKiB,
            65_537..=262_144 => Self::TwoFiftySixKiB,
            262_145..=1_048_576 => Self::OneMiB,
            _ => Self::Exact(capacity),
        }
    }

    /// Capacity represented by this class.
    #[must_use]
    pub const fn capacity(self) -> usize {
        match self {
            Self::FourKiB => 4_096,
            Self::SixteenKiB => 16_384,
            Self::SixtyFourKiB => 65_536,
            Self::TwoFiftySixKiB => 262_144,
            Self::OneMiB => 1_048_576,
            Self::Exact(capacity) => capacity,
        }
    }
}

/// An owned byte allocation that can move from I/O to CPU without copying.
///
/// The I/O owner obtains these from its local size-class pool. A successful
/// read transfers unique ownership to CPU work; failed or superseded attempts
/// return their allocation to that same owner without a cross-worker lock.
#[derive(Debug, PartialEq, Eq)]
pub struct BufferLease {
    class: BufferClass,
    bytes: Vec<u8>,
}

impl BufferLease {
    /// Allocate a standard-class buffer with logical length zero.
    #[must_use]
    pub fn with_capacity(requested_capacity: usize) -> Self {
        let class = BufferClass::for_capacity(requested_capacity);
        Self {
            class,
            bytes: Vec::with_capacity(class.capacity()),
        }
    }

    /// Allocate an initialized buffer suitable for a future direct `Read` call.
    #[must_use]
    pub fn readable(requested_capacity: usize) -> Self {
        let class = BufferClass::for_capacity(requested_capacity);
        Self {
            class,
            bytes: vec![0; class.capacity()],
        }
    }

    /// Wrap an already-owned source allocation without copying it.
    #[must_use]
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        let class = BufferClass::for_capacity(bytes.capacity());
        Self { class, bytes }
    }

    /// Return the allocation class selected at construction.
    #[must_use]
    pub const fn class(&self) -> BufferClass {
        self.class
    }

    /// Return borrowed source bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Return mutable bytes to the single owner before handoff.
    #[must_use]
    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Return the logical byte length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Return the allocation capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// Whether this lease holds no logical bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Change the logical length after a partial read into [`Self::readable`].
    pub fn truncate(&mut self, len: usize) {
        self.bytes.truncate(len);
    }

    /// Clear logical content while retaining the allocation for a future pool.
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    /// Consume the lease and return its owned allocation.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

/// Fixed-owner reusable allocations for one I/O worker.
///
/// This pool is deliberately not `Send` or shared. Reusing failed, retried,
/// and short-lived allocations locally improves cache locality while avoiding
/// a global allocator-side lock or a return path that could block a CPU worker.
/// Successful source allocations move to CPU and are released normally when
/// parsing finishes; they are never synchronously returned to an I/O worker.
struct IoBufferPool {
    retained_bytes: usize,
    max_retained_bytes: usize,
    four_kib: Vec<Vec<u8>>,
    sixteen_kib: Vec<Vec<u8>>,
    sixty_four_kib: Vec<Vec<u8>>,
    two_fifty_six_kib: Vec<Vec<u8>>,
    one_mib: Vec<Vec<u8>>,
}

impl IoBufferPool {
    fn new(max_retained_bytes: usize) -> Self {
        Self {
            retained_bytes: 0,
            max_retained_bytes,
            four_kib: Vec::new(),
            sixteen_kib: Vec::new(),
            sixty_four_kib: Vec::new(),
            two_fifty_six_kib: Vec::new(),
            one_mib: Vec::new(),
        }
    }

    fn take(&mut self, requested_capacity: usize) -> BufferLease {
        let class = BufferClass::for_capacity(requested_capacity);
        let capacity = class.capacity();
        let bytes = self.bucket_mut(class).and_then(|bucket| bucket.pop());
        if bytes.is_some() {
            self.retained_bytes = self.retained_bytes.saturating_sub(capacity);
        }
        let mut bytes = bytes.unwrap_or_else(|| Vec::with_capacity(capacity));
        // `Read` requires an initialized slice. The source is still allocated
        // exactly once and is handed to CPU without a second source copy.
        bytes.resize(capacity, 0);
        BufferLease { class, bytes }
    }

    fn recycle(&mut self, buffer: BufferLease) {
        let class = buffer.class;
        let capacity = class.capacity();
        if capacity > self.max_retained_bytes.saturating_sub(self.retained_bytes) {
            return;
        }
        let mut bytes = buffer.into_vec();
        bytes.clear();
        // Exact-size buffers are intentionally not retained. Standard-class
        // allocations must retain their class capacity or they are discarded.
        if bytes.capacity() != capacity {
            return;
        }
        let Some(bucket) = self.bucket_mut(class) else {
            return;
        };
        bucket.push(bytes);
        self.retained_bytes = self.retained_bytes.saturating_add(capacity);
    }

    fn bucket_mut(&mut self, class: BufferClass) -> Option<&mut Vec<Vec<u8>>> {
        match class {
            BufferClass::FourKiB => Some(&mut self.four_kib),
            BufferClass::SixteenKiB => Some(&mut self.sixteen_kib),
            BufferClass::SixtyFourKiB => Some(&mut self.sixty_four_kib),
            BufferClass::TwoFiftySixKiB => Some(&mut self.two_fifty_six_kib),
            BufferClass::OneMiB => Some(&mut self.one_mib),
            BufferClass::Exact(_) => None,
        }
    }
}

/// Stable source identity used to restore deterministic ordering after parallel
/// execution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputIdentity {
    /// Repository-relative, normalized source path supplied by the control plane.
    pub normalized_path: Arc<str>,
    /// Position in the control plane's deterministic sorted file list.
    pub source_ordinal: u64,
}

impl InputIdentity {
    /// Create an identity from a normalized path and its stable ordinal.
    #[must_use]
    pub fn new(normalized_path: impl Into<Arc<str>>, source_ordinal: u64) -> Self {
        Self {
            normalized_path: normalized_path.into(),
            source_ordinal,
        }
    }
}

/// Input materialized by I/O and ready for an extractor to borrow or mutate.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadyInput {
    /// Deterministic identity retained through extraction and resolution.
    pub identity: InputIdentity,
    /// Immutable metadata captured by the I/O owner for this exact source
    /// generation. CPU workers use this instead of probing the source path.
    pub file_identity: FileIdentity,
    /// Strong, opaque source-generation evidence captured by the I/O owner.
    ///
    /// `None` is reserved for synthetic buffers that were not produced by a
    /// verified filesystem/backend read. The opaque digest binds physical
    /// path and root identity without revealing either value.
    source_identity_evidence: Option<SourceIdentityEvidence>,
    /// Optional raw BLAKE3 digest, filled by the CPU preflight stage.
    pub content_digest: Option<[u8; 32]>,
    buffer: BufferLease,
}

impl ReadyInput {
    /// Construct a ready input by moving a uniquely owned buffer into CPU work.
    #[must_use]
    pub fn new(identity: InputIdentity, buffer: BufferLease) -> Self {
        Self {
            identity,
            file_identity: FileIdentity {
                length_bytes: buffer.as_bytes().len() as u64,
                modified: None,
            },
            source_identity_evidence: None,
            content_digest: None,
            buffer,
        }
    }

    fn with_file_identity(
        identity: InputIdentity,
        buffer: BufferLease,
        source_identity: IoReadIdentity,
        source_identity_evidence: Option<SourceIdentityEvidence>,
    ) -> Self {
        Self {
            identity,
            file_identity: source_identity.file_identity,
            source_identity_evidence,
            content_digest: None,
            buffer,
        }
    }

    /// Return strong source-generation evidence captured around the complete
    /// source read, if the active backend can provide it.
    #[must_use]
    pub const fn source_identity_evidence(&self) -> Option<SourceIdentityEvidence> {
        self.source_identity_evidence
    }

    /// Attach a preflight content digest without allocating or copying bytes.
    #[must_use]
    pub fn with_content_digest(mut self, content_digest: [u8; 32]) -> Self {
        self.content_digest = Some(content_digest);
        self
    }

    /// Borrow input bytes for a parser.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    /// Mutably borrow the exclusive source allocation for parsers such as
    /// simd-json that intentionally use in-place parsing.
    #[must_use]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.buffer.as_mut_bytes()
    }

    /// Return the bytes retained by the owned source allocation.
    ///
    /// This deliberately reports allocation capacity rather than logical
    /// source length. A downstream stage that keeps the allocation after CPU
    /// extraction must charge this value to its own memory partition; charging
    /// [`Self::bytes`] length would undercount pooled 4 KiB pages for tiny or
    /// empty sources.
    #[must_use]
    pub fn retained_capacity_bytes(&self) -> usize {
        self.buffer.capacity()
    }

    /// Consume the input and recover the owned source allocation for reuse.
    #[must_use]
    pub fn into_buffer(self) -> BufferLease {
        self.buffer
    }
}

/// A source read admitted by the control plane.
///
/// `identity` controls deterministic output ordering. `path` is intentionally
/// retained only by the I/O request; CPU extractors receive [`ReadyInput`],
/// which has no filesystem API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReadRequest {
    /// Stable ordering identity.
    pub identity: InputIdentity,
    /// Filesystem path exclusively consumed by the I/O plane.
    pub path: PathBuf,
    /// Maximum accepted logical source size, before standard buffer-class
    /// rounding. Larger files are rejected before allocating a read buffer.
    pub max_bytes: usize,
    /// Control-plane identity captured after symlink policy and canonical-root
    /// validation. When present, I/O refuses a different source generation
    /// even if replacement happened before the worker's first probe.
    expected_identity: Option<IoReadIdentity>,
    /// Canonical root retained only for control/I/O-plane containment checks.
    /// CPU-facing [`ReadyInput`] values never receive this path capability.
    verified_root: Option<PathBuf>,
    /// Stable platform identity of the canonical root directory. Unlike a
    /// file generation it deliberately excludes directory mtime/ctime, which
    /// change when unrelated children are added or removed.
    verified_root_identity: Option<[u8; 32]>,
}

impl FileReadRequest {
    /// Construct a request with the phase-one 64 MiB source safety limit.
    #[must_use]
    pub fn new(identity: InputIdentity, path: PathBuf) -> Self {
        Self {
            identity,
            path,
            max_bytes: DEFAULT_MAX_INPUT_BYTES,
            expected_identity: None,
            verified_root: None,
            verified_root_identity: None,
        }
    }

    /// Bind a request to the regular file currently present at `path`.
    ///
    /// The final component must not be a symlink/reparse point. The captured
    /// platform identity closes the gap between control-plane discovery and an
    /// I/O worker's first observation; the ordinary before/open/after checks
    /// continue protecting the read itself.
    pub fn new_verified(identity: InputIdentity, path: PathBuf) -> io::Result<Self> {
        #[cfg(windows)]
        let expected_identity = {
            let file = open_source_nofollow(&path)?;
            windows_file_identity(&file)?
        };
        #[cfg(not(windows))]
        let expected_identity = {
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "verified source must be a regular non-symlink file",
                ));
            }
            IoReadIdentity::from_metadata(&metadata)
        };
        Ok(Self {
            identity,
            path,
            max_bytes: DEFAULT_MAX_INPUT_BYTES,
            expected_identity: Some(expected_identity),
            verified_root: None,
            verified_root_identity: None,
        })
    }

    /// Bind a verified request to a target that still resolves beneath `root`.
    ///
    /// Canonicalization is checked on both sides of the metadata identity
    /// snapshot. A replacement after the second check is caught by the stored
    /// identity before the worker reads, while a replacement during either
    /// control-plane check fails closed here.
    pub fn new_verified_under(
        identity: InputIdentity,
        path: PathBuf,
        root: &std::path::Path,
    ) -> io::Result<Self> {
        let canonical_root = fs::canonicalize(root)?;
        let root_identity_before = root_platform_identity(&canonical_root)?;
        let canonical_before = fs::canonicalize(&path)?;
        if !canonical_before.starts_with(&canonical_root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "verified source resolves outside its scan root",
            ));
        }
        let mut request = Self::new_verified(identity, canonical_before.clone())?;
        let canonical_after = fs::canonicalize(&path)?;
        let root_identity_after = root_platform_identity(&canonical_root)?;
        if canonical_after != canonical_before
            || !canonical_after.starts_with(&canonical_root)
            || root_identity_after != root_identity_before
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "verified source changed while binding it to the scan root",
            ));
        }
        request.verified_root = Some(canonical_root);
        request.verified_root_identity = root_identity_before;
        Ok(request)
    }

    /// Set a caller-selected source limit.
    #[must_use]
    pub const fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Return the strong source identity captured while this verified request
    /// was admitted. Unverified requests and unsupported platforms return
    /// `None` and therefore cannot authorize metadata-only cache reuse.
    #[must_use]
    pub fn source_identity_evidence(&self) -> Option<SourceIdentityEvidence> {
        self.expected_identity
            .and_then(|identity| self.bound_source_identity_evidence(identity))
    }

    /// Begin a metadata-only cache validation window.
    ///
    /// The returned guard holds the exact no-follow source handle observed at
    /// the start of the window. Callers may perform a cache lookup and decode
    /// while holding it, then must call [`MetadataOnlyValidationGuard::finish`]
    /// before accepting the cached result. Only requests verified beneath a
    /// canonical root can produce a guard.
    pub fn begin_metadata_only_validation(
        &self,
        cancellation: &RuntimeCancellation,
    ) -> io::Result<Option<MetadataOnlyValidationGuard>> {
        if cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "metadata-only cache validation cancelled",
            ));
        }
        let Some(root) = self.verified_root.as_ref() else {
            return Ok(None);
        };
        let Some(root_identity) = self.verified_root_identity else {
            return Ok(None);
        };
        if !verified_path_binding_is_current(&self.path, root, root_identity)? {
            return Ok(None);
        }
        let file = open_source_nofollow(&self.path)?;
        let identity = opened_source_identity(&file)?;
        if self
            .expected_identity
            .is_some_and(|expected| expected != identity)
        {
            return Ok(None);
        }
        let Some(evidence) = self.bound_source_identity_evidence(identity) else {
            return Ok(None);
        };
        Ok(Some(MetadataOnlyValidationGuard {
            path: self.path.clone(),
            verified_root: root.clone(),
            verified_root_identity: root_identity,
            file,
            identity,
            evidence,
        }))
    }

    fn bound_source_identity_evidence(
        &self,
        identity: IoReadIdentity,
    ) -> Option<SourceIdentityEvidence> {
        let root = self.verified_root.as_ref()?;
        let root_identity = self.verified_root_identity?;
        let platform_digest = identity.platform_identity_digest()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"graphoxide-bound-source-identity-v1\0");
        hasher.update(&platform_digest);
        hasher.update(&root_identity);
        hash_path_identity(&mut hasher, root);
        hasher.update(b"\0");
        hash_path_identity(&mut hasher, &self.path);
        Some(SourceIdentityEvidence(*hasher.finalize().as_bytes()))
    }
}

/// Portable identity snapshot used to detect changes while a source is read.
///
/// Length and modification time are available across supported backends and
/// are checked before open, after open, after read, and again through the
/// backend path probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    /// Source length at the observation point.
    pub length_bytes: u64,
    /// Last-modified instant when available from the filesystem.
    pub modified: Option<SystemTime>,
}

/// Opaque, persistent evidence for one strongly identified source generation.
///
/// The digest binds the admitted canonical root and physical source path to
/// Unix device/inode/ctime identity or Windows volume, 128-bit file ID,
/// creation time, last-write time, and change time.
/// On Windows, change time detects ordinary same-size rewrites even when the
/// caller restores last-write time. It is not an authenticity primitive:
/// metadata-only reuse assumes no same-user actor with `FILE_WRITE_ATTRIBUTES`
/// is concurrently forging the source's identity fields.
/// Cache callers must additionally bind their normalized logical path,
/// extractor version, canonical options, and content evidence. Other targets
/// safely fall back to payload reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceIdentityEvidence([u8; 32]);

impl SourceIdentityEvidence {
    /// Restore opaque evidence persisted in a trusted Graphoxide manifest.
    #[must_use]
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Raw stable digest for a cache envelope or manifest.
    #[must_use]
    pub const fn digest(self) -> [u8; 32] {
        self.0
    }
}

/// Opaque strong identity for a caller-owned, already-open regular file.
///
/// This is intended for bounded control-plane readers such as the runtime
/// manifest loader. Compare observations from the same held handle before and
/// after reading. It is not source-path-bound evidence and must not be used to
/// authorize metadata-only source cache hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenedFileIdentity {
    identity: IoReadIdentity,
}

impl OpenedFileIdentity {
    /// Logical file length at this handle observation.
    #[must_use]
    pub const fn length_bytes(self) -> u64 {
        self.identity.file_identity.length_bytes
    }
}

/// Validate a caller-owned opened handle as regular, non-reparse, single-link,
/// and strongly identified by the current filesystem.
///
/// `Ok(None)` means the platform/filesystem did not provide strong generation
/// evidence; callers must disable persistent replay and take their cold path.
/// The caller remains responsible for opening the handle with a final-component
/// no-follow policy and validating any path/root containment it requires.
pub fn validate_opened_regular_single_link(file: &File) -> io::Result<Option<OpenedFileIdentity>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "opened cache control file must be regular",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "opened cache control file must have exactly one link",
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::MetadataExt as _, io::AsRawHandle as _};
        use windows_sys::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
                FILE_ATTRIBUTE_REPARSE_POINT,
            },
        };
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "opened cache control file must not be a reparse point",
            ));
        }
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` owns a live handle and `information` is exact writable
        // output storage for this synchronous query.
        let succeeded = unsafe {
            GetFileInformationByHandle(
                file.as_raw_handle() as HANDLE,
                std::ptr::addr_of_mut!(information),
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }
        if information.nNumberOfLinks != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "opened cache control file must have exactly one link",
            ));
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        return Ok(None);
    }
    let identity = opened_source_identity(file)?;
    if identity.strong_revision.is_none() {
        return Ok(None);
    }
    Ok(Some(OpenedFileIdentity { identity }))
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            length_bytes: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

/// Internal identity snapshot used only by the I/O plane while a file is
/// being read. The portable public identity remains stable. Unix builds also
/// compare device, inode, and ctime. Windows ordinary read-race checks compare
/// the handle's volume serial, legacy file index, creation time, and last-write
/// time; persistent strong evidence additionally requires the full 128-bit
/// file ID and change time. Under the same-user metadata-forgery exclusion,
/// these generation fields catch ordinary equal-length replacement and writes
/// that a coarse or restored portable mtime misses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoReadIdentity {
    /// The portable snapshot copied into [`ReadyInput`] after a stable read.
    pub file_identity: FileIdentity,
    /// Opaque backend revision fields used only to compare read observations.
    ///
    /// Filesystem backends use device/inode/ctime on Unix and the legacy
    /// volume/file-index plus handle timestamps on Windows. Test and alternate
    /// I/O backends may provide any deterministic generation values; the
    /// runtime only checks equality and never interprets these values.
    revision: [u64; 4],
    /// Generation evidence strong enough for persistent metadata-only reuse
    /// within the documented same-user metadata-forgery exclusion. On Windows
    /// this contains the volume, full 128-bit file ID, creation time,
    /// last-write time, and change time. Injected backends and filesystems
    /// without trustworthy generation fields deliberately leave this absent
    /// while retaining ordinary read-race checks.
    strong_revision: Option<[u64; 6]>,
}

impl IoReadIdentity {
    /// Construct an identity observation for an injected I/O backend.
    ///
    /// `revision` must change whenever the underlying source generation
    /// changes even if its portable metadata remains equal. This lets the
    /// runtime reject replacement and racing writes without giving CPU work
    /// access to the backend or source path.
    #[must_use]
    pub const fn new(file_identity: FileIdentity, revision: [u64; 4]) -> Self {
        Self {
            file_identity,
            revision,
            strong_revision: None,
        }
    }

    const fn with_strong_revision(
        file_identity: FileIdentity,
        revision: [u64; 4],
        strong_revision: [u64; 6],
    ) -> Self {
        Self {
            file_identity,
            revision,
            strong_revision: Some(strong_revision),
        }
    }

    /// Produce opaque strong evidence suitable for persistent cache
    /// validation. Unsupported platforms deliberately return `None`.
    #[must_use]
    fn platform_identity_digest(self) -> Option<[u8; 32]> {
        #[cfg(any(unix, windows))]
        {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"graphoxide-source-identity-v1\0");
            #[cfg(unix)]
            hasher.update(b"unix\0");
            #[cfg(windows)]
            hasher.update(b"windows\0");
            hasher.update(&self.file_identity.length_bytes.to_le_bytes());
            hash_system_time(&mut hasher, self.file_identity.modified);
            for component in self.strong_revision? {
                hasher.update(&component.to_le_bytes());
            }
            Some(*hasher.finalize().as_bytes())
        }
        #[cfg(not(any(unix, windows)))]
        {
            None
        }
    }

    #[cfg(not(windows))]
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            let revision = [
                metadata.dev(),
                metadata.ino(),
                metadata.ctime() as u64,
                metadata.ctime_nsec() as u64,
            ];
            let portable = FileIdentity::from_metadata(metadata);
            unix_strong_identity_components(revision).map_or_else(
                || Self::new(portable, revision),
                |strong| Self::with_strong_revision(portable, revision, strong),
            )
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self::new(FileIdentity::from_metadata(metadata), [0; 4])
        }
    }
}

#[cfg(any(unix, test))]
fn unix_strong_identity_components(revision: [u64; 4]) -> Option<[u64; 6]> {
    let [device, inode, ctime_seconds, ctime_nanoseconds] = revision;
    if device == 0 || inode == 0 || (ctime_seconds == 0 && ctime_nanoseconds == 0) {
        return None;
    }
    Some([device, inode, ctime_seconds, ctime_nanoseconds, 0, 0])
}

#[cfg(any(windows, test))]
fn windows_strong_identity_components(
    observed_volume: u64,
    file_id_volume: u64,
    file_id: [u8; 16],
    creation_time: i64,
    last_write_time: i64,
    change_time: i64,
) -> Option<[u64; 6]> {
    if observed_volume == 0
        || file_id_volume != observed_volume
        || file_id == [0; 16]
        || creation_time == 0
        || last_write_time == 0
        || change_time == 0
    {
        return None;
    }
    Some([
        file_id_volume,
        u64::from_le_bytes(file_id[..8].try_into().expect("8-byte file ID half")),
        u64::from_le_bytes(file_id[8..].try_into().expect("8-byte file ID half")),
        creation_time as u64,
        last_write_time as u64,
        change_time as u64,
    ])
}

#[cfg(unix)]
fn hash_path_identity(hasher: &mut blake3::Hasher, path: &Path) {
    use std::os::unix::ffi::OsStrExt as _;
    hasher.update(path.as_os_str().as_bytes());
}

#[cfg(windows)]
fn hash_path_identity(hasher: &mut blake3::Hasher, path: &Path) {
    use std::os::windows::ffi::OsStrExt as _;
    for unit in path.as_os_str().encode_wide() {
        hasher.update(&unit.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn hash_path_identity(hasher: &mut blake3::Hasher, path: &Path) {
    hasher.update(path.to_string_lossy().as_bytes());
}

#[cfg(unix)]
fn root_platform_identity(path: &Path) -> io::Result<Option<[u8; 32]>> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "verified root must be a non-symlink directory",
        ));
    }
    if metadata.dev() == 0 || metadata.ino() == 0 {
        return Ok(None);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"graphoxide-root-identity-v1\0unix\0");
    hasher.update(&metadata.dev().to_le_bytes());
    hasher.update(&metadata.ino().to_le_bytes());
    Ok(Some(*hasher.finalize().as_bytes()))
}

#[cfg(windows)]
fn root_platform_identity(path: &Path) -> io::Result<Option<[u8; 32]>> {
    use std::os::windows::{fs::MetadataExt as _, fs::OpenOptionsExt as _, io::AsRawHandle as _};
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        },
    };

    let mut options = OpenOptions::new();
    options
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options.open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "verified root must be a non-reparse directory",
        ));
    }
    let mut basic = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `directory` owns a live handle and `basic` is exact writable
    // output storage for this synchronous query.
    let succeeded = unsafe {
        GetFileInformationByHandle(
            directory.as_raw_handle() as HANDLE,
            std::ptr::addr_of_mut!(basic),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut id = FILE_ID_INFO::default();
    // SAFETY: same live handle and exact `FILE_ID_INFO` output size.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            directory.as_raw_handle() as HANDLE,
            FileIdInfo,
            std::ptr::addr_of_mut!(id).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if succeeded == 0
        || id.VolumeSerialNumber != u64::from(basic.dwVolumeSerialNumber)
        || id.VolumeSerialNumber == 0
        || id.FileId.Identifier == [0; 16]
    {
        return Ok(None);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"graphoxide-root-identity-v1\0windows\0");
    hasher.update(&id.VolumeSerialNumber.to_le_bytes());
    hasher.update(&id.FileId.Identifier);
    Ok(Some(*hasher.finalize().as_bytes()))
}

#[cfg(not(any(unix, windows)))]
fn root_platform_identity(_path: &Path) -> io::Result<Option<[u8; 32]>> {
    Ok(None)
}

#[cfg(any(unix, windows))]
fn hash_system_time(hasher: &mut blake3::Hasher, value: Option<SystemTime>) {
    match value {
        None => {
            hasher.update(&[0]);
        }
        Some(value) => match value.duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => {
                hasher.update(&[1]);
                hasher.update(&duration.as_secs().to_le_bytes());
                hasher.update(&duration.subsec_nanos().to_le_bytes());
            }
            Err(error) => {
                let duration = error.duration();
                hasher.update(&[2]);
                hasher.update(&duration.as_secs().to_le_bytes());
                hasher.update(&duration.subsec_nanos().to_le_bytes());
            }
        },
    }
}

/// Handle-held validation for a manifest-authorized metadata-only cache hit.
///
/// Keeping the original source handle open across lookup and decode prevents
/// two independent path probes from blessing different source generations.
/// `finish` also verifies that the path still resolves to this handle beneath
/// its admitted root before a caller may accept the cached value.
#[derive(Debug)]
pub struct MetadataOnlyValidationGuard {
    path: PathBuf,
    verified_root: PathBuf,
    verified_root_identity: [u8; 32],
    file: File,
    identity: IoReadIdentity,
    evidence: SourceIdentityEvidence,
}

impl MetadataOnlyValidationGuard {
    /// Evidence a cache envelope must match before it is worth decoding.
    #[must_use]
    pub const fn evidence(&self) -> SourceIdentityEvidence {
        self.evidence
    }

    /// Finish the validation window after cache lookup and decoding.
    ///
    /// `Ok(false)` is a safe cache miss: the held source or its current path
    /// binding changed. I/O faults remain errors so callers can surface the
    /// same source-read diagnostic they would have produced on a cold path.
    pub fn finish(self, cancellation: &RuntimeCancellation) -> io::Result<bool> {
        if cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "metadata-only cache validation cancelled",
            ));
        }
        let held_after = opened_source_identity(&self.file)?;
        if held_after != self.identity
            || !verified_path_binding_is_current(
                &self.path,
                &self.verified_root,
                self.verified_root_identity,
            )?
        {
            return Ok(false);
        }
        let current = open_source_nofollow(&self.path)?;
        let current_identity = opened_source_identity(&current)?;
        if current_identity != self.identity {
            return Ok(false);
        }
        if cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "metadata-only cache validation cancelled",
            ));
        }
        verified_path_binding_is_current(
            &self.path,
            &self.verified_root,
            self.verified_root_identity,
        )
    }
}

fn verified_path_binding_is_current(
    path: &Path,
    root: &Path,
    expected_root_identity: [u8; 32],
) -> io::Result<bool> {
    if root_platform_identity(root)? != Some(expected_root_identity) {
        return Ok(false);
    }
    let current = match fs::canonicalize(path) {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(current == path && current.starts_with(root))
}

/// I/O-only adapter for source metadata and byte reads.
///
/// The runtime invokes this trait exclusively on dedicated I/O workers. It is
/// intentionally absent from [`ReadyInput`] and the CPU computation closure,
/// so injecting a backend for testing, sandboxing, or platform integration
/// cannot grant filesystem access to extractors.
pub trait IoReadBackend: Send + Sync + 'static {
    /// Observe the source identity before opening it.
    fn probe(&self, request: &FileReadRequest) -> io::Result<IoReadIdentity>;

    /// Open a source after queue and byte-credit admission.
    fn open(&self, request: &FileReadRequest) -> io::Result<Box<dyn IoReadHandle>>;
}

/// Open source handle returned only to the I/O plane by [`IoReadBackend`].
pub trait IoReadHandle: Send {
    /// Observe the opened source generation.
    fn identity(&self) -> io::Result<IoReadIdentity>;

    /// Read one caller-bounded batch of source bytes.
    fn read_batch(&mut self, destination: &mut [u8]) -> io::Result<usize>;
}

#[derive(Debug, Default)]
struct FileSystemIoBackend;

impl IoReadBackend for FileSystemIoBackend {
    fn probe(&self, request: &FileReadRequest) -> io::Result<IoReadIdentity> {
        #[cfg(windows)]
        {
            let file = open_source_nofollow(&request.path)?;
            windows_file_identity(&file)
        }
        #[cfg(not(windows))]
        {
            fs::metadata(&request.path).map(|metadata| IoReadIdentity::from_metadata(&metadata))
        }
    }

    fn open(&self, request: &FileReadRequest) -> io::Result<Box<dyn IoReadHandle>> {
        let file = open_source_nofollow(&request.path)?;
        #[cfg(windows)]
        windows_file_identity(&file)?;
        Ok(Box::new(FileSystemIoHandle { file }))
    }
}

fn open_source_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // O_NOFOLLOW alone does not prevent a FIFO substitution from blocking
        // before the held-handle regular-file validation. O_NONBLOCK is inert
        // for regular files and makes special-file rejection bounded.
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        // Do not follow a final-component reparse replacement. Ancestor
        // junctions are still resolved by Windows, so the stable handle
        // identity is compared before/open/after to detect their retargeting.
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn opened_source_identity(file: &File) -> io::Result<IoReadIdentity> {
    #[cfg(windows)]
    {
        windows_file_identity(file)
    }
    #[cfg(not(windows))]
    {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "verified source must be a regular file",
            ));
        }
        Ok(IoReadIdentity::from_metadata(&metadata))
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> io::Result<IoReadIdentity> {
    use std::os::windows::{fs::MetadataExt as _, io::AsRawHandle as _};
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FileBasicInfo, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
            FILE_ID_INFO,
        },
    };

    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "verified source must be a regular non-reparse file",
        ));
    }

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns a live handle for the duration of the call and
    // `information` points to initialized, writable storage of the exact type
    // required by `GetFileInformationByHandle`.
    let succeeded = unsafe {
        GetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            std::ptr::addr_of_mut!(information),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    let portable = FileIdentity::from_metadata(&metadata);
    let fallback_revision = [
        u64::from(information.dwVolumeSerialNumber),
        file_index,
        metadata.creation_time(),
        metadata.last_write_time(),
    ];

    let mut id = FILE_ID_INFO::default();
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: `file` owns a live handle and both outputs are exact writable
    // structures for their respective synchronous information classes.
    let id_succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            std::ptr::addr_of_mut!(id).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    // SAFETY: same handle lifetime and exact output-size argument as above.
    let basic_succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileBasicInfo,
            std::ptr::addr_of_mut!(basic).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if id_succeeded == 0 || basic_succeeded == 0 {
        // The ordinary read-race identity remains available. Metadata-only
        // persistent reuse is disabled because the filesystem did not supply
        // its 128-bit ID and change-time field. Within the same-user
        // metadata-forgery exclusion, that field detects ordinary same-size
        // rewrites even when last-write time is restored.
        return Ok(IoReadIdentity::new(portable, fallback_revision));
    }
    let Some(strong_revision) = windows_strong_identity_components(
        u64::from(information.dwVolumeSerialNumber),
        id.VolumeSerialNumber,
        id.FileId.Identifier,
        basic.CreationTime,
        basic.LastWriteTime,
        basic.ChangeTime,
    ) else {
        return Ok(IoReadIdentity::new(portable, fallback_revision));
    };
    Ok(IoReadIdentity::with_strong_revision(
        portable,
        fallback_revision,
        strong_revision,
    ))
}

#[derive(Debug)]
struct FileSystemIoHandle {
    file: File,
}

impl IoReadHandle for FileSystemIoHandle {
    fn identity(&self) -> io::Result<IoReadIdentity> {
        opened_source_identity(&self.file)
    }

    fn read_batch(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        self.file.read(destination)
    }
}

/// Classified reason an I/O request did not yield a ready CPU input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileReadFailureKind {
    /// An I/O operation failed. The `ErrorKind` is stable enough for callers to
    /// aggregate diagnostics without exposing an owned `std::io::Error`.
    Io(std::io::ErrorKind),
    /// The source exceeds the request's explicit safety limit.
    TooLarge {
        /// Observed source length.
        observed_bytes: u64,
        /// Request maximum.
        max_bytes: usize,
    },
    /// The rounded contiguous input allocation cannot fit the ready-input
    /// credit partition, so waiting could never make progress.
    ExceedsReadyBudget {
        /// Required buffer allocation size.
        required_bytes: usize,
        /// Total ready-input credit capacity.
        ready_budget_bytes: usize,
    },
    /// The source changed across one or more before/open/after observations on
    /// every allowed attempt.
    ChangedDuringRead,
    /// The caller or a terminal worker state cancelled the run before this
    /// request could be safely admitted.
    Cancelled,
}

/// Deterministic read failure retained for diagnostics and retry policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReadFailure {
    /// Stable source identity.
    pub identity: InputIdentity,
    /// I/O path that failed.
    pub path: PathBuf,
    /// Classified cause.
    pub kind: FileReadFailureKind,
}

impl FileReadFailure {
    fn new(request: &FileReadRequest, kind: FileReadFailureKind) -> Self {
        Self {
            identity: request.identity.clone(),
            path: request.path.clone(),
            kind,
        }
    }
}

/// One completed CPU computation, retained with its source identity.
#[derive(Debug)]
pub struct ComputedInput<T> {
    /// Source identity used for deterministic final ordering.
    pub identity: InputIdentity,
    /// User computation result.
    pub value: T,
}

/// Completion report from [`read_files_concurrently`].
#[derive(Debug)]
pub struct ConcurrentReadResult<T> {
    /// Successful CPU computations, sorted by [`InputIdentity`].
    pub completed: Vec<ComputedInput<T>>,
    /// Read failures, sorted by source identity.
    pub failures: Vec<FileReadFailure>,
}

/// Fatal infrastructure failure from [`read_files_concurrently`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConcurrentReadError {
    /// The explicit runtime configuration cannot make progress.
    InvalidConfig(RuntimeConfigError),
    /// A dedicated I/O worker panicked.
    IoWorkerPanicked,
    /// A dedicated CPU worker or its computation closure panicked.
    ComputeWorkerPanicked,
    /// Two admitted tickets used the same normalized source path.
    DuplicateNormalizedPath(Arc<str>),
    /// Two admitted tickets used the same deterministic source ordinal.
    DuplicateSourceOrdinal(u64),
    /// The same physical source path was admitted more than once.
    DuplicateSourcePath(PathBuf),
    /// The caller cancelled the indexing run, or cancellation propagated from
    /// a terminal worker state before all work could finish.
    Cancelled,
}

/// Cooperative cancellation handle for an isolated indexing run.
///
/// Cancellation is checked before queue admission, before every filesystem
/// open, while reads are batched, and while workers are parked on a bounded
/// ring or byte credit. It is intentionally capability-free: CPU callers can
/// stop work but cannot perform I/O through this handle.
#[derive(Debug, Clone)]
pub struct RuntimeCancellation {
    control: Arc<RunControl>,
}

impl RuntimeCancellation {
    /// Create a fresh cancellation handle in the running state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            control: Arc::new(RunControl::new()),
        }
    }

    /// Request cooperative cancellation. A worker panic has higher terminal
    /// priority and is retained if it races with this request.
    pub fn cancel(&self) {
        self.control.request_cancel();
    }

    /// Whether this run has entered any terminal state.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.control.is_terminal()
    }
}

impl Default for RuntimeCancellation {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalState {
    Running = 0,
    Cancelled = 1,
    IoWorkerPanicked = 2,
    ComputeWorkerPanicked = 3,
}

impl TerminalState {
    const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Running,
            1 => Self::Cancelled,
            2 => Self::IoWorkerPanicked,
            3 => Self::ComputeWorkerPanicked,
            _ => Self::IoWorkerPanicked,
        }
    }
}

#[derive(Debug)]
struct RunControl {
    terminal: AtomicU8,
}

impl RunControl {
    const fn new() -> Self {
        Self {
            terminal: AtomicU8::new(TerminalState::Running as u8),
        }
    }

    fn state(&self) -> TerminalState {
        TerminalState::from_u8(self.terminal.load(Ordering::Acquire))
    }

    fn is_terminal(&self) -> bool {
        self.state() != TerminalState::Running
    }

    fn request_cancel(&self) {
        let _ = self.terminal.compare_exchange(
            TerminalState::Running as u8,
            TerminalState::Cancelled as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn fail(&self, state: TerminalState) {
        debug_assert!(matches!(
            state,
            TerminalState::IoWorkerPanicked | TerminalState::ComputeWorkerPanicked
        ));
        let mut observed = self.terminal.load(Ordering::Acquire);
        loop {
            let current = TerminalState::from_u8(observed);
            if current == state || current == TerminalState::ComputeWorkerPanicked {
                return;
            }
            match self.terminal.compare_exchange_weak(
                observed,
                state as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => observed = actual,
            }
        }
    }

    fn error(&self) -> Option<ConcurrentReadError> {
        match self.state() {
            TerminalState::Running => None,
            TerminalState::Cancelled => Some(ConcurrentReadError::Cancelled),
            TerminalState::IoWorkerPanicked => Some(ConcurrentReadError::IoWorkerPanicked),
            TerminalState::ComputeWorkerPanicked => {
                Some(ConcurrentReadError::ComputeWorkerPanicked)
            }
        }
    }
}

/// Materialize files on fixed I/O workers and invoke `compute` only on fixed
/// CPU workers after an owned [`ReadyInput`] is available.
///
/// This is intentionally a synchronous orchestration API for the first runtime
/// integration. It uses a deterministic fixed owner for every request and a
/// bounded matrix of SPSC rings from I/O workers to CPU workers. An I/O worker
/// reserves its unique producer slot and byte credit before opening a source;
/// it can wait only before the read. A CPU computation never waits on a
/// filesystem operation: it sees either a fully materialized input or no work
/// and parks/yields as an idle worker.
///
/// The I/O worker reads each file into one contiguous [`BufferLease`] using at
/// most [`DEFAULT_READ_BATCH_BYTES`] per read operation (or the configured
/// smaller batch), then checks portable identity snapshots before open, after
/// open, after the read, and through the path. Unix builds additionally check
/// device, inode, and ctime; Windows builds check volume serial, file index,
/// and handle timestamps. A changed file is retried up to three times and is
/// otherwise reported as [`FileReadFailureKind::ChangedDuringRead`].
pub fn read_files_concurrently<T, F>(
    config: IndexRuntimeConfig,
    requests: impl IntoIterator<Item = FileReadRequest>,
    compute: F,
) -> Result<ConcurrentReadResult<T>, ConcurrentReadError>
where
    T: Send + 'static,
    F: Fn(ReadyInput) -> T + Send + Sync + 'static,
{
    read_files_concurrently_with_backend(config, requests, Arc::new(FileSystemIoBackend), compute)
}

/// Run the fixed-owner I/O/CPU pipeline with cooperative cancellation.
///
/// The cancellation token is the only cross-plane control path. It does not
/// expose filesystem or queue capabilities, and terminal worker failures use
/// it to wake every bounded wait before joining the worker set.
pub fn read_files_concurrently_with_cancellation<T, F>(
    config: IndexRuntimeConfig,
    requests: impl IntoIterator<Item = FileReadRequest>,
    cancellation: RuntimeCancellation,
    compute: F,
) -> Result<ConcurrentReadResult<T>, ConcurrentReadError>
where
    T: Send + 'static,
    F: Fn(ReadyInput) -> T + Send + Sync + 'static,
{
    read_files_concurrently_with_backend_and_cancellation(
        config,
        requests,
        Arc::new(FileSystemIoBackend),
        cancellation,
        compute,
    )
}

/// Run the fixed-owner pipeline with a caller-supplied I/O backend.
///
/// The backend is moved only into dedicated I/O workers. CPU callbacks still
/// receive just [`ReadyInput`], which deliberately has no filesystem or
/// backend capability. This is primarily useful for deterministic fault
/// injection and platform-owned I/O integrations.
pub fn read_files_concurrently_with_backend<T, F>(
    config: IndexRuntimeConfig,
    requests: impl IntoIterator<Item = FileReadRequest>,
    backend: Arc<dyn IoReadBackend>,
    compute: F,
) -> Result<ConcurrentReadResult<T>, ConcurrentReadError>
where
    T: Send + 'static,
    F: Fn(ReadyInput) -> T + Send + Sync + 'static,
{
    read_files_concurrently_with_backend_and_cancellation(
        config,
        requests,
        backend,
        RuntimeCancellation::new(),
        compute,
    )
}

/// Run the fixed-owner pipeline with injected I/O and cancellation.
///
/// This is the only runtime entry point that accepts an I/O capability. The
/// implementation passes that capability to I/O workers only; resolver and
/// extractor compute callbacks are type-isolated from it.
pub fn read_files_concurrently_with_backend_and_cancellation<T, F>(
    config: IndexRuntimeConfig,
    requests: impl IntoIterator<Item = FileReadRequest>,
    backend: Arc<dyn IoReadBackend>,
    cancellation: RuntimeCancellation,
    compute: F,
) -> Result<ConcurrentReadResult<T>, ConcurrentReadError>
where
    T: Send + 'static,
    F: Fn(ReadyInput) -> T + Send + Sync + 'static,
{
    config
        .validate()
        .map_err(ConcurrentReadError::InvalidConfig)?;

    let mut requests: Vec<_> = requests.into_iter().collect();
    requests.sort_unstable_by(|left, right| left.identity.cmp(&right.identity));
    if requests.is_empty() {
        return Ok(ConcurrentReadResult {
            completed: Vec::new(),
            failures: Vec::new(),
        });
    }

    if cancellation.is_cancelled() {
        return Err(ConcurrentReadError::Cancelled);
    }

    for pair in requests.windows(2) {
        if pair[0].identity.normalized_path == pair[1].identity.normalized_path {
            return Err(ConcurrentReadError::DuplicateNormalizedPath(Arc::clone(
                &pair[0].identity.normalized_path,
            )));
        }
    }
    let mut source_ordinals = BTreeSet::new();
    let mut source_paths = BTreeSet::new();
    for request in &requests {
        if !source_ordinals.insert(request.identity.source_ordinal) {
            return Err(ConcurrentReadError::DuplicateSourceOrdinal(
                request.identity.source_ordinal,
            ));
        }
        if !source_paths.insert(request.path.clone()) {
            return Err(ConcurrentReadError::DuplicateSourcePath(
                request.path.clone(),
            ));
        }
    }

    let layout = config.bounded_layout(requests.len());
    let io_workers = layout.io_workers;
    let compute_workers = layout.compute_workers;
    let ready_budget = config.memory_budget().ready_inputs_bytes;
    let credits = ByteCreditLedger::new(ready_budget);
    let compute = Arc::new(compute);

    let mut work_senders: Vec<Vec<SpscProducer<ComputeWork>>> = (0..io_workers)
        .map(|_| Vec::with_capacity(compute_workers))
        .collect();
    let mut work_receivers: Vec<Vec<SpscConsumer<ComputeWork>>> = (0..compute_workers)
        .map(|_| Vec::with_capacity(io_workers))
        .collect();
    for senders in &mut work_senders {
        for receivers in &mut work_receivers {
            let queue = SpscQueue::new(READY_QUEUE_CAPACITY)
                .expect("non-zero internal ready queue capacity");
            let (sender, receiver) = queue.split();
            senders.push(sender);
            receivers.push(receiver);
        }
    }

    let mut compute_threads = Vec::with_capacity(compute_workers);
    for receivers in work_receivers {
        let compute = Arc::clone(&compute);
        let control = Arc::clone(&cancellation.control);
        compute_threads.push(thread::spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                run_compute_worker(receivers, compute, &control)
            }));
            match result {
                Ok(completed) => ComputeWorkerOutcome { completed },
                Err(_) => {
                    control.fail(TerminalState::ComputeWorkerPanicked);
                    ComputeWorkerOutcome {
                        completed: Vec::new(),
                    }
                }
            }
        }));
    }

    let mut partitions: Vec<Vec<FileReadRequest>> = (0..io_workers).map(|_| Vec::new()).collect();
    for request in requests {
        let owner = (request.identity.source_ordinal as usize) % io_workers;
        partitions[owner].push(request);
    }

    let mut io_threads = Vec::with_capacity(io_workers);
    for (worker_index, (requests, senders)) in partitions.into_iter().zip(work_senders).enumerate()
    {
        let backend = Arc::clone(&backend);
        let credits = credits.clone();
        let control = Arc::clone(&cancellation.control);
        let control_for_panic = Arc::clone(&control);
        io_threads.push(thread::spawn(move || {
            let runtime = IoWorkerRuntime {
                worker_index,
                compute_workers,
                read_batch_bytes: layout.read_batch_bytes,
                io_pool_bytes: layout.io_pool_bytes,
                credits,
                control,
                backend,
            };
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                run_io_worker(runtime, requests, senders)
            }));
            match result {
                Ok(failures) => IoWorkerOutcome { failures },
                Err(_) => {
                    control_for_panic.fail(TerminalState::IoWorkerPanicked);
                    IoWorkerOutcome {
                        failures: Vec::new(),
                    }
                }
            }
        }));
    }

    let mut failures = Vec::new();
    for io_thread in io_threads {
        match io_thread.join() {
            Ok(mut worker) => failures.append(&mut worker.failures),
            Err(_) => cancellation.control.fail(TerminalState::IoWorkerPanicked),
        }
    }

    let mut completed = Vec::new();
    for compute_thread in compute_threads {
        match compute_thread.join() {
            Ok(mut worker) => completed.append(&mut worker.completed),
            Err(_) => cancellation
                .control
                .fail(TerminalState::ComputeWorkerPanicked),
        }
    }
    if let Some(error) = cancellation.control.error() {
        return Err(error);
    }
    completed.sort_unstable_by(|left, right| left.identity.cmp(&right.identity));
    failures.sort_unstable_by(|left, right| left.identity.cmp(&right.identity));
    Ok(ConcurrentReadResult {
        completed,
        failures,
    })
}

#[derive(Debug)]
enum ComputeWork {
    Ready {
        input: ReadyInput,
        _credit: ByteCreditLease,
    },
    End,
}

struct IoWorkerOutcome {
    failures: Vec<FileReadFailure>,
}

struct ComputeWorkerOutcome<T> {
    completed: Vec<ComputedInput<T>>,
}

struct IoWorkerRuntime {
    worker_index: usize,
    compute_workers: usize,
    read_batch_bytes: usize,
    io_pool_bytes: usize,
    credits: ByteCreditLedger,
    control: Arc<RunControl>,
    backend: Arc<dyn IoReadBackend>,
}

fn run_io_worker(
    runtime: IoWorkerRuntime,
    requests: Vec<FileReadRequest>,
    mut senders: Vec<SpscProducer<ComputeWork>>,
) -> Vec<FileReadFailure> {
    let mut failures = Vec::new();
    let mut buffers = IoBufferPool::new(runtime.io_pool_bytes);
    for request in requests {
        if runtime.control.is_terminal() {
            break;
        }
        let owner = (request.identity.source_ordinal as usize) % runtime.compute_workers;
        // This worker is the sole producer for this ring. Observing a free
        // slot is therefore a reservation: no other producer can consume it
        // between this admission point and the following enqueue.
        if !reserve_ready_slot(&senders[owner], &runtime.control) {
            break;
        }
        match read_ready_input(
            &request,
            runtime.read_batch_bytes,
            &runtime.credits,
            &mut buffers,
            &runtime.control,
            runtime.backend.as_ref(),
            &NoopReadObserver,
        ) {
            Ok((input, credit)) => {
                if !send_reserved_work(
                    &mut senders[owner],
                    ComputeWork::Ready {
                        input,
                        _credit: credit,
                    },
                    &runtime.control,
                ) {
                    break;
                }
            }
            Err(failure) if failure.kind == FileReadFailureKind::Cancelled => break,
            Err(failure) => failures.push(failure),
        }
    }

    // Each receiver has one sender from every I/O worker. An explicit end
    // marker lets compute workers terminate without a shared blocking queue.
    if !runtime.control.is_terminal() {
        for sender in &mut senders {
            if !send_terminal_work(sender, &runtime.control) {
                break;
            }
        }
    }
    let _ = runtime.worker_index; // Retained for fixed-owner telemetry integration.
    failures
}

fn reserve_ready_slot(sender: &SpscProducer<ComputeWork>, control: &RunControl) -> bool {
    loop {
        if control.is_terminal() {
            return false;
        }
        if sender.available_slots() > 0 {
            return true;
        }
        thread::yield_now();
    }
}

fn send_reserved_work(
    sender: &mut SpscProducer<ComputeWork>,
    work: ComputeWork,
    control: &RunControl,
) -> bool {
    if control.is_terminal() {
        return false;
    }
    // `reserve_ready_slot` observed capacity on this exact unique-producer
    // ring. Consumers only free slots, so a full result would violate the
    // SPSC ownership invariant rather than justify spinning after allocation.
    sender.try_send(work).map_or_else(|_| false, |_| true)
}

fn send_terminal_work(sender: &mut SpscProducer<ComputeWork>, control: &RunControl) -> bool {
    loop {
        if control.is_terminal() {
            return false;
        }
        match sender.try_send(ComputeWork::End) {
            Ok(()) => return true,
            Err(ComputeWork::End) => thread::yield_now(),
            Err(ComputeWork::Ready { .. }) => unreachable!("terminal send changed work type"),
        }
    }
}

fn run_compute_worker<T, F>(
    mut receivers: Vec<SpscConsumer<ComputeWork>>,
    compute: Arc<F>,
    control: &RunControl,
) -> Vec<ComputedInput<T>>
where
    T: Send + 'static,
    F: Fn(ReadyInput) -> T + Send + Sync + 'static,
{
    let mut completed = Vec::new();
    let mut ended = vec![false; receivers.len()];
    let mut finished_senders = 0;
    let mut next_receiver = 0;
    while finished_senders < receivers.len() {
        if control.is_terminal() {
            break;
        }
        let mut made_progress = false;
        for offset in 0..receivers.len() {
            let index = (next_receiver + offset) % receivers.len();
            if ended[index] {
                continue;
            }
            match receivers[index].try_recv() {
                Some(ComputeWork::Ready { input, _credit }) => {
                    let identity = input.identity.clone();
                    // Hashing is an explicit CPU preflight stage: I/O workers
                    // only materialize bytes, and extractors receive a ready
                    // immutable source plus its content identity. `blake3`
                    // uses portable runtime dispatch internally without
                    // spawning Rayon work or copying the source allocation.
                    let content_digest = *blake3::hash(input.bytes()).as_bytes();
                    let input = input.with_content_digest(content_digest);
                    let value = match panic::catch_unwind(AssertUnwindSafe(|| compute(input))) {
                        Ok(value) => value,
                        Err(_) => {
                            control.fail(TerminalState::ComputeWorkerPanicked);
                            return completed;
                        }
                    };
                    completed.push(ComputedInput { identity, value });
                    drop(_credit);
                    made_progress = true;
                }
                Some(ComputeWork::End) => {
                    ended[index] = true;
                    finished_senders += 1;
                    made_progress = true;
                }
                None => {}
            }
        }
        next_receiver = (next_receiver + 1) % receivers.len();
        if !made_progress {
            thread::yield_now();
        }
    }
    completed
}

fn read_ready_input(
    request: &FileReadRequest,
    read_batch_bytes: usize,
    credits: &ByteCreditLedger,
    buffers: &mut IoBufferPool,
    control: &RunControl,
    backend: &dyn IoReadBackend,
    observer: &impl ReadObserver,
) -> Result<(ReadyInput, ByteCreditLease), FileReadFailure> {
    for _attempt in 0..FILE_READ_ATTEMPTS {
        if control.is_terminal() {
            return Err(FileReadFailure::new(
                request,
                FileReadFailureKind::Cancelled,
            ));
        }
        // Hold a minimum source credit before even the metadata probe. This
        // bounds in-flight I/O tickets independently of their eventual file
        // size; once metadata establishes the exact class, this ticket is
        // replaced by the full lease before `open` or any source-byte read.
        let credit = reserve_input_credit(credits, MIN_POOL_BUFFER_BYTES, request, control)?;
        let before = backend.probe(request).map_err(|error| {
            FileReadFailure::new(request, FileReadFailureKind::Io(error.kind()))
        })?;
        if request
            .expected_identity
            .is_some_and(|expected| expected != before)
        {
            return Err(FileReadFailure::new(
                request,
                FileReadFailureKind::ChangedDuringRead,
            ));
        }
        if before.file_identity.length_bytes > request.max_bytes as u64 {
            return Err(FileReadFailure::new(
                request,
                FileReadFailureKind::TooLarge {
                    observed_bytes: before.file_identity.length_bytes,
                    max_bytes: request.max_bytes,
                },
            ));
        }
        let logical_len = usize::try_from(before.file_identity.length_bytes).map_err(|_| {
            FileReadFailure::new(
                request,
                FileReadFailureKind::TooLarge {
                    observed_bytes: before.file_identity.length_bytes,
                    max_bytes: request.max_bytes,
                },
            )
        })?;
        let class = BufferClass::for_capacity(logical_len);
        let allocation_len = class.capacity();
        // Do not hold a small ticket credit while waiting to grow it. If all
        // I/O owners did that simultaneously, mutually held ticket credits
        // could prevent every owner from reaching its exact source class.
        // Releasing and reacquiring the exact lease still guarantees that no
        // source open/read happens without full downstream byte admission.
        let credit = if allocation_len > credit.bytes() {
            drop(credit);
            reserve_input_credit(credits, allocation_len, request, control)?
        } else {
            credit
        };
        observer.after_before_identity(request);
        if control.is_terminal() {
            return Err(FileReadFailure::new(
                request,
                FileReadFailureKind::Cancelled,
            ));
        }

        let mut file = backend.open(request).map_err(|error| {
            FileReadFailure::new(request, FileReadFailureKind::Io(error.kind()))
        })?;
        let opened = file.identity().map_err(|error| {
            FileReadFailure::new(request, FileReadFailureKind::Io(error.kind()))
        })?;
        if opened != before {
            drop(credit);
            continue;
        }

        let mut buffer = buffers.take(logical_len);
        let mut offset = 0;
        while offset < logical_len {
            if control.is_terminal() {
                buffers.recycle(buffer);
                return Err(FileReadFailure::new(
                    request,
                    FileReadFailureKind::Cancelled,
                ));
            }
            let chunk_len = (logical_len - offset).min(read_batch_bytes);
            let read = match file.read_batch(&mut buffer.as_mut_bytes()[offset..offset + chunk_len])
            {
                Ok(read) => read,
                Err(error) => {
                    buffers.recycle(buffer);
                    return Err(FileReadFailure::new(
                        request,
                        FileReadFailureKind::Io(error.kind()),
                    ));
                }
            };
            if read == 0 {
                break;
            }
            offset += read;
        }
        buffer.truncate(offset);
        let after_file = match file.identity() {
            Ok(identity) => identity,
            Err(error) => {
                buffers.recycle(buffer);
                return Err(FileReadFailure::new(
                    request,
                    FileReadFailureKind::Io(error.kind()),
                ));
            }
        };
        let after_path = match backend.probe(request) {
            Ok(identity) => identity,
            Err(error) => {
                buffers.recycle(buffer);
                return Err(FileReadFailure::new(
                    request,
                    FileReadFailureKind::Io(error.kind()),
                ));
            }
        };
        if offset == logical_len && after_file == before && after_path == before {
            return Ok((
                ReadyInput::with_file_identity(
                    request.identity.clone(),
                    buffer,
                    before,
                    request.bound_source_identity_evidence(before),
                ),
                credit,
            ));
        }
        buffers.recycle(buffer);
        drop(credit);
    }
    Err(FileReadFailure::new(
        request,
        FileReadFailureKind::ChangedDuringRead,
    ))
}

fn reserve_input_credit(
    credits: &ByteCreditLedger,
    allocation_len: usize,
    request: &FileReadRequest,
    control: &RunControl,
) -> Result<ByteCreditLease, FileReadFailure> {
    loop {
        if control.is_terminal() {
            return Err(FileReadFailure::new(
                request,
                FileReadFailureKind::Cancelled,
            ));
        }
        match credits.try_reserve(allocation_len) {
            Ok(credit) => return Ok(credit),
            Err(CreditReservationError::TooLarge { capacity, .. }) => {
                return Err(FileReadFailure::new(
                    request,
                    FileReadFailureKind::ExceedsReadyBudget {
                        required_bytes: allocation_len,
                        ready_budget_bytes: capacity,
                    },
                ));
            }
            Err(CreditReservationError::Insufficient { .. }) => thread::yield_now(),
        }
    }
}

trait ReadObserver {
    fn after_before_identity(&self, _request: &FileReadRequest) {}
}

struct NoopReadObserver;

impl ReadObserver for NoopReadObserver {}

/// A nonblocking, byte-counted capacity ledger.
#[derive(Debug, Clone)]
pub struct ByteCreditLedger {
    inner: Arc<CreditInner>,
}

#[derive(Debug)]
struct CreditInner {
    capacity: usize,
    reserved: AtomicUsize,
}

impl ByteCreditLedger {
    /// Create a byte-credit ledger with a fixed non-zero capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(CreditInner {
                capacity,
                reserved: AtomicUsize::new(0),
            }),
        }
    }

    /// Return the maximum concurrently reserved byte count.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Return the current non-authoritative available byte estimate.
    #[must_use]
    pub fn available(&self) -> usize {
        self.inner
            .capacity
            .saturating_sub(self.inner.reserved.load(Ordering::Acquire))
    }

    /// Reserve `bytes` without waiting. The returned lease automatically
    /// returns capacity when dropped.
    pub fn try_reserve(&self, bytes: usize) -> Result<ByteCreditLease, CreditReservationError> {
        if bytes > self.inner.capacity {
            return Err(CreditReservationError::TooLarge {
                requested: bytes,
                capacity: self.inner.capacity,
            });
        }

        let mut observed = self.inner.reserved.load(Ordering::Acquire);
        loop {
            let available = self.inner.capacity.saturating_sub(observed);
            if bytes > available {
                return Err(CreditReservationError::Insufficient {
                    requested: bytes,
                    available,
                    capacity: self.inner.capacity,
                });
            }
            let next = observed + bytes;
            match self.inner.reserved.compare_exchange_weak(
                observed,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(ByteCreditLease {
                        inner: Arc::clone(&self.inner),
                        bytes,
                        active: true,
                    });
                }
                Err(actual) => observed = actual,
            }
        }
    }
}

/// Failed nonblocking credit reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditReservationError {
    /// The request exceeds the ledger's total capacity.
    TooLarge {
        /// Requested byte count.
        requested: usize,
        /// Total ledger capacity.
        capacity: usize,
    },
    /// Existing reservations leave too few bytes for the request.
    Insufficient {
        /// Requested byte count.
        requested: usize,
        /// Snapshot of capacity available at the failed attempt.
        available: usize,
        /// Total ledger capacity.
        capacity: usize,
    },
}

/// RAII byte-credit reservation returned by [`ByteCreditLedger::try_reserve`].
#[derive(Debug)]
pub struct ByteCreditLease {
    inner: Arc<CreditInner>,
    bytes: usize,
    active: bool,
}

impl ByteCreditLease {
    /// Return the number of bytes held by this lease.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Return capacity immediately rather than waiting for drop.
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.active {
            let previous = self.inner.reserved.fetch_sub(self.bytes, Ordering::AcqRel);
            debug_assert!(previous >= self.bytes, "credit ledger underflow");
            self.active = false;
        }
    }
}

impl Drop for ByteCreditLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// Cache-line padding for independently written ring cursors.
#[repr(align(64))]
struct CachePadded<T>(T);

struct Slot<T> {
    value: std::cell::UnsafeCell<MaybeUninit<T>>,
}

// A slot is synchronized by the release/acquire cursor handoff in the SPSC
// algorithm. Exactly one producer writes and exactly one consumer reads it.
unsafe impl<T: Send> Sync for Slot<T> {}

struct SpscInner<T> {
    slots: Box<[Slot<T>]>,
    capacity: usize,
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
}

impl<T> SpscInner<T> {
    fn new(capacity: usize) -> Self {
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, || Slot {
            value: std::cell::UnsafeCell::new(MaybeUninit::uninit()),
        });
        Self {
            slots: slots.into_boxed_slice(),
            capacity,
            head: CachePadded(AtomicUsize::new(0)),
            tail: CachePadded(AtomicUsize::new(0)),
        }
    }
}

impl<T> Drop for SpscInner<T> {
    fn drop(&mut self) {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        let queued = tail.wrapping_sub(head).min(self.capacity);
        for offset in 0..queued {
            let index = head.wrapping_add(offset) % self.capacity;
            // Both endpoints have released the final `Arc`, so no producer or
            // consumer can access an initialized slot while it is dropped.
            unsafe {
                (*self.slots[index].value.get()).assume_init_drop();
            }
        }
    }
}

/// Construction error for a bounded SPSC queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpscQueueError {
    /// A ring needs at least one slot.
    ZeroCapacity,
}

/// A bounded, lock-free, single-producer/single-consumer ring constructor.
///
/// Call [`SpscQueue::split`] exactly once to obtain the unique producer and
/// consumer. The endpoints are `Send` but deliberately not `Sync`, preventing
/// accidental concurrent use by more than one producer or consumer.
pub struct SpscQueue<T> {
    inner: Arc<SpscInner<T>>,
}

impl<T> SpscQueue<T> {
    /// Construct a fixed-capacity ring.
    pub fn new(capacity: usize) -> Result<Self, SpscQueueError> {
        if capacity == 0 {
            return Err(SpscQueueError::ZeroCapacity);
        }
        Ok(Self {
            inner: Arc::new(SpscInner::new(capacity)),
        })
    }

    /// Return the fixed number of values this ring can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Split this queue into its sole producer and sole consumer endpoints.
    #[must_use]
    pub fn split(self) -> (SpscProducer<T>, SpscConsumer<T>) {
        let producer = SpscProducer {
            inner: Arc::clone(&self.inner),
            not_sync: std::marker::PhantomData,
        };
        let consumer = SpscConsumer {
            inner: self.inner,
            not_sync: std::marker::PhantomData,
        };
        (producer, consumer)
    }
}

/// The only sending endpoint for a [`SpscQueue`].
pub struct SpscProducer<T> {
    inner: Arc<SpscInner<T>>,
    not_sync: std::marker::PhantomData<Cell<()>>,
}

impl<T> SpscProducer<T> {
    /// Attempt to enqueue without allocating, locking, or waiting.
    pub fn try_send(&mut self, value: T) -> Result<(), T> {
        let tail = self.inner.tail.0.load(Ordering::Relaxed);
        let head = self.inner.head.0.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= self.inner.capacity {
            return Err(value);
        }
        let index = tail % self.inner.capacity;
        // Only this unique producer writes this slot, and the consumer cannot
        // observe it until the release store of `tail` below.
        unsafe {
            (*self.inner.slots[index].value.get()).write(value);
        }
        self.inner
            .tail
            .0
            .store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Return an approximate number of currently free slots.
    #[must_use]
    pub fn available_slots(&self) -> usize {
        let tail = self.inner.tail.0.load(Ordering::Relaxed);
        let head = self.inner.head.0.load(Ordering::Acquire);
        self.inner
            .capacity
            .saturating_sub(tail.wrapping_sub(head).min(self.inner.capacity))
    }
}

/// The only receiving endpoint for a [`SpscQueue`].
pub struct SpscConsumer<T> {
    inner: Arc<SpscInner<T>>,
    not_sync: std::marker::PhantomData<Cell<()>>,
}

impl<T> SpscConsumer<T> {
    /// Attempt to dequeue without allocating, locking, or waiting.
    #[must_use]
    pub fn try_recv(&mut self) -> Option<T> {
        let head = self.inner.head.0.load(Ordering::Relaxed);
        let tail = self.inner.tail.0.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let index = head % self.inner.capacity;
        // The producer's release store of `tail` makes the initialized value
        // visible here. Only this unique consumer reads this slot.
        let value = unsafe { (*self.inner.slots[index].value.get()).assume_init_read() };
        self.inner
            .head
            .0
            .store(head.wrapping_add(1), Ordering::Release);
        Some(value)
    }

    /// Return an approximate number of currently queued values.
    #[must_use]
    pub fn len(&self) -> usize {
        let head = self.inner.head.0.load(Ordering::Relaxed);
        let tail = self.inner.tail.0.load(Ordering::Acquire);
        tail.wrapping_sub(head).min(self.inner.capacity)
    }

    /// Whether the queue is currently observed empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        fs, io,
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc, Condvar, Mutex,
        },
        thread,
        time::Duration,
    };

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum BackendCall {
        Probe(PathBuf),
        Open(PathBuf),
        Identity(PathBuf),
        Read(PathBuf),
    }

    struct FakeReadBackend {
        sources: Mutex<BTreeMap<PathBuf, FakeSourcePlan>>,
        calls: Arc<Mutex<Vec<BackendCall>>>,
    }

    impl FakeReadBackend {
        fn new(sources: impl IntoIterator<Item = (PathBuf, FakeSourcePlan)>) -> Self {
            Self {
                sources: Mutex::new(sources.into_iter().collect()),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<BackendCall> {
            self.calls.lock().expect("calls lock").clone()
        }

        fn call_count(&self, expected: BackendCall) -> usize {
            self.calls()
                .into_iter()
                .filter(|call| call == &expected)
                .count()
        }
    }

    impl IoReadBackend for FakeReadBackend {
        fn probe(&self, request: &FileReadRequest) -> io::Result<IoReadIdentity> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(BackendCall::Probe(request.path.clone()));
            let result = self
                .sources
                .lock()
                .expect("sources lock")
                .get_mut(&request.path)
                .and_then(|source| source.probes.pop_front())
                .unwrap_or(Err(io::ErrorKind::NotFound));
            result.map_err(io::Error::from)
        }

        fn open(&self, request: &FileReadRequest) -> io::Result<Box<dyn IoReadHandle>> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(BackendCall::Open(request.path.clone()));
            let plan = self
                .sources
                .lock()
                .expect("sources lock")
                .get_mut(&request.path)
                .and_then(|source| source.opens.pop_front())
                .unwrap_or(FakeOpenPlan::Failure(io::ErrorKind::NotFound));
            match plan {
                FakeOpenPlan::Failure(kind) => Err(io::Error::from(kind)),
                FakeOpenPlan::Handle(handle) => Ok(Box::new(FakeReadHandle {
                    path: request.path.clone(),
                    bytes: handle.bytes,
                    offset: 0,
                    identities: Mutex::new(handle.identities),
                    read_error: handle.read_error,
                    gate: handle.gate,
                    waited_on_gate: false,
                    calls: Arc::clone(&self.calls),
                })),
            }
        }
    }

    struct FakeSourcePlan {
        probes: VecDeque<Result<IoReadIdentity, io::ErrorKind>>,
        opens: VecDeque<FakeOpenPlan>,
    }

    impl FakeSourcePlan {
        fn stable(bytes: &[u8], revision: u64) -> Self {
            let identity = fake_identity(bytes.len(), revision);
            Self {
                probes: VecDeque::from([Ok(identity), Ok(identity)]),
                opens: VecDeque::from([FakeOpenPlan::Handle(FakeHandlePlan::stable(
                    bytes, identity,
                ))]),
            }
        }

        fn failing_probe(kind: io::ErrorKind) -> Self {
            Self {
                probes: VecDeque::from([Err(kind)]),
                opens: VecDeque::new(),
            }
        }
    }

    enum FakeOpenPlan {
        Failure(io::ErrorKind),
        Handle(FakeHandlePlan),
    }

    struct FakeHandlePlan {
        bytes: Vec<u8>,
        identities: VecDeque<Result<IoReadIdentity, io::ErrorKind>>,
        read_error: Option<io::ErrorKind>,
        gate: Option<Arc<ReadGate>>,
    }

    impl FakeHandlePlan {
        fn stable(bytes: &[u8], identity: IoReadIdentity) -> Self {
            Self {
                bytes: bytes.to_vec(),
                identities: VecDeque::from([Ok(identity), Ok(identity)]),
                read_error: None,
                gate: None,
            }
        }

        fn gated(bytes: &[u8], identity: IoReadIdentity, gate: Arc<ReadGate>) -> Self {
            Self {
                gate: Some(gate),
                ..Self::stable(bytes, identity)
            }
        }
    }

    struct FakeReadHandle {
        path: PathBuf,
        bytes: Vec<u8>,
        offset: usize,
        identities: Mutex<VecDeque<Result<IoReadIdentity, io::ErrorKind>>>,
        read_error: Option<io::ErrorKind>,
        gate: Option<Arc<ReadGate>>,
        waited_on_gate: bool,
        calls: Arc<Mutex<Vec<BackendCall>>>,
    }

    impl IoReadHandle for FakeReadHandle {
        fn identity(&self) -> io::Result<IoReadIdentity> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(BackendCall::Identity(self.path.clone()));
            self.identities
                .lock()
                .expect("identities lock")
                .pop_front()
                .unwrap_or(Err(io::ErrorKind::InvalidData))
                .map_err(io::Error::from)
        }

        fn read_batch(&mut self, destination: &mut [u8]) -> io::Result<usize> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(BackendCall::Read(self.path.clone()));
            if !self.waited_on_gate {
                if let Some(gate) = &self.gate {
                    gate.wait_for_release();
                }
                self.waited_on_gate = true;
            }
            if let Some(kind) = self.read_error.take() {
                return Err(io::Error::from(kind));
            }
            let remaining = &self.bytes[self.offset..];
            let copied = remaining.len().min(destination.len());
            destination[..copied].copy_from_slice(&remaining[..copied]);
            self.offset += copied;
            Ok(copied)
        }
    }

    #[derive(Debug)]
    struct ReadGate {
        state: Mutex<ReadGateState>,
        changed: Condvar,
    }

    #[derive(Debug, Default)]
    struct ReadGateState {
        entered: bool,
        released: bool,
    }

    impl ReadGate {
        fn new() -> Self {
            Self {
                state: Mutex::new(ReadGateState::default()),
                changed: Condvar::new(),
            }
        }

        fn wait_for_release(&self) {
            let mut state = self.state.lock().expect("gate lock");
            state.entered = true;
            self.changed.notify_all();
            while !state.released {
                state = self.changed.wait(state).expect("gate wait");
            }
        }

        fn wait_until_entered(&self) -> bool {
            let state = self.state.lock().expect("gate lock");
            let (state, _) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(1), |state| !state.entered)
                .expect("gate wait timeout");
            state.entered
        }

        fn release(&self) {
            let mut state = self.state.lock().expect("gate lock");
            state.released = true;
            self.changed.notify_all();
        }
    }

    fn fake_identity(length_bytes: usize, revision: u64) -> IoReadIdentity {
        IoReadIdentity::new(
            FileIdentity {
                length_bytes: length_bytes as u64,
                modified: None,
            },
            [revision, 0, 0, 0],
        )
    }

    #[cfg(unix)]
    #[test]
    fn fifo_source_open_is_nonblocking_and_held_handle_validation_rejects_it() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

        let temp = tempfile::tempdir().expect("temporary source root");
        let fifo = temp.path().join("source.fifo");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path has no NUL");
        // SAFETY: `fifo_c` is a live NUL-terminated pathname and mkfifo does
        // not retain the pointer after returning.
        let result = unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) };
        assert_eq!(result, 0, "mkfifo failed: {}", io::Error::last_os_error());

        let file = open_source_nofollow(&fifo).expect("nonblocking FIFO open");
        assert_eq!(
            opened_source_identity(&file)
                .expect_err("FIFO must fail held-handle regular-file validation")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn generation_revision_distinguishes_equal_portable_metadata() {
        let portable = FileIdentity {
            length_bytes: 6,
            modified: None,
        };
        assert_ne!(
            IoReadIdentity::new(portable, [11, 22, 33, 44]),
            IoReadIdentity::new(portable, [11, 23, 33, 44]),
            "a different platform file index must reject an equal-size replacement"
        );
    }

    #[test]
    fn persistent_identity_components_fail_closed_when_generation_is_weak() {
        assert!(unix_strong_identity_components([1, 2, 3, 4]).is_some());
        assert!(unix_strong_identity_components([0, 2, 3, 4]).is_none());
        assert!(unix_strong_identity_components([1, 0, 3, 4]).is_none());
        assert!(unix_strong_identity_components([1, 2, 0, 0]).is_none());

        let file_id = [7; 16];
        assert!(windows_strong_identity_components(9, 9, file_id, 1, 2, 3).is_some());
        assert!(windows_strong_identity_components(9, 8, file_id, 1, 2, 3).is_none());
        assert!(windows_strong_identity_components(9, 9, [0; 16], 1, 2, 3).is_none());
        assert!(windows_strong_identity_components(9, 9, file_id, 1, 2, 0).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_handle_identity_distinguishes_equal_length_files() {
        let fixture = tempfile::tempdir().expect("temporary identity fixture");
        let left_path = fixture.path().join("left.txt");
        let right_path = fixture.path().join("right.txt");
        fs::write(&left_path, b"same!!").expect("write left source");
        fs::write(&right_path, b"same!!").expect("write right source");
        let left_file = open_source_nofollow(&left_path).expect("open left source");
        let right_file = open_source_nofollow(&right_path).expect("open right source");
        let left = windows_file_identity(&left_file).expect("identify left source");
        let right = windows_file_identity(&right_file).expect("identify right source");

        assert_eq!(
            left.file_identity.length_bytes,
            right.file_identity.length_bytes
        );
        assert_eq!(left.revision[0], right.revision[0], "same volume serial");
        assert_ne!(left.revision[1], right.revision[1], "distinct file indexes");
        assert_ne!(left, right);
    }

    fn runtime_config(io_workers: usize, compute_workers: usize) -> IndexRuntimeConfig {
        IndexRuntimeConfig {
            memory_budget_bytes: 128 * 1024,
            io_workers,
            compute_workers,
            io_backend: IoBackendSelection::Threaded,
            read_batch_bytes: 3,
        }
    }

    #[test]
    fn cgroup_limit_wins_and_budget_is_clamped() {
        let limits = RuntimeMemoryLimits {
            host_memory_bytes: Some(64 * 1024 * 1024 * 1024),
            cgroup_memory_bytes: Some(2 * 1024 * 1024 * 1024),
        };
        assert_eq!(
            limits.effective_memory_bytes(),
            Some(2 * 1024 * 1024 * 1024)
        );
        assert_eq!(limits.automatic_budget_bytes(), 256 * 1024 * 1024);

        let larger = RuntimeMemoryLimits {
            host_memory_bytes: Some(64 * 1024 * 1024 * 1024),
            cgroup_memory_bytes: Some(24 * 1024 * 1024 * 1024),
        };
        assert_eq!(larger.automatic_budget_bytes(), 3 * 1024 * 1024 * 1024);

        let constrained = RuntimeMemoryLimits {
            host_memory_bytes: Some(256 * 1024 * 1024),
            cgroup_memory_bytes: Some(96 * 1024 * 1024),
        };
        assert_eq!(constrained.effective_memory_bytes(), Some(96 * 1024 * 1024));
        assert_eq!(constrained.automatic_budget_bytes(), 12 * 1024 * 1024);
        assert!(constrained.automatic_budget_bytes() <= 96 * 1024 * 1024);

        assert_eq!(
            RuntimeMemoryLimits::default().automatic_budget_bytes(),
            MIN_MEMORY_BUDGET_BYTES
        );
    }

    #[test]
    fn control_plane_limit_parsers_handle_linux_and_unlimited_inputs() {
        assert_eq!(
            parse_meminfo_total_bytes("MemFree: 1 kB\nMemTotal: 1048576 kB\n"),
            Some(1024 * 1024 * 1024)
        );
        assert_eq!(parse_meminfo_total_bytes("MemTotal: unknown kB"), None);
        assert_eq!(parse_cgroup_memory_limit("max\n"), None);
        assert_eq!(parse_cgroup_memory_limit("\n"), None);
        assert_eq!(parse_cgroup_memory_limit("9223372036854771712\n"), None);
        assert_eq!(
            parse_cgroup_memory_limit("2147483648\n"),
            Some(2 * 1024 * 1024 * 1024)
        );
        assert_eq!(parse_cgroup_memory_limit("not-a-limit"), None);
        assert_eq!(
            cgroup_memory_limit_paths("0::/slice/job\n5:memory:/legacy/job\n"),
            vec![
                PathBuf::from("/sys/fs/cgroup/slice/job/memory.max"),
                PathBuf::from("/sys/fs/cgroup/memory/legacy/job/memory.limit_in_bytes"),
            ]
        );
    }

    #[test]
    fn runtime_defaults_reserve_control_and_io_capacity() {
        let config = IndexRuntimeConfig::from_limits(16, RuntimeMemoryLimits::default());
        assert_eq!(config.io_workers, 4);
        assert_eq!(config.compute_workers, 11);
        assert_eq!(config.read_batch_bytes, DEFAULT_READ_BATCH_BYTES);
        assert_eq!(
            config.io_backend.resolve().effective,
            EffectiveIoBackend::Threaded
        );
        assert!(config.validate().is_ok());

        let budget = config.memory_budget();
        assert_eq!(
            budget.io_buffers_bytes
                + budget.ready_inputs_bytes
                + budget.cpu_arenas_bytes
                + budget.cache_and_runs_bytes
                + budget.query_reserve_bytes
                + budget.emergency_reserve_bytes,
            budget.total_bytes
        );
    }

    #[test]
    fn execution_evidence_reports_effective_not_requested_owner_counts() {
        let config = IndexRuntimeConfig {
            memory_budget_bytes: 128 * 1024,
            io_workers: 64,
            compute_workers: 64,
            io_backend: IoBackendSelection::Threaded,
            read_batch_bytes: usize::MAX,
        };
        let evidence = config.execution_evidence(2);
        assert_eq!(evidence.admitted_requests, 2);
        assert_eq!(evidence.effective_io_workers, 2);
        assert_eq!(evidence.effective_compute_workers, 1);
        assert!(evidence.effective_read_batch_bytes <= evidence.io_pool_bytes_per_worker);
        assert_eq!(
            evidence.io_buffers_bytes
                + evidence.ready_inputs_bytes
                + evidence.cpu_arenas_bytes
                + evidence.cache_and_runs_bytes
                + evidence.query_reserve_bytes
                + evidence.emergency_reserve_bytes,
            config.memory_budget_bytes
        );
    }

    #[test]
    fn io_uring_request_records_portable_fallback() {
        let resolution = IoBackendSelection::IoUring.resolve();
        assert_eq!(resolution.effective, EffectiveIoBackend::Threaded);
        assert!(resolution.fallback_reason.is_some());
    }

    #[test]
    fn buffer_moves_into_ready_input_without_copying() {
        let bytes = b"ready source".to_vec();
        let source_ptr = bytes.as_ptr();
        let lease = BufferLease::from_vec(bytes);
        let input = ReadyInput::new(InputIdentity::new("src/lib.rs", 7), lease)
            .with_content_digest([9; 32]);
        assert_eq!(input.bytes().as_ptr(), source_ptr);
        assert_eq!(input.bytes(), b"ready source");
        assert_eq!(input.content_digest, Some([9; 32]));
        assert_eq!(input.identity.source_ordinal, 7);

        let pooled = ReadyInput::new(
            InputIdentity::new("tiny.json", 8),
            BufferLease::with_capacity(1),
        );
        assert_eq!(pooled.bytes().len(), 0);
        assert_eq!(pooled.retained_capacity_bytes(), 4 * 1024);
    }

    #[test]
    fn duplicate_runtime_ticket_identities_and_paths_are_rejected_before_io() {
        let config = runtime_config(2, 2);
        let duplicate_normalized_path = read_files_concurrently(
            config,
            [
                FileReadRequest::new(InputIdentity::new("same.rs", 0), PathBuf::from("a.rs")),
                FileReadRequest::new(InputIdentity::new("same.rs", 1), PathBuf::from("b.rs")),
            ],
            |_| (),
        )
        .expect_err("duplicate normalized path must be rejected");
        assert_eq!(
            duplicate_normalized_path,
            ConcurrentReadError::DuplicateNormalizedPath(Arc::from("same.rs"))
        );

        let duplicate_ordinal = read_files_concurrently(
            config,
            [
                FileReadRequest::new(InputIdentity::new("a.rs", 7), PathBuf::from("a.rs")),
                FileReadRequest::new(InputIdentity::new("b.rs", 7), PathBuf::from("b.rs")),
            ],
            |_| (),
        )
        .expect_err("duplicate ordinal must be rejected");
        assert_eq!(
            duplicate_ordinal,
            ConcurrentReadError::DuplicateSourceOrdinal(7)
        );

        let duplicate_path = read_files_concurrently(
            config,
            [
                FileReadRequest::new(InputIdentity::new("a.rs", 0), PathBuf::from("same.rs")),
                FileReadRequest::new(InputIdentity::new("b.rs", 1), PathBuf::from("same.rs")),
            ],
            |_| (),
        )
        .expect_err("duplicate source path must be rejected");
        assert_eq!(
            duplicate_path,
            ConcurrentReadError::DuplicateSourcePath(PathBuf::from("same.rs"))
        );
    }

    #[test]
    fn credit_reservations_are_bounded_and_raii_released() {
        let ledger = ByteCreditLedger::new(10);
        let first = ledger.try_reserve(6).expect("first reservation");
        assert_eq!(ledger.available(), 4);
        assert!(matches!(
            ledger.try_reserve(5),
            Err(CreditReservationError::Insufficient {
                requested: 5,
                available: 4,
                capacity: 10,
            })
        ));
        assert_eq!(first.bytes(), 6);
        drop(first);
        assert_eq!(ledger.available(), 10);
        assert!(matches!(
            ledger.try_reserve(11),
            Err(CreditReservationError::TooLarge { .. })
        ));
    }

    #[test]
    fn spsc_queue_is_bounded_and_fifo() {
        let queue = SpscQueue::new(2).expect("queue");
        assert_eq!(queue.capacity(), 2);
        let (mut producer, mut consumer) = queue.split();
        assert_eq!(producer.try_send(1), Ok(()));
        assert_eq!(producer.try_send(2), Ok(()));
        assert_eq!(producer.try_send(3), Err(3));
        assert_eq!(consumer.try_recv(), Some(1));
        assert_eq!(producer.try_send(3), Ok(()));
        assert_eq!(consumer.try_recv(), Some(2));
        assert_eq!(consumer.try_recv(), Some(3));
        assert!(consumer.is_empty());
        assert!(matches!(
            SpscQueue::<u8>::new(0),
            Err(SpscQueueError::ZeroCapacity)
        ));
    }

    #[test]
    fn spsc_queue_transfers_between_threads_without_loss() {
        let (mut producer, mut consumer) = SpscQueue::new(64).expect("queue").split();
        let producer_thread = thread::spawn(move || {
            for value in 0..10_000_u32 {
                let mut pending = value;
                loop {
                    match producer.try_send(pending) {
                        Ok(()) => break,
                        Err(value) => {
                            pending = value;
                            thread::yield_now();
                        }
                    }
                }
            }
        });
        let consumer_thread = thread::spawn(move || {
            let mut seen = Vec::with_capacity(10_000);
            while seen.len() < 10_000 {
                if let Some(value) = consumer.try_recv() {
                    seen.push(value);
                } else {
                    thread::yield_now();
                }
            }
            seen
        });
        producer_thread.join().expect("producer exits");
        let seen = consumer_thread.join().expect("consumer exits");
        assert_eq!(seen, (0..10_000).collect::<Vec<_>>());
    }

    #[test]
    fn queued_values_are_dropped_when_endpoints_close() {
        #[derive(Debug)]
        struct DropCounter(Arc<AtomicUsize>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let (mut producer, consumer) = SpscQueue::new(2).expect("queue").split();
        producer
            .try_send(DropCounter(Arc::clone(&drops)))
            .expect("enqueue");
        drop(producer);
        drop(consumer);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn concurrent_reader_separates_ready_inputs_from_cpu_computation() {
        static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "graphoxide-index-runtime-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let alpha = directory.join("alpha.txt");
        let beta = directory.join("beta.txt");
        fs::write(&alpha, b"alpha through small batches").expect("write alpha");
        fs::write(&beta, b"beta through small batches").expect("write beta");

        let config = IndexRuntimeConfig {
            memory_budget_bytes: 128 * 1024,
            io_workers: 2,
            compute_workers: 2,
            io_backend: IoBackendSelection::Threaded,
            read_batch_bytes: 3,
        };
        let result = read_files_concurrently(
            config,
            vec![
                FileReadRequest::new(InputIdentity::new("z/beta.txt", 2), beta),
                FileReadRequest::new(InputIdentity::new("a/alpha.txt", 1), alpha),
            ],
            |input| String::from_utf8(input.bytes().to_vec()).expect("utf-8 fixture"),
        )
        .expect("runtime succeeds");

        assert!(result.failures.is_empty());
        assert_eq!(result.completed.len(), 2);
        assert_eq!(
            result.completed[0].identity.normalized_path.as_ref(),
            "a/alpha.txt"
        );
        assert_eq!(result.completed[0].value, "alpha through small batches");
        assert_eq!(
            result.completed[1].identity.normalized_path.as_ref(),
            "z/beta.txt"
        );
        assert_eq!(result.completed[1].value, "beta through small batches");
        fs::remove_dir_all(&directory).expect("remove test directory");
    }

    #[test]
    fn concurrent_reader_populates_cpu_preflight_blake3_digest() {
        static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "graphoxide-index-runtime-preflight-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("source.txt");
        let bytes = b"hash only after I/O admission";
        fs::write(&path, bytes).expect("write source");

        let result = read_files_concurrently(
            IndexRuntimeConfig {
                memory_budget_bytes: 128 * 1024,
                io_workers: 1,
                compute_workers: 1,
                io_backend: IoBackendSelection::Threaded,
                read_batch_bytes: 7,
            },
            vec![FileReadRequest::new(
                InputIdentity::new("source.txt", 0),
                path,
            )],
            |input| input.content_digest,
        )
        .expect("runtime succeeds");

        assert_eq!(result.failures, Vec::new());
        assert_eq!(
            result.completed[0].value,
            Some(*blake3::hash(bytes).as_bytes())
        );
        fs::remove_dir_all(&directory).expect("remove test directory");
    }

    #[test]
    fn concurrent_reader_rejects_sources_larger_than_request_limit() {
        static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "graphoxide-index-runtime-limit-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let path: PathBuf = directory.join("large.txt");
        fs::write(&path, b"larger than eight bytes").expect("write source");

        let result = read_files_concurrently(
            IndexRuntimeConfig {
                memory_budget_bytes: 128 * 1024,
                io_workers: 1,
                compute_workers: 1,
                io_backend: IoBackendSelection::Threaded,
                read_batch_bytes: DEFAULT_READ_BATCH_BYTES,
            },
            vec![FileReadRequest::new(InputIdentity::new("large.txt", 0), path).with_max_bytes(8)],
            |input| input.bytes().len(),
        )
        .expect("runtime succeeds with a deterministic failure report");

        assert!(result.completed.is_empty());
        assert!(matches!(
            result.failures.as_slice(),
            [FileReadFailure {
                kind: FileReadFailureKind::TooLarge { max_bytes: 8, .. },
                ..
            }]
        ));
        fs::remove_dir_all(&directory).expect("remove test directory");
    }

    #[test]
    fn bounded_layout_clamps_untrusted_worker_counts_to_memory_partitions() {
        let config = IndexRuntimeConfig {
            memory_budget_bytes: 128 * 1024,
            io_workers: usize::MAX,
            compute_workers: usize::MAX,
            io_backend: IoBackendSelection::Threaded,
            read_batch_bytes: usize::MAX,
        };
        config.validate().expect("minimal partitions are usable");

        let layout = config.bounded_layout(10_000);
        assert!(layout.io_workers <= 6, "I/O pool partition bounds owners");
        assert_eq!(
            layout.compute_workers, 1,
            "CPU arena partition bounds workers"
        );
        assert!(layout.io_workers.saturating_mul(layout.compute_workers) <= 6);
        assert!(layout.io_pool_bytes >= MIN_POOL_BUFFER_BYTES);
        assert!(layout.read_batch_bytes <= layout.io_pool_bytes);
    }

    #[test]
    fn io_owner_pool_reuses_standard_class_allocations_without_sharing() {
        let mut pool = IoBufferPool::new(BufferClass::SixteenKiB.capacity());
        let first = pool.take(127);
        let pointer = first.as_bytes().as_ptr();
        assert_eq!(first.class(), BufferClass::FourKiB);
        pool.recycle(first);
        assert_eq!(pool.retained_bytes, BufferClass::FourKiB.capacity());

        let second = pool.take(512);
        assert_eq!(second.as_bytes().as_ptr(), pointer);
        assert_eq!(pool.retained_bytes, 0);
        pool.recycle(second);

        let exact = pool.take(BufferClass::OneMiB.capacity() + 1);
        pool.recycle(exact);
        assert_eq!(pool.retained_bytes, BufferClass::FourKiB.capacity());
    }

    #[test]
    fn injected_io_delay_does_not_block_ready_cpu_work_and_output_is_deterministic() {
        let slow_path = PathBuf::from("virtual/z-slow.txt");
        let fast_path = PathBuf::from("virtual/a-fast.txt");
        let slow_identity = fake_identity(b"slow".len(), 1);
        let gate = Arc::new(ReadGate::new());
        let backend = Arc::new(FakeReadBackend::new([
            (
                slow_path.clone(),
                FakeSourcePlan {
                    probes: VecDeque::from([Ok(slow_identity), Ok(slow_identity)]),
                    opens: VecDeque::from([FakeOpenPlan::Handle(FakeHandlePlan::gated(
                        b"slow",
                        slow_identity,
                        Arc::clone(&gate),
                    ))]),
                },
            ),
            (fast_path.clone(), FakeSourcePlan::stable(b"fast", 2)),
        ]));
        let (computed, received) = mpsc::channel();
        let worker_backend = Arc::clone(&backend);
        let worker = thread::spawn(move || {
            read_files_concurrently_with_backend(
                runtime_config(2, 2),
                vec![
                    FileReadRequest::new(InputIdentity::new("z-slow.txt", 0), slow_path),
                    FileReadRequest::new(InputIdentity::new("a-fast.txt", 1), fast_path),
                ],
                worker_backend,
                move |input| {
                    let ordinal = input.identity.source_ordinal;
                    computed.send(ordinal).expect("report computed input");
                    ordinal
                },
            )
        });

        assert!(gate.wait_until_entered(), "slow I/O read entered its gate");
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(1))
                .expect("fast ready input reaches a CPU worker"),
            1,
            "a ready input on another owner proceeds while slow I/O remains blocked"
        );
        assert!(
            !worker.is_finished(),
            "the coordinator still waits for the deliberately delayed I/O owner"
        );
        gate.release();

        let result = worker
            .join()
            .expect("runtime worker exits")
            .expect("runtime succeeds");
        assert!(result.failures.is_empty());
        assert_eq!(
            result
                .completed
                .iter()
                .map(|completed| completed.value)
                .collect::<Vec<_>>(),
            vec![1, 0],
            "completion output is sorted by identity, not nondeterministic I/O completion order"
        );
        assert_eq!(
            backend.call_count(BackendCall::Read(PathBuf::from("virtual/z-slow.txt"))),
            2,
            "the bounded 3-byte batch reader performs two reads for four bytes"
        );
        assert_eq!(
            backend.call_count(BackendCall::Read(PathBuf::from("virtual/a-fast.txt"))),
            2,
            "the bounded 3-byte batch reader performs two reads for four bytes"
        );
    }

    #[test]
    fn injected_io_failure_is_a_diagnostic_and_never_reaches_cpu_work() {
        let rejected_path = PathBuf::from("virtual/rejected.txt");
        let accepted_path = PathBuf::from("virtual/accepted.txt");
        let backend = Arc::new(FakeReadBackend::new([
            (
                rejected_path.clone(),
                FakeSourcePlan::failing_probe(io::ErrorKind::PermissionDenied),
            ),
            (
                accepted_path.clone(),
                FakeSourcePlan::stable(b"accepted", 4),
            ),
        ]));
        let computed = Arc::new(AtomicUsize::new(0));
        let computed_in_callback = Arc::clone(&computed);
        let result = read_files_concurrently_with_backend(
            runtime_config(2, 2),
            vec![
                FileReadRequest::new(InputIdentity::new("b-rejected.txt", 0), rejected_path),
                FileReadRequest::new(InputIdentity::new("a-accepted.txt", 1), accepted_path),
            ],
            Arc::clone(&backend) as Arc<dyn IoReadBackend>,
            move |input| {
                computed_in_callback.fetch_add(1, Ordering::Relaxed);
                input.identity.normalized_path.to_string()
            },
        )
        .expect("an individual I/O failure is reported, not fatal");

        assert_eq!(computed.load(Ordering::Relaxed), 1);
        assert_eq!(result.completed.len(), 1);
        assert_eq!(result.completed[0].value, "a-accepted.txt");
        assert!(matches!(
            result.failures.as_slice(),
            [FileReadFailure {
                kind: FileReadFailureKind::Io(io::ErrorKind::PermissionDenied),
                ..
            }]
        ));
        assert_eq!(
            backend.call_count(BackendCall::Open(PathBuf::from("virtual/rejected.txt"))),
            0,
            "a failed metadata probe cannot open or enqueue source bytes"
        );
        assert_eq!(
            backend.call_count(BackendCall::Read(PathBuf::from("virtual/rejected.txt"))),
            0,
            "CPU work has no failed-source bytes to consume"
        );
    }

    #[test]
    fn injected_backend_does_not_probe_before_credit_admission() {
        let path = PathBuf::from("virtual/admission.txt");
        let backend = Arc::new(FakeReadBackend::new([(
            path.clone(),
            FakeSourcePlan::stable(b"admitted", 5),
        )]));
        let request = FileReadRequest::new(InputIdentity::new("admission.txt", 0), path.clone());
        let credits = ByteCreditLedger::new(BufferClass::FourKiB.capacity());
        let held = credits
            .try_reserve(BufferClass::FourKiB.capacity())
            .expect("hold all ready-input credit");
        let control = Arc::new(RunControl::new());
        let (sender, receiver) = mpsc::channel();
        let worker_backend = Arc::clone(&backend);
        let worker_credits = credits.clone();
        let worker_control = Arc::clone(&control);
        let worker = thread::spawn(move || {
            let mut buffers = IoBufferPool::new(BufferClass::FourKiB.capacity());
            sender
                .send(read_ready_input(
                    &request,
                    DEFAULT_READ_BATCH_BYTES,
                    &worker_credits,
                    &mut buffers,
                    &worker_control,
                    worker_backend.as_ref(),
                    &NoopReadObserver,
                ))
                .expect("report read admission result");
        });

        assert!(
            matches!(
                receiver.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "request remains parked while all credits are held"
        );
        assert!(
            backend.calls().is_empty(),
            "I/O must not even probe metadata before ready-input admission"
        );
        drop(held);
        let (input, credit) = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("request resumes after credit release")
            .expect("injected source reads successfully");
        assert_eq!(input.bytes(), b"admitted");
        drop(credit);
        worker.join().expect("admission worker exits");
        assert_eq!(credits.available(), credits.capacity());
        assert_eq!(backend.call_count(BackendCall::Probe(path)), 2);
    }

    #[test]
    fn injected_changed_source_retries_then_reports_without_cpu_work() {
        let path = PathBuf::from("virtual/changing.txt");
        let before = fake_identity(b"state!".len(), 6);
        let changed = fake_identity(b"state!".len(), 7);
        let source = FakeSourcePlan {
            probes: (0..FILE_READ_ATTEMPTS)
                .flat_map(|_| [Ok(before), Ok(changed)])
                .collect(),
            opens: (0..FILE_READ_ATTEMPTS)
                .map(|_| {
                    FakeOpenPlan::Handle(FakeHandlePlan {
                        bytes: b"state!".to_vec(),
                        identities: VecDeque::from([Ok(before), Ok(changed)]),
                        read_error: None,
                        gate: None,
                    })
                })
                .collect(),
        };
        let backend = Arc::new(FakeReadBackend::new([(path.clone(), source)]));
        let computed = Arc::new(AtomicUsize::new(0));
        let computed_in_callback = Arc::clone(&computed);
        let result = read_files_concurrently_with_backend(
            runtime_config(1, 1),
            vec![FileReadRequest::new(
                InputIdentity::new("changing.txt", 0),
                path.clone(),
            )],
            Arc::clone(&backend) as Arc<dyn IoReadBackend>,
            move |_| {
                computed_in_callback.fetch_add(1, Ordering::Relaxed);
            },
        )
        .expect("a racing source is a per-file diagnostic");

        assert!(result.completed.is_empty());
        assert_eq!(computed.load(Ordering::Relaxed), 0);
        assert!(matches!(
            result.failures.as_slice(),
            [FileReadFailure {
                kind: FileReadFailureKind::ChangedDuringRead,
                ..
            }]
        ));
        assert_eq!(
            backend.call_count(BackendCall::Open(path.clone())),
            FILE_READ_ATTEMPTS
        );
        assert_eq!(
            backend.call_count(BackendCall::Read(path.clone())),
            FILE_READ_ATTEMPTS * 2,
            "each six-byte attempt uses two bounded 3-byte reads"
        );
        assert_eq!(
            backend.call_count(BackendCall::Probe(path)),
            FILE_READ_ATTEMPTS * 2
        );
    }

    #[test]
    fn verified_generation_rejects_equal_metadata_retarget_before_open() {
        let path = PathBuf::from("virtual/ancestor-junction/source.txt");
        let approved = fake_identity(b"state!".len(), 41);
        let retargeted = fake_identity(b"state!".len(), 42);
        let backend = FakeReadBackend::new([(
            path.clone(),
            FakeSourcePlan {
                probes: VecDeque::from([Ok(retargeted)]),
                opens: VecDeque::from([FakeOpenPlan::Handle(FakeHandlePlan::stable(
                    b"state!", retargeted,
                ))]),
            },
        )]);
        let mut request = FileReadRequest::new(InputIdentity::new("source.txt", 0), path.clone());
        request.expected_identity = Some(approved);
        let credits = ByteCreditLedger::new(BufferClass::FourKiB.capacity());
        let mut buffers = IoBufferPool::new(BufferClass::FourKiB.capacity());
        let control = RunControl::new();

        let failure = read_ready_input(
            &request,
            DEFAULT_READ_BATCH_BYTES,
            &credits,
            &mut buffers,
            &control,
            &backend,
            &NoopReadObserver,
        )
        .expect_err("a retargeted verified path must fail before its source is opened");

        assert_eq!(failure.kind, FileReadFailureKind::ChangedDuringRead);
        assert_eq!(backend.call_count(BackendCall::Open(path)), 0);
        assert_eq!(credits.available(), credits.capacity());
    }

    #[test]
    fn saturated_ready_ring_blocks_admission_before_any_source_read() {
        let (mut producer, mut consumer) = SpscQueue::new(1).expect("queue").split();
        producer.try_send(ComputeWork::End).expect("saturate queue");
        let control = Arc::new(RunControl::new());
        let admitted = Arc::new(AtomicUsize::new(0));
        let admitted_in_thread = Arc::clone(&admitted);
        let control_in_thread = Arc::clone(&control);
        let worker = thread::spawn(move || {
            let reserved = reserve_ready_slot(&producer, &control_in_thread);
            admitted_in_thread.store(usize::from(reserved), Ordering::Release);
        });

        thread::sleep(Duration::from_millis(20));
        assert_eq!(
            admitted.load(Ordering::Acquire),
            0,
            "no I/O request may pass admission while its sole producer ring is full"
        );
        assert!(matches!(consumer.try_recv(), Some(ComputeWork::End)));
        worker.join().expect("admission worker exits");
        assert_eq!(admitted.load(Ordering::Acquire), 1);
    }

    #[test]
    fn held_byte_credit_blocks_even_metadata_io_until_admission_is_available() {
        let credits = ByteCreditLedger::new(BufferClass::FourKiB.capacity());
        let held = credits
            .try_reserve(BufferClass::FourKiB.capacity())
            .expect("hold all admission credit");
        let request = FileReadRequest::new(
            InputIdentity::new("missing.txt", 0),
            std::env::temp_dir().join(format!(
                "graphoxide-index-runtime-missing-{}",
                std::process::id()
            )),
        );
        let control = Arc::new(RunControl::new());
        let (sender, receiver) = mpsc::channel();
        let credits_in_thread = credits.clone();
        let control_in_thread = Arc::clone(&control);
        let worker = thread::spawn(move || {
            let mut buffers = IoBufferPool::new(BufferClass::FourKiB.capacity());
            let backend = FileSystemIoBackend;
            let outcome = read_ready_input(
                &request,
                DEFAULT_READ_BATCH_BYTES,
                &credits_in_thread,
                &mut buffers,
                &control_in_thread,
                &backend,
                &NoopReadObserver,
            );
            sender.send(outcome).expect("report terminal read outcome");
        });

        thread::sleep(Duration::from_millis(20));
        assert!(
            matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "the unavailable admission credit must prevent the metadata probe"
        );
        drop(held);
        let outcome = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("read attempt resumes after credit release");
        assert!(matches!(
            outcome,
            Err(FileReadFailure {
                kind: FileReadFailureKind::Io(std::io::ErrorKind::NotFound),
                ..
            })
        ));
        worker.join().expect("admission worker exits");
    }

    #[test]
    fn concurrent_ticket_credits_do_not_deadlock_exact_source_admission() {
        static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "graphoxide-index-runtime-ticket-upgrade-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let requests = (0..5)
            .map(|ordinal| {
                let path = directory.join(format!("{ordinal}.txt"));
                fs::write(&path, vec![b'x'; 5 * 1024]).expect("write source");
                FileReadRequest::new(InputIdentity::new(format!("{ordinal}.txt"), ordinal), path)
            })
            .collect::<Vec<_>>();
        let before = std::time::Instant::now();
        let result = read_files_concurrently(
            IndexRuntimeConfig {
                memory_budget_bytes: 128 * 1024,
                io_workers: 5,
                compute_workers: 1,
                io_backend: IoBackendSelection::Threaded,
                read_batch_bytes: DEFAULT_READ_BATCH_BYTES,
            },
            requests,
            |input| input.bytes().len(),
        )
        .expect("ticket admission upgrades complete");

        assert!(result.failures.is_empty());
        assert_eq!(result.completed.len(), 5);
        assert!(
            before.elapsed() < Duration::from_secs(2),
            "ticket credits must be released before waiting for exact classes"
        );
        fs::remove_dir_all(&directory).expect("remove test directory");
    }

    #[test]
    fn cancellation_is_terminal_before_any_compute_work() {
        static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "graphoxide-index-runtime-cancel-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("source.txt");
        fs::write(&path, b"source").expect("write source");
        let invoked = Arc::new(AtomicUsize::new(0));
        let invoked_in_compute = Arc::clone(&invoked);
        let cancellation = RuntimeCancellation::new();
        cancellation.cancel();

        let result = read_files_concurrently_with_cancellation(
            IndexRuntimeConfig {
                memory_budget_bytes: 128 * 1024,
                io_workers: 1,
                compute_workers: 1,
                io_backend: IoBackendSelection::Threaded,
                read_batch_bytes: DEFAULT_READ_BATCH_BYTES,
            },
            vec![FileReadRequest::new(
                InputIdentity::new("source.txt", 0),
                path,
            )],
            cancellation,
            move |_| {
                invoked_in_compute.fetch_add(1, Ordering::Relaxed);
            },
        );
        assert!(matches!(result, Err(ConcurrentReadError::Cancelled)));
        assert_eq!(invoked.load(Ordering::Relaxed), 0);
        fs::remove_dir_all(&directory).expect("remove test directory");
    }

    #[test]
    fn cancellation_after_credit_reservation_releases_every_byte() {
        static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "graphoxide-index-runtime-credit-cancel-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("source.txt");
        fs::write(&path, b"source").expect("write source");
        let request = FileReadRequest::new(InputIdentity::new("source.txt", 0), path);
        let credits = ByteCreditLedger::new(BufferClass::FourKiB.capacity());
        let cancellation = RuntimeCancellation::new();
        let observer = CancellingObserver {
            cancellation: cancellation.clone(),
        };
        let backend = FileSystemIoBackend;
        let mut buffers = IoBufferPool::new(BufferClass::FourKiB.capacity());
        let failure = read_ready_input(
            &request,
            DEFAULT_READ_BATCH_BYTES,
            &credits,
            &mut buffers,
            &cancellation.control,
            &backend,
            &observer,
        )
        .expect_err("cancellation wins before open");

        assert_eq!(failure.kind, FileReadFailureKind::Cancelled);
        assert_eq!(credits.available(), credits.capacity());
        fs::remove_dir_all(&directory).expect("remove test directory");
    }

    #[test]
    fn injected_cancellation_after_probe_prevents_open_and_releases_credit() {
        let path = PathBuf::from("virtual/cancelled.txt");
        let backend = Arc::new(FakeReadBackend::new([(
            path.clone(),
            FakeSourcePlan::stable(b"cancelled", 8),
        )]));
        let request = FileReadRequest::new(InputIdentity::new("cancelled.txt", 0), path.clone());
        let credits = ByteCreditLedger::new(BufferClass::FourKiB.capacity());
        let cancellation = RuntimeCancellation::new();
        let observer = CancellingObserver {
            cancellation: cancellation.clone(),
        };
        let mut buffers = IoBufferPool::new(BufferClass::FourKiB.capacity());
        let failure = read_ready_input(
            &request,
            DEFAULT_READ_BATCH_BYTES,
            &credits,
            &mut buffers,
            &cancellation.control,
            backend.as_ref(),
            &observer,
        )
        .expect_err("cancellation after a probe wins before open");

        assert_eq!(failure.kind, FileReadFailureKind::Cancelled);
        assert_eq!(credits.available(), credits.capacity());
        assert_eq!(backend.call_count(BackendCall::Probe(path.clone())), 1);
        assert_eq!(backend.call_count(BackendCall::Open(path.clone())), 0);
        assert_eq!(backend.call_count(BackendCall::Read(path)), 0);
    }

    #[test]
    fn compute_panic_cancels_io_and_returns_without_liveness_failure() {
        static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "graphoxide-index-runtime-panic-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let requests = (0..8)
            .map(|ordinal| {
                let path = directory.join(format!("{ordinal}.txt"));
                fs::write(&path, b"panic fixture").expect("write source");
                FileReadRequest::new(InputIdentity::new(format!("{ordinal}.txt"), ordinal), path)
            })
            .collect::<Vec<_>>();
        let before = std::time::Instant::now();
        let result = read_files_concurrently(
            IndexRuntimeConfig {
                memory_budget_bytes: 128 * 1024,
                io_workers: 1,
                compute_workers: 1,
                io_backend: IoBackendSelection::Threaded,
                read_batch_bytes: 1,
            },
            requests,
            |_| -> () { panic!("intentional compute panic") },
        );

        assert!(matches!(
            result,
            Err(ConcurrentReadError::ComputeWorkerPanicked)
        ));
        assert!(
            before.elapsed() < Duration::from_secs(2),
            "I/O must observe the terminal state instead of waiting on a dead CPU consumer"
        );
        fs::remove_dir_all(&directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn changed_identity_is_retried_then_reported_without_credit_leak() {
        static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "graphoxide-index-runtime-change-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("source.txt");
        fs::write(&path, b"before").expect("write source");
        let request = FileReadRequest::new(InputIdentity::new("source.txt", 0), path.clone());
        let credits = ByteCreditLedger::new(BufferClass::FourKiB.capacity());
        let mut buffers = IoBufferPool::new(BufferClass::FourKiB.capacity());
        let control = RunControl::new();
        let backend = FileSystemIoBackend;
        let failure = read_ready_input(
            &request,
            DEFAULT_READ_BATCH_BYTES,
            &credits,
            &mut buffers,
            &control,
            &backend,
            &ReplacingObserver { path },
        )
        .expect_err("every identity observation changes");

        assert_eq!(failure.kind, FileReadFailureKind::ChangedDuringRead);
        assert_eq!(credits.available(), credits.capacity());
        fs::remove_dir_all(&directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn verified_source_retargeted_after_probe_is_never_followed() {
        let fixture = tempfile::tempdir().expect("temporary retarget fixture");
        let path = fixture.path().join("source.txt");
        let outside = fixture.path().join("outside-secret.txt");
        fs::write(&path, b"approved").expect("write approved source");
        fs::write(&outside, b"outside secret that must not be read").expect("write outside source");
        let request =
            FileReadRequest::new_verified(InputIdentity::new("source.txt", 0), path.clone())
                .expect("bind approved source identity");
        let credits = ByteCreditLedger::new(BufferClass::FourKiB.capacity());
        let mut buffers = IoBufferPool::new(BufferClass::FourKiB.capacity());
        let control = RunControl::new();
        let backend = FileSystemIoBackend;
        let failure = read_ready_input(
            &request,
            DEFAULT_READ_BATCH_BYTES,
            &credits,
            &mut buffers,
            &control,
            &backend,
            &RetargetingObserver {
                path,
                target: outside,
            },
        )
        .expect_err("no-follow open rejects a post-probe retarget");

        assert!(matches!(failure.kind, FileReadFailureKind::Io(_)));
        assert_eq!(credits.available(), credits.capacity());
    }

    #[test]
    fn verified_source_outside_root_is_rejected_before_runtime_admission() {
        let root = tempfile::tempdir().expect("temporary scan root");
        let outside = tempfile::tempdir().expect("temporary outside root");
        let path = outside.path().join("source.txt");
        fs::write(&path, b"outside").expect("write outside source");

        let error = FileReadRequest::new_verified_under(
            InputIdentity::new("alias.txt", 0),
            path,
            root.path(),
        )
        .expect_err("out-of-root source must not receive a runtime ticket");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn source_evidence_binds_canonical_root_and_physical_path() {
        let fixture = tempfile::tempdir().expect("temporary fixture");
        let first_root = fixture.path().join("first");
        let second_root = fixture.path().join("second");
        fs::create_dir(&first_root).expect("first root");
        fs::create_dir(&second_root).expect("second root");
        let first = first_root.join("source.rs");
        let second = second_root.join("alias.rs");
        fs::write(&first, b"fn source() {}\n").expect("source");
        fs::hard_link(&first, &second).expect("same physical source under another root");

        let first_request = FileReadRequest::new_verified_under(
            InputIdentity::new("source.rs", 0),
            first,
            &first_root,
        )
        .expect("first request");
        let second_request = FileReadRequest::new_verified_under(
            InputIdentity::new("alias.rs", 0),
            second,
            &second_root,
        )
        .expect("second request");
        assert_ne!(
            first_request.source_identity_evidence(),
            second_request.source_identity_evidence(),
            "moving the same physical generation with its whole root cannot authorize replay"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_evidence_and_guard_reject_replaced_root_at_the_same_path() {
        let fixture = tempfile::tempdir().expect("temporary fixture");
        let root = fixture.path().join("scan");
        let moved = fixture.path().join("moved-scan");
        fs::create_dir(&root).expect("root");
        let source = root.join("source.rs");
        fs::write(&source, b"fn source() {}\n").expect("source");
        let request = FileReadRequest::new_verified_under(
            InputIdentity::new("source.rs", 0),
            source.clone(),
            &root,
        )
        .expect("original request");
        let original_evidence = request.source_identity_evidence().expect("evidence");
        let guard = request
            .begin_metadata_only_validation(&RuntimeCancellation::new())
            .expect("guard")
            .expect("strong guard");

        fs::rename(&root, &moved).expect("move original root");
        fs::create_dir(&root).expect("replacement root");
        fs::hard_link(moved.join("source.rs"), &source)
            .expect("same physical source in replacement root");
        let replacement =
            FileReadRequest::new_verified_under(InputIdentity::new("source.rs", 0), source, &root)
                .expect("replacement request");
        assert_ne!(
            replacement.source_identity_evidence(),
            Some(original_evidence)
        );
        assert!(!guard
            .finish(&RuntimeCancellation::new())
            .expect("finish old guard"));
    }

    #[cfg(unix)]
    #[test]
    fn metadata_guard_rejects_same_size_rewrite_with_restored_mtime() {
        use std::ffi::CString;
        use std::os::unix::{ffi::OsStrExt as _, fs::MetadataExt as _};

        let root = tempfile::tempdir().expect("temporary root");
        let path = root.path().join("source.rs");
        fs::write(&path, b"old bytes").expect("initial source");
        let metadata = fs::metadata(&path).expect("initial metadata");
        let request = FileReadRequest::new_verified_under(
            InputIdentity::new("source.rs", 0),
            path.clone(),
            root.path(),
        )
        .expect("verified request");
        let guard = request
            .begin_metadata_only_validation(&RuntimeCancellation::new())
            .expect("begin validation")
            .expect("strong identity");

        thread::sleep(std::time::Duration::from_millis(2));
        fs::write(&path, b"new bytes").expect("same-size rewrite");
        let path_c = CString::new(path.as_os_str().as_bytes()).expect("path without NUL");
        let times = [
            libc::timespec {
                tv_sec: metadata.atime(),
                tv_nsec: metadata.atime_nsec() as _,
            },
            libc::timespec {
                tv_sec: metadata.mtime(),
                tv_nsec: metadata.mtime_nsec() as _,
            },
        ];
        // SAFETY: `path_c` is NUL-terminated and `times` contains the two
        // initialized timestamps required by `utimensat`.
        let restored =
            unsafe { libc::utimensat(libc::AT_FDCWD, path_c.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(restored, 0, "restore source mtime");
        assert!(!guard
            .finish(&RuntimeCancellation::new())
            .expect("finish validation"));
    }

    #[test]
    fn verified_read_exports_the_same_bound_source_evidence() {
        let root = tempfile::tempdir().expect("temporary root");
        let path = root.path().join("source.rs");
        fs::write(&path, b"fn source() {}\n").expect("source");
        let request = FileReadRequest::new_verified_under(
            InputIdentity::new("source.rs", 0),
            path,
            root.path(),
        )
        .expect("verified request");
        let expected = request
            .source_identity_evidence()
            .expect("strong request evidence");
        let result = read_files_concurrently(runtime_config(1, 1), [request], |input| {
            input.source_identity_evidence()
        })
        .expect("verified read");
        assert_eq!(result.completed[0].value, Some(expected));
    }

    #[cfg(unix)]
    #[test]
    fn opened_file_validator_is_stable_and_rejects_hardlinks() {
        let root = tempfile::tempdir().expect("temporary root");
        let path = root.path().join("manifest.json");
        fs::write(&path, b"{}").expect("manifest");
        let file = File::open(&path).expect("opened manifest");
        let before = validate_opened_regular_single_link(&file)
            .expect("validate before")
            .expect("strong filesystem identity");
        assert_eq!(before.length_bytes(), 2);
        let after = validate_opened_regular_single_link(&file)
            .expect("validate after")
            .expect("strong filesystem identity");
        assert_eq!(before, after);

        fs::hard_link(&path, root.path().join("alias.json")).expect("hardlink");
        assert_eq!(
            validate_opened_regular_single_link(&file)
                .expect_err("hardlinked manifest rejected")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    struct CancellingObserver {
        cancellation: RuntimeCancellation,
    }

    impl ReadObserver for CancellingObserver {
        fn after_before_identity(&self, _request: &FileReadRequest) {
            self.cancellation.cancel();
        }
    }

    #[cfg(unix)]
    struct ReplacingObserver {
        path: PathBuf,
    }

    #[cfg(unix)]
    impl ReadObserver for ReplacingObserver {
        fn after_before_identity(&self, _request: &FileReadRequest) {
            fs::write(&self.path, b"after!").expect("replace same-length source");
        }
    }

    #[cfg(unix)]
    struct RetargetingObserver {
        path: PathBuf,
        target: PathBuf,
    }

    #[cfg(unix)]
    impl ReadObserver for RetargetingObserver {
        fn after_before_identity(&self, _request: &FileReadRequest) {
            fs::remove_file(&self.path).expect("remove approved source after probe");
            std::os::unix::fs::symlink(&self.target, &self.path)
                .expect("retarget source after probe");
        }
    }
}
